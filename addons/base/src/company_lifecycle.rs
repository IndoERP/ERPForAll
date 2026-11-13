//! Private company lifecycle serialization and bounded integrity traversal.
//!
//! PostgreSQL owns the fence and company state. Integrity queries discover identities only;
//! every business write still uses caller-bound ORM dispatch and authorization.
use std::collections::HashSet;
use std::sync::{Arc, Mutex, PoisonError};

use erp_sdk::__private::{CoreTableRequirement, core_ddl};
use erp_sdk::prelude::*;
use erp_sdk::{Operation, Records};

const FENCE_DDL: &str = "\
CREATE TABLE IF NOT EXISTS res_company_lifecycle_fence (id int4 PRIMARY KEY CHECK (id = 1));
INSERT INTO res_company_lifecycle_fence (id) VALUES (1) ON CONFLICT DO NOTHING;
REVOKE ALL ON res_company_lifecycle_fence FROM PUBLIC;";
core_ddl!(
    COMPANY_LIFECYCLE_FENCE,
    owner = "base",
    name = "res_company_lifecycle_fence",
    purpose = "company lifecycle serialization fence",
    tables = ["res_company_lifecycle_fence"],
    sql = FENCE_DDL
);
pub(crate) const REQUIREMENT: CoreTableRequirement =
    CoreTableRequirement::new("res_company_lifecycle_fence", "base");
pub(crate) fn keep_registration() {
    std::hint::black_box(&COMPANY_LIFECYCLE_FENCE);
}

pub(crate) async fn fence(tx: &mut Transaction) -> Result<()> {
    let count = tx
        .execute(
            "UPDATE res_company_lifecycle_fence SET id = id WHERE id = 1",
            &[],
        )
        .await?;
    if count != 1 {
        return Err(Error::db(
            "res_company_lifecycle_fence row missing: expected one row",
        ));
    }
    Ok(())
}

#[derive(Default)]
struct State {
    depth: usize,
    scopes: usize,
    visited: HashSet<i64>,
}
#[derive(Default)]
struct Slot(Arc<Mutex<State>>);
pub(crate) struct Guard {
    state: Arc<Mutex<State>>,
    counted: bool,
}
impl Guard {
    pub(crate) fn enter(tx: &mut Transaction, ids: &[i64]) -> Result<Self> {
        Self::new(tx, ids, true)
    }
    fn scope(tx: &mut Transaction, ids: &[i64]) -> Result<Self> {
        Self::new(tx, ids, false)
    }
    fn new(tx: &mut Transaction, ids: &[i64], counted: bool) -> Result<Self> {
        let state = Arc::clone(&tx.extension::<Slot>().0);
        {
            let mut current = state
                .lock()
                .map_err(|_| Error::programming("Company lifecycle state is poisoned"))?;
            if current.scopes == 0 {
                current.visited.clear();
            }
            if counted && current.depth == 256 {
                return Err(Error::validation(
                    "Company lifecycle maximum depth of 256 exceeded.",
                ));
            }
            admit(&mut current, ids)?;
            current.scopes = current
                .scopes
                .checked_add(1)
                .ok_or_else(|| Error::programming("Company lifecycle scope count overflow"))?;
            if counted {
                current.depth += 1;
            }
        }
        Ok(Self { state, counted })
    }
    pub(crate) fn admit(&self, ids: &[i64]) -> Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| Error::programming("Company lifecycle state is poisoned"))?;
        admit(&mut state, ids)
    }
}
fn admit(state: &mut State, ids: &[i64]) -> Result<()> {
    for id in ids {
        if !state.visited.contains(id) && state.visited.len() == 4096 {
            return Err(Error::validation(
                "Company lifecycle maximum visited companies of 4096 exceeded.",
            ));
        }
        state.visited.insert(*id);
    }
    Ok(())
}
impl Drop for Guard {
    fn drop(&mut self) {
        // Cleanup is infallible; a poisoned mutex remains poisoned for future operations.
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        // Every successful constructor charges one scope, and counted entries charge one depth.
        state.scopes -= 1;
        if self.counted {
            state.depth -= 1;
        }
        if state.scopes == 0 {
            state.visited.clear();
        }
    }
}

pub(crate) async fn flush(tx: &mut Transaction, model: &str, names: &[&str]) -> Result<()> {
    let spec = tx
        .registry()
        .model(model)
        .ok_or_else(|| Error::programming(format!("Company lifecycle requires {model}")))?;
    let fields = names
        .iter()
        .map(|name| {
            spec.field_id(name).ok_or_else(|| {
                Error::programming(format!("Company lifecycle requires {model}.{name}"))
            })
        })
        .collect::<Result<Vec<_>>>()?;
    tx.flush_fields(spec.id, &fields).await
}

/// Discover the full subtree, archived/inaccessible rows included. No business data is returned
/// to the caller; ordinary record authorization below refuses an incomplete propagation.
pub(crate) async fn descendants(
    tx: &mut Transaction,
    records: &Records,
    guard: &Guard,
) -> Result<Records> {
    flush(tx, "res.company", &["parent_id"]).await?;
    let mut frontier = records.ids();
    let mut seen: HashSet<i64> = frontier.iter().copied().collect();
    guard.admit(&frontier)?;
    let mut all = Vec::new();
    for _ in 0..256 {
        if frontier.is_empty() {
            return records.browse(all);
        }
        // A frontier may contain children already seeded by an overlapping selection.
        // They still consume SQL result rows, so remaining distinct capacity cannot be
        // used as the row limit. One more than the whole identity budget either captures
        // every possible admitted row or proves the union exceeds that budget below.
        let limit = 4097i64;
        let rows = tx
            .query(
                "SELECT id FROM res_company WHERE parent_id = ANY($1::int8[]) ORDER BY id LIMIT $2",
                &[&frontier, &limit],
            )
            .await?;
        frontier.clear();
        for row in rows {
            let id = row
                .try_get::<_, i64>(0)
                .map_err(|e| Error::db(format!("Company traversal identity: {e}")))?;
            // Overlapping selections can seed both an ancestor and its child. Visit each once.
            if !seen.insert(id) {
                continue;
            }
            if seen.len() > 4096 {
                return Err(Error::validation(
                    "Company lifecycle maximum visited companies of 4096 exceeded.",
                ));
            }
            frontier.push(id);
            all.push(id);
        }
        guard.admit(&frontier)?;
    }
    if frontier.is_empty() {
        records.browse(all)
    } else {
        Err(Error::validation(
            "Company lifecycle maximum depth of 256 exceeded.",
        ))
    }
}

pub(crate) async fn root_currency(tx: &mut Transaction, company: &Records) -> Result<Cell> {
    let guard = Guard::scope(tx, &company.ids())?;
    let mut current = company.clone();
    let mut seen = HashSet::new();
    for _ in 0..256 {
        current.ensure_one()?;
        guard.admit(&current.ids())?;
        if !seen.insert(current.int()?) {
            return Err(Error::validation("Company hierarchy contains a cycle."));
        }
        let parent = current.related(tx, "parent_id").await?;
        if parent.is_empty() {
            return current.cell(tx, "currency_id").await;
        }
        current = parent;
    }
    Err(Error::validation(
        "Company lifecycle maximum depth of 256 exceeded.",
    ))
}

/// Refuse archiving companies that are the default company of an active user. One grouped
/// statement covers every company of `records`; when several are protected, the one with the
/// lowest id is named, with its own count of active users, so the answer does not depend on the
/// order of the selection.
pub(crate) async fn protect_archive(tx: &mut Transaction, records: &Records) -> Result<()> {
    flush(tx, "res.users", &["active", "company_id"]).await?;
    let ids = records.ids();
    if ids.is_empty() {
        return Ok(());
    }
    let rows = tx
        .query(
            "SELECT company_id::int8, count(*)::int8 FROM res_users \
              WHERE active IS TRUE AND company_id = ANY($1::int8[]) \
              GROUP BY company_id ORDER BY company_id LIMIT 1",
            &[&ids],
        )
        .await?;
    let Some(row) = rows.first() else {
        return Ok(());
    };
    let decode = |e| Error::db(format!("Company active users: {e}"));
    let id: i64 = row.try_get(0).map_err(decode)?;
    let count: i64 = row.try_get(1).map_err(decode)?;
    let name = records.browse([id])?.cell(tx, "name").await?;
    let Cell::Str(name) = name else {
        return Err(Error::validation(
            "Company name is required for archive validation.",
        ));
    };
    Err(Error::validation(format!(
        "The company {name} cannot be archived because it is the default company of {count} active users."
    )))
}

pub(crate) async fn check_users(tx: &mut Transaction, users: &Records) -> Result<()> {
    flush(tx, "res.users", &["active", "company_id"]).await?;
    flush(tx, "res.company", &["active"]).await?;
    let ids = users.ids();
    let rows = tx.query("SELECT 1 FROM res_users u JOIN res_company c ON c.id = u.company_id WHERE u.id = ANY($1::int8[]) AND u.active IS TRUE AND c.active IS NOT TRUE LIMIT 1", &[&ids]).await?;
    if !rows.is_empty() {
        return Err(Error::validation(
            "An active user must have an active default company.",
        ));
    }
    Ok(())
}

pub(crate) async fn admit_write(tx: &mut Transaction, records: &Records) -> Result<()> {
    // Relation reads apply Read rules independently of Write rules. Refuse before mutation
    // when a read-hidden child would otherwise disappear from recursive propagation.
    if records.has_access(tx, Operation::Read).await?
        && records.has_access(tx, Operation::Write).await?
    {
        Ok(())
    } else {
        Err(Error::access(
            "Company lifecycle requires read and write access to every affected branch.",
        ))
    }
}
