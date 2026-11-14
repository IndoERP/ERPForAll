// SPDX-License-Identifier: MIT OR Apache-2.0
//! Validate nested row writes against the completed batch, never an intermediate state.
use erp_sdk::prelude::*;
use std::collections::BTreeSet;
use std::sync::{Arc, Mutex, PoisonError};

#[derive(Default)]
struct State {
    depth: usize,
    ids: BTreeSet<i64>,
}
#[derive(Default)]
struct Slot(Arc<Mutex<State>>);
pub(super) struct Guard {
    state: Arc<Mutex<State>>,
    outer: bool,
}
impl Guard {
    pub(super) fn enter(tx: &mut Transaction, ids: &[i64]) -> Result<Self> {
        let state = Arc::clone(&tx.extension::<Slot>().0);
        let outer;
        {
            let mut state = state
                .lock()
                .map_err(|_| Error::internal("Fiscal position edit scope is poisoned."))?;
            outer = state.depth == 0;
            state.depth = state
                .depth
                .checked_add(1)
                .ok_or_else(|| Error::validation("Fiscal position edit scope overflow."))?;
            state.ids.extend(ids);
        }
        Ok(Self { state, outer })
    }
    pub(super) async fn validate(&self, tx: &mut Transaction, model: &Records) -> Result<()> {
        if !self.outer {
            return Ok(());
        }
        let ids: Vec<i64> = self
            .state
            .lock()
            .map_err(|_| Error::internal("Fiscal position edit scope is poisoned."))?
            .ids
            .iter()
            .copied()
            .collect();
        super::validation::validate(&model.browse(ids)?, tx).await
    }
}
impl Drop for Guard {
    fn drop(&mut self) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.depth -= 1;
        if self.outer {
            state.ids.clear();
        }
    }
}
