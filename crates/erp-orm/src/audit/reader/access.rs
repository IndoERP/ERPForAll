// SPDX-License-Identifier: MIT OR Apache-2.0
use super::{denied, integrity, store::Candidate};
use crate::{Env, FieldOp, Operation, SearchOptions, Transaction};
use erp_domain::{Domain, Operator, Value};
use erp_fields::{ErrorKind, Result};
use std::collections::{BTreeMap, HashSet};

pub(super) async fn caller(tx: &mut Transaction, fields: &[String]) -> Result<Env> {
    if tx.route() != crate::Route::Primary {
        return Err(denied());
    }
    let env = tx.env().audit_reader_caller()?;
    tx.prepare_access(&env).await?;
    let capability = crate::CAPABILITIES
        .iter()
        .find(|cap| cap.name() == "audit.read")
        .ok_or_else(super::unavailable)?;
    if !tx.has_capability(&env, capability).await? {
        return Err(denied());
    }
    let spec = env.registry().model("ir.audit.entry").ok_or_else(denied)?;
    if spec.namespace != erp_registry::TableNamespace::Event {
        return Err(denied());
    }
    tx.check_model(&env, spec.id, Operation::Read)
        .map_err(sanitize)?;
    let mask = tx.field_mask(&env, spec.id)?;
    for name in fields
        .iter()
        .map(String::as_str)
        .chain(["entry_id", "create_date"])
    {
        let field = spec.field(name).ok_or_else(denied)?;
        if !mask.allows(field.id, FieldOp::Read) {
            return Err(denied());
        }
    }
    Ok(env)
}
pub(super) fn sanitize(e: erp_fields::Error) -> erp_fields::Error {
    match e.kind() {
        ErrorKind::Access | ErrorKind::AccessDenied => denied(),
        ErrorKind::ResourceExhausted => super::limit(),
        _ => super::unavailable(),
    }
}
pub(super) async fn model(tx: &mut Transaction, env: &Env, name: &str) -> Result<bool> {
    let Some(spec) = env.registry().model(name) else {
        return Ok(false);
    };
    if !spec.has_generic_table() {
        return Ok(false);
    }
    match tx.check_model(env, spec.id, Operation::Read) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == ErrorKind::Access => Ok(false),
        Err(e) => Err(e),
    }
}

pub(super) async fn eligible(
    tx: &mut Transaction,
    env: &Env,
    rows: &[Candidate],
) -> Result<Vec<bool>> {
    let mut by_model: BTreeMap<&str, HashSet<i64>> = BTreeMap::new();
    let mut known = vec![false; rows.len()];
    for (i, row) in rows.iter().enumerate() {
        if row.version != 1 {
            continue;
        }
        let (Some(name), Some(method)) = (&row.model, &row.method) else {
            continue;
        };
        if row.targets.is_empty() || !model(tx, env, name).await? {
            continue;
        }
        let spec = env.registry().model(name).ok_or_else(integrity)?;
        if tx.shared.exposure.get(spec.id, method).is_none() {
            continue;
        }
        known[i] = true;
        by_model
            .entry(name)
            .or_default()
            .extend(row.targets.iter().copied());
    }
    let mut visible: BTreeMap<&str, HashSet<i64>> = BTreeMap::new();
    for (name, ids) in by_model {
        let mut ids: Vec<i64> = ids.into_iter().collect();
        ids.sort_unstable();
        let records = env.records(name)?;
        let mut found = HashSet::new();
        for chunk in ids.chunks(4096) {
            tx.check_not_cancelled()?;
            let domain = Domain::condition(
                "id",
                Operator::In,
                Value::list(chunk.iter().copied().map(Value::Int)),
            )?;
            match records
                .search_with(tx, &domain, &SearchOptions::new().active_test(false))
                .await
            {
                Ok(result) => found.extend(result.ids()),
                // A target field/record refusal conceals its entries exactly as a missing key.
                // Operational and policy-engine failures are not authorization refusals.
                Err(e) if e.kind() == ErrorKind::Access => {}
                Err(e) => return Err(e),
            }
        }
        visible.insert(name, found);
    }
    for (i, row) in rows.iter().enumerate() {
        if known[i] {
            let name = row.model.as_deref().ok_or_else(integrity)?;
            let ids = visible.get(name).ok_or_else(integrity)?;
            known[i] = row.targets.iter().all(|id| ids.contains(id));
        }
    }
    Ok(known)
}
