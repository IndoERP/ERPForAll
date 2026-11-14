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
    for (key, value) in table {
        // Conversion failures are mapped to the named refusal below, never to defaults.
        let integer = |name: &'static str| {
            value
                .as_integer()
                .and_then(|n| u32::try_from(n).ok())
                .ok_or_else(|| invalid(format!("auth {name} must be an unsigned 32-bit integer")))
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
            _ => return Err(invalid("auth contains unknown keys")),
        };
    }
    Ok(config)
}
