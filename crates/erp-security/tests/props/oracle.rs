//! The oracle of the property tests (R-S6: the in-memory evaluator binds principal
//! parameters **as a test oracle only**) and the engine's answers it is compared with.
//!
//! - [`Oracle`] holds the values of one principal's parameters in one transaction: its subject,
//!   companies, the transaction's start time (what the SQL lowering compares with) and the user
//!   paths the generated rules use, read as superuser through the ORM.
//! - [`Oracle::allowed`] takes the policy's [`erp_orm::AccessDomain`] for (model, operation),
//!   binds its parameters ([`erp_domain::Domain::bind_params`]) and evaluates it in memory
//!   (`filtered_domain` as superuser, so the predicate's own traversals read every target as the
//!   SQL predicate's `any!` does).
//! - [`engine_allowed`] asks the engine: `filtered_access` (one statement per chunk) and, for
//!   reads, `search([])`.
//! - [`classify`] evaluates the SQL text of the predicate per row as `TRUE`, `FALSE` or `NULL`
//!   (property P2).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use erp_domain::{
    Domain, DomainError, ErrorKind as DomainErrorKind, ErrorReason, NowKind, OptimizationLevel,
    PrincipalParam, Value,
};
use erp_fields::{DateTime, Error, ErrorKind, Result};
use erp_orm::{AccessPolicy, Env, Operation, Principal, Records, Transaction};
use erp_registry::ModelSpec;
use erp_sql::filter::{Unaccent, WhereContext, compile_where};
use erp_sql::param::{Param, Scalar, ScalarKind, primitive_of};
use erp_testkit::tokio_postgres::types::ToSql;

use super::bind;

/// Record ids, sorted.
pub type IdSet = BTreeSet<i64>;

/// The parameter values of one principal in one transaction.
#[derive(Debug, Clone)]
pub struct Oracle {
    uid: i64,
    current: Option<i64>,
    allowed: Vec<i64>,
    today: Value,
    now: Value,
    /// User path (as written after `user.`) → bound value.
    paths: BTreeMap<String, Value>,
}

fn ids_value(records: &Records) -> Value {
    Value::list(records.ids())
}

fn id_or_false(records: &Records) -> Value {
    match records.ids().as_slice() {
        [] => Value::FALSE,
        [id] => Value::Int(*id),
        more => panic!("a many2one path reached {} records", more.len()),
    }
}

impl Oracle {
    /// The values of `principal`'s parameters in `tx` (one statement for the start time, the
    /// user paths read as superuser).
    pub async fn load(tx: &mut Transaction, principal: &Principal) -> Self {
        let rows = tx
            .query("SELECT (extract(epoch FROM now()) * 1000000)::int8", &[])
            .await
            .unwrap();
        let micros: i64 = rows[0].get(0);
        let start = primitive_of(DateTime::from_micros(micros)).unwrap();
        let uid = principal.subject().get();
        let su = tx.env().sudo(true);
        let user = su.records("res.users").unwrap().browse([uid]).unwrap();
        let partner = user.related(tx, "partner_id").await.unwrap();
        let commercial = partner.related(tx, "commercial_partner_id").await.unwrap();
        let mut paths = BTreeMap::new();
        paths.insert("partner_id.id".to_owned(), id_or_false(&partner));
        paths.insert("partner_id.ids".to_owned(), ids_value(&partner));
        paths.insert(
            "partner_id.commercial_partner_id.id".to_owned(),
            id_or_false(&commercial),
        );
        paths.insert(
            "partner_id.commercial_partner_id.ids".to_owned(),
            ids_value(&commercial),
        );
        let companies = principal.companies();
        Self {
            uid,
            current: companies.current().map(|c| c.get()),
            allowed: companies.allowed().iter().map(|c| c.get()).collect(),
            today: Value::Date(start.date()),
            now: Value::Datetime(start),
            paths,
        }
    }

    /// The value of `param` (a collection as a list that spreads into the `in` set holding it).
    ///
    /// # Errors
    ///
    /// A parameter the fixture has no value for (a generator the oracle was not extended for).
    pub fn value(&self, param: &PrincipalParam) -> std::result::Result<Value, DomainError> {
        let unknown = || {
            DomainError::new(
                DomainErrorKind::Programming,
                ErrorReason::UnsupportedValue(format!("the oracle has no value for {param}")),
            )
        };
        Ok(match param {
            PrincipalParam::Uid => Value::Int(self.uid),
            PrincipalParam::CompanyId => Value::Int(self.current.ok_or_else(unknown)?),
            PrincipalParam::CompanyIds => Value::list(self.allowed.iter().copied()),
            PrincipalParam::Now(NowKind::Date) => self.today.clone(),
            PrincipalParam::Now(NowKind::Timestamp) => self.now.clone(),
            PrincipalParam::UserPath(path) => self
                .paths
                .get(&path.to_string())
                .cloned()
                .ok_or_else(unknown)?,
            _ => return Err(unknown()),
        })
    }

    /// The records of `model` among `ids` that the policy's predicate of `op` lets `principal`
    /// see, evaluated in memory; the policy's error when it has no predicate.
    pub async fn allowed(
        &self,
        tx: &mut Transaction,
        policy: &dyn AccessPolicy,
        principal: &Principal,
        model: &str,
        ids: &[i64],
        op: Operation,
    ) -> Result<(IdSet, Domain)> {
        let spec: &ModelSpec = tx.registry().model(model).unwrap();
        let access = policy.access_domain(&bind(policy, principal), spec, op)?;
        let bound = access
            .domain()
            .bind_params(&|p| self.value(p))
            .map_err(Error::from)?;
        let all = tx
            .env()
            .sudo(true)
            .records(model)
            .unwrap()
            .browse(ids.iter().copied())
            .unwrap();
        let kept = all.filtered_domain(tx, &bound).await.unwrap_or_else(|e| {
            panic!("the in-memory oracle failed on {bound:?} ({model}, {op}): {e}")
        });
        Ok((kept.ids().into_iter().collect(), access.domain().clone()))
    }
}

/// What the engine answers for the transaction's principal.
#[derive(Debug)]
pub struct EngineAnswer {
    /// `filtered_access(op)` over the ids (a model-level refusal is the empty set there).
    pub filtered: Result<IdSet>,
    /// `search([])`, for reads only (a model-level refusal raises there).
    pub searched: Option<Result<IdSet>>,
}

/// Ask the engine for `env`'s principal (in `tx`): `filtered_access(op)` over `ids` and, for
/// reads, `search([])`.
pub async fn engine_allowed(
    tx: &mut Transaction,
    env: &Env,
    model: &str,
    ids: &[i64],
    op: Operation,
) -> EngineAnswer {
    let records = env
        .records(model)
        .unwrap()
        .browse(ids.iter().copied())
        .unwrap();
    let filtered = records
        .filtered_access(tx, op)
        .await
        .map(|r| r.ids().into_iter().collect());
    let searched = if op == Operation::Read {
        Some(
            env.records(model)
                .unwrap()
                .search(tx, &Domain::TRUE)
                .await
                .map(|r| r.ids().into_iter().collect()),
        )
    } else {
        None
    };
    EngineAnswer { filtered, searched }
}

/// The records the engine lets the principal `op`: both forms must agree, a model-level access
/// refusal counts as nothing visible.
///
/// # Panics
///
/// When the forms disagree or the engine fails with anything but an access refusal.
pub async fn visible(
    tx: &mut Transaction,
    env: &Env,
    model: &str,
    ids: &[i64],
    op: Operation,
) -> IdSet {
    let answer = engine_allowed(tx, env, model, ids, op).await;
    let set = |r: Result<IdSet>, form: &str| match r {
        Ok(set) => set,
        Err(error) if is_access(&error) => IdSet::new(),
        Err(error) => panic!("{model} {op} ({form}): {error} ({:?})", error.kind()),
    };
    let filtered = set(answer.filtered, "filtered_access");
    if let Some(searched) = answer.searched {
        let searched = set(searched, "search");
        assert_eq!(
            filtered,
            searched,
            "{model} {op}: filtered_access and search disagree ({})",
            diff(&filtered, &searched)
        );
    }
    filtered
}

/// Where a predicate's SQL value is `TRUE`, `FALSE` and `NULL`, per row.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Classes {
    pub true_: IdSet,
    pub false_: IdSet,
    pub null: IdSet,
}

/// Lower `domain` (a policy predicate of `model`, with principal parameters) as the engine does
/// for a search — FULL optimization in the principal's environment, the `erp-sql` planner, the
/// parameters bound from `principal` — and select `(predicate)` for every row of the table.
///
/// The engine's own lowering also completes relational sub-domains (`prepare`, crate-private);
/// the predicates generated here only traverse with `any!`, which it leaves as they are. The
/// callers compare the `TRUE` class with the engine's answer, so a lowering that drifted from the
/// engine's would fail them rather than pass silently.
///
/// # Errors
///
/// Optimization, planning and statement errors.
pub async fn classify(
    tx: &mut Transaction,
    principal: &Principal,
    model: &str,
    domain: &Domain,
) -> Result<Classes> {
    let records = tx.env().records(model)?;
    let optimized = records.optimize_domain_in(tx, domain, OptimizationLevel::Full, None)?;
    let registry = Arc::clone(tx.registry());
    let spec = registry.model(model).unwrap();
    let filter = compile_where(
        &optimized,
        &WhereContext {
            registry: &registry,
            model: spec,
            unaccent: Unaccent::Off,
            lang: None,
        },
    )?;
    let table = format!("\"{}\"", spec.table);
    let joins: String = filter.joins.iter().map(|(_, text)| text.as_str()).collect();
    let sql = format!(
        "SELECT {table}.\"id\", ({}) FROM {table}{joins}",
        filter.sql
    );
    let params = bind_params(principal, &filter.params)?;
    let refs: Vec<&(dyn ToSql + Sync)> = params
        .iter()
        .map(|p| p.as_ref() as &(dyn ToSql + Sync))
        .collect();
    let rows = tx.query(&sql, &refs).await?;
    let mut classes = Classes::default();
    for row in rows {
        let id: i64 = row.get(0);
        let value: Option<bool> = row.get(1);
        let class = match value {
            Some(true) => &mut classes.true_,
            Some(false) => &mut classes.false_,
            None => &mut classes.null,
        };
        if !class.insert(id) {
            return Err(Error::programming(format!(
                "row {id} of {model} appears twice in the classification statement {sql}"
            )));
        }
    }
    Ok(classes)
}

type Bound = Box<dyn ToSql + Sync + Send>;

/// The statement parameters of `params`, principal parameters bound from `principal` (the
/// engine's `bind_principal_params`: subject, current company, allowed companies).
fn bind_params(principal: &Principal, params: &[Param]) -> Result<Vec<Bound>> {
    params
        .iter()
        .map(|param| -> Result<Bound> {
            Ok(match param {
                Param::Principal { param, .. } => match param {
                    PrincipalParam::Uid => Box::new(principal.subject().get()),
                    PrincipalParam::CompanyId => Box::new(
                        principal
                            .companies()
                            .current()
                            .ok_or_else(|| Error::programming("no current company"))?
                            .get(),
                    ),
                    PrincipalParam::CompanyIds => Box::new(
                        principal
                            .companies()
                            .allowed()
                            .iter()
                            .map(|c| c.get())
                            .collect::<Vec<i64>>(),
                    ),
                    other => {
                        return Err(Error::programming(format!(
                            "the classification statement has no value for {other}"
                        )));
                    }
                },
                Param::Count(n) => Box::new(*n),
                Param::Scalar(scalar) => scalar_box(scalar)?,
                Param::Array(kind, items) => array_box(*kind, items)?,
            })
        })
        .collect()
}

fn scalar_box(scalar: &Scalar) -> Result<Bound> {
    Ok(match scalar {
        Scalar::BigInt(v) => Box::new(v.get()),
        Scalar::Int8(v) => Box::new(*v),
        Scalar::Float8(v) => Box::new(*v),
        Scalar::Numeric(v) => Box::new(*v),
        Scalar::Text(v) => Box::new(v.to_string()),
        Scalar::Date(v) => Box::new(*v),
        Scalar::Timestamp(v) => Box::new(*v),
        Scalar::Uuid(v) => Box::new(*v),
        Scalar::Xid8(v) => Box::new(*v),
        Scalar::TimestampTz(v) => Box::new(*v),
        // the oracle's generated rules have no range fields: refuse loudly
        Scalar::DateRange(..) | Scalar::TsRange(..) => {
            return Err(Error::programming(
                "the oracle has no binding for a range parameter".to_owned(),
            ));
        }
    })
}

#[test]
fn scalar_box_preserves_native_scalar_types_and_binary_payloads() {
    use erp_fields::{BigInt, TimestampTz, Uuid, Xid8};
    use erp_testkit::tokio_postgres::types::{IsNull, Type};

    let cases = [
        (
            Scalar::BigInt(BigInt::from_i64(i64::MIN)),
            Type::INT8,
            i64::MIN.to_be_bytes().to_vec(),
        ),
        (
            Scalar::BigInt(BigInt::from_i64(0)),
            Type::INT8,
            0_i64.to_be_bytes().to_vec(),
        ),
        (
            Scalar::BigInt(BigInt::from_i64(i64::MAX)),
            Type::INT8,
            i64::MAX.to_be_bytes().to_vec(),
        ),
        (Scalar::Uuid(Uuid::NIL), Type::UUID, vec![0; 16]),
        (
            Scalar::Xid8(Xid8::from_u64(u64::MAX)),
            Type::XID8,
            u64::MAX.to_be_bytes().to_vec(),
        ),
        (
            Scalar::TimestampTz(TimestampTz::parse("2000-01-01T00:00:00Z").unwrap()),
            Type::TIMESTAMPTZ,
            vec![0; 8],
        ),
    ];
    for (scalar, ty, expected) in cases {
        let bound = scalar_box(&scalar).unwrap();
        let mut bytes = Default::default();
        let nullness = bound.to_sql_checked(&ty, &mut bytes).unwrap();
        assert!(matches!(nullness, IsNull::No));
        assert_eq!(bytes.as_ref(), expected.as_slice(), "native parameter {ty}");
    }
}

fn array_box(kind: ScalarKind, items: &[Scalar]) -> Result<Bound> {
    fn all<T>(items: &[Scalar], pick: impl Fn(&Scalar) -> Option<T>) -> Result<Vec<T>> {
        items
            .iter()
            .map(|s| pick(s).ok_or_else(|| Error::programming("mixed array parameter")))
            .collect()
    }
    Ok(match kind {
        ScalarKind::Int8 => Box::new(all(items, |s| match s {
            Scalar::Int8(v) => Some(*v),
            _ => None,
        })?),
        ScalarKind::Float8 => Box::new(all(items, |s| match s {
            Scalar::Float8(v) => Some(*v),
            _ => None,
        })?),
        ScalarKind::Text => Box::new(all(items, |s| match s {
            Scalar::Text(v) => Some(v.to_string()),
            _ => None,
        })?),
        ScalarKind::Date => Box::new(all(items, |s| match s {
            Scalar::Date(v) => Some(*v),
            _ => None,
        })?),
        ScalarKind::Timestamp => Box::new(all(items, |s| match s {
            Scalar::Timestamp(v) => Some(*v),
            _ => None,
        })?),
        other => {
            return Err(Error::programming(format!(
                "the classification statement cannot bind {other:?} arrays"
            )));
        }
    })
}

/// Whether `error` is an access refusal.
pub fn is_access(error: &Error) -> bool {
    error.kind() == ErrorKind::Access
}

/// `a` and `b` as a readable difference.
pub fn diff(a: &IdSet, b: &IdSet) -> String {
    let only_a: Vec<&i64> = a.difference(b).collect();
    let only_b: Vec<&i64> = b.difference(a).collect();
    format!("only in the first: {only_a:?}; only in the second: {only_b:?}")
}
