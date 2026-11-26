// SPDX-License-Identifier: MIT OR Apache-2.0
//! Node-local auth admission and hook execution configuration.
use crate::config::ConfigError;
use erp_security::auth::AuthConfig;

fn invalid(message: impl Into<String>) -> ConfigError {
    ConfigError::Auth(erp_fields::Error::validation(message))
}

pub(crate) fn read(value: Option<toml::Value>) -> Result<AuthConfig, ConfigError> {
    let Some(value) = value else {
        return Ok(AuthConfig::DEFAULT);
    };
    let table = value
        .as_table()
        .ok_or_else(|| invalid("auth must be a table"))?;
    let mut config = AuthConfig::DEFAULT;
    let mut burst = config.login_burst();
    let mut rate = config.login_rate_per_sec();
    let mut bearer_burst = config.bearer_burst();
    let mut bearer_rate = config.bearer_rate_per_sec();
    let mut key_burst = config.api_key_burst();
    let mut key_rate = config.api_key_rate_per_sec();
    for (key, value) in table {
        // Conversion failures are mapped to the named refusal below, never to defaults.
        let integer = |name: &'static str| {
            value
                .as_integer()
                .and_then(|n| u32::try_from(n).ok())
                .ok_or_else(|| invalid(format!("auth {name} must be an unsigned 32-bit integer")))
        };
        let number = |name: &'static str, bounds: &str| {
            value
                .as_float()
                .or_else(|| value.as_integer().map(|n| n as f64))
                .ok_or_else(|| invalid(format!("auth {name} must be a finite number in {bounds}")))
        };
        let bounded = |error: erp_security::auth::ConfigError| ConfigError::Auth(error.into());
        config = match key.as_str() {
            "limits.credential_bytes" => config
                .with_credential_bytes(integer("limits.credential_bytes")?)
                .map_err(bounded)?,
            "throttle.table_entries" => config
                .with_throttle_entries(integer("throttle.table_entries")?)
                .map_err(bounded)?,
            "limits.cooldown_hook_deadline_ms" => config
                .with_cooldown_hook_deadline_ms(integer("limits.cooldown_hook_deadline_ms")?)
                .map_err(bounded)?,
            "limits.session_login_hook_deadline_ms" => config
                .with_session_login_deadline_ms(integer("limits.session_login_hook_deadline_ms")?)
                .map_err(bounded)?,
            "session.data_bytes" => config
                .with_session_data_bytes(integer("session.data_bytes")?)
                .map_err(bounded)?,
            "limits.token_plan_hook_deadline_ms" => config
                .with_token_plan_hook_deadline_ms(integer("limits.token_plan_hook_deadline_ms")?)
                .map_err(bounded)?,
            "throttle.login_burst" => {
                burst = integer("throttle.login_burst")?;
                config.with_login_prefilter(burst, rate).map_err(bounded)?
            }
            "throttle.login_rate_per_sec" => {
                rate = value.as_float().or_else(|| value.as_integer().map(|n| n as f64))
                    .ok_or_else(|| invalid("auth throttle.login_rate_per_sec must be a finite number in 0.1..=1000"))?;
                config.with_login_prefilter(burst, rate).map_err(bounded)?
            }
            "throttle.bearer_burst" => {
                bearer_burst = integer("throttle.bearer_burst")?;
                config
                    .with_bearer_prefilter(bearer_burst, bearer_rate)
                    .map_err(bounded)?
            }
            "throttle.bearer_rate_per_sec" => {
                bearer_rate = number("throttle.bearer_rate_per_sec", "0.1..=10000")?;
                config
                    .with_bearer_prefilter(bearer_burst, bearer_rate)
                    .map_err(bounded)?
            }
            "throttle.api_key_burst" => {
                key_burst = integer("throttle.api_key_burst")?;
                config
                    .with_api_key_budget(key_burst, key_rate)
                    .map_err(bounded)?
            }
            "throttle.api_key_rate_per_sec" => {
                key_rate = number("throttle.api_key_rate_per_sec", "0.1..=10000")?;
                config
                    .with_api_key_budget(key_burst, key_rate)
                    .map_err(bounded)?
            }
            _ => return Err(invalid("auth contains unknown keys")),
        };
    }
    Ok(config)
}
