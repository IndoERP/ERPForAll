// SPDX-License-Identifier: MIT OR Apache-2.0
//! Typed failures of the stock addon.

use std::error::Error as StdError;
use std::fmt;

/// A stock request the addon refuses.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum StockError {
    /// The negative-stock guard is not delivered yet, so a location policy row
    /// cannot switch it on (amendment B3).
    ForbidNegativeUnavailable {
        /// The location whose policy row was written.
        location_id: i64,
    },
    /// A tracking change would leave lot or serial rows the new mode cannot hold (6).
    TrackingChangeBlocked {
        /// Display name of the product.
        product: String,
        /// Lots, serial numbers or open serial rows found.
        count: i64,
    },
}

impl fmt::Display for StockError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ForbidNegativeUnavailable { location_id } => write!(
                f,
                "Location {location_id} cannot forbid negative stock yet: the negative-stock \
                 guard is not supported until a later release."
            ),
            Self::TrackingChangeBlocked { product, count } => write!(
                f,
                "Tracking of {product} cannot be changed while it has stock or open operations with \
                 lots or serial numbers ({count} found). Converting existing stock is not available \
                 yet (a later release)."
            ),
        }
    }
}

impl StdError for StockError {}

impl From<StockError> for erp_sdk::Error {
    fn from(error: StockError) -> Self {
        Self::validation(error.to_string()).with_source(error)
    }
}
