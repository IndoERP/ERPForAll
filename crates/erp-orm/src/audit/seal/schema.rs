// SPDX-License-Identifier: MIT OR Apache-2.0
//! Closed owner-side chain metadata. Ordinary event tables remain partitioned and independent.
use crate::Transaction;
use erp_fields::{Error, Result};
use std::fmt;

mod catalog;

/// A chain schema or epoch transition refusal; no row payload is included.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ChainProblem {
    /// The named closed object's catalog no longer matches its declaration.
    Schema {
        /// Fully qualified closed object name.
        object: &'static str,
        /// Catalog property that failed validation.
        property: &'static str,
    },
    /// An explicit initialization would reset existing history.
    AlreadyInitialized,
    /// No explicit initialization has occurred.
    NotInitialized,
    /// A linked epoch requires a final predecessor seal.
    EmptyEpoch,
    /// Persisted epoch/seal metadata is inconsistent.
    InvalidHead,
    /// The positive SQL bigint epoch space is exhausted.
    EpochOverflow,
    /// Epoch mutation requires a fresh snapshot after acquiring its transaction lock.
    Isolation,
    /// A reason is not a bounded identifier.
    Reason,
}
impl fmt::Display for ChainProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a later release: ")?;
        match self {
            Self::Schema { object, property } => write!(f, "{object} has incompatible {property}; owner repair is required"),
            Self::AlreadyInitialized => f.write_str("chain is already initialized; use a linked epoch transition"),
            Self::NotInitialized => f.write_str("chain is not initialized; initialize it explicitly"),
            Self::EmptyEpoch => f.write_str("current epoch has no seal; seal it before starting another epoch"),
            Self::InvalidHead => f.write_str("epoch or predecessor seal is inconsistent; owner investigation is required"),
            Self::EpochOverflow => f.write_str("epoch number exhausted SQL bigint; owner investigation is required"),
            Self::Isolation => f.write_str("epoch mutation requires Read Committed isolation"),
            Self::Reason => f.write_str("reason must be an ASCII identifier of 1 to 128 bytes (letters, digits, dot, underscore or hyphen)"),
        }
    }
}
impl std::error::Error for ChainProblem {}
impl From<ChainProblem> for Error {
    fn from(p: ChainProblem) -> Self {
        Error::programming(p.to_string()).with_source(p)
    }
}
pub(super) fn drift(object: &'static str, property: &'static str) -> Error {
    ChainProblem::Schema { object, property }.into()
}

/// Whether this installation created any of the closed storage objects. It creates no epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct ChainSchemaReport {
    created: bool,
}
impl ChainSchemaReport {
    /// At least one missing table/sequence was installed in the caller's transaction.
    pub const fn created(self) -> bool {
        self.created
    }
}

pub(super) struct Column {
    name: &'static str,
    ty: &'static str,
    nullable: bool,
    default: Option<&'static str>,
}
const fn c(name: &'static str, ty: &'static str) -> Column {
    Column {
        name,
        ty,
        nullable: false,
        default: None,
    }
}
const fn optional(name: &'static str, ty: &'static str) -> Column {
    Column {
        name,
        ty,
        nullable: true,
        default: None,
    }
}
const fn generated(name: &'static str, ty: &'static str, default: &'static str) -> Column {
    Column {
        name,
        ty,
        nullable: false,
        default: Some(default),
    }
}
pub(super) struct Constraint {
    name: &'static str,
    ddl: &'static str,
    catalog: &'static str,
}
macro_rules! constraint {
    ($name:literal, $ddl:literal, $catalog:literal) => {
        Constraint {
            name: $name,
            ddl: $ddl,
            catalog: $catalog,
        }
    };
}
pub(super) struct Table {
    name: &'static str,
    columns: &'static [Column],
    constraints: &'static [Constraint],
    indexes: &'static [&'static str],
}
pub(super) const CHAIN: Table = Table {
    name: "erp_event.ir_audit_chain",
    columns: &[
        c("epoch", "bigint"),
        generated("incarnation", "uuid", "uuidv7()"),
        c("format_version", "smallint"),
        generated(
            "started_at",
            "timestamp with time zone",
            "transaction_timestamp()",
        ),
        optional("previous_epoch", "bigint"),
        optional("link_hash", "bytea"),
        c("reason", "text"),
    ],
    constraints: &[
        constraint!(
            "ir_audit_chain_pkey",
            "PRIMARY KEY (epoch)",
            "PRIMARY KEY (epoch)"
        ),
        constraint!(
            "ir_audit_chain_successor",
            "UNIQUE (previous_epoch)",
            "UNIQUE (previous_epoch)"
        ),
        constraint!(
            "ir_audit_chain_previous",
            "FOREIGN KEY (previous_epoch) REFERENCES erp_event.ir_audit_chain(epoch)",
            "FOREIGN KEY (previous_epoch) REFERENCES erp_event.ir_audit_chain(epoch)"
        ),
        constraint!(
            "ir_audit_chain_epoch",
            "CHECK (epoch > 0)",
            "CHECK ((epoch > 0))"
        ),
        constraint!(
            "ir_audit_chain_incarnation",
            "CHECK (incarnation <> '00000000-0000-0000-0000-000000000000'::uuid)",
            "CHECK ((incarnation <> '00000000-0000-0000-0000-000000000000'::uuid))"
        ),
        constraint!(
            "ir_audit_chain_format",
            "CHECK (format_version = 1)",
            "CHECK ((format_version = 1))"
        ),
        constraint!(
            "ir_audit_chain_genesis",
            "CHECK ((previous_epoch IS NULL) = (epoch = 1))",
            "CHECK (((previous_epoch IS NULL) = (epoch = 1)))"
        ),
        constraint!(
            "ir_audit_chain_link",
            "CHECK ((link_hash IS NULL) = (epoch = 1))",
            "CHECK (((link_hash IS NULL) = (epoch = 1)))"
        ),
        constraint!(
            "ir_audit_chain_link_bytes",
            "CHECK (octet_length(link_hash) = 32)",
            "CHECK ((octet_length(link_hash) = 32))"
        ),
        constraint!(
            "ir_audit_chain_previous_number",
            "CHECK (previous_epoch = epoch - 1)",
            "CHECK ((previous_epoch = (epoch - 1)))"
        ),
        constraint!(
            "ir_audit_chain_reason",
            "CHECK (reason ~ '^[A-Za-z0-9_.-]{1,128}$')",
            "CHECK ((reason ~ '^[A-Za-z0-9_.-]{1,128}$'::text))"
        ),
    ],
    indexes: &["ir_audit_chain_pkey", "ir_audit_chain_successor"],
};
pub(super) const SEAL: Table = Table {
    name: "erp_event.ir_audit_seal",
    columns: &[
        generated(
            "id",
            "bigint",
            "nextval('erp_event.ir_audit_seal_id_seq'::regclass)",
        ),
        c("chain_epoch", "bigint"),
        c("batch_no", "bigint"),
        c("lower_xid", "xid8"),
        c("lower_id", "bigint"),
        c("upper_xid", "xid8"),
        c("upper_id", "bigint"),
        c("row_count", "bigint"),
        c("batch_digest", "bytea"),
        c("prev_seal_hash", "bytea"),
        c("seal_hash", "bytea"),
        c("format_version", "smallint"),
        generated(
            "create_date",
            "timestamp with time zone",
            "transaction_timestamp()",
        ),
    ],
    constraints: &[
        constraint!("ir_audit_seal_pkey", "PRIMARY KEY (id)", "PRIMARY KEY (id)"),
        constraint!(
            "ir_audit_seal_batch",
            "UNIQUE (chain_epoch,batch_no)",
            "UNIQUE (chain_epoch, batch_no)"
        ),
        constraint!(
            "ir_audit_seal_epoch",
            "FOREIGN KEY (chain_epoch) REFERENCES erp_event.ir_audit_chain(epoch)",
            "FOREIGN KEY (chain_epoch) REFERENCES erp_event.ir_audit_chain(epoch)"
        ),
        constraint!("ir_audit_seal_id", "CHECK (id > 0)", "CHECK ((id > 0))"),
        constraint!(
            "ir_audit_seal_batch_no",
            "CHECK (batch_no > 0)",
            "CHECK ((batch_no > 0))"
        ),
        constraint!(
            "ir_audit_seal_count",
            "CHECK (row_count > 0)",
            "CHECK ((row_count > 0))"
        ),
        constraint!(
            "ir_audit_seal_lower_id",
            "CHECK (lower_id >= 0)",
            "CHECK ((lower_id >= 0))"
        ),
        constraint!(
            "ir_audit_seal_upper_id",
            "CHECK (upper_id > 0)",
            "CHECK ((upper_id > 0))"
        ),
        constraint!(
            "ir_audit_seal_upper_xid",
            "CHECK (upper_xid > '0'::xid8)",
            "CHECK ((upper_xid > '0'::xid8))"
        ),
        constraint!(
            "ir_audit_seal_initial_cursor",
            "CHECK ((lower_xid = '0'::xid8) = (lower_id = 0))",
            "CHECK (((lower_xid = '0'::xid8) = (lower_id = 0)))"
        ),
        constraint!(
            "ir_audit_seal_order",
            "CHECK ((lower_xid,lower_id) < (upper_xid,upper_id))",
            "CHECK ((ROW(lower_xid, lower_id) < ROW(upper_xid, upper_id)))"
        ),
        constraint!(
            "ir_audit_seal_digest",
            "CHECK (octet_length(batch_digest) = 32)",
            "CHECK ((octet_length(batch_digest) = 32))"
        ),
        constraint!(
            "ir_audit_seal_predecessor",
            "CHECK (octet_length(prev_seal_hash) = 32)",
            "CHECK ((octet_length(prev_seal_hash) = 32))"
        ),
        constraint!(
            "ir_audit_seal_hash",
            "CHECK (octet_length(seal_hash) = 32)",
            "CHECK ((octet_length(seal_hash) = 32))"
        ),
        constraint!(
            "ir_audit_seal_format",
            "CHECK (format_version = 1)",
            "CHECK ((format_version = 1))"
        ),
    ],
    indexes: &["ir_audit_seal_pkey", "ir_audit_seal_batch"],
};
pub(super) const SEQUENCE: &str = "erp_event.ir_audit_seal_id_seq";

impl Transaction {
    /// Install or validate the optional unpartitioned chain storage, without initializing an epoch.
    /// The caller owns commit/rollback. Existing objects are checked and never repaired.
    /// # Errors
    /// `Access` before SQL unless System/full; `Programming` with [`ChainProblem`] on catalog drift;
    /// `Database` for failed SQL. Use the owner database connection and roll back after an error.
    pub async fn install_audit_chain_schema(&mut self) -> Result<ChainSchemaReport> {
        self.require_system_full("Transaction::install_audit_chain_schema")?;
        // Existing installer owns the namespace lock, server admission and audit prerequisites.
        self.install_audit_schema().await?;
        let created = self.chain_catalog_install().await?;
        Ok(ChainSchemaReport { created })
    }
    pub(super) async fn check_chain_storage(&mut self) -> Result<()> {
        self.chain_catalog_check().await
    }
}
