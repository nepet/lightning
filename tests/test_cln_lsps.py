from fixtures import *  # noqa: F401,F403
from pyln.client import RpcError
from pyln.testing.utils import RUST
from utils import only_one
import os
import pytest
import time
import unittest

RUST_PROFILE = os.environ.get("RUST_PROFILE", "debug")


# ============================================================================
# MPP Test Helper Functions
# ============================================================================


def setup_lsps2_mpp_topology(node_factory, bitcoind, policy_plugin=None, extra_l2_opts=None):
    """Create a 3-node topology for LSPS2 MPP testing.

    Topology:
             (LSP)
        l1    l2----l3
      client  LSP   payer

    Args:
        node_factory: pytest node factory fixture
        bitcoind: pytest bitcoind fixture
        policy_plugin: path to policy plugin (defaults to lsps2_policy.py)
        extra_l2_opts: additional options dict for the LSP node (l2)

    Returns:
        tuple: (l1, l2, l3, chanid) where chanid is the l3<->l2 channel
    """
    if policy_plugin is None:
        policy_plugin = os.path.join(
            os.path.dirname(__file__), "plugins/lsps2_policy.py"
        )

    l2_opts = {
        "experimental-lsps2-service": None,
        "experimental-lsps2-promise-secret": "0" * 64,
        "plugin": policy_plugin,
        "fee-base": 0,
        "fee-per-satoshi": 0,
    }
    if extra_l2_opts:
        l2_opts.update(extra_l2_opts)

    l1, l2, l3 = node_factory.get_nodes(
        3,
        opts=[
            {"experimental-lsps-client": None},
            l2_opts,
            {},
        ],
    )

    # Give the LSP funds to open JIT channels
    l2.fundwallet(1_000_000)

    # Create channel between payer and LSP
    node_factory.join_nodes([l3, l2], fundchannel=True, wait_for_announce=True)

    # Connect client to LSP (no funded channel yet)
    node_factory.join_nodes([l1, l2], fundchannel=False)

    # Get the channel ID between l3 and l2
    chanid = only_one(l3.rpc.listpeerchannels(l2.info["id"])["channels"])[
        "short_channel_id"
    ]

    return l1, l2, l3, chanid


def buy_and_create_invoice(client, lsp, amount_msat):
    """Buy JIT channel and create invoice with route hint.

    Args:
        client: LSPS2 client node (l1)
        lsp: LSP node (l2)
        amount_msat: Payment amount in millisatoshis (int)

    Returns:
        dict with keys:
            - invoice: raw invoice response
            - bolt11: bolt11 string
            - decoded: decoded invoice
            - routehint: extracted route hint for the JIT channel
            - payment_hash: payment hash
            - payment_secret: payment secret
            - opening_fee_params: the fee params used
    """
    # Get fee parameters from LSP
    info = client.rpc.lsps_lsps2_getinfo(lsp_id=lsp.info["id"])
    opening_fee_params = info["opening_fee_params_menu"][0]

    # Buy JIT channel with expected payment size
    client.rpc.lsps_lsps2_buy(
        lsp_id=lsp.info["id"],
        opening_fee_params=opening_fee_params,
        amount_msat=f"{amount_msat}msat",
    )

    # Create invoice with route hint to JIT channel
    invoice = client.rpc.lsps_lsps2_invoice(
        lsp_id=lsp.info["id"],
        amount_msat=f"{amount_msat}msat",
        description="mpp-test-invoice",
        label=f"mpp-test-{amount_msat}-{time.time()}",
    )

    # Decode the invoice
    decoded = client.rpc.decode(invoice["bolt11"])

    # Extract route hint (should be exactly one route with one hop)
    routehint = only_one(only_one(decoded["routes"]))

    return {
        "invoice": invoice,
        "bolt11": invoice["bolt11"],
        "decoded": decoded,
        "routehint": routehint,
        "payment_hash": decoded["payment_hash"],
        "payment_secret": invoice["payment_secret"],
        "opening_fee_params": opening_fee_params,
    }


def send_mpp_parts(payer, lsp, client, chanid, invoice_data, num_parts):
    """Send MPP payment with specified number of parts.

    Args:
        payer: Payer node (l3)
        lsp: LSP node (l2)
        client: Client node (l1)
        chanid: Channel ID between payer and LSP
        invoice_data: Dict returned by buy_and_create_invoice
        num_parts: Number of HTLC parts to send

    Returns:
        waitsendpay result containing payment_preimage on success
    """
    amount_msat = invoice_data["decoded"]["amount_msat"]
    # Handle both int and string with 'msat' suffix
    if isinstance(amount_msat, str):
        amount_msat = int(amount_msat.replace("msat", ""))

    routehint = invoice_data["routehint"]
    payment_hash = invoice_data["payment_hash"]
    payment_secret = invoice_data["payment_secret"]
    bolt11 = invoice_data["bolt11"]

    # Calculate amount per part
    amount_per_part = amount_msat // num_parts

    # Build route for each part
    route_part = [
        {
            "amount_msat": amount_per_part,
            "id": lsp.info["id"],
            "delay": routehint["cltv_expiry_delta"] + 6,
            "channel": chanid,
        },
        {
            "amount_msat": amount_per_part,
            "id": client.info["id"],
            "delay": 6,
            "channel": routehint["short_channel_id"],
        },
    ]

    # Send all parts
    for partid in range(1, num_parts + 1):
        payer.rpc.sendpay(
            route_part,
            payment_hash,
            payment_secret=payment_secret,
            bolt11=bolt11,
            amount_msat=f"{amount_msat}msat",
            groupid=1,
            partid=partid,
        )

    # Wait for payment to complete
    result = payer.rpc.waitsendpay(payment_hash, partid=num_parts, groupid=1)
    return result


def send_mpp_parts_no_wait(payer, lsp, client, chanid, invoice_data, num_parts):
    """Send MPP payment parts without waiting for completion.

    Same as send_mpp_parts but does not call waitsendpay. Useful for
    testing error cases where we expect the payment to fail.

    Args:
        payer: Payer node (l3)
        lsp: LSP node (l2)
        client: Client node (l1)
        chanid: Channel ID between payer and LSP
        invoice_data: Dict returned by buy_and_create_invoice
        num_parts: Number of HTLC parts to send
    """
    amount_msat = invoice_data["decoded"]["amount_msat"]
    if isinstance(amount_msat, str):
        amount_msat = int(amount_msat.replace("msat", ""))

    routehint = invoice_data["routehint"]
    payment_hash = invoice_data["payment_hash"]
    payment_secret = invoice_data["payment_secret"]
    bolt11 = invoice_data["bolt11"]

    amount_per_part = amount_msat // num_parts

    route_part = [
        {
            "amount_msat": amount_per_part,
            "id": lsp.info["id"],
            "delay": routehint["cltv_expiry_delta"] + 6,
            "channel": chanid,
        },
        {
            "amount_msat": amount_per_part,
            "id": client.info["id"],
            "delay": 6,
            "channel": routehint["short_channel_id"],
        },
    ]

    for partid in range(1, num_parts + 1):
        payer.rpc.sendpay(
            route_part,
            payment_hash,
            payment_secret=payment_secret,
            bolt11=bolt11,
            amount_msat=f"{amount_msat}msat",
            groupid=1,
            partid=partid,
        )


def wait_for_jit_channel(client, timeout=30):
    """Wait for JIT channel to appear on client.

    Args:
        client: Client node to check for channels
        timeout: Maximum seconds to wait

    Returns:
        Channel info dict

    Raises:
        TimeoutError: If no channel appears within timeout
    """
    start = time.time()
    while time.time() - start < timeout:
        channels = client.rpc.listpeerchannels()["channels"]
        if channels:
            return channels[0]
        time.sleep(0.5)
    raise TimeoutError(f"No JIT channel appeared within {timeout} seconds")


def verify_cleanup(client):
    """Assert that LSPS datastore is cleaned up after test.

    Args:
        client: Client node to check
    """
    assert client.rpc.listdatastore(["lsps"]) == {"datastore": []}


def test_lsps_service_disabled(node_factory):
    """By default we disable the LSPS service plugin.

    It should only be enabled if we explicitly set the config option
    `lsps-service=True`.
    """

    l1 = node_factory.get_node(1)
    l1.daemon.is_in_log("`lsps-service` not enabled")


@unittest.skipUnless(RUST, "RUST is not enabled")
def test_lsps0_listprotocols(node_factory):
    l1, l2 = node_factory.get_nodes(
        2,
        opts=[
            {"experimental-lsps-client": None},
            {
                "experimental-lsps2-service": None,
                "experimental-lsps2-promise-secret": "0" * 64,
            },
        ],
    )

    # We don't need a channel to query for lsps services
    node_factory.join_nodes([l1, l2], fundchannel=False)

    res = l1.rpc.lsps_listprotocols(lsp_id=l2.info["id"])
    assert res


def test_lsps2_enabled(node_factory):
    l1, l2 = node_factory.get_nodes(
        2,
        opts=[
            {"experimental-lsps-client": None},
            {
                "experimental-lsps2-service": None,
                "experimental-lsps2-promise-secret": "0" * 64,
            },
        ],
    )

    node_factory.join_nodes([l1, l2], fundchannel=False)

    res = l1.rpc.lsps_listprotocols(lsp_id=l2.info["id"])
    assert res["protocols"] == [2]


def test_lsps2_getinfo(node_factory):
    plugin = os.path.join(os.path.dirname(__file__), "plugins/lsps2_policy.py")

    l1, l2 = node_factory.get_nodes(
        2,
        opts=[
            {"experimental-lsps-client": None},
            {
                "experimental-lsps2-service": None,
                "experimental-lsps2-promise-secret": "0" * 64,
                "plugin": plugin,
            },
        ],
    )

    node_factory.join_nodes([l1, l2], fundchannel=False)

    res = l1.rpc.lsps_lsps2_getinfo(lsp_id=l2.info["id"])
    assert res["opening_fee_params_menu"]


def test_lsps2_buy(node_factory):
    # We need a policy service to fetch from.
    plugin = os.path.join(os.path.dirname(__file__), "plugins/lsps2_policy.py")

    l1, l2 = node_factory.get_nodes(
        2,
        opts=[
            {"experimental-lsps-client": None},
            {
                "experimental-lsps2-service": None,
                "experimental-lsps2-promise-secret": "0" * 64,
                "plugin": plugin,
            },
        ],
    )

    # We don't need a channel to query for lsps services
    node_factory.join_nodes([l1, l2], fundchannel=False)

    res = l1.rpc.lsps_lsps2_getinfo(lsp_id=l2.info["id"])
    params = res["opening_fee_params_menu"][0]

    res = l1.rpc.lsps_lsps2_buy(lsp_id=l2.info["id"], opening_fee_params=params)
    assert res


def test_lsps2_buyjitchannel_no_mpp_var_invoice(node_factory, bitcoind):
    """Tests the creation of a "Just-In-Time-Channel" (jit-channel).

    At the beginning we have the following situation where l2 acts as the LSP
         (LSP)
    l1    l2----l3

    l1 now wants to get a channel from l2 via the lsps2 jit-channel protocol:
    - l1 requests a new jit channel form l2
    - l1 creates an invoice based on the opening fee parameters it got from l2
    - l3 pays the invoice
    - l2 opens a channel to l1 and forwards the payment (deducted by a fee)

    eventualy this will result in the following situation
         (LSP)
    l1----l2----l3
    """
    # We need a policy service to fetch from.
    plugin = os.path.join(os.path.dirname(__file__), "plugins/lsps2_policy.py")

    l1, l2, l3 = node_factory.get_nodes(
        3,
        opts=[
            {"experimental-lsps-client": None},
            {
                "experimental-lsps2-service": None,
                "experimental-lsps2-promise-secret": "0" * 64,
                "plugin": plugin,
                "fee-base": 0,  # We are going to deduct our fee anyways,
                "fee-per-satoshi": 0,  # We are going to deduct our fee anyways,
            },
            {},
        ],
    )

    # Give the LSP some funds to open jit-channels
    l2.fundwallet(1_000_000)

    node_factory.join_nodes([l3, l2], fundchannel=True, wait_for_announce=True)
    node_factory.join_nodes([l1, l2], fundchannel=False)

    chanid = only_one(l3.rpc.listpeerchannels(l2.info["id"])["channels"])[
        "short_channel_id"
    ]

    inv = l1.rpc.lsps_lsps2_invoice(
        lsp_id=l2.info["id"],
        amount_msat="any",
        description="lsp-jit-channel-0",
        label="lsp-jit-channel-0",
    )
    assert inv

    dec = l3.rpc.decode(inv["bolt11"])
    assert dec

    routehint = only_one(only_one(dec["routes"]))

    amt = 10000000

    route = [
        {"amount_msat": amt, "id": l2.info["id"], "delay": 14, "channel": chanid},
        {
            "amount_msat": amt,
            "id": l1.info["id"],
            "delay": 8,
            "channel": routehint["short_channel_id"],
        },
    ]

    l3.rpc.sendpay(
        route,
        dec["payment_hash"],
        payment_secret=inv["payment_secret"],
        bolt11=inv["bolt11"],
        partid=0,
    )

    res = l3.rpc.waitsendpay(dec["payment_hash"])
    assert res["payment_preimage"]

    # l1 should have gotten a jit-channel.
    chs = l1.rpc.listpeerchannels()["channels"]
    assert len(chs) == 1

    # Check that the client cleaned up after themselves.
    assert l1.rpc.listdatastore(["lsps"]) == {"datastore": []}


def test_lsps2_buyjitchannel_mpp_fixed_invoice(node_factory, bitcoind):
    """Tests the creation of a "Just-In-Time-Channel" (jit-channel).

    At the beginning we have the following situation where l2 acts as the LSP
         (LSP)
    l1    l2----l3

    l1 now wants to get a channel from l2 via the lsps2 jit-channel protocol:
    - l1 requests a new jit channel form l2
    - l1 creates an invoice based on the opening fee parameters it got from l2
    - l3 pays the invoice
    - l2 opens a channel to l1 and forwards the payment (deducted by a fee)

    eventualy this will result in the following situation
         (LSP)
    l1----l2----l3
    """
    # A mock for lsps2 mpp payments, contains the policy plugin as well.
    plugin = os.path.join(os.path.dirname(__file__), "plugins/lsps2_service_mock.py")

    l1, l2, l3 = node_factory.get_nodes(
        3,
        opts=[
            {"experimental-lsps-client": None},
            {
                "experimental-lsps2-service": None,
                "experimental-lsps2-promise-secret": "0" * 64,
                "plugin": plugin,
                "fee-base": 0,  # We are going to deduct our fee anyways,
                "fee-per-satoshi": 0,  # We are going to deduct our fee anyways,
            },
            {},
        ],
    )

    # Give the LSP some funds to open jit-channels
    l2.fundwallet(1_000_000)

    node_factory.join_nodes([l3, l2], fundchannel=True, wait_for_announce=True)
    node_factory.join_nodes([l1, l2], fundchannel=False)

    chanid = only_one(l3.rpc.listpeerchannels(l2.info["id"])["channels"])[
        "short_channel_id"
    ]

    amt = 10_000_000
    inv = l1.rpc.lsps_lsps2_invoice(
        lsp_id=l2.info["id"],
        amount_msat=f"{amt}msat",
        description="lsp-jit-channel-0",
        label="lsp-jit-channel-0",
    )
    dec = l3.rpc.decode(inv["bolt11"])

    l2.rpc.setuplsps2service(
        peer_id=l1.info["id"], channel_cap=100_000, opening_fee_msat=1000_000
    )

    routehint = only_one(only_one(dec["routes"]))

    parts = 10
    route_part = [
        {
            "amount_msat": amt // parts,
            "id": l2.info["id"],
            "delay": routehint["cltv_expiry_delta"] + 6,
            "channel": chanid,
        },
        {
            "amount_msat": amt // parts,
            "id": l1.info["id"],
            "delay": 6,
            "channel": routehint["short_channel_id"],
        },
    ]

    # MPP-payment of fixed amount
    for partid in range(1, parts + 1):
        r = l3.rpc.sendpay(
            route_part,
            dec["payment_hash"],
            payment_secret=inv["payment_secret"],
            bolt11=inv["bolt11"],
            amount_msat=f"{amt}msat",
            groupid=1,
            partid=partid,
        )
        assert r

    res = l3.rpc.waitsendpay(dec["payment_hash"], partid=parts, groupid=1)
    assert res["payment_preimage"]

    # l1 should have gotten a jit-channel.
    chs = l1.rpc.listpeerchannels()["channels"]
    assert len(chs) == 1

    # Check that the client cleaned up after themselves.
    assert l1.rpc.listdatastore("lsps") == {"datastore": []}


def test_lsps2_non_approved_zero_conf(node_factory, bitcoind):
    """Checks that we don't allow zerof_conf channels from an LSP if we did
    not approve it first.
    """
    # We need a policy service to fetch from.
    plugin = os.path.join(os.path.dirname(__file__), "plugins/lsps2_policy.py")

    l1, l2, l3 = node_factory.get_nodes(
        3,
        opts=[
            {"experimental-lsps-client": None},
            {
                "experimental-lsps2-service": None,
                "experimental-lsps2-promise-secret": "0" * 64,
                "plugin": plugin,
                "fee-base": 0,  # We are going to deduct our fee anyways,
                "fee-per-satoshi": 0,  # We are going to deduct our fee anyways,
            },
            {"disable-mpp": None},
        ],
    )

    # Give the LSP some funds to open jit-channels
    l2.fundwallet(1_000_000)

    node_factory.join_nodes([l3, l2], fundchannel=True, wait_for_announce=True)
    node_factory.join_nodes([l1, l2], fundchannel=False)

    fee_opt = l1.rpc.lsps_lsps2_getinfo(lsp_id=l2.info["id"])[
        "opening_fee_params_menu"
    ][0]
    buy_res = l1.rpc.lsps_lsps2_buy(lsp_id=l2.info["id"], opening_fee_params=fee_opt)

    hint = [
        [
            {
                "id": l2.info["id"],
                "short_channel_id": buy_res["jit_channel_scid"],
                "fee_base_msat": 0,
                "fee_proportional_millionths": 0,
                "cltv_expiry_delta": buy_res["lsp_cltv_expiry_delta"],
            }
        ]
    ]

    bolt11 = l1.dev_invoice(
        amount_msat="any",
        description="lsp-invoice-1",
        label="lsp-invoice-1",
        dev_routes=hint,
    )["bolt11"]

    with pytest.raises(ValueError):
        l3.rpc.pay(bolt11, amount_msat=10000000)

    # l1 shouldn't have a new channel.
    chs = l1.rpc.listpeerchannels()["channels"]
    assert len(chs) == 0


# ============================================================================
# MPP Integration Tests (using Rust implementation, not Python mock)
# ============================================================================


@unittest.skipUnless(RUST, "RUST is not enabled")
def test_lsps2_mpp_happy_path_fixed_invoice(node_factory, bitcoind):
    """Tests MPP JIT channel creation using the Rust service implementation.

    This test verifies the complete MPP flow:
    1. Client buys JIT channel with fixed payment size
    2. Client creates invoice with route hint
    3. Payer sends multiple HTLC parts
    4. LSP collects parts, opens zero-conf channel
    5. LSP forwards HTLCs after channel ready
    6. Client receives payment, session completes

    Setup: 3-node topology with l1 (client), l2 (LSP with Rust service), l3 (payer)
    Action: Client buys JIT channel, payer sends 10-part MPP payment
    Expected: JIT channel created, payment succeeds, datastore cleaned up

    Note: This test uses lsps2_policy.py (NOT lsps2_service_mock.py) to test
    the actual Rust MPP implementation in service.rs.
    """
    # Setup topology: l1 (client) -- l2 (LSP) -- l3 (payer)
    l1, l2, l3, chanid = setup_lsps2_mpp_topology(node_factory, bitcoind)

    # Payment amount: 10,000,000 msat (will be split into 10 parts)
    amount_msat = 10_000_000

    # Buy JIT channel and create invoice
    invoice_data = buy_and_create_invoice(l1, l2, amount_msat)

    # Send MPP payment with 10 parts (1,000,000 msat each)
    result = send_mpp_parts(l3, l2, l1, chanid, invoice_data, num_parts=10)

    # Verify payment succeeded
    assert result["payment_preimage"], "Payment should have a preimage"

    # Verify JIT channel was created
    channel = wait_for_jit_channel(l1)
    assert channel, "JIT channel should exist"

    # Verify client datastore is cleaned up
    verify_cleanup(l1)


@unittest.skipUnless(RUST, "RUST is not enabled")
def test_lsps2_mpp_many_parts(node_factory, bitcoind):
    """Tests MPP JIT channel with many HTLC parts (20 parts).

    Verifies that the state machine handles a large number of concurrent HTLC
    parts correctly: all parts are collected, channel opens after sum is
    reached, and all HTLCs are forwarded and settle.

    Setup: 3-node topology with l1 (client), l2 (LSP), l3 (payer)
    Action: Client buys JIT channel, payer sends 20-part MPP payment
    Expected: All parts collected, channel created, payment succeeds
    """
    l1, l2, l3, chanid = setup_lsps2_mpp_topology(node_factory, bitcoind)

    # Payment amount: 10,000,000 msat split into 20 parts (500,000 msat each)
    amount_msat = 10_000_000

    # Buy JIT channel and create invoice
    invoice_data = buy_and_create_invoice(l1, l2, amount_msat)

    # Send MPP payment with 20 parts
    result = send_mpp_parts(l3, l2, l1, chanid, invoice_data, num_parts=20)

    # Verify payment succeeded
    assert result["payment_preimage"], "Payment should have a preimage"

    # Verify JIT channel was created
    channel = wait_for_jit_channel(l1)
    assert channel, "JIT channel should exist"

    # Verify client datastore is cleaned up
    verify_cleanup(l1)


@unittest.skipUnless(RUST, "RUST is not enabled")
def test_lsps2_mpp_collect_timeout(node_factory, bitcoind):
    """Tests that incomplete MPP sessions fail after collect timeout.

    Setup: 3-node topology with l2 configured for short collect timeout (5s)
    Action: Client buys JIT channel, payer sends only 5 of 10 parts (partial)
    Expected: After ~5s timeout, all held HTLCs fail with temporary_channel_failure,
              no channel is created
    """
    l1, l2, l3, chanid = setup_lsps2_mpp_topology(
        node_factory, bitcoind,
        extra_l2_opts={"lsps2-collect-timeout": 5},
    )

    amount_msat = 10_000_000

    # Buy JIT channel and create invoice
    invoice_data = buy_and_create_invoice(l1, l2, amount_msat)

    routehint = invoice_data["routehint"]
    payment_hash = invoice_data["payment_hash"]
    payment_secret = invoice_data["payment_secret"]
    bolt11 = invoice_data["bolt11"]

    # Send only 5 partial parts, each carrying 1/10th of the total.
    # Total sent = 5 * (10_000_000 / 10) = 5_000_000, which is less than
    # the 10_000_000 needed. The sum won't be reached.
    total_split = 10
    send_count = 5
    amount_per_part = amount_msat // total_split

    route_part = [
        {
            "amount_msat": amount_per_part,
            "id": l2.info["id"],
            "delay": routehint["cltv_expiry_delta"] + 6,
            "channel": chanid,
        },
        {
            "amount_msat": amount_per_part,
            "id": l1.info["id"],
            "delay": 6,
            "channel": routehint["short_channel_id"],
        },
    ]

    for partid in range(1, send_count + 1):
        l3.rpc.sendpay(
            route_part,
            payment_hash,
            payment_secret=payment_secret,
            bolt11=bolt11,
            amount_msat=f"{amount_msat}msat",
            groupid=1,
            partid=partid,
        )

    # Wait for collect timeout to fire (~5s + 1s check interval + buffer)
    # waitsendpay will block until the HTLC is failed back
    with pytest.raises(RpcError) as exc_info:
        l3.rpc.waitsendpay(payment_hash, partid=1, groupid=1, timeout=15)

    # Verify payment failed with temporary_channel_failure (wire code 0x1007 = 4103)
    assert exc_info.value.error["code"] == 204
    assert exc_info.value.error["data"]["failcode"] == 4103

    # Verify no JIT channel was created
    channels = l1.rpc.listpeerchannels()["channels"]
    assert len(channels) == 0, "No channel should be created on timeout"


@unittest.skipUnless(RUST, "RUST is not enabled")
def test_lsps2_mpp_valid_until_expired(node_factory, bitcoind):
    """Tests that sessions fail when valid_until timestamp expires.

    Setup: 3-node topology, buy JIT channel normally, then modify the
           datastore entry to have a short valid_until before sending HTLCs.
    Action: Client buys JIT channel, datastore valid_until is shortened,
            payer sends only partial payment
    Expected: After valid_until expires, all held HTLCs fail with
              unknown_next_peer, no channel is created

    Note: We modify the datastore after buy because the client plugin
    requires valid_until > now + 1 minute for safety.
    """
    from datetime import datetime, timedelta, timezone
    import json

    l1, l2, l3, chanid = setup_lsps2_mpp_topology(node_factory, bitcoind)

    amount_msat = 10_000_000

    # Buy JIT channel and create invoice (uses normal 1h valid_until)
    invoice_data = buy_and_create_invoice(l1, l2, amount_msat)

    routehint = invoice_data["routehint"]
    payment_hash = invoice_data["payment_hash"]
    payment_secret = invoice_data["payment_secret"]
    bolt11 = invoice_data["bolt11"]
    scid = routehint["short_channel_id"]

    # Modify the datastore entry to have a very short valid_until (5 seconds)
    ds_key = ["lsps", "lsps2", scid]
    ds_entries = l2.rpc.listdatastore(ds_key)["datastore"]
    assert len(ds_entries) == 1, "Expected exactly one datastore entry for SCID"

    entry_data = json.loads(ds_entries[0]["string"])
    short_valid_until = (
        datetime.now(timezone.utc) + timedelta(seconds=5)
    ).isoformat().replace("+00:00", "Z")
    entry_data["opening_fee_params"]["valid_until"] = short_valid_until

    # Update the datastore entry with the short valid_until
    l2.rpc.datastore(
        key=ds_key,
        string=json.dumps(entry_data),
        mode="must-replace",
    )

    # Send only 5 partial parts (half the amount needed)
    total_split = 10
    send_count = 5
    amount_per_part = amount_msat // total_split

    route_part = [
        {
            "amount_msat": amount_per_part,
            "id": l2.info["id"],
            "delay": routehint["cltv_expiry_delta"] + 6,
            "channel": chanid,
        },
        {
            "amount_msat": amount_per_part,
            "id": l1.info["id"],
            "delay": 6,
            "channel": routehint["short_channel_id"],
        },
    ]

    for partid in range(1, send_count + 1):
        l3.rpc.sendpay(
            route_part,
            payment_hash,
            payment_secret=payment_secret,
            bolt11=bolt11,
            amount_msat=f"{amount_msat}msat",
            groupid=1,
            partid=partid,
        )

    # Wait for valid_until to expire (~5s + 1s check interval + buffer)
    with pytest.raises(RpcError) as exc_info:
        l3.rpc.waitsendpay(payment_hash, partid=1, groupid=1, timeout=15)

    # Verify payment failed with unknown_next_peer (wire code 0x400a = 16394)
    assert exc_info.value.error["code"] == 204
    assert exc_info.value.error["data"]["failcode"] == 16394

    # Verify no JIT channel was created
    channels = l1.rpc.listpeerchannels()["channels"]
    assert len(channels) == 0, "No channel should be created on valid_until expiry"
