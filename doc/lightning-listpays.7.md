lightning-listpays -- Command for querying payment status
=========================================================

SYNOPSIS
--------

**listpays** [*bolt11*] [*payment_hash*] [*status*]

DESCRIPTION
-----------

The **listpay** RPC command gets the status of all *pay* commands, or a
single one if either *bolt11* or *payment_hash* was specified.
It is possible filter the payments also by *status*.

RETURN VALUE
------------

[comment]: # (GENERATE-FROM-SCHEMA-START)
On success, an object containing **pays** is returned.  It is an array of objects, where each object contains:
- **payment_hash** (hex): the hash of the *payment_preimage* which will prove payment (always 64 characters)
- **status** (string): status of the payment (one of "pending", "failed", "complete")
- **created_at** (u64): the UNIX timestamp showing when this payment was initiated
- **destination** (pubkey, optional): the final destination of the payment if known
- **completed_at** (u64, optional): the UNIX timestamp showing when this payment was completed
- **label** (string, optional): the label, if given to sendpay
- **bolt11** (string, optional): the bolt11 string (if pay supplied one)
- **description** (string, optional): the description matching the bolt11 description hash (if pay supplied one)
- **bolt12** (string, optional): the bolt12 string (if supplied for pay: **experimental-offers** only).

If **status** is "complete":
  - **preimage** (hex): proof of payment (always 64 characters)
  - **number_of_parts** (u64, optional): the number of parts for a successful payment (only if more than one).

If **status** is "failed":
  - **erroronion** (hex, optional): the error onion returned on failure, if any.

[comment]: # (GENERATE-FROM-SCHEMA-END)

The returned array is ordered by increasing **created_at** fields.

AUTHOR
------

Rusty Russell <<rusty@rustcorp.com.au>> is mainly responsible.

SEE ALSO
--------

lightning-pay(7), lightning-paystatus(7), lightning-listsendpays(7).

RESOURCES
---------

Main web site: <https://github.com/ElementsProject/lightning>

[comment]: # ( SHA256STAMP:b98b1d549d4368438ad377d404c703c9bcff2030907496f3c7911ad7c9053006)
