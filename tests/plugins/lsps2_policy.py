#!/usr/bin/env python3
""" A simple implementation of a LSPS2 compatible policy plugin. It is the job
of this plugin to deliver a fee options menu to the LSPS2 service plugin.
"""

from pyln.client import Plugin


plugin = Plugin()


@plugin.method("dev-lsps2-getpolicy")
def lsps2_getpolicy(request):
    """ Returns a opening fee menu for the LSPS2 plugin.
    """
    return { "policy_opening_fee_params_menu": [
        {
            "min_fee_msat": "1000",
            "proportional": 1000,
            "valid_until": "3000-02-23T08:47:30.511Z",
            "min_lifetime": 2000,
            "max_client_to_self_delay": 144,
            "min_payment_size_msat": "1000",
            "max_payment_size_msat": "100000000000",
        },
        {
            "min_fee_msat": "1092000",
            "proportional": 2400,
            "valid_until": "2023-02-27T21:23:57.984Z",
            "min_lifetime": 1008,
            "max_client_to_self_delay": 2016,
            "min_payment_size_msat": "1000",
            "max_payment_size_msat": "1000000",
        }
    ]
}


plugin.run()
