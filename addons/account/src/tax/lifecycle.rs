// SPDX-License-Identifier: MIT OR Apache-2.0
//! Private, transaction-local coordination of parent and nested child mutations.
use erp_sdk::prelude::*;
use std::collections::BTreeSet;
use std::sync::{Arc, Mutex, PoisonError};

#[derive(Default)]
struct State {
    depth: usize,
    taxes: BTreeSet<i64>,
    deleting: BTreeSet<i64>,
}
#[derive(Default)]
struct Slot(Arc<Mutex<State>>);
pub(crate) struct Guard {
    state: Arc<Mutex<State>>,
    outer: bool,
}
impl Guard {
    pub(crate) async fn enter(tx: &mut Transaction, ids: &[i64]) -> Result<Self> {
        let state = Arc::clone(&tx.extension::<Slot>().0);
        let outer;
        {
            let mut s = state
                .lock()
                .map_err(|_| Error::internal("Tax edit scope is poisoned."))?;
            outer = s.depth == 0;
            s.depth = s
                .depth
                .checked_add(1)
                .ok_or_else(|| Error::validation("Tax edit scope overflow."))?;
            s.taxes.extend(ids);
        }
        let guard = Self { state, outer };
        if !ids.is_empty() {
            // Lock in ID order, then UPDATE the parent version. A row lock alone cannot refresh
            // a repeatable-read snapshot of child rows; a stale writer must serialize/retry.
            // Nested moves can introduce another owner, so fence those owners too. Do not cache
            // a "fenced" flag in this slot: a caught savepoint rollback can undo that UPDATE.
            tx.query(
                "SELECT id FROM account_tax WHERE id=ANY($1::int8[]) ORDER BY id FOR UPDATE",
                &[&ids],
            )
            .await?;
            tx.execute("UPDATE account_tax SET repartition_serial=repartition_serial+1 WHERE id=ANY($1::int8[])", &[&ids]).await?;
            // the cache must not keep the serial read before the bump
            tx.invalidate_model("account.tax", Some(&["repartition_serial"]))
                .await?;
        }
        Ok(guard)
    }
    pub(crate) fn add(&self, ids: &[i64]) -> Result<()> {
        self.state
            .lock()
            .map_err(|_| Error::internal("Tax edit scope is poisoned."))?
            .taxes
            .extend(ids);
        Ok(())
    }
    pub(crate) fn deleting(&self, ids: &[i64]) -> Result<()> {
        self.state
            .lock()
            .map_err(|_| Error::internal("Tax edit scope is poisoned."))?
            .deleting
            .extend(ids);
        Ok(())
    }
    pub(crate) async fn validate(&self, tx: &mut Transaction, env: &Env) -> Result<()> {
        if !self.outer {
            return Ok(());
        }
        let ids: Vec<i64> = {
            let s = self
                .state
                .lock()
                .map_err(|_| Error::internal("Tax edit scope is poisoned."))?;
            s.taxes.difference(&s.deleting).copied().collect()
        };
        super::validation::validate(&env.records("account.tax")?.browse(ids)?, tx).await
    }
}
impl Drop for Guard {
    fn drop(&mut self) {
        let mut s = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        s.depth -= 1;
        if self.outer {
            s.taxes.clear();
            s.deleting.clear();
        }
    }
}
