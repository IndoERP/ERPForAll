//! Parsing shared by warning calculation and the object-write runner.

use super::{MAX_PATH_STEPS, RunLimit, ServerActionError};

/// Validate the step budget before resolving model fields.
pub(super) fn steps(path: &str) -> Result<impl Iterator<Item = &str>, ServerActionError> {
    // Only inspect enough borrowed segments to establish an excess. In particular, do not
    // collect the untrusted stored path before checking its budget (also used by warnings).
    if path.split('.').nth(MAX_PATH_STEPS).is_some() {
        return Err(ServerActionError::Limit {
            kind: RunLimit::PathSteps,
        });
    }
    Ok(path.split('.'))
}
