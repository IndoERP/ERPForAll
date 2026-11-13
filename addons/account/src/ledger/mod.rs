// SPDX-License-Identifier: MIT OR Apache-2.0
//! Ledger configuration models: accounts, tags, roots and per-company codes.
//! Journals, journal groups and the company's accounting fields follow in sibling slices.

pub mod account;
pub mod code;
pub mod root;
pub mod tag;

pub use account::AccountAccount;
pub use code::AccountCodeMapping;
pub use root::AccountRoot;
pub use tag::AccountAccountTag;
