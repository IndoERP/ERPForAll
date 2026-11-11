//! The base-install grant of every [`crate::Harness`] ORM, for tests.
//!
//! A [`erp_orm::BaseInstallGrant`] belongs to the module-install executor and is minted only by
//! [`erp_orm::OrmBuilder::build_with_base_install_grant`], so only the build site can hold one.
//! Test databases have none of `base`'s data, and many tests need the "base is installing"
//! window from helpers that only have a `&mut Transaction`. The harness plays the data loader: it
//! builds its ORM with a grant and keeps the grant in this table, keyed by the ORM's id, until the
//! harness ends ([`GrantEntry`] removes it on drop, so `cleanup` and a plain drop both clean up).
//!
//! A table is acceptable here because `erp-testkit` is a dev-dependency only: the layering check
//! (`scripts/check-layers.py`) keeps it out of the non-dev dependencies of `base` and
//! `erp-security`, so production code can neither reach this table nor depend on this crate.

use std::sync::{Mutex, PoisonError};

use erp_fields::{Error, Result};
use erp_orm::{BaseInstallGrant, Orm, Transaction};
use rustc_hash::FxHashMap;

static GRANTS: Mutex<Option<FxHashMap<u64, BaseInstallGrant>>> = Mutex::new(None);

/// Keeps a grant in the table for the life of this value (held by the [`crate::Harness`]).
#[derive(Debug)]
pub(crate) struct GrantEntry(u64);

impl GrantEntry {
    /// Keep `grant` for [`begin_base_install`] until the entry is dropped.
    pub(crate) fn register(grant: BaseInstallGrant) -> Self {
        let id = grant.orm_id();
        lock()
            .get_or_insert_with(FxHashMap::default)
            .insert(id, grant);
        Self(id)
    }
}

impl Drop for GrantEntry {
    fn drop(&mut self) {
        if let Some(map) = lock().as_mut() {
            map.remove(&self.0);
        }
    }
}

/// Lock the table. A poisoned lock is recovered with `into_inner`: the table only maps ids to
/// grants and every update (insert, remove) is a single call, so a panic in another test thread
/// cannot leave it half-updated.
fn lock() -> std::sync::MutexGuard<'static, Option<FxHashMap<u64, BaseInstallGrant>>> {
    GRANTS.lock().unwrap_or_else(PoisonError::into_inner)
}

fn with_grant<T>(orm_id: u64, f: impl FnOnce(&BaseInstallGrant) -> Result<T>) -> Result<T> {
    let guard = lock();
    match guard.as_ref().and_then(|m| m.get(&orm_id)) {
        Some(grant) => f(grant),
        None => Err(Error::programming(format!(
            "no base-install grant is kept for Orm {orm_id}: only the ORM of a Harness has one"
        ))),
    }
}

/// Mark `tx` as loading the base module's data, with the grant of the harness ORM that opened it.
///
/// # Errors
///
/// As [`Transaction::begin_base_install`]; `Programming` when `tx` was not opened by a harness ORM.
pub fn begin_base_install(tx: &mut Transaction) -> Result<()> {
    with_grant(tx.orm_id(), |grant| tx.begin_base_install(grant))
}

/// Hands out another ORM's grant, for the foreign-grant test only: as [`begin_base_install`]
/// with the grant of `grant_orm`, which may be another ORM than the one that opened `tx`, to
/// show that such a grant is refused.
///
/// # Errors
///
/// As [`begin_base_install`].
pub fn begin_base_install_with_grant_of(tx: &mut Transaction, grant_orm: &Orm) -> Result<()> {
    with_grant(grant_orm.id(), |grant| tx.begin_base_install(grant))
}

/// Clear the state ([`Transaction::end_base_install`]).
pub fn end_base_install(tx: &mut Transaction) {
    tx.end_base_install();
}

/// Set the state to `on` (turning it on as [`begin_base_install`]).
///
/// # Errors
///
/// As [`begin_base_install`] when `on`.
pub fn set_base_installing(tx: &mut Transaction, on: bool) -> Result<()> {
    if on {
        begin_base_install(tx)
    } else {
        end_base_install(tx);
        Ok(())
    }
}
