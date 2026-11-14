// SPDX-License-Identifier: MIT OR Apache-2.0
//! Company lock policy (foundation). Entry hooks and exceptions are deferred;
//! a successful check is a decision in this transaction, never a reusable execution permission.
mod check;
mod company;
mod policy;

pub use check::check_lock_dates;
pub use company::{LockPatch, change_lock_dates};
pub use erp_sdk::audit::AccountLockField as LockField;
pub use policy::{JournalClass, LockCheck, LockRefusal, LockVerdict, LockViolation, TaxImpact};
