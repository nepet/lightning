use serde::{
    de::{Error, Visitor},
    Deserialize, Deserializer, Serialize, Serializer,
};

// The amount suffix representing Satoshi as per LSPS0.
const SAT_SUFFIX: &str = "_sat";
// The amount suffix representing MilliSatoshi as per LSPS0.
const MSAT_SUFFIX: &str = "_msat";
const MSAT_PER_SAT: u64 = 1000;

/// Represents a monetary amount as defined in LSPS0.msat. Is converted to a
/// `String` in json messages with a suffix `_msat` or `_sat` and internally
/// represented as Millisatoshi `u64`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Msat(pub u64);

impl Msat {
    /// Constructs a new `Msat` struct from a `u64`.
    pub fn from_msat(msat: u64) -> Self {
        Msat(msat)
    }

    /// Returns the sat amount of the field. Is a floored integer division e.g
    /// 100678 becomes 100.
    pub fn to_sats_floor(&self) -> u64 {
        self.0 / 1000
    }

    /// Returns the msat value as `u64`. Is the inner value of `Msat`.
    pub fn msat(&self) -> u64 {
        self.0
    }
}

impl core::fmt::Display for Msat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}_msat", self.0)
    }
}

impl Serialize for Msat {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let formatted_string = format!("{}_msat", self.0);
        serializer.serialize_str(&formatted_string)
    }
}

struct MsatVisitor;

impl<'de> Visitor<'de> for MsatVisitor {
    type Value = Msat;

    fn expecting(&self, formatter: &mut core::fmt::Formatter) -> core::fmt::Result {
        formatter.write_str("a string formatted as '<numeric_value>_sat' or '<numeric_value>_msat'")
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
    where
        E: Error,
    {
        let msat_val = if let Some(stripped) = value.strip_suffix(SAT_SUFFIX) {
            let num_sats = stripped.parse::<u64>().map_err(|e| {
                Error::custom(format!(
                    "Failed to parse '{}' as u64 (from '{}'): {}",
                    stripped, value, e
                ))
            })?;
            num_sats.checked_mul(MSAT_PER_SAT).ok_or_else(|| {
                Error::custom(format!(
                    "Satoshi value '{}' too large, results in millisatoshi overflow",
                    num_sats
                ))
            })?
        } else if let Some(stripped) = value.strip_suffix(MSAT_SUFFIX) {
            stripped.parse::<u64>().map_err(|e| {
                Error::custom(format!(
                    "Failed to parse '{}' as u64 (from '{}'): {}",
                    stripped, value, e
                ))
            })?
        } else {
            return Err(Error::custom(format!(
                "Expected string ending with '{}' or '{}', found: '{}'",
                SAT_SUFFIX, MSAT_SUFFIX, value
            )));
        };
        Ok(Msat(msat_val))
    }

    // other visit methods returning invalid_type errors.
    fn visit_u64<E>(self, v: u64) -> Result<Self::Value, E>
    where
        E: Error,
    {
        Err(Error::invalid_type(
            serde::de::Unexpected::Unsigned(v),
            &self,
        ))
    }
    fn visit_borrowed_str<E>(self, v: &'de str) -> Result<Self::Value, E>
    where
        E: Error,
    {
        self.visit_str(v)
    }
    fn visit_string<E>(self, v: String) -> Result<Self::Value, E>
    where
        E: Error,
    {
        self.visit_str(&v)
    }
}

impl<'de> Deserialize<'de> for Msat {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_str(MsatVisitor)
    }
}

/// Represents parts-per-million as defined in LSPS0.ppm. Gets it's own type
/// from the rationals: "This is its own type so that fractions can be expressed
/// using this type, instead of as a floating-point type which might lose
/// accuracy when serialized into text.". Having it as a separate type also
/// provides more clarity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)] // Key attribute! Serialize/Deserialize as the inner u32
pub struct Ppm(pub u32); // u32 is sufficient as 1,000,000 fits easily

impl Ppm {
    /// Constructs a new `Ppm` from a u32.
    pub const fn from_ppm(value: u32) -> Self {
        Ppm(value)
    }

    /// Applies the proportion to a base amount (e.g., in msats).
    pub fn apply_to(&self, base_msat: u64) -> u64 {
        // Careful about integer division order and potential overflow
        (base_msat as u128 * self.0 as u128 / 1_000_000) as u64
    }

    /// Returns the ppm.
    pub fn ppm(&self) -> u32 {
        self.0
    }
}

impl core::fmt::Display for Ppm {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}ppm", self.0)
    }
}

/// Represents a short channel id as defined in LSPS0.scid. Matches with the
/// implementation in cln_rpc.
pub type ShortChannelId = cln_rpc::primitives::ShortChannelId;

/// Represents a datetime as defined in LSPS0.datetime. Uses ISO8601 in UTC
/// timezone.
pub type DateTime = chrono::DateTime<chrono::Utc>;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json;

    #[derive(Debug, Serialize, Deserialize)]
    struct TestMessage {
        amount: Msat,
    }

    /// Test serialization of a struct containing Msat.
    #[test]
    fn test_msat_serialization() {
        let msg = TestMessage {
            amount: Msat(12345000),
        };

        let expected_amount_json = r#""amount":"12345000_msat""#;

        // Assert that the serialized string contains the expected _msat suffix.
        let json_string = serde_json::to_string(&msg).expect("Serialization failed");
        assert!(
            json_string.contains(expected_amount_json),
            "Serialized JSON should contain '{}'",
            expected_amount_json
        );

        // Parse back to generic json value and check field.
        let json_value: serde_json::Value =
            serde_json::from_str(&json_string).expect("Failed to parse JSON back");
        assert_eq!(
            json_value
                .get("amount")
                .expect("JSON should have 'amount' field"),
            &serde_json::Value::String("12345000_msat".to_string()),
            "JSON 'amount' field should have the correct string value"
        );
    }

    /// Test deserialization into a struct containing Msat.
    #[test]
    fn test_msat_deserialization_and_errors() {
        // Case 1: Input string uses "_msat" suffix
        let json_input_msat = r#"{"amount":"987654321_msat"}"#;
        let expected_value_msat = Msat(987654321);
        let message1: TestMessage = serde_json::from_str(json_input_msat)
            .expect("Deserialization from _msat string failed");
        assert_eq!(message1.amount, expected_value_msat);

        // Case 2: Input string uses "_sat" suffix
        let json_input_sat = r#"{"amount":"12345_sat"}"#;
        let expected_value_sat = Msat(12345 * 1000);
        let message2: TestMessage =
            serde_json::from_str(json_input_sat).expect("Deserialization from _sat string failed");
        assert_eq!(message2.amount, expected_value_sat);
        println!("Deserialized (from _sat): {:?}", message2);

        // Case 3: Invalid Suffix (e.g., "_btc" instead of "_sat" or "_msat")
        let json_invalid_suffix = r#"{"amount":"100_btc"}"#;
        let result_suffix = serde_json::from_str::<TestMessage>(json_invalid_suffix);
        assert!(
            result_suffix.is_err(),
            "Deserialization should fail for invalid suffix"
        );

        // Case 4: Missing Suffix (just a number string)
        let json_missing_suffix = r#"{"amount":"200"}"#;
        let result_missing = serde_json::from_str::<TestMessage>(json_missing_suffix);
        assert!(
            result_missing.is_err(),
            "Deserialization should fail for missing suffix"
        );

        // Case 5: Non-numeric Value before suffix
        let json_non_numeric = r#"{"amount":"abc_msat"}"#;
        let result_non_numeric = serde_json::from_str::<TestMessage>(json_non_numeric);
        assert!(
            result_non_numeric.is_err(),
            "Deserialization should fail for non-numeric value"
        );

        // Case 6: Wrong JSON Type (Number instead of String)
        let json_wrong_type = r#"{"amount":12345}"#;
        let result_wrong_type = serde_json::from_str::<TestMessage>(json_wrong_type);
        assert!(
            result_wrong_type.is_err(),
            "Deserialization should fail for wrong JSON type (number)"
        );

        // Case 7: Overflow when converting from _sat
        // u64::MAX / 1000 is roughly 1.844e16
        let value_too_large = (u64::MAX / 500).to_string(); // Guaranteed to overflow when * 1000
        let json_overflow = format!(r#"{{"amount":"{}_sat"}}"#, value_too_large);
        let result_overflow = serde_json::from_str::<TestMessage>(&json_overflow);
        assert!(
            result_overflow.is_err(),
            "Deserialization should fail on sat->msat overflow"
        );
    }
}
