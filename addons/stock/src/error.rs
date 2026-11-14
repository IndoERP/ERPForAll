// SPDX-License-Identifier: MIT OR Apache-2.0
//! Typed failures of the stock addon.

use std::error::Error as StdError;
use std::fmt;

/// A stock request the addon refuses.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum StockError {
    /// Company scrap/default stock links are computed outputs.
    CompanyLocationsDerived,
    /// Existing warehouses and operation types keep their company, even while archived.
    /// Archive the record and create a replacement in the desired company instead.
    ///
    /// ```
    /// let error: erp_sdk::Error = erp_addon_stock::StockError::CompanyChangeForbidden.into();
    /// assert_eq!(error.kind(), erp_sdk::ErrorKind::UserError);
    /// ```
    CompanyChangeForbidden,
    /// Location warehouse membership is maintained from the tree and warehouse roots.
    LocationOwnershipDerived,
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
    /// An ORM or RPC create or write named a column only the stock engine writes (done time,
    /// membership, correction counter; lead ruling L6').
    EngineOnlyField {
        /// `stock move` or `stock move line`.
        what: &'static str,
        /// The label of the field written.
        field: String,
    },
    /// A create carried a `default_<field>` context key for one of those columns.
    EngineOnlyDefault {
        /// `stock move` or `stock move line`.
        what: &'static str,
        /// The label of the field the key names.
        field: String,
    },
    /// A create or write named the generated identity of a route or rule (warehouse, addon, role),
    /// or a create carried a `default_<field>` context key for it. Only the warehouse setup that
    /// generates the record sets it, through [`crate::set_generated_identity`].
    GeneratedIdentity {
        /// `route` or `rule`.
        what: &'static str,
        /// The label of the field named.
        field: String,
    },
}

impl fmt::Display for StockError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CompanyLocationsDerived => f.write_str("Company scrap and default stock locations are derived and cannot be assigned."),
            Self::CompanyChangeForbidden => f.write_str(
                "The company of this record cannot be changed once it exists. Archive it and create a new record for the other company."
            ),
            Self::LocationOwnershipDerived => f.write_str(
                "Location warehouse ownership is derived from its hierarchy and cannot be assigned."
            ),
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
            Self::EngineOnlyField { what, field } => write!(
                f,
                "{field} of a {what} is recorded by the stock engine when the operation is \
                 completed or corrected; it cannot be set directly."
            ),
            Self::EngineOnlyDefault { what, field } => write!(
                f,
                "{field} of a {what} is recorded by the stock engine; a new {what} cannot take \
                 it from a default in the context."
            ),
            Self::GeneratedIdentity { what, field } => write!(
                f,
                "{field} of a stock {what} is filled in only when a warehouse generates the \
                 {what}; it cannot be set directly or from a default."
            ),
        }
    }
}

impl StdError for StockError {}

impl From<StockError> for erp_sdk::Error {
    fn from(error: StockError) -> Self {
        let result = match &error {
            StockError::CompanyLocationsDerived
            | StockError::CompanyChangeForbidden
            | StockError::LocationOwnershipDerived
            | StockError::EngineOnlyField { .. }
            | StockError::EngineOnlyDefault { .. }
            | StockError::GeneratedIdentity { .. } => Self::user(error.to_string()),
            StockError::ForbidNegativeUnavailable { .. }
            | StockError::TrackingChangeBlocked { .. } => Self::validation(error.to_string()),
        };
        result.with_source(error)
    }
}
