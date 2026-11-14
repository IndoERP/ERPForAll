// SPDX-License-Identifier: MIT OR Apache-2.0
//! Internal company identity discovery. Never an RPC or a caller-domain API.
use erp_sdk::prelude::*;
use erp_sdk::{Operation, PrincipalKind};
use std::collections::{BTreeMap, BTreeSet};

const INVALID_TREE: &str =
    "The company tree is inconsistent; repair its parent links and paths before continuing.";

fn invalid_tree() -> Error {
    Error::validation(INVALID_TREE)
}

/// Validate root-to-self paths against the actual parent edges, in batches. Only identities are
/// read: caller access to source records is admitted by the public read/method entry points.
pub(crate) async fn roots(tx: &mut Transaction, ids: &[i64]) -> Result<BTreeMap<i64, i64>> {
    if ids.is_empty() {
        return Ok(BTreeMap::new());
    }
    crate::company_lifecycle::flush(tx, "res.company", &["parent_id", "parent_path"]).await?;
    let rows = tx
        .query(
            "SELECT id::int8,parent_path FROM res_company WHERE id=ANY($1::int8[])",
            &[&ids],
        )
        .await?;
    let mut paths = BTreeMap::new();
    let mut ancestors = BTreeSet::new();
    for row in rows {
        let id: i64 = row.try_get(0).map_err(decode)?;
        let path: Option<String> = row.try_get(1).map_err(decode)?;
        let path = path.ok_or_else(invalid_tree)?;
        let body = path.strip_suffix('/').ok_or_else(invalid_tree)?;
        let mut chain = Vec::new();
        let mut seen = BTreeSet::new();
        for part in body.split('/') {
            let ancestor = part.parse::<i64>().map_err(|_| invalid_tree())?;
            if ancestor <= 0 || part != ancestor.to_string() || !seen.insert(ancestor) {
                return Err(invalid_tree());
            }
            ancestors.insert(ancestor);
            chain.push(ancestor);
        }
        if chain.last() != Some(&id) {
            return Err(invalid_tree());
        }
        paths.insert(id, chain);
    }
    if ids.iter().any(|id| !paths.contains_key(id)) {
        return Err(Error::missing(
            "The company tree query needs existing companies.",
        ));
    }
    let ancestors: Vec<i64> = ancestors.into_iter().collect();
    let rows = tx
        .query(
            "SELECT id::int8,parent_id::int8 FROM res_company WHERE id=ANY($1::int8[])",
            &[&ancestors],
        )
        .await?;
    let mut parents = BTreeMap::new();
    for row in rows {
        let id: i64 = row.try_get(0).map_err(decode)?;
        let parent: Option<i64> = row.try_get(1).map_err(decode)?;
        parents.insert(id, parent);
    }
    let mut result = BTreeMap::new();
    for (id, chain) in paths {
        let mut expected_parent = None;
        for ancestor in &chain {
            if parents.get(ancestor) != Some(&expected_parent) {
                return Err(invalid_tree());
            }
            expected_parent = Some(*ancestor);
        }
        let root = chain.first().copied().ok_or_else(invalid_tree)?;
        result.insert(id, root);
    }
    Ok(result)
}

/// A draft path stays in the ORM cache. A saved ancestor uses the same validated batched query.
pub(crate) async fn draft_root(
    tx: &mut Transaction,
    records: &Records,
    id: RecId,
) -> Result<RecId> {
    let mut current = id;
    let mut seen = BTreeSet::new();
    loop {
        if !seen.insert(current) {
            return Err(invalid_tree());
        }
        if let Some(real) = current.as_real() {
            // a saved parent's root is shown only to a caller who may read that parent
            records
                .browse_ids([current])
                .check_access(tx, Operation::Read)
                .await?;
            let root = roots(tx, &[real.get()])
                .await?
                .get(&real.get())
                .copied()
                .ok_or_else(invalid_tree)?;
            return RecId::from_real(root).ok_or_else(invalid_tree);
        }
        match records.browse_ids([current]).cell(tx, "parent_id").await? {
            Cell::Null => return Ok(current),
            Cell::Id(parent) => {
                current = RecId::from_raw(parent).ok_or_else(invalid_tree)?;
            }
            _ => return Err(invalid_tree()),
        }
    }
}

pub(crate) async fn accessible(tx: &mut Transaction, source: &Records) -> Result<Records> {
    let id = source.ensure_one()?.int()?;
    let selected = tx.companies(source.env()).await?;
    source.check_access(tx, Operation::Read).await?;
    roots(tx, &[id]).await?;
    crate::company_lifecycle::flush(tx, "res.company", &["active"]).await?;
    // Internal identity traversal must see unselected intermediate parents. The final identities
    // are intersected with the validated selection before records leave this helper.
    // Level by level as the reference walks child_ids: a level keeps its parents' order, siblings
    // follow the company order (point 2), so callers that take the first branch agree.
    let rows = tx
        .query(
            "WITH RECURSIVE ranked AS (\
        SELECT id,parent_id,row_number() OVER (PARTITION BY parent_id ORDER BY name,id) AS rank \
        FROM res_company WHERE active AND parent_id IS NOT NULL\
        ), tree(id,path) AS (\
        SELECT id,ARRAY[]::int8[] FROM res_company WHERE id=$1 UNION ALL \
        SELECT r.id,t.path||r.rank FROM ranked r JOIN tree t ON r.parent_id=t.id\
        ) SELECT id::int8 FROM tree ORDER BY cardinality(path),path",
            &[&id],
        )
        .await?;
    let selected: BTreeSet<i64> = selected.iter().map(|id| id.get()).collect();
    let mut found = Vec::new();
    for row in rows {
        let child: i64 = row.try_get(0).map_err(decode)?;
        if selected.contains(&child) {
            found.push(child);
        }
    }
    let principal = source.env().principal();
    if found.is_empty() && principal.kind() == PrincipalKind::System && principal.actor().is_none()
    {
        found.push(id);
    }
    source.browse(found)
}

pub(crate) async fn all_selected(tx: &mut Transaction, source: &Records) -> Result<bool> {
    tx.companies(source.env()).await?;
    source.check_access(tx, Operation::Read).await?;
    let selected: BTreeSet<i64> = source.ids().into_iter().collect();
    let root_ids: BTreeSet<i64> = roots(tx, &source.ids()).await?.into_values().collect();
    if selected.is_empty() {
        return Ok(true);
    }
    crate::company_lifecycle::flush(tx, "res.company", &["active"]).await?;
    let root_ids: Vec<i64> = root_ids.into_iter().collect();
    // H1: one fixed-shape completeness discovery, identities only, no caller domain,
    // no record-rule filtering. Reduce the result to a boolean here. The boolean can disclose
    // one bit: an omitted (possibly unreadable) active company exists under a selected root.
    let selected: Vec<i64> = selected.into_iter().collect();
    let rows = tx
        .query(
            "WITH RECURSIVE tree(id,active) AS (\
         SELECT id,active FROM res_company WHERE id=ANY($1::int8[]) UNION \
         SELECT c.id,c.active FROM res_company c JOIN tree t ON c.parent_id=t.id\
         ), wanted(id) AS (SELECT unnest($2::int8[])), mismatch(id) AS (\
         (SELECT id FROM tree WHERE active EXCEPT SELECT id FROM wanted) UNION ALL \
         (SELECT id FROM wanted EXCEPT SELECT id FROM tree WHERE active)\
         ) SELECT id::int8 FROM mismatch LIMIT 1",
            &[&root_ids, &selected],
        )
        .await?;
    // At most one internal identity is materialized; no identity escapes to the caller.
    Ok(rows.is_empty())
}

fn decode(error: impl std::error::Error + Send + Sync + 'static) -> Error {
    Error::db("Cannot read a company-tree identity.").with_source(error)
}
