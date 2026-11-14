// SPDX-License-Identifier: MIT OR Apache-2.0
//! Database-wide cadence of generic partition upkeep.
use std::time::Duration;

use erp_sdk::{Error, Result};

/// The Technical system parameter controlling generic partition maintenance.
pub const KEY: &str = "base.partition_upkeep_interval_secs";
/// Default delay between periodic maintenance runs.
pub const DEFAULT_SECS: u64 = 3_600;
/// Shortest administrator-selected interval.
pub const FLOOR_SECS: u64 = 60;
/// Hard bound on the configured delay; an active run is not interrupted.
pub const HARD_MAX_SECS: u64 = 3_600;
/// Longest decimal representation of an i64, including its sign. This bounds input
/// representation, not an operator-selected quantity.
pub const MAX_VALUE_BYTES: usize = 20;

/// Parse the declared policy without exposing a corrupt stored value.
///
/// An absent, NULL or empty parameter resets to the default, as the parameter API specifies.
///
/// # Errors
///
/// `Validation` naming the key and range for malformed or out-of-range input.
pub fn interval(value: Option<&str>) -> Result<Duration> {
    let invalid = || {
        Error::validation(format!(
            "{KEY} must be an integer from {FLOOR_SECS} to {HARD_MAX_SECS}."
        ))
    };
    let seconds = match value {
        None | Some("") => DEFAULT_SECS,
        Some(text) if text.len() <= MAX_VALUE_BYTES => {
            text.parse::<u64>().map_err(|_| invalid())?
        }
        Some(_) => return Err(invalid()),
    };
    if !(FLOOR_SECS..=HARD_MAX_SECS).contains(&seconds) {
        return Err(invalid());
    }
    Ok(Duration::from_secs(seconds))
}

pub(crate) fn validate(key: &str, value: Option<&str>) -> Result<()> {
    if key == KEY {
        interval(value)?;
    }
    Ok(())
}
