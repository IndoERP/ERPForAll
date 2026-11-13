//! Typed failures from unit identities and exact conversions.

use std::error::Error as StdError;
use std::fmt;

use erp_sdk::money::MoneyError;

/// A rejected unit identity or conversion. Numeric errors retain their original source.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum UomError {
    /// Unit record identifiers must be positive.
    InvalidUnitId {
        /// The rejected identifier.
        id: i64,
    },
    /// A resolved reference root must have a positive record identifier.
    InvalidRootId {
        /// The rejected identifier.
        id: i64,
    },
    /// No conversion may divide by a zero factor.
    ZeroFactor {
        /// The unit carrying the invalid factor.
        id: i64,
    },
    /// Units from distinct dimensions cannot be converted.
    IncompatibleRoots {
        /// The source unit record identifier.
        source_id: i64,
        /// The destination unit record identifier.
        target_id: i64,
    },
    /// One pack must remain nonzero after its quantity conversion boundary.
    RoundedZeroPack {
        /// The packaging unit record identifier.
        source_id: i64,
        /// The destination unit record identifier.
        target_id: i64,
    },
    /// The Product Unit digits are outside the supported 0 to 28 range.
    InvalidDigits {
        /// The rejected number of decimals.
        digits: u32,
    },
    /// A column name given to the SQL expression builder is not a lowercase SQL identifier.
    InvalidSqlIdentifier {
        /// The rejected text.
        identifier: String,
    },
    /// A bounded exact-number operation was refused.
    Numeric(MoneyError),
}

impl fmt::Display for UomError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidUnitId { id } => write!(f, "Unit of measure id {id} must be positive."),
            Self::InvalidRootId { id } => {
                write!(f, "Unit of measure root id {id} must be positive.")
            }
            Self::ZeroFactor { id } => {
                write!(
                    f,
                    "The conversion factor for unit of measure {id} cannot be 0."
                )
            }
            Self::IncompatibleRoots {
                source_id,
                target_id,
            } => write!(
                f,
                "Units of measure {source_id} and {target_id} do not share a reference root."
            ),
            Self::RoundedZeroPack {
                source_id,
                target_id,
            } => write!(
                f,
                "One pack of unit of measure {source_id} rounds to 0 in unit of measure {target_id}."
            ),
            Self::InvalidDigits { digits } => write!(
                f,
                "Product Unit digits {digits} are outside the supported range 0 to 28."
            ),
            Self::InvalidSqlIdentifier { identifier } => write!(
                f,
                "'{identifier}' is not a lowercase SQL identifier (letters, digits and underscores, at most 63 bytes, not starting with a digit)."
            ),
            Self::Numeric(error) => error.fmt(f),
        }
    }
}

impl StdError for UomError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::Numeric(error) => Some(error),
            Self::InvalidUnitId { .. }
            | Self::InvalidRootId { .. }
            | Self::ZeroFactor { .. }
            | Self::IncompatibleRoots { .. }
            | Self::InvalidDigits { .. }
            | Self::InvalidSqlIdentifier { .. }
            | Self::RoundedZeroPack { .. } => None,
        }
    }
}

impl From<MoneyError> for UomError {
    fn from(error: MoneyError) -> Self {
        Self::Numeric(error)
    }
}

impl From<UomError> for erp_sdk::Error {
    fn from(error: UomError) -> Self {
        match error {
            // A caller bug or a corrupt setting, never something the user typed.
            UomError::InvalidSqlIdentifier { .. } | UomError::InvalidDigits { .. } => {
                Self::programming(error.to_string()).with_source(error)
            }
            UomError::InvalidUnitId { .. }
            | UomError::InvalidRootId { .. }
            | UomError::ZeroFactor { .. }
            | UomError::IncompatibleRoots { .. }
            | UomError::RoundedZeroPack { .. }
            | UomError::Numeric(_) => Self::validation(error.to_string()).with_source(error),
        }
    }
}
