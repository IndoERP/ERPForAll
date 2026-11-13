//! Search-panel counter batches. Reuse grouped-read terms, access predicates and flush inputs;
//! send every distinct group domain in one statement instead of one count per filter choice.

use std::sync::Arc;

use erp_domain::Domain;
use erp_fields::{Error, Result};
use erp_sql::param::Param;
use rustc_hash::FxHashMap;

use super::build::{Builder, Statement};
use crate::Transaction;
use crate::records::Records;
use crate::search::{compile_for_at, flush_read_inputs};

/// Return one id-to-count map per domain. Domains already include the non-null field predicate.
/// Each branch groups the same relational field under its own predicate, so a source record
/// linked to choices in two groups cannot leak one group's custom restriction into the other.
pub(crate) async fn panel_counts(
    tx: &mut Transaction,
    records: &Records,
    field_name: &str,
    domains: &[Domain],
) -> Result<Vec<FxHashMap<i64, i64>>> {
    tx.admit_records(records).await?;
    let env = records.env().clone();
    let registry = Arc::clone(env.registry());
    let model = registry.model_by_id(records.model_id());
    let mut params: Vec<Param> = Vec::new();
    let mut fields = Vec::new();
    let mut branches = Vec::new();
    let mut out = vec![FxHashMap::default(); domains.len()];
    for (index, domain) in domains.iter().enumerate() {
        let filter = compile_for_at(tx, records, domain, true, params.len()).await?;
        let mut builder = Builder::new(&registry, env.clone(), model, true, &filter);
        // compile_for_at offsets the predicate's placeholders. Prefix parameters also need to
        // occupy the builder's slots before it adds parameters for relational access checks.
        builder.params.splice(0..0, params.iter().cloned());
        builder.validate_companies(tx).await?;
        let group = builder
            .groupby_term(
                tx,
                model,
                model.table.clone(),
                field_name.to_owned(),
                field_name,
            )
            .await?;
        let aggregate = builder.aggregate_term("__count")?;
        tx.check_read_pairs(&env, &builder.touched)?;
        if filter.constant == Some(false) {
            // No rows exist for this branch; its empty count map is the defined result.
            continue;
        }
        let group_by = Some(group.group_item());
        let branch = builder.render(
            &filter,
            &Statement {
                lead: Some(format!("{index}::int8")),
                groups: &[group],
                aggs: &[aggregate],
                group_by,
                having: None,
                order: Vec::new(),
                count_groups: false,
                limit: None,
                offset: 0,
            },
        )?;
        params = builder.params;
        fields.extend(builder.fields);
        branches.push(format!("({branch})"));
    }
    if branches.is_empty() {
        return Ok(out);
    }
    fields.sort_unstable();
    fields.dedup();
    flush_read_inputs(tx, &fields).await?;
    let sql = branches.join(" UNION ALL ");
    let policy = tx.plan_policy(&params);
    tx.query_each_with(&sql, &params, policy, |_, row| {
        let batch: i64 = row.try_get(0).map_err(|error| {
            Error::programming(format!("search panel count batch decode: {error}"))
        })?;
        let id: Option<i64> = row.try_get(1).map_err(|error| {
            Error::programming(format!("search panel count id decode: {error}"))
        })?;
        let count: i64 = row
            .try_get(2)
            .map_err(|error| Error::programming(format!("search panel counter decode: {error}")))?;
        let batch = usize::try_from(batch)
            .map_err(|_| Error::programming("search panel count returned a negative batch"))?;
        let counts = out
            .get_mut(batch)
            .ok_or_else(|| Error::programming("search panel count returned an invalid batch"))?;
        // A many2many grouping can emit the empty relation group even when the source predicate
        // requires some relation. It has no panel choice, just as in formatted_read_group.
        if let Some(id) = id {
            counts.insert(id, count);
        }
        Ok(())
    })
    .await?;
    Ok(out)
}
