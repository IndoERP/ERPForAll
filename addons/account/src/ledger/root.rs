// SPDX-License-Identifier: MIT OR Apache-2.0

use std::collections::{BTreeMap, BTreeSet};

use erp_sdk::prelude::*;

/// Number of leading characters of a code that name its root.
pub const ROOT_LEN: usize = 2;

/// `account.root`: `10` is the child of `1`.
#[model(
    name = "account.root",
    description = "Account codes first 2 digits",
    order = "name, id",
    keys = [key(name = "_name_uniq", fields = ["name"], message = "An account root exists once.")],
)]
pub struct AccountRoot {
    /// The code prefix.
    #[field(char, required = true)]
    pub name: Option<Arc<str>>,
    /// The root one character shorter.
    #[field(
        many2one,
        comodel = "account.root",
        ondelete = "restrict",
        index = true
    )]
    pub parent_id: Option<RecId>,
}

/// The root name of a code: its first [`ROOT_LEN`] characters, `None` for an empty code.
pub fn root_name(code: &str) -> Option<String> {
    let name: String = code.chars().take(ROOT_LEN).collect();
    (!name.is_empty()).then_some(name)
}

/// Create the roots (and the parents they point to) that `codes` need and that do not exist yet.
///
/// Each shortest-first layer is one insert-if-absent on the `_name_uniq` key (the ORM upsert with
/// nothing to update), so two transactions that first use the same prefix do not fail on the key:
/// the later one waits for the earlier one, and under the default snapshot isolation gets a
/// serialization failure that `Orm::run` retries. That retry claim holds under REPEATABLE READ; under
/// READ COMMITTED the later insert would see no conflict row and the upsert would end with a
/// `Programming` error (no addon code here runs at that level).
///
/// # Errors
/// Any error of the underlying upsert.
pub async fn ensure_roots(tx: &mut Transaction, env: &Env, codes: &[String]) -> Result<()> {
    let wanted: BTreeSet<String> = codes.iter().filter_map(|c| root_name(c)).collect();
    if wanted.is_empty() {
        return Ok(());
    }
    let roots = env.sudo(true).records("account.root")?;
    let mut names = wanted.clone();
    for name in &wanted {
        let mut parent = name.clone();
        while parent.pop().is_some() && !parent.is_empty() {
            names.insert(parent.clone());
        }
    }
    // Shortest names first, so a parent has its id before its child is inserted.
    let mut by_len: BTreeMap<usize, Vec<String>> = BTreeMap::new();
    for name in names {
        by_len.entry(name.chars().count()).or_default().push(name);
    }
    let mut ids: BTreeMap<String, i64> = BTreeMap::new();
    for layer in by_len.into_values() {
        let mut rows = Vec::with_capacity(layer.len());
        for name in &layer {
            let mut row = RawVals::new().set_cell("name", Cell::Str(name.as_str().into()));
            let mut parent = name.clone();
            parent.pop();
            if !parent.is_empty() {
                let id = ids.get(&parent).copied().ok_or_else(|| {
                    Error::programming(format!(
                        "account.root {parent:?} is missing while creating {name:?}"
                    ))
                })?;
                row = row.set_cell("parent_id", Cell::Id(id));
            }
            rows.push(row);
        }
        let done = roots
            .upsert(tx, UpsertCall::new("_name_uniq").with_rows(rows))
            .await?;
        for (name, row) in layer.into_iter().zip(done.rows()) {
            if let Some(real) = row.id().as_real() {
                ids.insert(name, real.get());
            }
        }
    }
    Ok(())
}
