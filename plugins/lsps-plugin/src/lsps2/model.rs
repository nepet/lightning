use crate::{
    jsonrpc::JsonRpcRequest,
    lsps0::primitives::{DateTime, Msat, Ppm, ShortChannelId},
};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Lsps2GetInfoRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
}

impl JsonRpcRequest for Lsps2GetInfoRequest {
    const METHOD: &'static str = "lsps2.get_info";
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Lsps2GetInfoResponse {
    pub opening_fee_params_menu: Vec<OpeningFeeParams>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum PromiseError {
    TooLong { length: usize, max: usize },
}

impl core::fmt::Display for PromiseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PromiseError::TooLong { length, max } => {
                write!(
                    f,
                    "promise string is too long: {} bytes (max allowed {})",
                    length, max
                )
            }
        }
    }
}

impl core::error::Error for PromiseError {}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "String")]
pub struct Promise(String);

impl Promise {
    pub const MAX_BYTES: usize = 512;
}

impl TryFrom<String> for Promise {
    type Error = PromiseError;

    fn try_from(s: String) -> Result<Self, Self::Error> {
        let len = s.len();
        if len <= Promise::MAX_BYTES {
            Ok(Promise(s))
        } else {
            Err(PromiseError::TooLong {
                length: len,
                max: Promise::MAX_BYTES,
            })
        }
    }
}

impl TryFrom<&str> for Promise {
    type Error = PromiseError;

    fn try_from(s: &str) -> Result<Self, Self::Error> {
        let len = s.len();
        if len <= Promise::MAX_BYTES {
            Ok(Promise(s.to_owned()))
        } else {
            Err(PromiseError::TooLong {
                length: len,
                max: Promise::MAX_BYTES,
            })
        }
    }
}

impl core::fmt::Display for Promise {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Represents a set of parameters for calculating the opening fee for a JIT
/// channel.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)] // LSPS2 requires the client to fail if a field is unrecognized.
pub struct OpeningFeeParams {
    pub min_fee_msat: Msat,
    pub proportional: Ppm,
    pub valid_until: DateTime,
    pub min_lifetime: u32,
    pub max_client_to_self_delay: u32,
    pub min_payment_size_msat: Msat,
    pub max_payment_size_msat: Msat,
    pub promise: String, // Max 512 bytes
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Lsps2BuyRequest {
    pub opening_fee_params: OpeningFeeParams,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payment_size_msat: Option<Msat>,
}

impl JsonRpcRequest for Lsps2BuyRequest {
    const METHOD: &'static str = "lsps2.buy";
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Lsps2BuyResponse {
    pub jit_channel_scid: ShortChannelId,
    pub lsp_cltv_expiry_delta: u32,
    // is an optional Boolean. If not specified, it defaults to false. If
    // specified and true, the client MUST trust the LSP to actually create and
    // confirm a valid channel funding transaction.
    #[serde(default)]
    pub client_trusts_lsp: bool,
}

/// Computes the opening fee in millisatoshis as described in LSPS2.
/// Returns None if an arithmetic overflow occurs during calculation.
///
/// # Arguments
/// * `payment_size_msat` - The size of the payment for which the channel is
///   being opened.
/// * `opening_fee_min_fee_msat` - The minimum fee to be paid by the client to
///   the LSP
/// * `opening_fee_proportional` - The proportional fee charged by the LSP
pub fn compute_opening_fee(
    payment_size_msat: u64,
    opening_fee_min_fee_msat: u64,
    opening_fee_proportional: u64,
) -> Option<u64> {
    payment_size_msat
        .checked_mul(opening_fee_proportional)
        .and_then(|f| f.checked_add(999999))
        .and_then(|f| f.checked_div(1000000))
        .map(|f| std::cmp::max(f, opening_fee_min_fee_msat))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Helper struct for testing Serde
    #[derive(Serialize, Deserialize, Debug, PartialEq)]
    struct TestData {
        label: String,
        value: Promise,
    }

    #[test]
    fn serde_promise_ok() {
        let json = r#"{"label": "short", "value": "This is valid"}"#;
        let result = serde_json::from_str::<TestData>(json);
        assert!(result.is_ok());
        let data = result.unwrap();
        assert_eq!(data.value.0, "This is valid");
    }

    #[test]
    fn serde_promise_too_long() {
        let long_value = "a".repeat(513); // Exceeds 512 bytes
        let json = format!(r#"{{"label": "long", "value": "{}"}}"#, long_value);
        let result = serde_json::from_str::<TestData>(&json);
        assert!(result.is_err());
        // Check the error message relates to our PromiseError
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("promise string is too long"));
    }

    #[test]
    fn serde_promise_wrong_type() {
        // Input JSON has a number where a string is expected for 'value'
        let json = r#"{"label": "wrong_type", "value": 123}"#;
        let result = serde_json::from_str::<TestData>(json);
        assert!(result.is_err());
        // This error occurs when Serde tries to deserialize 123 as the String
        // required by `try_from = "String"`.
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("invalid type: integer"));
    }
}
