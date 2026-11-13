//! ERPForAll's access engine (layer L6).
//!
//! `erp-orm` asks an `AccessPolicy` before every operation and fails closed without one. This
//! crate builds the policy from the database's `ir.access` rules, groups and capability mappings:
//!
//! - [`rows`]: the plain, validated rows the loader reads in one snapshot (the input of the
//!   compiler);
//! - [`rule`]: one rule's domain string, parsed and evaluated symbolically into a domain whose
//!   principal values are parameters; the `ir.access` write-time validation;
//! - [`group_spec`]: group specs (`a,!b`, `.`) parsed against the graph and evaluated for a
//!   principal (field `groups`, capabilities, `has_groups`);
//! - [`company_access`]: the company restrictions generated from model declarations (
//!   A1) and the check of the generated rows against them;
//! - [`snapshot`]: the compiler and the [`PolicySnapshot`] it builds, which implements
//!   `erp_orm::AccessPolicy` (combination, `access` resolution, allowing groups);
//! - [`explain`]: the SEC-008/009 texts and the blame list of a record-level denial;
//! - [`report`]: what a snapshot build found (rule errors, cycles, capability mappings);
//! - [`loader`]: reads the rows and the `policy` generation from a database in one
//!   `REPEATABLE READ` snapshot (five statements, a size check first);
//! - [`source`]: the [`source::PolicySource`] that builds, publishes and refreshes the shared
//!   snapshot (SIGNAL's `Refresh` for `policy`) and rebuilds a transaction's private one;
//! - [`resolver`]: the [`resolver::Resolver`] that reads a principal's subject, groups and
//!   companies (one statement at transaction start, one to re-read every bound subject);
//! - [`admin`]: what `erp access report` prints and `erp access disable-rule` does;
//! - [`auth`]: password login, sessions, the login throttle and `database.secret`.
//!
//! The rows and the compiler are pure; the loader, the source, the resolver and the admin
//! commands are the I/O.

#![allow(missing_docs)]

pub mod admin;
mod analyze;
pub mod audit;
pub mod auth;
mod bits;
pub mod company_access;
pub mod explain;
mod fields;
mod graph;
pub mod group_spec;
pub mod groups;
pub mod jobs;
pub mod loader;
mod memo;
pub mod principal;
pub mod report;
pub mod resolver;
pub mod rows;
pub mod rule;
pub mod rule_op;
mod schema;
pub mod snapshot;
pub mod source;

pub use rule::RuleError;
pub use snapshot::{CompiledRule, Compiler, PolicySnapshot, compile};

#[cfg(test)]
mod proptests;
#[cfg(test)]
mod testing;
