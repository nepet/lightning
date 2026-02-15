#!/usr/bin/env python3
"""A test-specific LSPS2 policy plugin with configurable timeouts.

Extends the basic lsps2_policy.py with runtime-configurable valid_until
duration for testing timeout and expiration scenarios.

Configuration:
    --lsps2-test-valid-until-seconds: Number of seconds until offer expires
                                      (default: 3600 = 1 hour)
"""

from pyln.client import Plugin
from datetime import datetime, timedelta, timezone


plugin = Plugin()

# Default: 1 hour (same as lsps2_policy.py)
valid_until_seconds = 3600


@plugin.init()
def init(options, configuration, plugin, **kwargs):
    global valid_until_seconds
    val = plugin.get_option("lsps2-test-valid-until-seconds")
    if val is not None:
        valid_until_seconds = int(val)
    plugin.log(f"lsps2_test_policy: valid_until_seconds={valid_until_seconds}")


plugin.add_option(
    "lsps2-test-valid-until-seconds",
    3600,
    "Number of seconds until the opening fee params expire",
)


@plugin.method("lsps2-policy-getpolicy")
def lsps2_policy_getpolicy(request):
    """Returns an opening fee menu with configurable valid_until."""
    now = datetime.now(timezone.utc)
    valid_until = (now + timedelta(seconds=valid_until_seconds)).isoformat().replace(
        "+00:00", "Z"
    )

    return {
        "policy_opening_fee_params_menu": [
            {
                "min_fee_msat": "1000",
                "proportional": 1000,
                "valid_until": valid_until,
                "min_lifetime": 2000,
                "max_client_to_self_delay": 2016,
                "min_payment_size_msat": "1000",
                "max_payment_size_msat": "100000000",
            },
        ]
    }


@plugin.method("lsps2-policy-getchannelcapacity")
def lsps2_policy_getchannelcapacity(request, init_payment_size, scid, opening_fee_params):
    """Returns channel capacity for the JIT channel."""
    return {"channel_capacity_msat": 100000000}


plugin.run()
