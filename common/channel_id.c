#include "config.h"
#include <bitcoin/pubkey.h>
#include <bitcoin/tx.h>
#include <common/channel_id.h>
#include <common/pseudorand.h>
#include <common/utils.h>
#include <wire/wire.h>

void derive_channel_id(struct channel_id *channel_id,
		       const struct bitcoin_outpoint *outpoint)
{
	CROSS_TYPE_ASSIGNMENT(channel_id, &outpoint->txid);
	channel_id->id[sizeof(*channel_id)-2] ^= outpoint->n >> 8;
	channel_id->id[sizeof(*channel_id)-1] ^= outpoint->n;
}

void derive_channel_id_v2(struct channel_id *channel_id,
			  const struct pubkey *basepoint_1,
			  const struct pubkey *basepoint_2)
{
	/* BOLT #2:
	 * `channel_id`, v2
	 *  For channels established using the v2 protocol, the
	 *  `channel_id` is the
	 *      `SHA256(lesser-revocation-basepoint ||
	 *              greater-revocation-basepoint)`,
	 *  where the lesser and greater is based off the order of
	 *  the basepoint.
	 */
	u8 der_keys[PUBKEY_CMPR_LEN * 2];
	struct sha256 sha;
	int offset_1, offset_2;

	/* basepoint_1 is first? */
	if (pubkey_idx(basepoint_1, basepoint_2) == 0) {
		offset_1 = 0;
		offset_2 = PUBKEY_CMPR_LEN;
	} else {
		offset_1 = PUBKEY_CMPR_LEN;
		offset_2 = 0;
	}
	pubkey_to_der(der_keys + offset_1, basepoint_1);
	pubkey_to_der(der_keys + offset_2, basepoint_2);
	sha256(&sha, der_keys, sizeof(der_keys));
	CROSS_TYPE_ASSIGNMENT(channel_id, &sha);
}

void derive_tmp_channel_id(struct channel_id *channel_id,
			   const struct pubkey *opener_basepoint)
{
	struct sha256 sha;

	/* BOLT #2:
	 * When sending `open_channel2`, the peer's revocation basepoint is unknown.
	 * A `temporary_channel_id` must be computed by using a zeroed out basepoint
	 * for the non-initiator.
	 */
	u8 der_keys[PUBKEY_CMPR_LEN * 2];
	memset(der_keys, 0, PUBKEY_CMPR_LEN);
	pubkey_to_der(der_keys + PUBKEY_CMPR_LEN, opener_basepoint);
	sha256(&sha, der_keys, sizeof(der_keys));
	CROSS_TYPE_ASSIGNMENT(channel_id, &sha);
}

/* BOLT #2:
 *
 * The sending node:
 *...
 *  - MUST ensure `temporary_channel_id` is unique from any other channel ID
 *    with the same peer.
 */
void temporary_channel_id(struct channel_id *channel_id)
{
	size_t i;

	/* Randomness FTW. */
	for (i = 0; i < sizeof(*channel_id); i++)
		channel_id->id[i] = pseudorand(256);
}


void towire_channel_id(u8 **pptr, const struct channel_id *channel_id)
{
	towire(pptr, channel_id, sizeof(*channel_id));
}

bool fromwire_channel_id(const u8 **cursor, size_t *max,
			 struct channel_id *channel_id)
{
	return fromwire(cursor, max, channel_id, sizeof(*channel_id)) != NULL;
}

char *fmt_channel_id(const tal_t *ctx, const struct channel_id *channel_id)
{
	return tal_hexstr(ctx, channel_id, sizeof(*channel_id));
}
