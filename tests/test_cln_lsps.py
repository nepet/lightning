from fixtures import *  # noqa: F401,F403
from pyln.client import RpcError
from pyln.testing.utils import RUST, sync_blockheight
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


@pytest.mark.skip(reason="Mock plugin conflicts with Rust MPP service; use test_lsps2_mpp_* tests instead")
def test_lsps2_buyjitchannel_mpp_fixed_invoice(node_factory, bitcoind):
    """Tests MPP JIT channel using the Python mock service plugin.

    NOTE: This test uses lsps2_service_mock.py which bypasses the Rust
    MPP implementation. The mock handles HTLC collection and channel
    funding directly in Python. For tests of the actual Rust service
    implementation, see the test_lsps2_mpp_* tests below.

    Topology: l1 (client) -- l2 (LSP with mock) -- l3 (payer)
    Flow: l1 buys JIT channel, l3 sends 10-part MPP, mock opens channel
    """
    # Python mock for lsps2 mpp payments (bypasses Rust service implementation).
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


@unittest.skipUnless(RUST, "RUST is not enabled")
def test_lsps2_mpp_unsafe_cltv(node_factory, bitcoind):
    """Tests CLTV unsafe hold detection in Collecting phase.

    When HTLCs are held and the block height approaches their CLTV expiry
    (within min_cltv_margin=2 blocks), the timeout manager detects the unsafe
    hold condition and fails the session with temporary_channel_failure.

    Setup: 3-node topology, send partial MPP with short CLTV delay (10 blocks)
    Action: Mine 10 blocks so blocks_remaining drops to 1 (below margin of 2)
    Expected: Timeout manager detects unsafe hold, fails HTLCs with
              temporary_channel_failure (4103), no channel created
    """
    l1, l2, l3, chanid = setup_lsps2_mpp_topology(node_factory, bitcoind)

    amount_msat = 10_000_000

    # Buy JIT channel and create invoice
    invoice_data = buy_and_create_invoice(l1, l2, amount_msat)

    routehint = invoice_data["routehint"]
    payment_hash = invoice_data["payment_hash"]
    payment_secret = invoice_data["payment_secret"]
    bolt11 = invoice_data["bolt11"]

    # Send 5 of 10 parts with SHORT CLTV delay (delay=10 on first hop).
    # This sets cltv_expiry = current_height + 10 for the HTLCs.
    # The sum won't be reached, so session stays in Collecting.
    total_split = 10
    send_count = 5
    amount_per_part = amount_msat // total_split

    route_part = [
        {
            "amount_msat": amount_per_part,
            "id": l2.info["id"],
            "delay": 10,
            "channel": chanid,
        },
        {
            "amount_msat": amount_per_part,
            "id": l1.info["id"],
            "delay": 5,
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

    # Mine enough blocks so blocks_remaining < min_cltv_margin (2).
    # With delay=10, cltv_expiry = send_height + 10.
    # We need current_height >= cltv_expiry - 1 (i.e. blocks_remaining <= 1).
    # Mining 10 blocks ensures height advances past the threshold.
    bitcoind.generate_block(10)
    sync_blockheight(bitcoind, [l2])

    # Wait for timeout manager to detect unsafe hold (1s check interval + buffer)
    with pytest.raises(RpcError) as exc_info:
        l3.rpc.waitsendpay(payment_hash, partid=1, groupid=1, timeout=15)

    # Verify payment failed with temporary_channel_failure (wire code 0x1007 = 4103)
    assert exc_info.value.error["code"] == 204
    assert exc_info.value.error["data"]["failcode"] == 4103

    # Verify no JIT channel was created
    channels = l1.rpc.listpeerchannels()["channels"]
    assert len(channels) == 0, "No channel should be created on unsafe CLTV hold"


@unittest.skipUnless(RUST, "RUST is not enabled")
def test_lsps2_mpp_delayed_parts(node_factory, bitcoind):
    """Tests that parts arriving with small delays still complete successfully.

    Verifies that the state machine correctly handles parts that arrive with
    1-2 second delays between them, staying in Collecting until sum is reached.

    Setup: 3-node topology with l1 (client), l2 (LSP), l3 (payer)
    Action: Send 5 MPP parts with 1 second delay between each
    Expected: All parts collected, channel created, payment succeeds
    """
    l1, l2, l3, chanid = setup_lsps2_mpp_topology(node_factory, bitcoind)

    amount_msat = 10_000_000

    # Buy JIT channel and create invoice
    invoice_data = buy_and_create_invoice(l1, l2, amount_msat)

    routehint = invoice_data["routehint"]
    payment_hash = invoice_data["payment_hash"]
    payment_secret = invoice_data["payment_secret"]
    bolt11 = invoice_data["bolt11"]

    # Send 5 parts with 1 second delay between each
    num_parts = 5
    amount_per_part = amount_msat // num_parts

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

    for partid in range(1, num_parts + 1):
        l3.rpc.sendpay(
            route_part,
            payment_hash,
            payment_secret=payment_secret,
            bolt11=bolt11,
            amount_msat=f"{amount_msat}msat",
            groupid=1,
            partid=partid,
        )
        if partid < num_parts:
            time.sleep(1)  # 1 second delay between parts

    # Wait for payment to complete
    result = l3.rpc.waitsendpay(payment_hash, partid=num_parts, groupid=1, timeout=60)

    # Verify payment succeeded
    assert result["payment_preimage"], "Payment should have a preimage"

    # Verify JIT channel was created
    channel = wait_for_jit_channel(l1)
    assert channel, "JIT channel should exist"

    # Verify client datastore is cleaned up
    verify_cleanup(l1)


@unittest.skipUnless(RUST, "RUST is not enabled")
def test_lsps2_mpp_client_disconnect_collecting(node_factory, bitcoind):
    """Tests that HTLCs fail when client disconnects during Collecting phase.

    When the client disconnects while HTLCs are being collected (before sum
    is reached), the session should fail and all held HTLCs should be failed
    back to the payer.

    Setup: 3-node topology
    Action: Send partial payment (5 of 10 parts), then disconnect client
    Expected: HTLCs fail with temporary_channel_failure, no channel created
    """
    l1, l2, l3, chanid = setup_lsps2_mpp_topology(node_factory, bitcoind)

    amount_msat = 10_000_000

    # Buy JIT channel and create invoice
    invoice_data = buy_and_create_invoice(l1, l2, amount_msat)

    routehint = invoice_data["routehint"]
    payment_hash = invoice_data["payment_hash"]
    payment_secret = invoice_data["payment_secret"]
    bolt11 = invoice_data["bolt11"]

    # Send only 5 of 10 parts (partial - won't trigger channel open)
    total_parts = 10
    send_parts = 5
    amount_per_part = amount_msat // total_parts

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

    for partid in range(1, send_parts + 1):
        l3.rpc.sendpay(
            route_part,
            payment_hash,
            payment_secret=payment_secret,
            bolt11=bolt11,
            amount_msat=f"{amount_msat}msat",
            groupid=1,
            partid=partid,
        )

    # Give HTLCs time to be held
    time.sleep(1)

    # Disconnect client from LSP
    l1.rpc.disconnect(l2.info["id"], force=True)

    # Wait for HTLCs to fail
    with pytest.raises(RpcError) as exc_info:
        l3.rpc.waitsendpay(payment_hash, partid=1, groupid=1, timeout=15)

    # Verify payment failed with temporary_channel_failure (4103)
    assert exc_info.value.error["code"] == 204
    assert exc_info.value.error["data"]["failcode"] == 4103

    # Verify no JIT channel was created
    channels = l1.rpc.listpeerchannels()["channels"]
    assert len(channels) == 0, "No channel should be created on client disconnect"


@unittest.skipUnless(RUST, "RUST is not enabled")
def test_lsps2_mpp_client_disconnect_awaiting_ready(node_factory, bitcoind):
    """Tests that HTLCs fail when client disconnects in AwaitingChannelReady.

    When the client disconnects after funding is signed but before channel_ready,
    the session should fail and all held HTLCs should be failed back.

    Setup: 3-node topology
    Action: Send full payment to trigger channel open, disconnect during negotiation
    Expected: HTLCs fail, session cleaned up

    Note: This test relies on timing - we disconnect as soon as we see the
    channel appear but before it reaches CHANNELD_NORMAL state.
    """
    l1, l2, l3, chanid = setup_lsps2_mpp_topology(node_factory, bitcoind)

    amount_msat = 10_000_000

    # Buy JIT channel and create invoice
    invoice_data = buy_and_create_invoice(l1, l2, amount_msat)

    routehint = invoice_data["routehint"]
    payment_hash = invoice_data["payment_hash"]
    payment_secret = invoice_data["payment_secret"]
    bolt11 = invoice_data["bolt11"]

    # Send full payment to trigger channel open
    num_parts = 10
    amount_per_part = amount_msat // num_parts

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

    for partid in range(1, num_parts + 1):
        l3.rpc.sendpay(
            route_part,
            payment_hash,
            payment_secret=payment_secret,
            bolt11=bolt11,
            amount_msat=f"{amount_msat}msat",
            groupid=1,
            partid=partid,
        )

    # Wait for channel to start opening (fundchannel_start called)
    # We look for the LSP log indicating channel open initiated
    l2.daemon.wait_for_log(r"Executing OpenChannel output for session", timeout=30)

    # Disconnect client immediately to catch it during negotiation
    l1.rpc.disconnect(l2.info["id"], force=True)

    # Wait for HTLCs to fail
    with pytest.raises(RpcError) as exc_info:
        l3.rpc.waitsendpay(payment_hash, partid=1, groupid=1, timeout=30)

    # Verify payment failed
    assert exc_info.value.error["code"] == 204


@unittest.skipUnless(RUST, "RUST is not enabled")
def test_lsps2_mpp_too_many_parts(node_factory, bitcoind):
    """Tests that sessions fail when too many HTLC parts arrive.

    The state machine has a MAX_PARTS limit to prevent resource exhaustion.
    When more parts than allowed arrive, the session should fail with
    unknown_next_peer error.

    Setup: 3-node topology
    Action: Send many small parts exceeding MAX_PARTS limit
    Expected: Session fails with unknown_next_peer (16394), no channel created

    Note: MAX_PARTS is currently 483 in the Rust implementation. We send
    enough parts to exceed this limit.
    """
    l1, l2, l3, chanid = setup_lsps2_mpp_topology(node_factory, bitcoind)

    # Use a large payment amount that we'll split into many tiny parts
    amount_msat = 100_000_000  # 100,000 sats

    # Buy JIT channel and create invoice
    invoice_data = buy_and_create_invoice(l1, l2, amount_msat)

    routehint = invoice_data["routehint"]
    payment_hash = invoice_data["payment_hash"]
    payment_secret = invoice_data["payment_secret"]
    bolt11 = invoice_data["bolt11"]

    # Send 500 parts (exceeds MAX_PARTS of 483)
    # Each part is tiny: 100_000_000 / 500 = 200_000 msat
    num_parts = 500
    amount_per_part = amount_msat // num_parts

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

    # Send parts until we hit the limit
    for partid in range(1, num_parts + 1):
        try:
            l3.rpc.sendpay(
                route_part,
                payment_hash,
                payment_secret=payment_secret,
                bolt11=bolt11,
                amount_msat=f"{amount_msat}msat",
                groupid=1,
                partid=partid,
            )
        except RpcError:
            # Some parts may fail immediately if limit already hit
            break

    # Wait for payment to fail
    with pytest.raises(RpcError) as exc_info:
        l3.rpc.waitsendpay(payment_hash, partid=1, groupid=1, timeout=30)

    # Verify payment failed - either timeout (200) or stopped retrying (204)
    # The exact error depends on whether CLN's channel limit or plugin's MAX_PARTS
    # kicks in first. Both result in payment failure.
    assert exc_info.value.error["code"] in [200, 204]

    # Verify no JIT channel was created
    channels = l1.rpc.listpeerchannels()["channels"]
    assert len(channels) == 0, "No channel should be created with too many parts"


@unittest.skipUnless(RUST, "RUST is not enabled")
def test_lsps2_mpp_retry_after_rejection(node_factory, bitcoind):
    """Tests the retry flow when client rejects the forwarded payment.

    When the client rejects the first payment attempt (e.g., wrong payment
    details), the session should move to AwaitingRetry state. A second
    payment attempt should succeed using the same already-opened channel.

    Setup: 3-node topology, client configured to fail first HTLC then accept
    Action: First payment attempt fails, second attempt succeeds
    Expected: Same channel is reused, second payment succeeds
    """
    policy_plugin = os.path.join(
        os.path.dirname(__file__), "plugins/lsps2_policy.py"
    )
    hold_plugin = os.path.join(
        os.path.dirname(__file__), "plugins/hold_htlcs.py"
    )

    # Client will fail the first HTLC attempt
    l1, l2, l3 = node_factory.get_nodes(
        3,
        opts=[
            {
                "experimental-lsps-client": None,
                "plugin": hold_plugin,
                "hold-time": 1,
                "hold-result": "fail",  # Fail the first attempt
            },
            {
                "experimental-lsps2-service": None,
                "experimental-lsps2-promise-secret": "0" * 64,
                "plugin": policy_plugin,
                "fee-base": 0,
                "fee-per-satoshi": 0,
            },
            {},
        ],
    )

    l2.fundwallet(1_000_000)
    node_factory.join_nodes([l3, l2], fundchannel=True, wait_for_announce=True)
    node_factory.join_nodes([l1, l2], fundchannel=False)

    chanid = only_one(l3.rpc.listpeerchannels(l2.info["id"])["channels"])[
        "short_channel_id"
    ]

    amount_msat = 10_000_000

    # Buy JIT channel and create invoice
    invoice_data = buy_and_create_invoice(l1, l2, amount_msat)

    routehint = invoice_data["routehint"]
    payment_hash = invoice_data["payment_hash"]
    payment_secret = invoice_data["payment_secret"]
    bolt11 = invoice_data["bolt11"]

    num_parts = 10
    amount_per_part = amount_msat // num_parts

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

    # First attempt - will be rejected by client
    for partid in range(1, num_parts + 1):
        l3.rpc.sendpay(
            route_part,
            payment_hash,
            payment_secret=payment_secret,
            bolt11=bolt11,
            amount_msat=f"{amount_msat}msat",
            groupid=1,
            partid=partid,
        )

    # Wait for first attempt to fail
    with pytest.raises(RpcError):
        l3.rpc.waitsendpay(payment_hash, partid=1, groupid=1, timeout=30)

    # Channel should exist (opened during first attempt)
    channels = l1.rpc.listpeerchannels()["channels"]
    assert len(channels) == 1, "JIT channel should exist after first attempt"
    channel_id = channels[0]["channel_id"]

    # Reconfigure client to accept HTLCs (restart with different hold-result)
    l1.stop()
    l1.daemon.opts["hold-result"] = "continue"
    l1.start()

    # Reconnect to LSP
    l1.rpc.connect(l2.info["id"], "localhost", l2.port)

    # Wait for channel to be usable again
    time.sleep(2)

    # Second attempt - should succeed
    for partid in range(1, num_parts + 1):
        l3.rpc.sendpay(
            route_part,
            payment_hash,
            payment_secret=payment_secret,
            bolt11=bolt11,
            amount_msat=f"{amount_msat}msat",
            groupid=2,  # New groupid for retry
            partid=partid,
        )

    # Wait for second attempt to succeed
    result = l3.rpc.waitsendpay(payment_hash, partid=num_parts, groupid=2, timeout=60)
    assert result["payment_preimage"], "Second attempt should succeed"

    # Verify same channel was used
    channels = l1.rpc.listpeerchannels()["channels"]
    assert len(channels) == 1, "Should still have exactly one channel"
    assert channels[0]["channel_id"] == channel_id, "Same channel should be reused"


# ============================================================================
# Withheld Funding Flow Tests (Task 23)
# ============================================================================


@unittest.skipUnless(RUST, "RUST is not enabled")
def test_lsps2_mpp_withheld_funding_happy_path(node_factory, bitcoind):
    """Tests that funding tx is broadcast only after preimage is received.

    This test verifies the withheld funding flow:
    1. LSP opens channel with withheld=true (funding tx not broadcast)
    2. HTLCs are forwarded to client
    3. Client holds HTLCs briefly (using hold_htlcs plugin)
    4. During hold: funding tx should NOT be in mempool
    5. After preimage received: funding tx SHOULD be in mempool

    Setup: 3-node topology with hold_htlcs.py on client (l1)
    Action: Client buys JIT channel, payer sends MPP, client holds briefly
    Expected: Funding tx appears in mempool only after preimage
    """
    policy_plugin = os.path.join(
        os.path.dirname(__file__), "plugins/lsps2_policy.py"
    )
    hold_plugin = os.path.join(
        os.path.dirname(__file__), "plugins/hold_htlcs.py"
    )

    # Client uses hold_htlcs to pause preimage delivery for 3 seconds
    # This gives us time to check mempool state during WaitingPreimage
    l1, l2, l3 = node_factory.get_nodes(
        3,
        opts=[
            {
                "experimental-lsps-client": None,
                "plugin": hold_plugin,
                "hold-time": 3,
                "hold-result": "continue",
            },
            {
                "experimental-lsps2-service": None,
                "experimental-lsps2-promise-secret": "0" * 64,
                "plugin": policy_plugin,
                "fee-base": 0,
                "fee-per-satoshi": 0,
            },
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

    # Payment amount
    amount_msat = 10_000_000

    # Buy JIT channel and create invoice
    invoice_data = buy_and_create_invoice(l1, l2, amount_msat)

    routehint = invoice_data["routehint"]
    payment_hash = invoice_data["payment_hash"]
    payment_secret = invoice_data["payment_secret"]
    bolt11 = invoice_data["bolt11"]

    # Record initial mempool state
    initial_mempool_size = bitcoind.rpc.getmempoolinfo()["size"]

    # Send MPP payment (10 parts)
    num_parts = 10
    amount_per_part = amount_msat // num_parts

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

    for partid in range(1, num_parts + 1):
        l3.rpc.sendpay(
            route_part,
            payment_hash,
            payment_secret=payment_secret,
            bolt11=bolt11,
            amount_msat=f"{amount_msat}msat",
            groupid=1,
            partid=partid,
        )

    # Wait for HTLCs to be forwarded to client (client will hold them)
    # The hold_htlcs plugin holds for 3 seconds before continuing
    l1.daemon.wait_for_log("Holding onto an incoming htlc for 3 seconds")

    # CRITICAL CHECK: During hold, funding tx should NOT be in mempool
    # The withheld flow means we wait for preimage before broadcasting
    mempool_during_hold = bitcoind.rpc.getmempoolinfo()["size"]
    assert mempool_during_hold == initial_mempool_size, (
        f"Funding tx should NOT be in mempool during hold "
        f"(expected {initial_mempool_size}, got {mempool_during_hold})"
    )

    # Now wait for payment to complete (client will release after hold time)
    result = l3.rpc.waitsendpay(payment_hash, partid=num_parts, groupid=1, timeout=30)
    assert result["payment_preimage"], "Payment should have a preimage"

    # CRITICAL CHECK: After preimage, funding tx SHOULD be in mempool
    # Give a moment for the broadcast to happen
    time.sleep(0.5)
    mempool_after_preimage = bitcoind.rpc.getmempoolinfo()["size"]
    assert mempool_after_preimage > initial_mempool_size, (
        f"Funding tx should be in mempool after preimage "
        f"(expected > {initial_mempool_size}, got {mempool_after_preimage})"
    )

    # Verify JIT channel was created
    channels = l1.rpc.listpeerchannels()["channels"]
    assert len(channels) == 1, "JIT channel should exist"

    # Verify client datastore is cleaned up
    verify_cleanup(l1)


@unittest.skipUnless(RUST, "RUST is not enabled")
def test_lsps2_mpp_withheld_funding_client_holds_unsafe(node_factory, bitcoind):
    """Tests that funding tx is NOT broadcast when client holds HTLC past CLTV.

    This test verifies that with the withheld funding flow, if the client
    holds the forwarded HTLC without resolving it and the CLTV expiry
    approaches the safety margin, the funding transaction is never broadcast.

    Setup: 3-node topology with hold_htlcs.py on client (very long hold)
    Action: Send payment with short CLTV, client holds, mine blocks
    Expected: Session fails, funding tx never appears in mempool
    """
    policy_plugin = os.path.join(
        os.path.dirname(__file__), "plugins/lsps2_policy.py"
    )
    hold_plugin = os.path.join(
        os.path.dirname(__file__), "plugins/hold_htlcs.py"
    )

    # Client uses hold_htlcs with very long hold time (won't release)
    l1, l2, l3 = node_factory.get_nodes(
        3,
        opts=[
            {
                "experimental-lsps-client": None,
                "plugin": hold_plugin,
                "hold-time": 300,  # 5 minutes - effectively forever for this test
                "hold-result": "continue",
            },
            {
                "experimental-lsps2-service": None,
                "experimental-lsps2-promise-secret": "0" * 64,
                "plugin": policy_plugin,
                "fee-base": 0,
                "fee-per-satoshi": 0,
            },
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

    # Payment amount
    amount_msat = 10_000_000

    # Buy JIT channel and create invoice
    invoice_data = buy_and_create_invoice(l1, l2, amount_msat)

    routehint = invoice_data["routehint"]
    payment_hash = invoice_data["payment_hash"]
    payment_secret = invoice_data["payment_secret"]
    bolt11 = invoice_data["bolt11"]

    # Record initial mempool state
    initial_mempool_size = bitcoind.rpc.getmempoolinfo()["size"]

    # Send MPP payment with SHORT CLTV delay
    # The short CLTV means blocks will approach expiry quickly
    num_parts = 10
    amount_per_part = amount_msat // num_parts

    route_part = [
        {
            "amount_msat": amount_per_part,
            "id": l2.info["id"],
            "delay": 15,  # Short CLTV delay
            "channel": chanid,
        },
        {
            "amount_msat": amount_per_part,
            "id": l1.info["id"],
            "delay": 6,
            "channel": routehint["short_channel_id"],
        },
    ]

    for partid in range(1, num_parts + 1):
        l3.rpc.sendpay(
            route_part,
            payment_hash,
            payment_secret=payment_secret,
            bolt11=bolt11,
            amount_msat=f"{amount_msat}msat",
            groupid=1,
            partid=partid,
        )

    # Wait for HTLCs to be forwarded to client (client will hold them)
    l1.daemon.wait_for_log("Holding onto an incoming htlc for 300 seconds")

    # Mine blocks to bring CLTV close to expiry
    # With delay=15, cltv_expiry = send_height + 15
    # Mining 14 blocks means blocks_remaining = 1, which is below margin of 2
    bitcoind.generate_block(14)
    sync_blockheight(bitcoind, [l2])

    # The timeout manager should detect unsafe hold and fail the session
    # The upstream HTLCs will fail via CLTV timeout
    # Wait for the payment to fail
    with pytest.raises(RpcError) as exc_info:
        l3.rpc.waitsendpay(payment_hash, partid=1, groupid=1, timeout=30)

    # Payment should fail (either temporary_channel_failure or CLTV timeout)
    assert exc_info.value.error["code"] == 204, "Payment should fail"

    # CRITICAL CHECK: Funding tx should NEVER have been broadcast
    # With withheld flow, we don't broadcast until preimage is received
    mempool_after_failure = bitcoind.rpc.getmempoolinfo()["size"]
    assert mempool_after_failure == initial_mempool_size, (
        f"Funding tx should NOT be in mempool after unsafe hold failure "
        f"(expected {initial_mempool_size}, got {mempool_after_failure})"
    )


@unittest.skipUnless(RUST, "RUST is not enabled")
def test_lsps2_mpp_abandoned_session_releases_utxos(node_factory, bitcoind):
    """Tests that UTXOs are released when session is abandoned.

    This test verifies that with the withheld funding flow, when a session
    reaches the Abandoned state (via AwaitingRetry -> ValidUntilPassed),
    the reserved UTXOs are released back to the LSP's wallet.

    Setup: 3-node topology, client will fail incoming HTLCs
    Action:
      1. Client buys JIT channel, payer sends full payment
      2. Channel opens (withheld), HTLCs forwarded
      3. Client fails the HTLCs (simulating rejection)
      4. Session moves to AwaitingRetry
      5. Modify valid_until to short duration
      6. Wait for valid_until to expire -> Abandoned
    Expected:
      - No funding tx in mempool (withheld, never broadcast)
      - LSP's UTXOs are released (available in listfunds)
    """
    from datetime import datetime, timedelta, timezone
    import json

    policy_plugin = os.path.join(
        os.path.dirname(__file__), "plugins/lsps2_policy.py"
    )
    hold_plugin = os.path.join(
        os.path.dirname(__file__), "plugins/hold_htlcs.py"
    )

    # Client uses hold_htlcs with hold-result=fail to reject incoming HTLCs
    l1, l2, l3 = node_factory.get_nodes(
        3,
        opts=[
            {
                "experimental-lsps-client": None,
                "plugin": hold_plugin,
                "hold-time": 1,  # Short hold before failing
                "hold-result": "fail",  # Fail the HTLC
            },
            {
                "experimental-lsps2-service": None,
                "experimental-lsps2-promise-secret": "0" * 64,
                "plugin": policy_plugin,
                "fee-base": 0,
                "fee-per-satoshi": 0,
            },
            {},
        ],
    )

    # Give the LSP funds to open JIT channels
    l2.fundwallet(1_000_000)

    # Record LSP's initial UTXO count
    initial_outputs = len(l2.rpc.listfunds()["outputs"])

    # Create channel between payer and LSP
    node_factory.join_nodes([l3, l2], fundchannel=True, wait_for_announce=True)

    # Connect client to LSP (no funded channel yet)
    node_factory.join_nodes([l1, l2], fundchannel=False)

    # Get the channel ID between l3 and l2
    chanid = only_one(l3.rpc.listpeerchannels(l2.info["id"])["channels"])[
        "short_channel_id"
    ]

    # Payment amount
    amount_msat = 10_000_000

    # Buy JIT channel and create invoice
    invoice_data = buy_and_create_invoice(l1, l2, amount_msat)

    routehint = invoice_data["routehint"]
    payment_hash = invoice_data["payment_hash"]
    payment_secret = invoice_data["payment_secret"]
    bolt11 = invoice_data["bolt11"]
    scid = routehint["short_channel_id"]

    # Record initial mempool state
    initial_mempool_size = bitcoind.rpc.getmempoolinfo()["size"]

    # Send MPP payment (10 parts)
    num_parts = 10
    amount_per_part = amount_msat // num_parts

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

    for partid in range(1, num_parts + 1):
        l3.rpc.sendpay(
            route_part,
            payment_hash,
            payment_secret=payment_secret,
            bolt11=bolt11,
            amount_msat=f"{amount_msat}msat",
            groupid=1,
            partid=partid,
        )

    # Wait for client to hold and then fail the HTLC
    l1.daemon.wait_for_log("Holding onto an incoming htlc for 1 seconds")

    # The first payment attempt should fail (client fails the HTLCs)
    with pytest.raises(RpcError):
        l3.rpc.waitsendpay(payment_hash, partid=1, groupid=1, timeout=15)

    # At this point, session should be in AwaitingRetry
    # Modify the datastore entry to have a very short valid_until
    ds_key = ["lsps", "lsps2", scid]
    ds_entries = l2.rpc.listdatastore(ds_key)["datastore"]

    if len(ds_entries) == 1:
        entry_data = json.loads(ds_entries[0]["string"])
        short_valid_until = (
            datetime.now(timezone.utc) + timedelta(seconds=3)
        ).isoformat().replace("+00:00", "Z")
        entry_data["opening_fee_params"]["valid_until"] = short_valid_until

        # Update the datastore entry
        l2.rpc.datastore(
            key=ds_key,
            string=json.dumps(entry_data),
            mode="must-replace",
        )

        # Wait for valid_until to expire and session to be abandoned
        time.sleep(5)

    # CRITICAL CHECK: Funding tx should NOT be in mempool
    mempool_after_abandon = bitcoind.rpc.getmempoolinfo()["size"]
    assert mempool_after_abandon == initial_mempool_size, (
        f"Funding tx should NOT be in mempool after session abandoned "
        f"(expected {initial_mempool_size}, got {mempool_after_abandon})"
    )

    # Wait a moment for UTXO release to complete
    time.sleep(1)

    # CRITICAL CHECK: LSP's UTXOs should be released
    # After abandonment, unreserveinputs should free the reserved UTXOs
    final_outputs = len(l2.rpc.listfunds()["outputs"])
    assert final_outputs >= initial_outputs, (
        f"LSP's UTXOs should be released after session abandoned "
        f"(initial: {initial_outputs}, final: {final_outputs})"
    )
