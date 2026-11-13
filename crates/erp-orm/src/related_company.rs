//! Company-bound related reads.
//!
//! The hop's company is read in the same fetch as the
//! values read through it (the company field is in the default prefetch group), so a read costs
//! the statements of the same read of a `compute_sudo = true` field.

use std::sync::{Arc, Mutex, PoisonError};

use erp_fields::{Cell, Error, ErrorKind, FieldType, RecId, Result, Value};
use erp_registry::{CompanyMatch, CompanyScope, FieldId, FieldSpec, ModelId, ModelSpec};
use erp_sql::param::{Param, Scalar, ScalarKind};
use rustc_hash::FxHashSet;

use crate::access::CompanyHint;
use crate::company::CompanyError;
use crate::env::Env;
use crate::graph::FieldKey;
use crate::principal::CompanyId;
use crate::records::Records;
use crate::transaction::Transaction;

/// The start records of a prefetch batch that may fail their company bound without failing the
/// read: the batch holds the records asked for plus siblings read ahead (review SC2), and only
/// the records asked for may raise. A failing sibling is recorded here, reads as empty for the
/// rest of the batch, and is left out of the cache.
pub(crate) struct Lenient {
    requested: FxHashSet<RecId>,
    failed: Mutex<FxHashSet<RecId>>,
}

impl Lenient {
    /// A batch whose `requested` records raise and whose other records are recorded.
    pub(crate) fn new(requested: impl IntoIterator<Item = RecId>) -> Self {
        Self {
            requested: requested.into_iter().collect(),
            failed: Mutex::new(FxHashSet::default()),
        }
    }

    /// Whether `record` is a sibling (not asked for) whose failure is recorded.
    fn tolerates(&self, record: RecId) -> bool {
        !self.requested.contains(&record)
    }

    // the set holds plain ids and every update is one insert, so a panic elsewhere cannot leave it
    // half-written: a poisoned lock is still usable
    fn fail(&self, record: RecId) {
        self.failed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(record);
    }

    /// The siblings that failed their bound.
    pub(crate) fn into_failed(self) -> FxHashSet<RecId> {
        self.failed
            .into_inner()
            .unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn has_failed(&self, record: RecId) -> bool {
        self.failed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains(&record)
    }
}

/// The reading environment of a company-bound related field being computed.
#[derive(Clone, Copy)]
pub(crate) struct Bound<'a> {
    reader: &'a Env,
    model: ModelId,
    field: FieldId,
    lenient: Option<&'a Lenient>,
}

impl<'a> Bound<'a> {
    pub(crate) fn of(reader: &'a Env, model: ModelId, field: &FieldSpec) -> Option<Self> {
        let reader = reader.company_bound_reader();
        (field.company_bound() && !reader.su()).then_some(Self {
            reader,
            model,
            field: field.id,
            lenient: None,
        })
    }

    /// The batch whose siblings may fail, if the read is a prefetch batch.
    pub(crate) fn lenient(&self) -> Option<&'a Lenient> {
        self.lenient
    }

    /// The same bound, tolerating the failure of the siblings in `lenient` (review SC2).
    #[must_use]
    pub(crate) fn tolerating(self, lenient: &'a Lenient) -> Self {
        Self {
            lenient: Some(lenient),
            ..self
        }
    }
}

/// Whether a hop record whose companies are `companies` (empty: none) passes the match of its
/// model for a reader whose active companies are `active`; `closure` holds the active companies
/// and their ancestors (only read by the `parent_of` forms).
///
/// # Errors
///
/// `Programming` for a match this version does not know (fail closed).
fn passes(
    matching: CompanyMatch,
    companies: &[i64],
    active: &[i64],
    closure: &FxHashSet<i64>,
) -> Result<bool> {
    if companies.is_empty() {
        return Ok(true);
    }
    match matching {
        CompanyMatch::Exact | CompanyMatch::AnyOf => {
            Ok(companies.iter().any(|c| active.contains(c)))
        }
        CompanyMatch::ParentOf | CompanyMatch::ParentOfAny => {
            Ok(companies.iter().any(|c| closure.contains(c)))
        }
        other => Err(Error::programming(format!(
            "company-bound related reads do not know the company match {}",
            other.as_str()
        ))),
    }
}

/// The ids of a `parent_path` (`"1/5/9/"` gives `[1, 5, 9]`); `None` when malformed.
fn path_ids(path: &str) -> Option<Vec<i64>> {
    let body = path.strip_suffix('/')?;
    if body.is_empty() {
        return None;
    }
    body.split('/')
        .map(|part| part.parse::<i64>().ok().filter(|n| *n > 0))
        .collect()
}

/// The records `ids` of `model` as superuser: the company tree and names read on the way to a
/// refusal or a hint are the system's, not the reader's (the hop's own company already decided).
pub(crate) fn superuser_records(
    env: &Env,
    model: ModelId,
    ids: impl IntoIterator<Item = RecId>,
) -> Records {
    Records::empty(env.engine_sudo(), model).browse_ids(ids)
}

/// The text of the hop error (design note §4.1), without the hint.
pub(crate) struct HopError<'a> {
    pub(crate) label: &'a str,
    pub(crate) field: &'a str,
    pub(crate) model: &'a ModelSpec,
    pub(crate) record: RecId,
    pub(crate) hop_model: &'a ModelSpec,
    /// The hop record; `None` for a hop reached through another hop, whose id the reader may not
    /// be allowed to see (review SC6).
    pub(crate) hop: Option<RecId>,
}

impl HopError<'_> {
    fn description(model: &ModelSpec) -> &str {
        if model.description.is_empty() {
            model.name
        } else {
            &model.description
        }
    }

    /// The `Access` error, with the hint's text and its suggested-company context.
    pub(crate) fn into_error(self, hint: &CompanyHint) -> Error {
        let hop = match self.hop {
            Some(id) => format!("{}: {id}", self.hop_model.name),
            None => self.hop_model.name.to_owned(),
        };
        let mut message = format!(
            "You cannot read \u{201c}{}\u{201d} ({}.{}) of {} ({}: {}): it is read through {} \
             ({hop}), which belongs to a company outside your active companies.",
            self.label,
            self.model.name,
            self.field,
            Self::description(self.model),
            self.model.name,
            self.record,
            Self::description(self.hop_model),
        );
        if let Some(text) = hint.text() {
            message.push_str("\n\n");
            message.push_str(&text);
        }
        let error = Error::access(message);
        match hint.error_context() {
            Some(context) => error.with_context(context),
            None => error,
        }
    }
}

impl Transaction {
    /// Check the records `hops` reached through `step` from `starts` (same positions) against the
    /// company bound of the field being read: the first hop (in record order) of a
    /// company-scoped model whose company is outside the reader's active companies raises the
    /// `Access` error of [`HopError`]. `env` is the environment the path is read in (the
    /// superuser one), its reads of the hop's company fill the cache the path reads next.
    /// `direct` is true when `starts` are the records the field is read on (the error names the
    /// hop's id only then). With a [`Lenient`] bound, a failing sibling start is recorded and its
    /// hop emptied instead of raising.
    ///
    /// # Errors
    ///
    /// `Access` for a foreign hop; `Programming` for a hop whose company was not read; the reads'
    /// errors; `Database` for a malformed `parent_path`.
    pub(crate) async fn check_related_hop(
        &mut self,
        env: &Env,
        bound: Bound<'_>,
        step: FieldKey,
        starts: &[Option<RecId>],
        hops: &mut [Option<RecId>],
        direct: bool,
    ) -> Result<()> {
        let registry = Arc::clone(env.registry());
        let step_field = registry.model_by_id(step.0).field_by_id(step.1);
        // an `_inherits` parent is the same record as its child, not another one
        if step_field.delegate {
            return Ok(());
        }
        let hop_model = crate::relational::comodel_of(&registry, step_field)?;
        let (company_field, matching, many): (FieldId, CompanyMatch, bool) =
            match hop_model.company_scope {
                Some(CompanyScope::Single { field, matching }) => (field, matching.into(), false),
                Some(CompanyScope::Many { field, matching }) => (field, matching.into(), true),
                None | Some(CompanyScope::Itself) => return Ok(()),
                Some(other) => return Err(unknown_scope(other, hop_model.name)),
            };
        if hops.iter().all(Option::is_none) {
            return Ok(());
        }
        // the validated companies (review S1), not the environment's own list
        let active: Vec<i64> = self
            .companies(bound.reader)
            .await?
            .iter()
            .map(|c| c.get())
            .collect();
        let cells = self
            .cells_by_id(env, (hop_model.id, company_field), &*hops)
            .await?;
        let closure = if matching.needs_parent_path() {
            self.company_closure(env, &active).await?
        } else {
            FxHashSet::default()
        };
        for (i, slot) in hops.iter_mut().enumerate() {
            let Some(hop) = *slot else { continue };
            let companies: Vec<i64> = match cells.get(&hop) {
                Some(Cell::Id(id)) => vec![*id],
                Some(Cell::Ids(ids)) => ids.iter().map(|id| id.get()).collect(),
                // no company: the hop passes
                Some(Cell::Null) => Vec::new(),
                // the company of every hop was read above: a missing cell is a bug, not a pass
                None => {
                    return Err(Error::programming(format!(
                        "the company field of {} was not read for record {hop}",
                        hop_model.name
                    )));
                }
                Some(other) => {
                    return Err(Error::programming(format!(
                        "the company field of {} returned {other:?} for record {hop}",
                        hop_model.name
                    )));
                }
            };
            if passes(matching, &companies, &active, &closure)? {
                continue;
            }
            let record = starts.get(i).copied().flatten().ok_or_else(|| {
                Error::programming("a company-bound related hop has no start record")
            })?;
            if let Some(lenient) = bound.lenient
                && lenient.tolerates(record)
            {
                lenient.fail(record);
                *slot = None;
                continue;
            }
            let model = registry.model_by_id(bound.model);
            let field = model.field_by_id(bound.field);
            let hint = self.hop_hint(env, bound, &companies, many).await?;
            return Err(HopError {
                label: &field.string,
                field: field.name,
                model,
                record,
                hop_model,
                hop: direct.then_some(hop),
            }
            .into_error(&hint));
        }
        Ok(())
    }

    /// The ids of `active` and their ancestors (the `parent_path` of each).
    pub(crate) async fn company_closure(
        &mut self,
        env: &Env,
        active: &[i64],
    ) -> Result<FxHashSet<i64>> {
        let companies = env.registry().model("res.company").ok_or_else(|| {
            Error::programming("a parent company match needs the res.company model")
        })?;
        let ids = active
            .iter()
            .map(|id| {
                RecId::from_real(*id)
                    .ok_or_else(|| Error::programming(format!("invalid company id {id}")))
            })
            .collect::<Result<Vec<RecId>>>()?;
        let su = superuser_records(env, companies.id, ids);
        let paths = su.cells(self, "parent_path").await?;
        let mut closure = FxHashSet::default();
        for (id, cell) in active.iter().zip(paths) {
            let Cell::Str(path) = cell else {
                return Err(Error::from(CompanyError::MalformedParentPath {
                    company: *id,
                }));
            };
            let ids = path_ids(&path).ok_or(CompanyError::MalformedParentPath { company: *id })?;
            closure.extend(ids);
        }
        Ok(closure)
    }

    /// The hint of a foreign hop: the company to switch to when it is one of
    /// the subject's, the "no access" note when it is not (a foreign company is never named).
    async fn hop_hint(
        &mut self,
        env: &Env,
        bound: Bound<'_>,
        companies: &[i64],
        many: bool,
    ) -> Result<CompanyHint> {
        // the subject's companies as resolved in this transaction (a membership change is seen)
        let own: Vec<CompanyId> = match bound
            .reader
            .principal()
            .company_authority()
            .and_then(|a| self.resolved(a.subject()))
        {
            Some(newest) => newest.all_companies().to_vec(),
            None => bound
                .reader
                .principal()
                .companies()
                .all_of_subject()
                .to_vec(),
        };
        let own_id = |id: i64| own.iter().copied().find(|c| c.get() == id);
        let chosen: Option<i64> = if many {
            let shared: Vec<i64> = companies
                .iter()
                .copied()
                .filter(|id| own_id(*id).is_some())
                .collect();
            self.first_in_company_order(env, &shared).await?
        } else {
            companies.first().copied()
        };
        let Some(id) = chosen else {
            return Ok(CompanyHint::None);
        };
        let Some(company) = own_id(id) else {
            return Ok(CompanyHint::OutsideYourCompanies);
        };
        Ok(CompanyHint::Suggested {
            company,
            display_name: self.company_name(env, company).await?,
        })
    }

    /// One statement, ordered by
    /// the database, so the collation is the one every other company listing uses (review RC9).
    async fn first_in_company_order(&mut self, env: &Env, ids: &[i64]) -> Result<Option<i64>> {
        match ids {
            [] => return Ok(None),
            [only] => return Ok(Some(*only)),
            _ => {}
        }
        let companies = env.registry().model("res.company").ok_or_else(|| {
            Error::programming("a multi-company hint needs the res.company model")
        })?;
        let stored = |name: &str| companies.field(name).is_some_and(|f| f.has_column());
        let (id, table) = (erp_sql::quote("id"), erp_sql::quote(&companies.table));
        let order = if stored("sequence") && stored("name") {
            format!(
                "{}, {}, {id}",
                erp_sql::quote("sequence"),
                erp_sql::quote("name")
            )
        } else {
            id.clone()
        };
        let mut fields = vec![];
        for name in ["sequence", "name"] {
            if let Some(f) = companies.field(name).filter(|f| f.has_column()) {
                fields.push((companies.id, f.id));
            }
        }
        crate::search::flush_read_inputs(self, &fields).await?;
        let params = [Param::Array(
            ScalarKind::Int8,
            ids.iter().map(|id| Scalar::Int8(*id)).collect(),
        )];
        let rows = self
            .run_params(
                &format!(
                    "SELECT {id} FROM {table} WHERE {id} = ANY($1::int8[]) ORDER BY {order} LIMIT 1"
                ),
                &params,
            )
            .await?;
        rows.first()
            .map(|row| {
                row.try_get::<_, i64>(0).map_err(|e| {
                    Error::new(ErrorKind::Database, "could not decode the company order")
                        .with_source(e)
                })
            })
            .transpose()
    }

    async fn company_name(&mut self, env: &Env, company: CompanyId) -> Result<String> {
        let companies = env.registry().model("res.company").ok_or_else(|| {
            Error::programming("a multi-company hint needs the res.company model")
        })?;
        let rec = RecId::from_real(company.get())
            .ok_or_else(|| Error::programming(format!("invalid company id {}", company.get())))?;
        let su = superuser_records(env, companies.id, [rec]);
        match su.cells(self, "display_name").await?.into_iter().next() {
            Some(Cell::Str(name)) => Ok(name.to_string()),
            other => Err(Error::programming(format!(
                "the company {} has no display name ({other:?})",
                company.get()
            ))),
        }
    }
}

/// The cache key part of a company-bound related field: the reader's active companies, or
/// `False` for a reader in superuser mode (whose read is not bound), so the two never share a
/// cached value.
pub(crate) fn cache_key_part(tx: &Transaction, env: &Env) -> Result<Value> {
    let env = env.company_bound_reader();
    if env.su() {
        return Ok(Value::False);
    }
    Ok(Value::list(
        tx.companies_now(env)?.iter().map(|c| Value::Int(c.get())),
    ))
}

/// The SQL condition "this row of `hop`, aliased `alias`, belongs to the companies `$n` (an
/// `int8[]` parameter), or to none" by the model's own company match, for the join of a
/// company-bound related group-by; `None` for a model without company scope.
/// `active` is the placeholder text of the parameter (`$3::int8[]`): the active companies, or for
/// a `parent_of` form their ancestor closure as the read builds it (`Transaction::company_closure`,
/// which refuses a damaged `parent_path` with the read's error, review FV1), so the test is a
/// plain array membership for every form.
///
/// # Errors
///
/// `Programming` for a match this version does not know (fail closed) or a many2many scope
/// without a stored relation.
pub(crate) fn within_companies_sql(
    hop: &ModelSpec,
    alias: &str,
    active: &str,
) -> Result<Option<String>> {
    use erp_sql::quote;
    let (field, many): (FieldId, bool) = match hop.company_scope {
        Some(CompanyScope::Single { field, .. }) => (field, false),
        Some(CompanyScope::Many { field, .. }) => (field, true),
        None | Some(CompanyScope::Itself) => return Ok(None),
        Some(other) => return Err(unknown_scope(other, hop.name)),
    };
    let field = hop.field_by_id(field);
    let (alias, id) = (quote(alias), quote("id"));
    // `expr` is one of the companies the reader may use
    let member = |expr: &str| -> String { format!("{expr} = ANY({active})") };
    if !many {
        let column = format!("{alias}.{}", quote(field.name));
        return Ok(Some(format!("({column} IS NULL OR {})", member(&column))));
    }
    let FieldType::Many2many {
        relation: Some(rel),
        ..
    } = &field.ty
    else {
        return Err(Error::programming(format!(
            "{}.{} is not a stored many2many field",
            hop.name, field.name
        )));
    };
    let (table, c1, c2) = (quote(&rel.table), quote(&rel.column1), quote(&rel.column2));
    // one subplan over the link rows of the record: no rows (no company) passes, otherwise some
    // row must be usable (`bool_or` of nothing is NULL)
    Ok(Some(format!(
        "COALESCE((SELECT bool_or({}) FROM {table} AS \"__r\" WHERE \"__r\".{c1} = {alias}.{id}), \
         TRUE)",
        member(&format!("\"__r\".{c2}"))
    )))
}

/// A company scope this version does not know (`CompanyScope` is `#[non_exhaustive]`): refused,
/// never read as "no scope", which would skip the company bound (review SF2).
pub(crate) fn unknown_scope(scope: erp_registry::CompanyScope, model: &str) -> Error {
    Error::programming(format!(
        "company-bound reads do not know the company scope {scope:?} of {model}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn closure(ids: &[i64]) -> FxHashSet<i64> {
        ids.iter().copied().collect()
    }

    #[test]
    fn a_hop_without_company_always_passes() {
        for matching in [
            CompanyMatch::Exact,
            CompanyMatch::ParentOf,
            CompanyMatch::AnyOf,
            CompanyMatch::ParentOfAny,
        ] {
            assert!(passes(matching, &[], &[], &closure(&[])).unwrap());
        }
    }

    #[test]
    fn exact_and_any_of_need_an_active_company() {
        let active = [1, 3];
        assert!(passes(CompanyMatch::Exact, &[3], &active, &closure(&[])).unwrap());
        assert!(!passes(CompanyMatch::Exact, &[2], &active, &closure(&[])).unwrap());
        assert!(passes(CompanyMatch::AnyOf, &[2, 1], &active, &closure(&[])).unwrap());
        assert!(!passes(CompanyMatch::AnyOf, &[2, 4], &active, &closure(&[])).unwrap());
    }

    #[test]
    fn parent_forms_accept_the_ancestors_of_an_active_company() {
        let active = [9];
        let ancestors = closure(&[1, 5, 9]);
        assert!(passes(CompanyMatch::ParentOf, &[5], &active, &ancestors).unwrap());
        assert!(!passes(CompanyMatch::ParentOf, &[7], &active, &ancestors).unwrap());
        assert!(passes(CompanyMatch::ParentOfAny, &[7, 1], &active, &ancestors).unwrap());
        assert!(!passes(CompanyMatch::ParentOfAny, &[7, 8], &active, &ancestors).unwrap());
    }

    #[test]
    fn parent_paths_are_parsed_strictly() {
        assert_eq!(path_ids("1/5/9/"), Some(vec![1, 5, 9]));
        for bad in ["", "/", "1/5", "1//9/", "1/x/", "0/", "-3/"] {
            assert_eq!(path_ids(bad), None, "{bad:?}");
        }
    }
}
