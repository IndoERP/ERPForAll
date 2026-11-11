//! Emission of the generated company restrictions at the end of a module operation (design
//! ): the `ir.model.company_restriction` ledger, the tamper
//! check, set-based reconciliation of the generated `ir.access` rows, the orphan probe and the
//! read-back.
//!
//! Everything runs on the operation's transaction; any problem fails the operation, which rolls
//! back, so nothing is published.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use erp_orm::{GroupId, Transaction};
use erp_registry::{CompanyAccessState, Registry, Restriction};
use erp_security::company_access::{
    GENERATED_OPERATION, GeneratedIssue, GeneratedRestriction, GeneratedRow, generate, verify,
};
use erp_security::rows::RuleId;

use crate::error::{Error, Result};
use crate::system::MAX_REPORTED_ROWS;

/// What was found wrong with a generated row since it was generated (tamper check).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum TamperProblem {
    /// The ledger names a row that is gone.
    Deleted,
    /// The row is archived.
    Archived,
    /// The domain, operation or group differs from the ledger.
    Changed {
        /// The part: `domain`, `operation` or `group`.
        what: &'static str,
        /// What the row holds now.
        found: String,
        /// What was generated.
        expected: String,
    },
    /// A generated row without a ledger entry.
    CreatedOutside,
}

impl fmt::Display for TamperProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Deleted => f.write_str("was deleted"),
            Self::Archived => f.write_str("was archived"),
            Self::Changed {
                what,
                found,
                expected,
            } => write!(
                f,
                "was changed (its {what} is {found}, it was generated as {expected})"
            ),
            Self::CreatedOutside => f.write_str("was created outside a module operation"),
        }
    }
}

/// A generated row rewritten because `restore_generated_restrictions` was set
/// ([`crate::system::OperationOutcome::restored_restrictions`]).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct GeneratedRestrictionRestored {
    /// The restricted model.
    pub model: String,
    /// What had been done to the row.
    pub what: TamperProblem,
}

#[derive(Debug)]
struct Found {
    model: String,
    ledger: Option<String>,
    row: Option<RowRead>,
}

#[derive(Debug)]
struct RowRead {
    id: i64,
    name: String,
    active: bool,
    group: Option<i64>,
    operation: String,
    domain: Option<String>,
}

const READ_SQL: &str = "SELECT m.model, m.company_restriction, a.id, a.name, a.active, \
     a.group_id, a.operation, a.domain \
     FROM ir_model m FULL JOIN (SELECT * FROM ir_access WHERE generated) a ON a.model_id = m.id \
     WHERE m.company_restriction IS NOT NULL OR a.id IS NOT NULL ORDER BY m.model, a.id";

fn db(e: &tokio_postgres::Error) -> Error {
    Error::Orm(erp_fields::Error::db(e.to_string()))
}

async fn read_rows(tx: &mut Transaction) -> Result<Vec<Found>> {
    let rows = tx.query(READ_SQL, &[]).await?;
    let mut out = Vec::with_capacity(rows.len());
    for r in &rows {
        let id: Option<i64> = r.try_get(2).map_err(|e| db(&e))?;
        let row = match id {
            None => None,
            Some(id) => Some(RowRead {
                id,
                name: r.try_get(3).map_err(|e| db(&e))?,
                // a NULL marker or flag in a hand-inserted row reads as archived: the tamper
                // check then names the model instead of failing on the decode
                active: r
                    .try_get::<_, Option<bool>>(4)
                    .map_err(|e| db(&e))?
                    .unwrap_or(false),
                group: r.try_get(5).map_err(|e| db(&e))?,
                operation: r.try_get(6).map_err(|e| db(&e))?,
                domain: r.try_get(7).map_err(|e| db(&e))?,
            }),
        };
        let ledger: Option<String> = r.try_get(1).map_err(|e| db(&e))?;
        out.push(Found {
            model: r.try_get(0).map_err(|e| db(&e))?,
            // the ledger is empty when the last operation emitted none
            ledger: ledger.filter(|l| !l.is_empty()),
            row,
        });
    }
    Ok(out)
}

fn tamper_problem(found: Option<&Found>) -> Option<TamperProblem> {
    let found = found?;
    match (&found.ledger, &found.row) {
        (None, None) => None,
        (Some(_), None) => Some(TamperProblem::Deleted),
        (None, Some(_)) => Some(TamperProblem::CreatedOutside),
        (Some(ledger), Some(row)) => {
            if !row.active {
                return Some(TamperProblem::Archived);
            }
            if row.operation != GENERATED_OPERATION {
                return Some(TamperProblem::Changed {
                    what: "operation",
                    found: row.operation.clone(),
                    expected: GENERATED_OPERATION.to_owned(),
                });
            }
            if let Some(group) = row.group {
                return Some(TamperProblem::Changed {
                    what: "group",
                    found: format!("group {group}"),
                    expected: "no group".to_owned(),
                });
            }
            let domain = row.domain.as_deref().unwrap_or("");
            if domain != ledger {
                return Some(TamperProblem::Changed {
                    what: "domain",
                    found: domain.to_owned(),
                    expected: ledger.clone(),
                });
            }
            None
        }
    }
}

fn declaration(want: &GeneratedRestriction) -> String {
    format!("{}, declared by {}", want.spelling(), want.source())
}

fn invalid(want: &GeneratedRestriction, problem: String) -> Error {
    Error::GeneratedRestrictionInvalid {
        model: want.model_name().to_owned(),
        declaration: declaration(want),
        problem,
    }
}

/// Emit the generated restrictions of `registry` on the operation's transaction (design §6.3).
///
/// `restore` is the operation's `restore_generated_restrictions` option: rows changed since they
/// were generated are rewritten and reported instead of failing the operation.
///
/// # Errors
///
/// [`Error::GeneratedRestrictionTampered`], [`Error::GeneratedRestrictionInvalid`],
/// [`Error::OrphanOwnerRows`], and database errors.
pub(crate) async fn emit(
    tx: &mut Transaction,
    registry: &Registry,
    restore: bool,
) -> Result<Vec<GeneratedRestrictionRestored>> {
    let wanted = generate(registry)?;
    // A registry without the base addon's reflection and access models (module-system tests of
    // other features) has no tables to emit into: nothing to do unless something must be emitted.
    if registry.model("ir.model").is_none() || registry.model("ir.access").is_none() {
        let Some(first) = wanted.first() else {
            return Ok(Vec::new());
        };
        return Err(invalid(
            first,
            "cannot be emitted: the base addon's ir.model and ir.access are not in the registry"
                .to_owned(),
        ));
    }
    let want_by_model: BTreeMap<&str, &GeneratedRestriction> =
        wanted.iter().map(|w| (w.model_name(), w)).collect();
    let found = read_rows(tx).await?;
    let found_by_model: BTreeMap<&str, &Found> =
        found.iter().map(|f| (f.model.as_str(), f)).collect();

    // 3. tamper check, before anything is written
    let mut restored = Vec::new();
    let mut rewrite: BTreeSet<&str> = BTreeSet::new();
    for want in &wanted {
        let seen = found_by_model.get(want.model_name()).copied();
        if let Some(problem) = tamper_problem(seen) {
            if !restore {
                return Err(Error::GeneratedRestrictionTampered {
                    model: want.model_name().to_owned(),
                    declaration: declaration(want),
                    problem,
                });
            }
            // a restoration is the trace of a change made outside a module operation: keep it in
            // the operator log as well as in the outcome
            tracing::warn!(
                target: "erp::modules",
                "the operation will restore the generated company restriction of {} ({}): the row {problem} since it was generated (logged before commit; a later failure rolls it back)",
                want.model_name(),
                declaration(want)
            );
            rewrite.insert(want.model_name());
            restored.push(GeneratedRestrictionRestored {
                model: want.model_name().to_owned(),
                what: problem,
            });
        }
    }

    // 4. reconcile with set-based SQL
    let (mut ins_model, mut ins_name, mut ins_domain) = (Vec::new(), Vec::new(), Vec::new());
    let (mut upd_id, mut upd_name, mut upd_domain) = (Vec::new(), Vec::new(), Vec::new());
    let mut delete: Vec<i64> = Vec::new();
    let (mut ledger_model, mut ledger_text): (Vec<String>, Vec<Option<String>>) =
        (Vec::new(), Vec::new());
    let mut probe: Vec<&GeneratedRestriction> = Vec::new();
    for want in &wanted {
        let seen = found_by_model.get(want.model_name()).copied();
        match seen.and_then(|f| f.row.as_ref()) {
            None => {
                ins_model.push(want.model_name().to_owned());
                ins_name.push(want.name().to_owned());
                ins_domain.push(want.text().to_owned());
            }
            Some(row) => {
                let stale = row.name != want.name()
                    || row.domain.as_deref() != Some(want.text())
                    || rewrite.contains(want.model_name());
                if stale {
                    upd_id.push(row.id);
                    upd_name.push(want.name().to_owned());
                    upd_domain.push(want.text().to_owned());
                }
            }
        }
        let old = seen.and_then(|f| f.ledger.as_deref());
        if old != Some(want.text()) {
            ledger_model.push(want.model_name().to_owned());
            ledger_text.push(Some(want.text().to_owned()));
            probe.push(want);
        }
    }
    for f in &found {
        if want_by_model.contains_key(f.model.as_str()) {
            continue;
        }
        if let Some(row) = &f.row {
            if f.ledger.is_none() {
                // evidence of a row created outside a module operation, on a model that no
                // longer (or never) generates a restriction: removed, but not silently
                tracing::warn!(
                    target: "erp::modules",
                    "the operation will remove the generated ir.access row {} of {}: it has no ledger entry and no declaration generates it (logged before commit)",
                    row.id,
                    f.model
                );
            }
            delete.push(row.id);
        }
        if f.ledger.is_some() {
            ledger_model.push(f.model.clone());
            ledger_text.push(None);
        }
    }

    if !ins_model.is_empty() {
        let n = tx
            .execute(
                "INSERT INTO ir_access (name, active, model_id, operation, domain, generated) \
                 SELECT u.name, true, m.id, 'crud', u.domain, true \
                 FROM unnest($1::text[], $2::text[], $3::text[]) AS u(model, name, domain) \
                 JOIN ir_model m ON m.model = u.model",
                &[&ins_model, &ins_name, &ins_domain],
            )
            .await?;
        if n != ins_model.len() as u64 {
            let rows = tx
                .query(
                    "SELECT model FROM ir_model WHERE model = ANY($1)",
                    &[&ins_model],
                )
                .await?;
            let mut have = BTreeSet::new();
            for r in &rows {
                have.insert(r.try_get::<_, String>(0).map_err(|e| db(&e))?);
            }
            let missing = wanted.iter().find(|w| {
                ins_model.iter().any(|m| m == w.model_name()) && !have.contains(w.model_name())
            });
            return Err(match missing {
                Some(w) => invalid(w, "has no ir.model row".to_owned()),
                None => Error::InvariantViolation {
                    message: format!(
                        "the generated restrictions inserted {n} rows, expected {}",
                        ins_model.len()
                    ),
                },
            });
        }
    }
    if !upd_id.is_empty() {
        tx.execute(
            "UPDATE ir_access a SET name = u.name, active = true, group_id = NULL, \
             operation = 'crud', domain = u.domain \
             FROM unnest($1::bigint[], $2::text[], $3::text[]) AS u(id, name, domain) \
             WHERE a.id = u.id AND a.generated",
            &[&upd_id, &upd_name, &upd_domain],
        )
        .await?;
    }
    if !delete.is_empty() {
        tx.execute(
            "DELETE FROM ir_access WHERE id = ANY($1) AND generated",
            &[&delete],
        )
        .await?;
    }
    if !ledger_model.is_empty() {
        tx.execute(
            "UPDATE ir_model m SET company_restriction = u.text \
             FROM unnest($1::text[], $2::text[]) AS u(model, text) WHERE m.model = u.model",
            &[&ledger_model, &ledger_text],
        )
        .await?;
    }

    // 5. orphan probe: a newly activated or changed owner restriction
    for want in probe {
        probe_orphans(tx, registry, want).await?;
    }

    // 6. the policy tables changed by SQL
    tx.mark_policy_dirty();

    // 7. read back and verify
    let rows = read_rows(tx).await?;
    let mut generated = Vec::new();
    for f in &rows {
        let Some(r) = &f.row else { continue };
        let id = RuleId::new(r.id).map_err(|e| erp_fields::Error::programming(e.to_string()))?;
        let mut row = GeneratedRow::new(id, &f.model, &r.name, r.active, &r.operation);
        if let Some(g) = r.group {
            row = row.with_group(
                GroupId::new(g).map_err(|e| erp_fields::Error::programming(e.to_string()))?,
            );
        }
        if let Some(d) = &r.domain {
            row = row.with_domain(d);
        }
        generated.push(row);
    }
    // The first finding is reported; a second one on the same install is not distinguishable.
    if let Some(finding) = verify(registry, &generated)?.into_iter().next() {
        let Some(want) = want_by_model.get(finding.model_name()) else {
            return Err(Error::InvariantViolation {
                message: format!(
                    "the read-back found a problem on model {}, which generates no restriction",
                    finding.model_name()
                ),
            });
        };
        let problem = match finding.issue() {
            GeneratedIssue::Changed { what } => {
                let seen = generated.iter().find(|g| g.model() == finding.model_name());
                let (found, expected) = match what.as_str() {
                    "domain" => (
                        seen.and_then(GeneratedRow::domain).unwrap_or("").to_owned(),
                        want.text().to_owned(),
                    ),
                    "operation" => (
                        seen.map_or(String::new(), |g| g.operation().to_owned()),
                        GENERATED_OPERATION.to_owned(),
                    ),
                    "group" => ("a group".to_owned(), "no group".to_owned()),
                    other => {
                        return Err(Error::InvariantViolation {
                            message: format!(
                                "the read-back reports an undeclared change kind '{other}' on model {}",
                                finding.model_name()
                            ),
                        });
                    }
                };
                format!("was changed: its {what} is {found}, the declaration gives {expected}")
            }
            GeneratedIssue::NoCompanyMention => "does not mention the company".to_owned(),
            other => other.describe(),
        };
        return Err(invalid(want, problem));
    }
    Ok(restored)
}

/// Records of a model restricted through its owner that have no owner (design §6.3 step 5):
/// at most [`MAX_REPORTED_ROWS`] ids.
async fn probe_orphans(
    tx: &mut Transaction,
    registry: &Registry,
    want: &GeneratedRestriction,
) -> Result<()> {
    let model = registry
        .model(want.model_name())
        .ok_or_else(|| Error::InvariantViolation {
            message: format!("model {} is not in the registry", want.model_name()),
        })?;
    let CompanyAccessState::Declared(access) = model.company_access else {
        return Ok(());
    };
    let owner = match access.restriction() {
        Restriction::Owner(o) | Restriction::DirectAndOwner(_, o) => o,
        _ => return Ok(()),
    };
    let edge = model.field_by_id(owner.edge()).name;
    let sql = format!(
        "SELECT id FROM {} WHERE {} IS NULL ORDER BY id LIMIT {}",
        erp_sql::quote(&model.table),
        erp_sql::quote(edge),
        MAX_REPORTED_ROWS + 1
    );
    let rows = tx.query(&sql, &[]).await?;
    if rows.is_empty() {
        return Ok(());
    }
    let mut ids = Vec::new();
    for r in &rows {
        ids.push(r.try_get::<_, i64>(0).map_err(|e| db(&e))?);
    }
    let more = ids.len() as i64 > MAX_REPORTED_ROWS;
    ids.truncate(MAX_REPORTED_ROWS as usize);
    Err(Error::OrphanOwnerRows {
        model: want.model_name().to_owned(),
        edge: edge.to_owned(),
        ids,
        more,
    })
}
