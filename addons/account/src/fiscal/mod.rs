// SPDX-License-Identifier: MIT OR Apache-2.0
//! Fiscal periods (R2, D7, D8): one row per company and
//! month with the statuses writers decide on, the generator `ensure_periods`, the question writers
//! ask before writing a dated row ([`period_admission`]) and the status machine of the lock
//! operations ([`status`]). Developer page.
//!
//! The `stock` addon never reads this table (G5): stock asks through `stock_account`.

pub mod admission;
pub mod calendar;
pub mod ensure;
pub mod period;
pub mod settings;
pub mod status;

pub use admission::{PeriodVerdict, WriteClass, period_admission, require_open_periods};
pub use ensure::{ensure_periods, ensure_periods_with};
pub use period::{AccountFiscalPeriod, PERIOD_MODEL};

/// First key of the period advisory locks (`"ACPP"`): writers take
/// `pg_advisory_xact_lock_shared(PERIOD_LOCK_CLASS, period_id)`, lock operations the exclusive
/// lock on the same key (R1d).
pub const PERIOD_LOCK_CLASS: i32 = 0x4143_5050;
