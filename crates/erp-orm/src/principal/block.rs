// SPDX-License-Identifier: MIT OR Apache-2.0
//! The lifetime of scoped elevation blocks (slice P1): the open blocks of one
//! transaction, the guard that closes a block on every exit, and the lifetime an environment
//! carries.
//!
//! A transaction takes a [`TxSerial`] when it opens its first block after `BEGIN` (a commit or a
//! rollback ends the database transaction, and the next block takes a new serial), and numbers its
//! blocks with a checked counter. Neither wraps. The guard is created only after the block was
//! admitted and its elevation recorded; dropping it (success, `Err`, unwind, a dropped future)
//! removes the block from the open set. An environment that carries a [`BlockLife`] is admitted
//! only while that block is open in the transaction it is used in.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use erp_fields::{Error, Result};

use super::elevation::{BlockId, Elevation, ElevationScope, TxSerial};
use crate::audit::ElevationReason;
use crate::env::Env;
use crate::transaction::Transaction;

/// The block an environment belongs to, and whether it was widened (`sudo(true)`,
/// `with_user(SUPERUSER_ID)`), which makes the environment refuse every use. Copied by every
/// derivation of the environment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct BlockLife {
    id: BlockId,
    reason: ElevationReason,
    widened: bool,
}

impl BlockLife {
    /// The lifetime of an environment of block `id`.
    pub(crate) fn new(id: BlockId, reason: ElevationReason) -> Self {
        Self {
            id,
            reason,
            widened: false,
        }
    }

    /// The block.
    pub(crate) fn id(self) -> BlockId {
        self.id
    }

    /// Whether the environment was widened to superuser mode (it then refuses every use).
    pub(crate) fn is_widened(self) -> bool {
        self.widened
    }

    /// The same lifetime, widened.
    pub(crate) fn widened(self) -> Self {
        Self {
            widened: true,
            ..self
        }
    }

    /// The refusal of a use outside the block (closed, or another transaction).
    pub(crate) fn outside_error(self) -> Error {
        outside_error(self.id, self.reason)
    }

    /// The refusal of every use of a widened environment.
    pub(crate) fn widened_error(self) -> Error {
        Error::programming(format!(
            "an environment of elevation block {}#{} (reason '{}') cannot be widened to superuser \
             mode",
            self.id.tx().get(),
            self.id.seq(),
            self.reason.as_str()
        ))
    }
}

/// The error of a block whose body failed and whose exit flush failed too.
///
/// Both messages are kept: `"<leading message> (and <added part>)"`. [`Error::and_also`] keeps the
/// kind, SQLSTATE, constraint, condition, context and source of the leading error; the other one
/// contributes its message only. Which leads, in order:
/// 1. a body error that is an invariant violation (a broken contract is never hidden);
/// 2. a flush error that is an invariant violation;
/// 3. a flush error of a superseded ORM (its caller retries on the current ORM);
/// 4. otherwise the body's error.
///
/// The added part always says which side it is: "the block's exit flush failed: …" when the body
/// leads, "the block's body failed: …" when the flush leads. A marked condition thereby stays
/// reachable through [`Error::condition`] on the returned error.
fn combine_exit_errors(body: Error, flush: Error) -> Error {
    let flush_leads =
        !body.is_invariant_violation() && (flush.is_invariant_violation() || flush.is_superseded());
    if flush_leads {
        let added = Error::new(
            body.kind(),
            format!("the block's body failed: {}", body.message()),
        );
        flush.and_also(&added)
    } else {
        let added = Error::new(
            flush.kind(),
            format!("the block's exit flush failed: {}", flush.message()),
        );
        body.and_also(&added)
    }
}

/// The refusal of an environment of block `id` used outside it.
pub(crate) fn outside_error(id: BlockId, reason: ElevationReason) -> Error {
    Error::programming(format!(
        "elevated environment of block {}#{} (reason '{}') used outside it",
        id.tx().get(),
        id.seq(),
        reason.as_str()
    ))
}

/// The open blocks of one transaction and its identity counters.
#[derive(Debug)]
pub(crate) struct OpenBlocks {
    /// The serial of the current database transaction, taken by its first block.
    serial: Option<TxSerial>,
    /// The number the next block takes; `None` once the last number was handed out.
    next_seq: Option<u32>,
    /// Open blocks, innermost last.
    open: Vec<BlockId>,
    /// Test seam: the next serial comes from a counter at its maximum.
    #[cfg(feature = "test-faults")]
    exhaust_serial: bool,
}

impl Default for OpenBlocks {
    fn default() -> Self {
        Self {
            serial: None,
            next_seq: Some(1),
            open: Vec::new(),
            #[cfg(feature = "test-faults")]
            exhaust_serial: false,
        }
    }
}

impl OpenBlocks {
    /// The identity of a new block: the transaction's serial (taken now for its first block) and
    /// the next number. Nothing is opened yet.
    ///
    /// # Errors
    ///
    /// `Programming` naming the exhausted counter.
    pub(crate) fn allocate(&mut self) -> Result<BlockId> {
        let serial = match self.serial {
            Some(serial) => serial,
            None => {
                let serial = self.next_serial()?;
                self.serial = Some(serial);
                serial
            }
        };
        let seq = self.next_seq.ok_or_else(|| {
            Error::programming(format!(
                "elevation block identity exhausted: transaction {} handed out every block \
                 number (block numbers are never reused)",
                serial.get()
            ))
        })?;
        self.next_seq = seq.checked_add(1);
        Ok(BlockId::new(serial, seq))
    }

    #[cfg(feature = "test-faults")]
    fn next_serial(&self) -> Result<TxSerial> {
        if self.exhaust_serial {
            return TxSerial::next_from(&std::sync::atomic::AtomicU64::new(u64::MAX));
        }
        TxSerial::next()
    }

    #[cfg(not(feature = "test-faults"))]
    fn next_serial(&self) -> Result<TxSerial> {
        TxSerial::next()
    }

    /// Whether `id` is open in the current database transaction.
    pub(crate) fn is_open(&self, id: BlockId) -> bool {
        self.serial == Some(id.tx()) && self.open.contains(&id)
    }

    fn close(&mut self, id: BlockId) {
        self.open.retain(|open| *open != id);
    }

    /// The database transaction ended (commit or rollback): no block of it is open any more, and
    /// the next block takes a new serial.
    pub(crate) fn reset(&mut self) {
        self.serial = None;
        self.next_seq = Some(1);
        self.open.clear();
    }

    /// The innermost open block, if any.
    pub(crate) fn innermost(&self) -> Option<BlockId> {
        self.open.last().copied()
    }

    /// Whether a block is open.
    #[cfg(test)]
    fn any_open(&self) -> bool {
        !self.open.is_empty()
    }

    /// Test seam: the next block takes number `next`.
    #[cfg(feature = "test-faults")]
    pub(crate) fn set_next_seq(&mut self, next: u32) {
        self.next_seq = Some(next);
    }

    /// Test seam: the next serial this transaction takes comes from an exhausted counter.
    #[cfg(feature = "test-faults")]
    pub(crate) fn exhaust_serial(&mut self) {
        self.serial = None;
        self.exhaust_serial = true;
    }
}

/// Lock the open blocks of a transaction.
///
/// # Errors
///
/// `Internal` when a thread panicked while holding them (the set cannot be trusted: nothing is
/// admitted).
pub(crate) fn lock(blocks: &Mutex<OpenBlocks>) -> Result<MutexGuard<'_, OpenBlocks>> {
    blocks.lock().map_err(|_| {
        Error::internal(
            "the open elevation blocks of this transaction are unreadable: a thread panicked \
             while holding them; roll back",
        )
    })
}

/// Closes its block when dropped, however the block's scope ends.
#[derive(Debug)]
pub(crate) struct BlockGuard {
    blocks: Arc<Mutex<OpenBlocks>>,
    id: BlockId,
}

impl BlockGuard {
    /// Open block `id` (allocated by [`OpenBlocks::allocate`]) until the guard drops.
    ///
    /// # Errors
    ///
    /// As [`lock`].
    pub(crate) fn open(blocks: &Arc<Mutex<OpenBlocks>>, id: BlockId) -> Result<Self> {
        lock(blocks)?.open.push(id);
        Ok(Self {
            blocks: Arc::clone(blocks),
            id,
        })
    }
}

impl Drop for BlockGuard {
    fn drop(&mut self) {
        // closing is the safe direction: a poisoned set still loses this block, so nothing it
        // granted stays admitted
        self.blocks
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .close(self.id);
    }
}

impl Transaction {
    /// Run `body` in a named scoped elevation block: `body`
    /// gets an environment of `env`'s principal carrying `scope`, valid only while the block is
    /// open. On every exit (success, `Err`, a panic, a dropped future) the block closes, and every
    /// use of an environment or recordset of it afterwards, or in another transaction, is refused
    /// with `Programming`. Closing a block rolls nothing back: savepoint and transaction rules
    /// still apply. A panic or a dropped future also cancels the transaction, as for any
    /// interrupted scope that changes its state (`savepoint_outcome` does the same): writes the
    /// body left pending are then never flushed, and the transaction must be rolled back.
    ///
    /// A future of this call that is polled and then leaked with [`std::mem::forget`] runs no
    /// destructor: its block stays in the open set and the transaction is not cancelled until the
    /// database transaction ends (a commit or a rollback resets the open set, so the block's
    /// environments are refused from then on).
    ///
    /// Entry, in order, before the body runs: a read-only transaction without a read-audit
    /// owner is refused (a transaction opened with [`BeginOptions::read_only_audited`] is
    /// admitted and stays `READ ONLY`; its owner writes the evidence as a `read_ok` attempt row
    /// before releasing a result); a cancelled, poisoned, aborted or released
    /// transaction is refused; a read-only transaction that has not begun is opened (its `BEGIN`
    /// is sent, so a reroute to the primary has settled before any block exists; a failure to
    /// open is the error of the call and nothing is recorded; a write transaction begins with its
    /// first statement as before); the caller's environment must be usable here (its own block open,
    /// not widened, its companies valid, this transaction's registry); every model and field of
    /// `scope` must exist in this transaction's registry; the transaction's access policy must
    /// accept the scope ([`crate::AccessPolicy::check_scope`]: a lifted rule only when it is a
    /// restriction of the item's model that compiles, does not bear the company, holds no
    /// `access` condition and applies to a named operation; no named operation whose company
    /// confinement is only in permission rules); the block takes its identity and opens; the
    /// elevation is recorded in the transaction's audit row. A refused entry records nothing (a
    /// failed recording closes the block again) and leaves an enclosing block open.
    ///
    /// A block opened from an environment of another block carries the inner scope only: the base
    /// principal plus `scope`, never a union (A3). The outer block stays open.
    ///
    /// Pending writes are flushed before the block closes, on success and when `body` returned an
    /// error (a company-dependent write is checked under its writer's environment at flush,
    /// A5): a flush error on success is the block's error; after an `Err` both are
    /// reported in one error (see the comment at the combination for which one leads). A
    /// transaction that is aborted or cancelled is not flushed. A dropped future or a panic does
    /// not flush (it cancels, see above).
    ///
    /// What the block grants: for a (model, operation) its scope names, the
    /// permission rules count as granted while every restriction, except the lifted ones, still
    /// applies, and an `_inherits` parent keeps its own (scoped or base) predicate; every other
    /// (model, operation) is decided exactly as for `env`'s principal without the block. Field
    /// access is not widened yet.
    ///
    /// ```no_run
    /// # async fn demo(tx: &mut erp_orm::Transaction) -> erp_fields::Result<()> {
    /// use erp_orm::audit::ElevationReason;
    /// use erp_orm::{ElevationScope, Operation, OperationSet, ScopeItem};
    /// let model = tx.registry().model_id("res.users").ok_or_else(|| {
    ///     erp_fields::Error::programming("res.users is missing")
    /// })?;
    /// let read = OperationSet::of([Operation::Read]);
    /// let scope = ElevationScope::new([ScopeItem::new(model, read)?])?;
    /// let reason = ElevationReason::new("mail.post.read_author")?;
    /// let base = tx.env();
    /// let users = tx
    ///     .elevated(&base, reason, scope, async |tx, env| {
    ///         env.records("res.users")?.search(tx, &erp_domain::Domain::TRUE).await
    ///     })
    ///     .await?;
    /// # let _ = users; Ok(()) }
    /// ```
    ///
    /// # Errors
    ///
    /// Before the body: `Programming` in a read-only transaction without a read-audit owner
    /// ("scoped elevation requires an audited transaction: this read-only transaction has no
    /// read-audit owner"), the cancellation error, the
    /// poison of a transaction that must be rolled back, `Programming` for an aborted or released
    /// transaction or a caller environment outside its block, widened or of another registry, for
    /// an invalid scope, a lifted rule or a named operation the policy refuses, for an exhausted
    /// block identity; the caller's company refusal; AUDIT's errors when the elevation cannot be
    /// recorded (`Internal` when the store is not ready). Then the body's error, or the flush's.
    pub async fn elevated<T, F>(
        &mut self,
        env: &Env,
        reason: ElevationReason,
        scope: ElevationScope,
        body: F,
    ) -> Result<T>
    where
        F: AsyncFnOnce(&mut Transaction, Env) -> Result<T>,
    {
        self.audited_read_admission()?;
        self.check_not_cancelled()?;
        // a transaction that must be rolled back records no evidence (as savepoint_outcome)
        if self.is_aborted_or_released() {
            return Err(Error::programming(
                "cannot open an elevation block: the transaction is aborted or released; roll back",
            ));
        }
        if let Some(poison) = self.poison_error() {
            return Err(poison);
        }
        // a read-only transaction sends its BEGIN lazily and the first statement may reroute it
        // to the primary, which ends every block: open it now, before any block identity exists,
        // so routing has settled when the block starts. A write transaction opens with its first
        // statement as before.
        if self.intent().is_read() {
            self.open().await?;
        }
        self.require_record_scope()?;
        // the switchers of a with_user chain are resolved before the admission check (A4)
        self.prepare_switch(env).await?;
        self.check_env_block(env)?;
        env.companies()?;
        if !Arc::ptr_eq(env.registry(), self.registry()) {
            return Err(Error::programming(
                "an environment of another registry than this transaction's cannot open an \
                 elevation block",
            ));
        }
        scope.validate_in(self.registry())?;
        // the scope (its lifts and named operations), against the policy that decides in this
        // transaction, brought up to date first: a refused scope takes no block identity and
        // records nothing (A5)
        self.ensure_policy_current().await?;
        self.policy().check_scope(&scope)?;
        let scope = Arc::new(scope);
        let id = lock(&self.blocks)?.allocate()?;
        // open before recording: a failed recording drops the guard and closes the block again
        let guard = BlockGuard::open(&self.blocks, id)?;
        // the evidence names the base principal: a nested block is its own observation
        let base = env.block_base();
        self.record_scoped_elevation(&base, reason, Arc::clone(&scope), id)?;
        let block_env = base.in_block(
            Elevation::Scoped { scope, block: id },
            BlockLife::new(id, reason),
        );
        // a panic or a dropped future in the body (or in the exit flush) cancels the transaction:
        // its pending writes can never be flushed outside the block (A5)
        let cancel = self.guard();
        let outcome = match body(self, block_env).await {
            Ok(value) => self.flush().await.map(|()| value),
            Err(error) if self.is_aborted_or_released() || self.check_not_cancelled().is_err() => {
                Err(error)
            }
            Err(error) => match self.flush().await {
                Ok(()) => Err(error),
                Err(flush) => Err(combine_exit_errors(error, flush)),
            },
        };
        cancel.disarm();
        drop(guard);
        outcome
    }

    /// The admission check of an environment, run by every engine entry before any memo, cache
    /// or decision (and A4): refuse `env` when it belongs to an elevation block that
    /// is not open in this transaction, or when it was widened (a scoped principal without its
    /// block's lifetime is refused too), then when it carries a `with_user` switch the switcher
    /// may not make ([`Transaction::check_switches`]). An async entry runs
    /// [`Transaction::prepare_switch`] (or [`Transaction::prepare_access`], which calls it) first.
    ///
    /// # Errors
    ///
    /// `Programming` naming the block and its reason; `Internal` when the open set is unreadable;
    /// the switch refusals of [`Transaction::check_switches`].
    pub(crate) fn check_env_block(&self, env: &Env) -> Result<()> {
        self.check_block_life(env)?;
        self.check_switches(env)
    }

    /// The block part of [`Transaction::check_env_block`].
    fn check_block_life(&self, env: &Env) -> Result<()> {
        let scoped = match env.principal().elevation() {
            Elevation::Scoped { block, .. } => Some(*block),
            Elevation::None | Elevation::Full => None,
        };
        let life = match (env.block(), scoped) {
            (None, None) => return Ok(()),
            // never built by the engine: refused rather than trusted (no lifetime, so no reason)
            (None, Some(block)) => {
                return Err(Error::programming(format!(
                    "elevated environment of block {}#{} used outside it (it carries no block \
                     lifetime)",
                    block.tx().get(),
                    block.seq()
                )));
            }
            (Some(life), Some(block)) if life.id() != block => {
                return Err(life.outside_error());
            }
            (Some(life), _) => life,
        };
        if !lock(&self.blocks)?.is_open(life.id()) {
            return Err(life.outside_error());
        }
        if life.is_widened() {
            return Err(life.widened_error());
        }
        Ok(())
    }

    /// Test seam: the next elevation block of this transaction takes number `next`.
    #[cfg(feature = "test-faults")]
    #[doc(hidden)]
    pub fn set_next_block_seq_for_tests(&mut self, next: u32) {
        self.blocks
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .set_next_seq(next);
    }

    /// Test seam: the next transaction serial this transaction takes comes from a counter at its
    /// maximum (the process counter is untouched).
    #[cfg(feature = "test-faults")]
    #[doc(hidden)]
    pub fn exhaust_tx_serial_for_tests(&mut self) {
        self.blocks
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .exhaust_serial();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use erp_registry::RegistryBuilder;

    use super::*;
    use crate::context::Context;
    use crate::principal::{
        OperationSet, Principal, PrincipalBuilder, SUPERUSER_ID, ScopeItem, UserId,
    };

    fn reason() -> ElevationReason {
        ElevationReason::new("core.test.block").unwrap()
    }

    /// it drops normally and when a panic unwinds through it; a block of an ended database
    /// transaction is not open.
    #[test]
    fn the_guard_closes_its_block_on_drop_and_unwind() {
        let blocks = Arc::new(Mutex::new(OpenBlocks::default()));
        let a = lock(&blocks).unwrap().allocate().unwrap();
        assert!(!lock(&blocks).unwrap().is_open(a), "allocated, not open");
        let guard = BlockGuard::open(&blocks, a).unwrap();
        let b = lock(&blocks).unwrap().allocate().unwrap();
        assert_eq!((a.tx(), a.seq() + 1), (b.tx(), b.seq()));
        let inner = BlockGuard::open(&blocks, b).unwrap();
        assert!(lock(&blocks).unwrap().is_open(a) && lock(&blocks).unwrap().is_open(b));
        drop(inner);
        assert!(lock(&blocks).unwrap().is_open(a) && !lock(&blocks).unwrap().is_open(b));
        let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _held = guard;
            panic!("unwinding");
        }));
        assert!(unwound.is_err());
        assert!(!lock(&blocks).unwrap().any_open());
        // a reset ends the database transaction: an open block of it is no longer open, a guard
        // that drops later closes nothing of the next one
        let c = lock(&blocks).unwrap().allocate().unwrap();
        let stale = BlockGuard::open(&blocks, c).unwrap();
        lock(&blocks).unwrap().reset();
        assert!(!lock(&blocks).unwrap().is_open(c));
        let d = lock(&blocks).unwrap().allocate().unwrap();
        assert_ne!(d.tx(), c.tx(), "a new serial after the reset");
        let fresh = BlockGuard::open(&blocks, d).unwrap();
        drop(stale);
        assert!(lock(&blocks).unwrap().is_open(d));
        drop(fresh);
    }

    /// is handed out once, then allocation refuses instead of wrapping.
    #[test]
    fn block_numbers_never_wrap() {
        let mut blocks = OpenBlocks {
            next_seq: Some(u32::MAX),
            ..OpenBlocks::default()
        };
        let last = blocks.allocate().unwrap();
        assert_eq!(last.seq(), u32::MAX);
        for _ in 0..2 {
            let e = blocks.allocate().unwrap_err();
            assert_eq!(e.kind(), erp_fields::ErrorKind::Programming);
            assert_eq!(
                e.message(),
                format!(
                    "elevation block identity exhausted: transaction {} handed out every block \
                     number (block numbers are never reused)",
                    last.tx().get()
                )
            );
        }
    }

    /// The four orders of [`combine_exit_errors`]: an invariant body error leads, then an invariant
    /// flush error, then a superseded flush error, otherwise the body; the added part names its
    /// side, and the leading error keeps its kind, SQLSTATE and condition.
    #[test]
    fn exit_errors_combine_in_order() {
        use erp_fields::{Condition, ErrorKind};
        let body = || Error::validation("body").with_sqlstate(Some("P0001".to_owned()));
        let flush = || Error::db("flush");
        let superseded = || {
            Error::new(ErrorKind::Concurrency, "superseded")
                .with_condition(Condition::OrmSuperseded)
        };

        // 1. an invariant body error leads, even over an invariant flush error
        let e = combine_exit_errors(
            Error::invariant("broken body"),
            Error::invariant("broken flush"),
        );
        assert_eq!(e.condition(), Some(Condition::InvariantViolation));
        assert_eq!(
            e.message(),
            "broken body (and the block's exit flush failed: broken flush)"
        );
        // 2. an invariant flush error leads over an ordinary body error
        let e = combine_exit_errors(body(), Error::invariant("broken flush"));
        assert_eq!(e.kind(), ErrorKind::Programming);
        assert_eq!(e.condition(), Some(Condition::InvariantViolation));
        assert_eq!(
            e.message(),
            "broken flush (and the block's body failed: body)"
        );
        // 3. a superseded flush error leads over an ordinary body error
        let e = combine_exit_errors(body(), superseded());
        assert_eq!(e.kind(), ErrorKind::Concurrency);
        assert!(e.is_superseded());
        assert_eq!(
            e.message(),
            "superseded (and the block's body failed: body)"
        );
        // 4. otherwise the body leads with its kind and SQLSTATE
        let e = combine_exit_errors(body(), flush());
        assert_eq!(e.kind(), ErrorKind::Validation);
        assert_eq!(e.sqlstate(), Some("P0001"));
        assert_eq!(e.condition(), None);
        assert_eq!(
            e.message(),
            "body (and the block's exit flush failed: flush)"
        );
    }

    fn user(uid: i64) -> Principal {
        PrincipalBuilder::user(UserId::new(uid).unwrap())
            .build()
            .unwrap()
    }

    /// of a block environment keeps its lifetime; `sudo(true)` and `with_user(SUPERUSER_ID)`
    /// widen it, which every later use refuses through the companies gate, and the refusal
    /// survives further derivations; a widened environment never acts as superuser.
    #[test]
    fn derivations_keep_the_lifetime_and_widening_refuses() {
        let registry = Arc::new(RegistryBuilder::new([]).build().unwrap());
        let users = registry.model_id("res.users").unwrap();
        let scope = Arc::new(
            ElevationScope::new([ScopeItem::new(
                users,
                OperationSet::of([crate::access::Operation::Read]),
            )
            .unwrap()])
            .unwrap(),
        );
        let mut blocks = OpenBlocks::default();
        let id = blocks.allocate().unwrap();
        let life = BlockLife::new(id, reason());
        let base = Env::new(Arc::clone(&registry), user(5), Context::new());
        let elevated = base.in_block(Elevation::Scoped { scope, block: id }, life);
        assert_eq!(elevated.block(), Some(life));
        assert_eq!(base.block(), None);
        assert_ne!(elevated, base);
        let derived = [
            elevated.with_context([("lang", erp_fields::Value::str("en_US"))]),
            elevated.with_debug(true),
            elevated.sudo(false),
            elevated.with_user(7),
            elevated.acting_as(&base),
            elevated.with_company(5).unwrap(),
        ];
        for env in &derived {
            assert_eq!(env.block(), Some(life), "{env:?}");
            assert!(env.companies().is_ok());
        }
        let off = elevated.sudo(false);
        assert_eq!(off.principal().elevation(), &Elevation::None);
        assert_ne!(
            off, base,
            "the lifetime is part of the environment's identity"
        );
        let message = life.widened().widened_error().message().to_owned();
        assert_eq!(
            message,
            format!(
                "an environment of elevation block {}#{} (reason 'core.test.block') cannot be \
                 widened to superuser mode",
                id.tx().get(),
                id.seq()
            )
        );
        for wide in [
            elevated.sudo(true),
            elevated.with_user(SUPERUSER_ID),
            off.sudo(true),
            elevated.with_user(7).sudo(true),
        ] {
            assert!(!wide.su(), "never superuser: {wide:?}");
            assert!(wide.block().is_some_and(BlockLife::is_widened));
            let e = wide.companies().unwrap_err();
            assert_eq!(e.kind(), erp_fields::ErrorKind::Programming);
            assert_eq!(e.message(), message);
            for further in [
                wide.sudo(false),
                wide.with_context([("lang", erp_fields::Value::str("en_US"))]),
                wide.with_user(7),
                wide.with_debug(true),
            ] {
                assert_eq!(further.companies().unwrap_err().message(), message);
            }
        }
        // the engine's own superuser mode keeps the lifetime and is not refused
        let engine = elevated.engine_sudo();
        assert!(engine.su());
        assert_eq!(engine.block(), Some(life));
        assert!(engine.companies().is_ok());
        // the base principal of a block environment: no block, no elevation
        let block_base = elevated.block_base();
        assert_eq!(block_base.block(), None);
        assert_eq!(block_base.principal().elevation(), &Elevation::None);
        assert_eq!(block_base, base);
    }
}
