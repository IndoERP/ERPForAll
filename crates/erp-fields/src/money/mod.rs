//! Exact money primitives (D3, hook H41; layer L1, no I/O, builds for WASM).
//!
//! - [`ExactRatio`]: signed exact quotients, at most 4096 bits per side, checked arithmetic.
//! - [`Increment`] and [`RoundingMode`]: positive increments and the seven explicit modes;
//!   [`Increment::quantize`], [`Increment::is_zero`], [`Increment::compare`].
//! - [`parse_decimal`] / [`canonical_string`]: the strict decimal text of the boundary.
//! - [`CurrencyCode`] and [`Amount`]: currency-tagged amounts that refuse to mix currencies.
//! - [`FxSnapshot`]: the rates a conversion used, by identity, with an explicit direction.
//!
//! There is no float conversion anywhere in this module and only a validating canonical-pair persistence boundary. Dated rate *lookup* is not here: it reads the database and lives in the base addon.

mod amount;
mod error;
mod fraction;
mod fx;
mod grid;
mod nat;
mod ratio;
mod rounding;
mod text;

pub use amount::{Amount, CurrencyCode};
pub use error::MoneyError;
pub use fraction::{CanonicalFraction, CanonicalInteger, ExactNumericError};
pub use fx::{FxSnapshot, RateDateRule, RateOrigin, RateRef};
pub use grid::{GRID_COEFFICIENT_BITS, GridValue, MAX_GRID_SCALE, QuantizedGrid};
pub use ratio::{ExactRatio, MAX_BITS};
pub use rounding::{Increment, RoundingMode};
pub use text::{MAX_DECIMAL_BYTES, canonical_string, parse_decimal};
