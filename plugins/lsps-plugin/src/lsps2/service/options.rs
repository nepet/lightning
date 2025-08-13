use crate::{
    lsps0::primitives::{Msat, Ppm},
    lsps2::model::OpeningFeeParams,
};
use chrono::{DateTime, Utc};
use cln_plugin::{options, Builder, Plugin};
use tokio::io::{AsyncRead, AsyncWrite};

#[derive(Debug, thiserror::Error)]
pub enum ConfigValidationError {
    #[error("Missing required field when service is enabled: {field}")]
    MissingRequiredField { field: &'static str },
    #[error("Invalid value for field {field}: {reason}")]
    InvalidValue { field: &'static str, reason: String },
    #[error("Plugin option error for {field}: {source}")]
    PluginError {
        field: &'static str,
        source: anyhow::Error,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    enabled: bool,
    fee_params: Option<OpeningFeeParams>,
}

// Convenience methods for Config
impl Config {
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    pub fn fee_params(&self) -> Option<&OpeningFeeParams> {
        self.fee_params.as_ref()
    }
}

#[derive(Debug, Clone)]
pub struct Options<'a> {
    pub enabled: options::FlagConfigOption<'a>,
    pub min_fee_msat: options::IntegerConfigOption<'a>,
    pub proportional: options::IntegerConfigOption<'a>,
    pub valid_until: options::StringConfigOption<'a>,
    pub min_lifetime: options::IntegerConfigOption<'a>,
    pub max_client_to_self_delay: options::IntegerConfigOption<'a>,
    pub min_payment_size_msat: options::IntegerConfigOption<'a>,
    pub max_payment_size_msat: options::IntegerConfigOption<'a>,
}

impl<'a> Options<'a> {
    pub fn new() -> Self {
        Self {
            enabled: options::ConfigOption::new_flag(
                "lsps2_service_enabled",
                "Enables lsps2 service",
            ),
            min_fee_msat: options::ConfigOption::new_i64_no_default(
                "lsps2_service_params_min_fee_msat",
                "Minimum fee to be paid by the client to the LSP in millisatoshis",
            ),
            proportional: options::ConfigOption::new_i64_no_default(
                "lsps2_service_params_proportional",
                "Parts-per-million fee to be charged proportionally",
            ),
            valid_until: options::ConfigOption::new_str_no_default(
                "lsps2_service_params_valid_until",
                "Validity period for this options menu. Is open ended if set to 0",
            ),
            min_lifetime: options::ConfigOption::new_i64_no_default(
                "lsps2_service_params_min_lifetime",
                "Minimum lifetime of a channel in blocks after confirmation",
            ),

            max_client_to_self_delay: options::ConfigOption::new_i64_no_default(
                "lsps2_service_params_max_client_to_self_delay",
                "Maximum allowed blocks to_self_delay imposed on the LSP",
            ),

            min_payment_size_msat: options::ConfigOption::new_i64_no_default(
                "lsps2_service_params_min_payment_size_msat",
                "Minimum guaranteed payment size in millisatoshi, the client is able to receive, not including fees",
            ),
            max_payment_size_msat: options::ConfigOption::new_i64_no_default(
                "lsps2_service_params_max_payment_size_msat",
                "Maximum guaranteed payment size in millisatoshi, the client is able to receive, not including fees",
            ),
        }
    }

    pub fn register_with_builder<S, I, O>(self, builder: Builder<S, I, O>) -> Builder<S, I, O>
    where
        O: Send + AsyncWrite + Unpin + 'static,
        S: Clone + Sync + Send + 'static,
        I: AsyncRead + Send + Unpin + 'static,
    {
        builder
            .option(self.enabled)
            .option(self.min_fee_msat)
            .option(self.proportional)
            .option(self.valid_until)
            .option(self.min_lifetime)
            .option(self.max_client_to_self_delay)
            .option(self.min_payment_size_msat)
            .option(self.max_payment_size_msat)
    }

    pub fn extract_config<S>(&self, plugin: &Plugin<S>) -> Result<Config, ConfigValidationError>
    where
        S: Clone + Send,
    {
        let enabled =
            plugin
                .option(&self.enabled)
                .map_err(|e| ConfigValidationError::PluginError {
                    field: "enabled",
                    source: e,
                })?;

        let mut builder = ConfigBuilder::new().enabled(enabled);

        if let Ok(value) = plugin.option(&self.min_fee_msat) {
            builder = builder.min_fee_msat(value);
        }

        if let Ok(value) = plugin.option(&self.proportional) {
            builder = builder.proportional(value);
        }

        if let Ok(value) = plugin.option(&self.valid_until) {
            builder = builder.valid_until(value);
        }

        if let Ok(value) = plugin.option(&self.min_lifetime) {
            builder = builder.min_lifetime(value);
        }

        if let Ok(value) = plugin.option(&self.max_client_to_self_delay) {
            builder = builder.max_client_to_self_delay(value);
        }

        if let Ok(value) = plugin.option(&self.min_payment_size_msat) {
            builder = builder.min_payment_size_msat(value);
        }

        if let Ok(value) = plugin.option(&self.max_payment_size_msat) {
            builder = builder.max_payment_size_msat(value);
        }

        builder.build()
    }
}

// Validation builder that ensures all required fields are present
#[derive(Debug, Default)]
pub struct ConfigBuilder {
    enabled: Option<bool>,
    min_fee_msat: Option<i64>,
    proportional: Option<i64>,
    valid_until: Option<String>,
    min_lifetime: Option<i64>,
    max_client_to_self_delay: Option<i64>,
    min_payment_size_msat: Option<i64>,
    max_payment_size_msat: Option<i64>,
}

impl ConfigBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = Some(enabled);
        self
    }

    pub fn min_fee_msat(mut self, value: Option<i64>) -> Self {
        self.min_fee_msat = value;
        self
    }

    pub fn proportional(mut self, value: Option<i64>) -> Self {
        self.proportional = value;
        self
    }

    pub fn valid_until(mut self, value: Option<String>) -> Self {
        self.valid_until = value;
        self
    }

    pub fn min_lifetime(mut self, value: Option<i64>) -> Self {
        self.min_lifetime = value;
        self
    }

    pub fn max_client_to_self_delay(mut self, value: Option<i64>) -> Self {
        self.max_client_to_self_delay = value;
        self
    }

    pub fn min_payment_size_msat(mut self, value: Option<i64>) -> Self {
        self.min_payment_size_msat = value;
        self
    }

    pub fn max_payment_size_msat(mut self, value: Option<i64>) -> Self {
        self.max_payment_size_msat = value;
        self
    }

    pub fn build(self) -> Result<Config, ConfigValidationError> {
        let enabled = self.enabled.unwrap_or(false);

        if !enabled {
            return Ok(Config {
                enabled: false,
                fee_params: None,
            });
        }

        // When enabled, all parameters are required
        let min_fee_msat =
            self.min_fee_msat
                .ok_or(ConfigValidationError::MissingRequiredField {
                    field: "min_fee_msat",
                })?;

        let proportional =
            self.proportional
                .ok_or(ConfigValidationError::MissingRequiredField {
                    field: "proportional",
                })?;

        let valid_until = self
            .valid_until
            .ok_or(ConfigValidationError::MissingRequiredField {
                field: "valid_until",
            })?;

        let min_lifetime =
            self.min_lifetime
                .ok_or(ConfigValidationError::MissingRequiredField {
                    field: "min_lifetime",
                })?;

        let max_client_to_self_delay =
            self.max_client_to_self_delay
                .ok_or(ConfigValidationError::MissingRequiredField {
                    field: "max_client_to_self_delay",
                })?;

        let min_payment_size_msat =
            self.min_payment_size_msat
                .ok_or(ConfigValidationError::MissingRequiredField {
                    field: "min_payment_size_msat",
                })?;

        let max_payment_size_msat =
            self.max_payment_size_msat
                .ok_or(ConfigValidationError::MissingRequiredField {
                    field: "max_payment_size_msat",
                })?;

        // Validate and convert values to proper types

        // Convert min_fee_msat to Msat
        if min_fee_msat < 0 {
            return Err(ConfigValidationError::InvalidValue {
                field: "min_fee_msat",
                reason: "must be non-negative".to_string(),
            });
        }
        let min_fee = Msat::from_msat(min_fee_msat as u64);

        // Convert proportional to Ppm
        if proportional < 0 {
            return Err(ConfigValidationError::InvalidValue {
                field: "proportional",
                reason: "must be non-negative".to_string(),
            });
        }
        if proportional > u32::MAX as i64 {
            return Err(ConfigValidationError::InvalidValue {
                field: "proportional",
                reason: format!("must be <= {}", u32::MAX),
            });
        }
        let proportional_ppm = Ppm::from_ppm(proportional as u32);

        // Parse valid_until to DateTime
        if valid_until.is_empty() {
            return Err(ConfigValidationError::InvalidValue {
                field: "valid_until",
                reason: "cannot be empty".to_string(),
            });
        }

        let parsed_valid_until = DateTime::parse_from_rfc3339(&valid_until)
            .map(|dt| dt.with_timezone(&Utc))
            .map_err(|e| ConfigValidationError::InvalidValue {
                field: "valid_until",
                reason: format!("invalid rfc3339 datetime format: {}", e),
            })?;

        // Convert min_lifetime to u32
        if min_lifetime <= 0 {
            return Err(ConfigValidationError::InvalidValue {
                field: "min_lifetime",
                reason: "must be positive".to_string(),
            });
        }
        if min_lifetime > u32::MAX as i64 {
            return Err(ConfigValidationError::InvalidValue {
                field: "min_lifetime",
                reason: format!("must be <= {}", u32::MAX),
            });
        }
        let min_lifetime_u32 = min_lifetime as u32;

        // Convert max_client_to_self_delay to u32
        if max_client_to_self_delay < 0 {
            return Err(ConfigValidationError::InvalidValue {
                field: "max_client_to_self_delay",
                reason: "must be non-negative".to_string(),
            });
        }
        if max_client_to_self_delay > u32::MAX as i64 {
            return Err(ConfigValidationError::InvalidValue {
                field: "max_client_to_self_delay",
                reason: format!("must be <= {}", u32::MAX),
            });
        }
        let max_delay_u32 = max_client_to_self_delay as u32;

        // Convert payment sizes to Msat
        if min_payment_size_msat < 0 {
            return Err(ConfigValidationError::InvalidValue {
                field: "min_payment_size_msat",
                reason: "must be non-negative".to_string(),
            });
        }
        if max_payment_size_msat < 0 {
            return Err(ConfigValidationError::InvalidValue {
                field: "max_payment_size_msat",
                reason: "must be non-negative".to_string(),
            });
        }
        if min_payment_size_msat > max_payment_size_msat {
            return Err(ConfigValidationError::InvalidValue {
                field: "payment_size",
                reason: "min_payment_size_msat must be <= max_payment_size_msat".to_string(),
            });
        }

        let min_payment_size = Msat::from_msat(min_payment_size_msat as u64);
        let max_payment_size = Msat::from_msat(max_payment_size_msat as u64);

        Ok(Config {
            enabled: true,
            fee_params: Some(OpeningFeeParams {
                min_fee_msat: min_fee,
                proportional: proportional_ppm,
                valid_until: parsed_valid_until,
                min_lifetime: min_lifetime_u32,
                max_client_to_self_delay: max_delay_u32,
                min_payment_size_msat: min_payment_size,
                max_payment_size_msat: max_payment_size,
                promise: String::new(), // or whatever default makes sense
            }),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_disabled_config() {
        let config = ConfigBuilder::new().enabled(false).build().unwrap();

        assert!(!config.is_enabled());
        assert!(config.fee_params().is_none());
    }

    #[test]
    fn test_enabled_config_missing_fields() {
        let result = ConfigBuilder::new().enabled(true).build();

        assert!(result.is_err());
        if let Err(ConfigValidationError::MissingRequiredField { field }) = result {
            assert_eq!(field, "min_fee_msat");
        }
    }

    #[test]
    fn test_enabled_config_invalid_values() {
        let result = ConfigBuilder::new()
            .enabled(true)
            .min_fee_msat(Some(-100))
            .proportional(Some(1000))
            .valid_until(Some("test".to_string()))
            .min_lifetime(Some(3600))
            .max_client_to_self_delay(Some(144))
            .min_payment_size_msat(Some(1000))
            .max_payment_size_msat(Some(100000))
            .build();

        assert!(result.is_err());
        if let Err(ConfigValidationError::InvalidValue { field, reason }) = result {
            assert_eq!(field, "min_fee_msat");
            assert!(reason.contains("non-negative"));
        }
    }

    #[test]
    fn test_valid_enabled_config() {
        let config = ConfigBuilder::new()
            .enabled(true)
            .min_fee_msat(Some(1000))
            .proportional(Some(1000))
            .valid_until(Some("2024-12-31T23:59:59Z".to_string()))
            .min_lifetime(Some(3600))
            .max_client_to_self_delay(Some(144))
            .min_payment_size_msat(Some(1000))
            .max_payment_size_msat(Some(100000))
            .build()
            .unwrap();

        assert!(config.is_enabled());
        assert!(config.fee_params().is_some());

        let fee_params = config.fee_params().unwrap();
        assert_eq!(fee_params.min_fee_msat.msat(), 1000);
        assert_eq!(fee_params.proportional.ppm(), 1000);
        assert_eq!(fee_params.min_lifetime, 3600);
        assert_eq!(fee_params.max_client_to_self_delay, 144);
        assert_eq!(fee_params.min_payment_size_msat.msat(), 1000);
        assert_eq!(fee_params.max_payment_size_msat.msat(), 100000);
    }

    #[test]
    fn test_payment_size_range_validation() {
        let result = ConfigBuilder::new()
            .enabled(true)
            .min_fee_msat(Some(1000))
            .proportional(Some(1000))
            .valid_until(Some("2024-12-31T23:59:59Z".to_string()))
            .min_lifetime(Some(3600))
            .max_client_to_self_delay(Some(144))
            .min_payment_size_msat(Some(100000)) // min > max
            .max_payment_size_msat(Some(1000))
            .build();

        assert!(result.is_err());
        if let Err(ConfigValidationError::InvalidValue { field, reason }) = result {
            assert_eq!(field, "payment_size");
            assert!(reason.contains("min_payment_size_msat must be <= max_payment_size_msat"));
        }
    }

    #[test]
    fn test_invalid_datetime_format() {
        let result = ConfigBuilder::new()
            .enabled(true)
            .min_fee_msat(Some(1000))
            .proportional(Some(1000))
            .valid_until(Some("invalid-date".to_string()))
            .min_lifetime(Some(3600))
            .max_client_to_self_delay(Some(144))
            .min_payment_size_msat(Some(1000))
            .max_payment_size_msat(Some(100000))
            .build();

        assert!(result.is_err());
        if let Err(ConfigValidationError::InvalidValue { field, .. }) = result {
            assert_eq!(field, "valid_until");
        }
    }

    #[test]
    fn test_u32_overflow_validation() {
        let result = ConfigBuilder::new()
            .enabled(true)
            .min_fee_msat(Some(1000))
            .proportional(Some(u32::MAX as i64 + 1)) // Too large for u32
            .valid_until(Some("2024-12-31T23:59:59Z".to_string()))
            .min_lifetime(Some(3600))
            .max_client_to_self_delay(Some(144))
            .min_payment_size_msat(Some(1000))
            .max_payment_size_msat(Some(100000))
            .build();

        assert!(result.is_err());
        if let Err(ConfigValidationError::InvalidValue { field, reason }) = result {
            assert_eq!(field, "proportional");
            assert!(reason.contains("must be <="));
        }
    }
}
