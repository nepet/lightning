use cln_plugin::options;

pub mod handler;
pub mod model;

pub const OPTION_ENABLED: options::FlagConfigOption =
    options::ConfigOption::new_flag("lsps2_service_enabled", "Enables lsps2 for the LSP service");

pub const OPTION_PROMISE_SECRET: options::StringConfigOption =
    options::ConfigOption::new_str_no_default(
        "lsps2_promise_secret",
        "A 64-character hex string that is the secret for promises",
    );
