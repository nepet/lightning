use anyhow::{Context, Result};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use bitcoin::psbt::{Output, Psbt};
use bitcoin::{Amount, ScriptBuf, TxOut};

/// Weight of a P2WSH funding output in weight units.
/// 8 bytes value + 1 byte scriptLen + 34 bytes script = 43 bytes = 172 WU.
pub const P2WSH_OUTPUT_WEIGHT: u32 = 172;

/// Adds a funding output to an existing PSBT.
///
/// Takes the PSBT from `fundpsbt` (base64-encoded, with wallet inputs and
/// change output) and appends a TxOut for the channel funding address
/// returned by `fundchannel_start`.
///
/// The `scriptpubkey_hex` is the hex-encoded P2WSH script from
/// `FundchannelStartResponse.scriptpubkey`.
pub fn add_funding_output(
    psbt_base64: &str,
    amount_sat: u64,
    scriptpubkey_hex: &str,
) -> Result<String> {
    let psbt_bytes = BASE64
        .decode(psbt_base64)
        .context("decoding PSBT from base64")?;

    let mut psbt =
        Psbt::deserialize(&psbt_bytes).context("deserializing PSBT from BIP-174 binary")?;

    let script_bytes =
        hex::decode(scriptpubkey_hex).context("decoding scriptpubkey from hex")?;

    let txout = TxOut {
        value: Amount::from_sat(amount_sat),
        script_pubkey: ScriptBuf::from(script_bytes),
    };

    psbt.unsigned_tx.output.push(txout);
    psbt.outputs.push(Output::default());

    let serialized = psbt.serialize();
    Ok(BASE64.encode(&serialized))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::blockdata::transaction::{Transaction, TxIn};
    use bitcoin::psbt::Input;
    use bitcoin::{absolute, transaction};

    /// Creates a minimal valid PSBT with one input and no outputs,
    /// returns it as base64.
    fn make_minimal_psbt() -> String {
        let tx = Transaction {
            version: transaction::Version(2),
            lock_time: absolute::LockTime::ZERO,
            input: vec![TxIn::default()],
            output: vec![],
        };
        let psbt = Psbt {
            unsigned_tx: tx,
            version: 0,
            xpub: Default::default(),
            proprietary: Default::default(),
            unknown: Default::default(),
            inputs: vec![Input::default()],
            outputs: vec![],
        };
        let bytes = psbt.serialize();
        BASE64.encode(&bytes)
    }

    #[test]
    fn test_add_funding_output_basic() {
        let psbt_b64 = make_minimal_psbt();
        // P2WSH scriptpubkey: OP_0 <32-byte hash>
        let scriptpubkey_hex = "0020".to_string() + &"ab".repeat(32);
        let amount_sat = 100_000;

        let result = add_funding_output(&psbt_b64, amount_sat, &scriptpubkey_hex).unwrap();

        // Parse the result back
        let result_bytes = BASE64.decode(&result).unwrap();
        let result_psbt = Psbt::deserialize(&result_bytes).unwrap();

        assert_eq!(result_psbt.unsigned_tx.output.len(), 1);
        assert_eq!(result_psbt.outputs.len(), 1);

        let txout = &result_psbt.unsigned_tx.output[0];
        assert_eq!(txout.value, Amount::from_sat(100_000));

        let expected_script = hex::decode(&scriptpubkey_hex).unwrap();
        assert_eq!(txout.script_pubkey.as_bytes(), &expected_script[..]);
    }

    #[test]
    fn test_add_funding_output_preserves_existing() {
        // Create a PSBT that already has a change output
        let change_script = ScriptBuf::from(hex::decode("0014").unwrap());
        let change_txout = TxOut {
            value: Amount::from_sat(50_000),
            script_pubkey: change_script.clone(),
        };

        let tx = Transaction {
            version: transaction::Version(2),
            lock_time: absolute::LockTime::ZERO,
            input: vec![TxIn::default()],
            output: vec![change_txout],
        };
        let psbt = Psbt {
            unsigned_tx: tx,
            version: 0,
            xpub: Default::default(),
            proprietary: Default::default(),
            unknown: Default::default(),
            inputs: vec![Input::default()],
            outputs: vec![Output::default()],
        };
        let psbt_b64 = BASE64.encode(&psbt.serialize());

        let scriptpubkey_hex = "0020".to_string() + &"cd".repeat(32);
        let result = add_funding_output(&psbt_b64, 200_000, &scriptpubkey_hex).unwrap();

        let result_bytes = BASE64.decode(&result).unwrap();
        let result_psbt = Psbt::deserialize(&result_bytes).unwrap();

        // Should have 2 outputs now: change + funding
        assert_eq!(result_psbt.unsigned_tx.output.len(), 2);
        assert_eq!(result_psbt.outputs.len(), 2);

        // First output is the original change
        assert_eq!(
            result_psbt.unsigned_tx.output[0].value,
            Amount::from_sat(50_000)
        );
        assert_eq!(
            result_psbt.unsigned_tx.output[0].script_pubkey,
            change_script
        );

        // Second output is the funding
        assert_eq!(
            result_psbt.unsigned_tx.output[1].value,
            Amount::from_sat(200_000)
        );
    }

    #[test]
    fn test_add_funding_output_invalid_base64() {
        let result = add_funding_output("not-valid-base64!!!", 100_000, "0020ab");
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("decoding PSBT from base64")
        );
    }

    #[test]
    fn test_add_funding_output_invalid_psbt() {
        let bad_psbt = BASE64.encode(b"not a psbt");
        let result = add_funding_output(&bad_psbt, 100_000, "0020ab");
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("deserializing PSBT")
        );
    }

    #[test]
    fn test_add_funding_output_invalid_hex() {
        let psbt_b64 = make_minimal_psbt();
        let result = add_funding_output(&psbt_b64, 100_000, "not_hex!");
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("decoding scriptpubkey")
        );
    }

    #[test]
    fn test_p2wsh_output_weight_constant() {
        // P2WSH output: 8 (value) + 1 (script_len) + 34 (0x0020 + 32-byte hash) = 43 bytes
        // In weight units: 43 * 4 = 172 (non-witness data is multiplied by 4)
        assert_eq!(P2WSH_OUTPUT_WEIGHT, 172);
    }
}
