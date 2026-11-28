// SPDX-License-Identifier: MIT OR Apache-2.0
//! Grouped counts behind the project count fields: one grouped read per count, with the caller's
//! rights, instead of one count query per record.

use std::collections::BTreeMap;

use erp_sdk::prelude::*;

/// Rows of `model` matching `extra` per value of its many2one `field`, for the rows whose `field`
/// is one of `ids`. An id without rows is absent from the map (its count is zero).
pub(crate) async fn count_by(
    tx: &mut Transaction,
    env: &Env,
    model: &str,
    field: &str,
    ids: &[i64],
    extra: Option<Domain>,
) -> Result<BTreeMap<i64, i64>> {
    let mut out = BTreeMap::new();
    if ids.is_empty() {
        return Ok(out);
    }
    let mut domain = Domain::field(field)?.in_(ids.to_vec());
    if let Some(extra) = extra {
        domain = domain & extra;
    }
    let groups = env
        .records(model)?
        ._read_group(
            tx,
            ReadGroup::new(domain)
                .groupby([field])
                .aggregates(["__count"]),
        )
        .await?;
    for row in 0..groups.len() {
        let key = groups.column(0)[row]
            .record_id()
            .ok_or_else(|| Error::internal(format!("a {model} count group has no {field}")))?
            .get();
        let count = match &groups.column(1)[row] {
            GroupValue::Int(n) => *n,
            other => return Err(Error::internal(format!("a {model} count is {other:?}"))),
        };
        out.insert(key, count);
    }
    Ok(out)
}

/// A count as the `integer` field that shows it.
pub(crate) fn as_field(count: i64, what: &str) -> Result<i32> {
    i32::try_from(count)
        .map_err(|_| Error::programming(format!("{what} count {count} exceeds i32")))
}
