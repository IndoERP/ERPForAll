//! `web_save`, `web_save_multi`, `web_resequence` and `web_unlink` (GRP §4.9).
//!
//! Writes are set-based: `web_save_multi` writes records sharing the same values with one `write`
//! (GRP §7 D4), and every per-record value ends up in the flush's one `UPDATE … FROM unnest(…)`
//! per field set, so resequencing 200 rows is one statement (GRP §8 W3).

use std::collections::hash_map::RandomState;
use std::hash::BuildHasher;
use std::sync::Arc;

use erp_fields::{Error, ErrorKind, OnDelete, RecId, Result, Value, ValueMap};
use erp_sql::quote;
use rustc_hash::{FxHashMap, FxHashSet};

use super::read::web_read;
use super::spec::Spec;
use crate::Transaction;
use crate::records::Records;

const PENDING_ATTACHMENTS: &str = "pending_attachment_ids";

/// `base.web_save`.
pub(crate) async fn web_save(
    tx: &mut Transaction,
    records: &Records,
    args: super::WebSave,
) -> Result<Vec<Value>> {
    let mut record = records.clone();
    if let Some(pending) = records.env().context().get(PENDING_ATTACHMENTS) {
        if pending.is_truthy() {
            return Err(Error::validation(
                "web_save with pending_attachment_ids needs ir.attachment: not supported until M3",
            ));
        }
        // the key never reaches create/write
        let mut context = records.env().context().clone();
        context.remove(PENDING_ATTACHMENTS);
        record = record.with_env(records.env().with_context_replaced(context));
    }
    let saved = if record.is_empty() {
        let vals = record.vals_from_values(&args.vals)?;
        record.create(tx, vec![vals]).await?
    } else {
        record.write_values(tx, &args.vals).await?;
        record
    };
    let target = match args.next_id {
        Some(id) => saved.browse([id])?,
        None => saved,
    };
    web_read(tx, &target, &args.specification).await
}

/// Maximum records that `web_save_multi` accepts in one call, a
/// [per-call shape bound](super#per-call-shape-bounds) (`Parity: divergent`). The
/// grouping below compares values maps, worst case O(N²) when every map lands in one bucket: 1,000
/// keeps that under a million map comparisons and is far above a list view's page of edited rows.
pub const MAX_SAVE_MULTI_RECORDS: usize = 1_000;

/// The bucket of a values map in `web_save_multi`'s grouping: equal maps (`ValueMap`'s `==`, which
/// ignores key order) always land in the same bucket, because each top-level entry is hashed on its
/// own with [`Value`]'s `Hash` (consistent with its `Eq`) and the entry hashes are summed. Only a
/// hint: the group is decided by `==`, so two different maps sharing a bucket stay apart. It cannot
/// fail. The top-level entries are keyed per call (`RandomState`), which makes chosen collisions
/// hard; the worst case is still one bucket, O(N²) map comparisons, bounded by
/// [`MAX_SAVE_MULTI_RECORDS`].
fn bucket_of(state: &RandomState, map: &ValueMap) -> u64 {
    let entries = map
        .iter()
        .fold(0u64, |sum, entry| sum.wrapping_add(state.hash_one(entry)));
    state.hash_one((map.len(), entries))
}

pub(crate) async fn web_save_multi(
    tx: &mut Transaction,
    records: &Records,
    vals_list: Vec<ValueMap>,
    spec: Spec,
) -> Result<Vec<Value>> {
    if vals_list.len() > MAX_SAVE_MULTI_RECORDS {
        return Err(Error::validation(format!(
            "web_save_multi accepts at most {MAX_SAVE_MULTI_RECORDS} records, got {}",
            vals_list.len()
        )));
    }
    if records.len() != vals_list.len() {
        return Err(Error::validation(
            "Each record must have a corresponding vals entry.",
        ));
    }
    let ids = records.rec_ids();
    let mut seen: FxHashSet<RecId> = FxHashSet::default();
    seen.reserve(ids.len());
    if !ids.iter().all(|id| seen.insert(*id)) {
        for (id, vals) in ids.iter().zip(&vals_list) {
            records.browse_ids(vec![*id]).write_values(tx, vals).await?;
        }
        return web_read(tx, records, &spec).await;
    }
    // groups in first-seen order; a bucket lists the groups whose maps hash to it
    let mut groups: Vec<(&ValueMap, Vec<RecId>)> = Vec::new();
    let mut buckets: FxHashMap<u64, Vec<usize>> = FxHashMap::default();
    let state = RandomState::new();
    for (id, vals) in ids.iter().zip(&vals_list) {
        let bucket = buckets.entry(bucket_of(&state, vals)).or_default();
        match bucket.iter().copied().find(|&g| groups[g].0 == vals) {
            Some(g) => groups[g].1.push(*id),
            None => {
                bucket.push(groups.len());
                groups.push((vals, vec![*id]));
            }
        }
    }
    for (vals, ids) in groups {
        records.browse_ids(ids).write_values(tx, vals).await?;
    }
    web_read(tx, records, &spec).await
}

/// Maximum records that `web_resequence` accepts in one call, a
/// [per-call shape bound](super#per-call-shape-bounds) (`Parity: divergent`). Every
/// record is one `write` through the method chain; 1,000 is above any list or kanban column the web
/// client drags in, where a 2 MiB body could otherwise name some 250,000 ids.
pub const MAX_RESEQUENCE_RECORDS: usize = 1_000;

pub(crate) async fn web_resequence(
    tx: &mut Transaction,
    records: &Records,
    spec: Spec,
    field_name: String,
    offset: i64,
) -> Result<Vec<Value>> {
    if records.len() > MAX_RESEQUENCE_RECORDS {
        return Err(Error::validation(format!(
            "web_resequence accepts at most {MAX_RESEQUENCE_RECORDS} records, got {}",
            records.len()
        )));
    }
    if records.spec().field(&field_name).is_none() {
        tracing::debug!(target: "erp::orm", model = records.model_name(), field = ?field_name, "web_resequence on an unknown field");
        return Ok(Vec::new());
    }
    for (i, record) in records.iter().enumerate() {
        let sequence = i64::try_from(i)
            .ok()
            .and_then(|i| offset.checked_add(i))
            .ok_or_else(|| {
                Error::validation(format!(
                    "web_resequence(): offset {offset} overflows for {} records",
                    records.len()
                ))
            })?;
        let mut vals = ValueMap::new();
        vals.insert(field_name.clone(), Value::Int(sequence));
        record.write_values(tx, &vals).await?;
    }
    web_read(tx, records, &spec).await
}

/// Whether a database error is a foreign-key or `RESTRICT` violation.
fn is_blocked(error: &Error) -> bool {
    matches!(error.sqlstate(), Some("23503" | "23001"))
}

/// The model blocking a deletion, as the foreign-key error names it (`dberror` attaches it).
fn blocking_model_of(error: &Error) -> Option<String> {
    match error.context() {
        Some(Value::Map(m)) => match m.get("blocking_model") {
            Some(Value::Str(s)) => Some(s.to_string()),
            _ => None,
        },
        _ => None,
    }
}

fn decode(e: tokio_postgres::Error) -> Error {
    Error::new(ErrorKind::Database, "could not decode a blocking reference").with_source(e)
}

/// `base.web_unlink`: `unlink()` in a savepoint.
/// Nothing
/// is deleted when it fails.
pub(crate) async fn web_unlink(tx: &mut Transaction, records: &Records) -> Result<bool> {
    let target = records.clone();
    let outcome = tx
        .savepoint(async |tx: &mut Transaction| target.unlink(tx).await)
        .await;
    let error = match outcome {
        Ok(()) => return Ok(true),
        Err(error) if is_blocked(&error) => error,
        Err(error) => return Err(error),
    };
    let registry = Arc::clone(records.env().registry());
    let spec = registry.model_by_id(records.model_id());
    let ids: Vec<i64> = records.ids();
    // fast path: the direct `restrict` references, one statement
    let mut parts: Vec<String> = Vec::new();
    let mut names: Vec<String> = Vec::new();
    for (ref_model, ref_field) in tx.shared.relations.referencing(records.model_id()).to_vec() {
        let rm = registry.model_by_id(ref_model);
        let rf = rm.field_by_id(ref_field);
        let restrict = matches!(
            rf.ty,
            erp_fields::FieldType::Many2one {
                ondelete: OnDelete::Restrict,
                ..
            }
        );
        // a company map is no foreign key: its restrict refusal is the unlink's own
        if !restrict || !rm.has_table() || !rf.has_column() || rf.company_dependent {
            continue;
        }
        parts.push(format!(
            "SELECT DISTINCT {k}::int4, {col} FROM {table} WHERE {col} = ANY($1::int8[])",
            k = names.len(),
            col = quote(rf.name),
            table = quote(&rm.table)
        ));
        names.push(rm.description.clone());
    }
    let mut blocked: FxHashSet<i64> = FxHashSet::default();
    let mut blocking: Option<String> = None;
    if !parts.is_empty() {
        let sql = parts.join(" UNION ALL ");
        for row in tx.query(&sql, &[&ids]).await? {
            let k: i32 = row.try_get(0).map_err(decode)?;
            let id: i64 = row.try_get(1).map_err(decode)?;
            if blocking.is_none() {
                blocking = usize::try_from(k).ok().and_then(|k| names.get(k).cloned());
            }
            blocked.insert(id);
        }
    }
    if blocked.is_empty() {
        // blocked further away: each record alone, always rolled back
        for record in records.iter() {
            let probe = record.clone();
            let result = tx
                .savepoint(async |tx: &mut Transaction| {
                    probe.unlink(tx).await?;
                    Err::<(), Error>(Error::new(ErrorKind::UserError, PROBE))
                })
                .await;
            match result {
                Err(e) if e.message() == PROBE => {}
                Err(e) if is_blocked(&e) => {
                    if blocking.is_none() {
                        blocking = blocking_model_of(&e)
                            .and_then(|m| registry.model(&m).map(|m| m.description.clone()));
                    }
                    blocked.extend(record.ids());
                }
                Err(e) => return Err(e),
                Ok(()) => {}
            }
        }
    }
    let model_name = blocking
        .or_else(|| {
            blocking_model_of(&error)
                .and_then(|m| registry.model(&m).map(|m| m.description.clone()))
        })
        .unwrap_or_else(|| "another model".to_owned());
    let mut context = ValueMap::new();
    context.insert(
        "archivable".to_owned(),
        Value::Bool(spec.active_name.is_some()),
    );
    context.insert("model_name".to_owned(), Value::str(&model_name));
    context.insert("res_model".to_owned(), Value::str(spec.name));
    context.insert(
        "res_ids".to_owned(),
        Value::list(ids.iter().map(|i| Value::Int(*i))),
    );
    context.insert(
        "blocked_ids".to_owned(),
        Value::list(
            ids.iter()
                .filter(|i| blocked.contains(i))
                .map(|i| Value::Int(*i)),
        ),
    );
    Err(Error::user(format!(
        "The database refused the change: {model_name} records still refer to this record"
    ))
    .with_context(Value::Map(Arc::new(context))))
}

/// The marker error that rolls a probing savepoint back.
const PROBE: &str = "__web_unlink_probe__";
