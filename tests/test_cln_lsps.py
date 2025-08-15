from fixtures import *  # noqa: F401,F403
import os

RUST_PROFILE = os.environ.get("RUST_PROFILE", "debug")


def test_lsps_service_disabled(node_factory):
    """By default we disable the LSPS service plugin.

    It should only be enabled if we explicitly set the config option
    `dev-lsps-service-enabled=True`.
    """

    l1 = node_factory.get_node(1)
    l1.daemon.is_in_log("`lsps-service` not enabled")


def test_lsps0_listprotocols(node_factory):
    l1, l2 = node_factory.get_nodes(2, opts=[
        {}, {"dev-lsps-service-enabled": None}
    ])

    # We don't need a channel to query for lsps services
    node_factory.join_nodes([l1, l2], fundchannel=False)

    res = l1.rpc.lsps_listprotocols(peer=l2.info['id'])
    assert res


def test_lsps2_getinfo(node_factory):
    # We need a policy service to fetch from.
    plugin = os.path.join(os.path.dirname(__file__), 'plugins/lsps2_policy.py')

    l1, l2 = node_factory.get_nodes(2, opts=[
        {}, {
            "dev-lsps-service-enabled": None,
            "dev-lsps2-service-enabled": None,
            "dev-lsps2-promise-secret": "00" * 32,
            "plugin": plugin}
    ])

    # We don't need a channel to query for lsps services
    node_factory.join_nodes([l1, l2], fundchannel=False)

    res = l1.rpc.lsps_lsps2_getinfo(lsp_id=l2.info['id'])
    assert res["opening_fee_params_menu"]


def test_lsps2_buy(node_factory):
    # We need a policy service to fetch from.
    plugin = os.path.join(os.path.dirname(__file__), 'plugins/lsps2_policy.py')

    l1, l2 = node_factory.get_nodes(2, opts=[
        {}, {
            "dev-lsps-service-enabled": None,
            "dev-lsps2-service-enabled": None,
            "dev-lsps2-promise-secret": "00" * 32,
            "plugin": plugin}
    ])

    # We don't need a channel to query for lsps services
    node_factory.join_nodes([l1, l2], fundchannel=False)

    res = l1.rpc.lsps_lsps2_getinfo(lsp_id=l2.info['id'])
    params = res["opening_fee_params_menu"][0]

    res = l1.rpc.lsps_lsps2_buy(lsp_id=l2.info['id'], payment_size_msat=None, opening_fee_params=params)
    assert res


def test_lsps2_buyjitchannel_no_mpp_var_invoice(node_factory):
    """ Tests the creation of a "Just-In-Time-Channel" (jit-channel).

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
    plugin = os.path.join(os.path.dirname(__file__), 'plugins/lsps2_policy.py')

    l1, l2, l3 = node_factory.get_nodes(3, opts=[
        {},
        {
            "dev-lsps-service-enabled": None,
            "dev-lsps2-service-enabled": None,
            "dev-lsps2-promise-secret": "00" * 32,
            "plugin": plugin
        },
        {}
    ])

    node_factory.join_nodes([l3, l2], fundchannel=True)
    node_factory.join_nodes([l1, l2], fundchannel=False)

    decoded = l1.rpc.lsps_buyjitchannel(lsp_id=l2.info['id'])
    assert decoded['bolt11']
    bolt11 = decoded['bolt11']

    decoded = l3.rpc.decode(bolt11)
    assert decoded
    route_hint = decoded['routes'][0][0]

    amount_msat = "10000000msat"
    route = l3.rpc.getroute(l2.info['id'], amount_msat, 0)['route']
    direction = 0 if l2.info['id'] < l1.info['id'] else 1
    route.append({
        "id": l1.info["id"],
        "channel": route_hint['short_channel_id'],
        "direction": direction,
        "amount_msat": amount_msat,
        "delay": 5, "style": "tlv"
    })
    res = l3.rpc.sendpay(route, decoded['payment_hash'], payment_secret=decoded['payment_secret'], bolt11=bolt11)
    assert res

    res = l3.rpc.waitsendpay(decoded['payment_hash'])
    assert res
    print(f"SENDPAY {res}")
