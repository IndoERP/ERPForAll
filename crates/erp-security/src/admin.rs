//! Administration of the policy from the command line (`erp access`,,
//! Revision 2 S3): a report of what the policy holds, and switching off a broken rule, the
//! recovery path when a rule cannot be compiled. `ir.access` itself is governed by permissions
//! only, so an administrator can always repair a rule; these run as the `System` principal.

use std::fmt;
use std::sync::Arc;

use erp_fields::{Error, ErrorKind, Result, Value};
use erp_orm::{QualifiedXmlId, Transaction};
use erp_registry::Registry;

use crate::report::BuildReport;
use crate::rows::PolicyRows;
use crate::snapshot::Compiler;

/// What `erp access report` prints: the `policy` generation the rows were read at and the build
/// report of compiling them (rule errors, capabilities without a mapping and mappings without
/// a capability, group cycles, counts).
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct AccessReport {
    /// The `policy` generation the rows were read at.
    pub generation: u64,
    /// What compiling them found.
    pub build: BuildReport,
}

impl AccessReport {
    /// Compile `rows` against `registry` (with the capabilities linked into the binary) and keep
    /// the build report.
    ///
    /// # Errors
    ///
    /// The compiler's errors (the build itself failing, not a rule).
    pub fn build(rows: &PolicyRows, registry: Arc<Registry>) -> Result<Self> {
        let snapshot = Compiler::new(registry).compile(rows)?;
        Ok(Self {
            generation: rows.generation(),
            build: snapshot.report().clone(),
        })
    }
}

impl fmt::Display for AccessReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "policy generation {}", self.generation)?;
        // the build report ends every line with a newline
        write!(f, "{}", self.build)
    }
}

/// A rule `erp access disable-rule` handled.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct DisabledRule {
    /// The rule's id.
    pub id: i64,
    /// Its label.
    pub name: String,
    /// The model it applies to.
    pub model: String,
    /// Whether it was active before (`false`: it already was disabled, nothing was written).
    pub was_active: bool,
}

impl fmt::Display for DisabledRule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = if self.was_active {
            "disabled"
        } else {
            "was already disabled"
        };
        write!(
            f,
            "access rule {} '{}' on {} {state}",
            self.id, self.name, self.model
        )
    }
}

/// Set `active = false` on the `ir.access` row named by `xmlid`, through the ORM (audit fields,
/// the policy-source hook; the table's trigger bumps the `policy` generation). Run it in a
/// `System` transaction and commit.
///
/// # Errors
///
/// `Missing` when no record has the external id, or the rule it names is gone; `Validation` when
/// the external id names a record of another model; the ORM's errors.
pub async fn disable_rule(tx: &mut Transaction, xmlid: &QualifiedXmlId) -> Result<DisabledRule> {
    let found = tx
        .query(
            "SELECT d.model::text, d.res_id::int8 FROM ir_model_data d \
             WHERE d.module = $1 AND d.name = $2",
            &[&xmlid.module(), &xmlid.name()],
        )
        .await?;
    let Some(row) = found.first() else {
        return Err(Error::new(
            ErrorKind::Missing,
            format!("no record has the external id '{xmlid}'"),
        ));
    };
    let model: String = decode(row, 0)?;
    let res_id: Option<i64> = decode(row, 1)?;
    if model != "ir.access" {
        return Err(Error::new(
            ErrorKind::Validation,
            format!("the external id '{xmlid}' names a {model} record, not an access rule"),
        ));
    }
    let gone = || {
        Error::new(
            ErrorKind::Missing,
            format!("the access rule of the external id '{xmlid}' no longer exists"),
        )
    };
    let id = res_id.ok_or_else(gone)?;
    let rule = tx
        .query(
            "SELECT a.name::text, m.model::text, a.active FROM ir_access a \
             JOIN ir_model m ON m.id = a.model_id WHERE a.id = $1",
            &[&id],
        )
        .await?;
    let row = rule.first().ok_or_else(gone)?;
    let name: String = decode(row, 0)?;
    let model: String = decode(row, 1)?;
    let was_active = decode::<Option<bool>>(row, 2)? == Some(true);
    if was_active {
        let rules = tx.env().records("ir.access")?.browse([id])?;
        let mut vals = std::collections::BTreeMap::new();
        vals.insert("active".to_owned(), Value::Bool(false));
        rules.write_values(tx, &vals).await?;
        tx.flush().await?;
    }
    Ok(DisabledRule {
        id,
        name,
        model,
        was_active,
    })
}

fn decode<'r, T: tokio_postgres::types::FromSql<'r>>(
    row: &'r tokio_postgres::Row,
    idx: usize,
) -> Result<T> {
    row.try_get(idx).map_err(|e| {
        Error::new(
            ErrorKind::Database,
            format!("could not decode column {idx} of an access rule row: {e}"),
        )
        .with_source(e)
    })
}
