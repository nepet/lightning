#!/usr/bin/env python3
""" A simple implementation of a LSPS2 compatible policy plugin. It is the job
of this plugin to deliver a fee options menu to the LSPS2 service plugin.
"""

from pyln.client import Plugin


plugin = Plugin()


@plugin.hook("htlc_accepted")
def on_htlc_accepted(htlc, onion, plugin, **kwargs):
    print(f"GOT HTLC: {htlc}")
    print(f"GOT HTLC: {onion}")
    return {"result": "continue"}


plugin.run()
