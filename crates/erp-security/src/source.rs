//! [`PolicySource`]: the shared policy snapshot of one database in one process, its refresh after
//! a `policy` generation change, and the private rebuild of a transaction that changed the
//! policy itself (§3.2, §3.4).
//!
//! - **Shared builds** load the rows on a connection of the source's own pool, never on the
//!   connection of a request (the requests that wait for a refresh hold theirs, SIGNAL's rule),
//!   in a `REPEATABLE READ READ ONLY` transaction of their own ([`crate::loader::load_rows_from`]),
//!   compile them, and publish the snapshot only when both succeeded. Builds are serialised, so a
//!   slow earlier build never replaces a later one. A failed build changes nothing and returns
//!   its error: SIGNAL then fails the requests that waited for it, keeps the `policy` generation
//!   unapplied, and the next request builds again.
//! - **SIGNAL's [`Refresh`]** for the `policy` key: a moved `policy` generation is a shared
//!   build. A moved `registry` generation is not this source's to handle: the registry of an
//!   [`erp_orm::Orm`] is immutable, the server's hook replaces the `Orm` and builds a
//!   source for the new registry, so a source used as the only hook fails such a refresh loudly.
//!   No bump is credited to this process (no `SelfApplied::Yes`, ruling a): the triggers are the
//!   only `policy` bumpers, and a writing transaction decides on a private snapshot that is never
//!   published.
//! - **Private rebuilds** load the rows through the writing transaction (its own uncommitted
//!   view, its own bump included) and compile a snapshot of origin
//!   [`SnapshotOrigin::Private`], which is returned and never published.

use std::fmt;
use std::sync::{Arc, PoisonError, RwLock};

use erp_fields::{Error, ErrorKind, Result};
use erp_orm::{
    AccessPolicy, BoxFuture, Changes, PolicyProvider, Refresh, Refreshed, SnapshotOrigin,
    Transaction,
};
use erp_registry::Registry;

use crate::loader::{check_registry, load_rows, load_rows_from};
use crate::rows::PolicyRows;
use crate::snapshot::Compiler;

/// Turns loaded rows into a policy snapshot: [`Compiler`] (pure, no I/O), or a stand-in in tests.
///
/// The snapshot must carry its own new [`erp_orm::SnapshotId`], the `origin` it was asked for,
/// and the rows' generation as its `policy` generation; [`PolicySource`] refuses a snapshot that
/// does not (a compiler bug would otherwise mislabel what it serves).
pub trait PolicyCompiler: Send + Sync + 'static {
    /// Compile `rows` into a snapshot of `origin`.
    ///
    /// # Errors
    ///
    /// A build that cannot produce a snapshot at all (per-rule problems are the snapshot's
    /// build report, not an error).
    fn compile(&self, rows: &PolicyRows, origin: SnapshotOrigin) -> Result<Arc<dyn AccessPolicy>>;
}

impl PolicyCompiler for Compiler {
    /// Compile with this compiler's options for `origin`. The build report of a shared snapshot
    /// is logged at `error` when it holds errors (once per build, so once per generation in a
    /// process); it stays readable on the snapshot ([`crate::PolicySnapshot::report`]).
    fn compile(&self, rows: &PolicyRows, origin: SnapshotOrigin) -> Result<Arc<dyn AccessPolicy>> {
        let snapshot = self.clone().origin(origin).compile(rows)?;
        let report = snapshot.report();
        if origin == SnapshotOrigin::Shared && report.has_errors() {
            tracing::error!(
                target: "erp::security",
                generation = rows.generation(),
                rule_errors = report.rule_errors().len(),
                missing_capabilities = report.missing_capabilities().len(),
                field_group_issues = report.field_group_issues().len(),
                report = %report,
                "the access policy has errors: the rules that do not compile deny their \
                 operations, the capabilities without a mapping fail when asked, a field group \
                 row that names no model or field restricts nothing"
            );
        }
        Ok(Arc::new(snapshot))
    }
}

/// The policy snapshots of one database in one process. A cheap handle: clones share the
/// snapshot, so one clone goes to [`erp_orm::OrmBuilder::policy`] (as an
/// `Arc<dyn PolicyProvider>`) and one to [`erp_orm::Signal::with_refresh`].
///
/// ```no_run
/// # async fn example(
/// #     registry: std::sync::Arc<erp_registry::Registry>,
/// #     pool: deadpool_postgres::Pool,
/// #     source_pool: deadpool_postgres::Pool,
/// #     baseline: erp_orm::Generations,
/// # ) -> erp_fields::Result<()> {
/// use std::sync::Arc;
/// use erp_orm::{Orm, PolicyProvider, Signal};
/// use erp_security::source::PolicySource;
///
/// // its own pool: a refresh never waits for a connection a waiting request holds
/// let source = PolicySource::build(Arc::clone(&registry), source_pool).await?;
/// let provider: Arc<dyn PolicyProvider> = Arc::new(source.clone());
/// let _orm = Orm::builder(registry, pool)
///     .policy(provider)
///     .signal(Signal::new(baseline).with_refresh(source))
///     .build()?;
/// # Ok(()) }
/// ```
#[derive(Clone)]
pub struct PolicySource {
    inner: Arc<Inner>,
}

struct Inner {
    registry: Arc<Registry>,
    pool: deadpool_postgres::Pool,
    compiler: Box<dyn PolicyCompiler>,
    current: RwLock<Arc<dyn AccessPolicy>>,
    /// Serialises shared builds.
    building: tokio::sync::Mutex<()>,
}

impl fmt::Debug for PolicySource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let current = self.current();
        f.debug_struct("PolicySource")
            .field("snapshot", &current.snapshot())
            .field("generation", &current.generation())
            .finish_non_exhaustive()
    }
}

impl PolicySource {
    /// Check `registry` has the policy models ([`check_registry`]), then build and publish the
    /// first shared snapshot on a connection of `pool`, with the [`Compiler`] of `registry`
    /// (the linked overrides, grant sources and capabilities).
    ///
    /// # Errors
    ///
    /// `Programming` for a registry without the policy models; the errors of a shared build
    /// ([`PolicySource::refresh_shared`]).
    pub async fn build(registry: Arc<Registry>, pool: deadpool_postgres::Pool) -> Result<Self> {
        let compiler = Compiler::new(Arc::clone(&registry));
        Self::build_with(registry, pool, compiler).await
    }

    /// [`PolicySource::build`] with another compiler (a configured [`Compiler`], or a stand-in in
    /// tests). `compiler` must compile against `registry`.
    ///
    /// # Errors
    ///
    /// As [`PolicySource::build`].
    pub async fn build_with(
        registry: Arc<Registry>,
        pool: deadpool_postgres::Pool,
        compiler: impl PolicyCompiler,
    ) -> Result<Self> {
        check_registry(&registry)?;
        let compiler: Box<dyn PolicyCompiler> = Box::new(compiler);
        let first = shared_build(&pool, compiler.as_ref()).await?;
        Ok(Self {
            inner: Arc::new(Inner {
                registry,
                pool,
                compiler,
                current: RwLock::new(first),
                building: tokio::sync::Mutex::new(()),
            }),
        })
    }

    /// The published shared snapshot (transactions pin it when they begin).
    pub fn current(&self) -> Arc<dyn AccessPolicy> {
        // a poisoned lock still holds a whole snapshot: the only write replaces one `Arc` with
        // another, so a panic elsewhere while it was held cannot have left it half written
        Arc::clone(
            &self
                .inner
                .current
                .read()
                .unwrap_or_else(PoisonError::into_inner),
        )
    }

    /// The registry the snapshots are compiled against.
    pub fn registry(&self) -> &Arc<Registry> {
        &self.inner.registry
    }

    /// Build a new shared snapshot on a connection of the source's pool and publish it.
    ///
    /// # Errors
    ///
    /// The pool's, the loader's or the compiler's error; the published snapshot stays as it was.
    pub async fn refresh_shared(&self) -> Result<Arc<dyn AccessPolicy>> {
        let _building = self.inner.building.lock().await;
        let built = shared_build(&self.inner.pool, self.inner.compiler.as_ref()).await;
        let snapshot = match built {
            Ok(snapshot) => snapshot,
            Err(error) => {
                tracing::error!(
                    target: "erp::security",
                    error = %error,
                    "building the shared policy snapshot failed; the previous one stays published"
                );
                return Err(error);
            }
        };
        // publish (a poisoned lock holds a whole snapshot, see `current`)
        *self
            .inner
            .current
            .write()
            .unwrap_or_else(PoisonError::into_inner) = Arc::clone(&snapshot);
        tracing::info!(
            target: "erp::security",
            snapshot = snapshot.snapshot().id().get(),
            generation = %snapshot.generation(),
            "published a shared policy snapshot"
        );
        Ok(snapshot)
    }

    /// Build a snapshot from `tx`'s own view (its uncommitted policy rows and their bump; SEC-018)
    /// with five statements on `tx`. The snapshot has origin [`SnapshotOrigin::Private`] and is
    /// never published.
    ///
    /// # Errors
    ///
    /// `Programming` when `tx` belongs to an `Orm` of another registry; the loader's and the
    /// compiler's errors.
    pub async fn rebuild_private(&self, tx: &mut Transaction) -> Result<Arc<dyn AccessPolicy>> {
        if !Arc::ptr_eq(tx.registry(), &self.inner.registry) {
            return Err(Error::programming(
                "a policy source rebuilds the transactions of its own registry only; this \
                 transaction belongs to an Orm of another registry",
            ));
        }
        let rows = load_rows(tx).await?;
        compile(self.inner.compiler.as_ref(), &rows, SnapshotOrigin::Private)
    }
}

/// Load on a connection of `pool` and compile a shared snapshot.
async fn shared_build(
    pool: &deadpool_postgres::Pool,
    compiler: &dyn PolicyCompiler,
) -> Result<Arc<dyn AccessPolicy>> {
    let mut client = pool.get().await.map_err(|e| {
        Error::new(
            ErrorKind::Database,
            format!("the policy source could not get a connection from its pool: {e}"),
        )
        .with_source(e)
    })?;
    let rows = load_rows_from(&mut client).await?;
    compile(compiler, &rows, SnapshotOrigin::Shared)
}

/// Compile and check the snapshot is labelled with what it was built from.
fn compile(
    compiler: &dyn PolicyCompiler,
    rows: &PolicyRows,
    origin: SnapshotOrigin,
) -> Result<Arc<dyn AccessPolicy>> {
    let generation = rows.generation();
    let snapshot = compiler.compile(rows, origin)?;
    let label = snapshot.snapshot();
    if label.origin() != origin || snapshot.generation().policy() != generation {
        return Err(Error::programming(format!(
            "the policy compiler labelled a {origin:?} snapshot of policy generation {generation} \
             as {:?} with generation {}",
            label.origin(),
            snapshot.generation().policy()
        )));
    }
    Ok(snapshot)
}

impl PolicyProvider for PolicySource {
    fn current(&self) -> Arc<dyn AccessPolicy> {
        PolicySource::current(self)
    }

    fn rebuild_private<'a>(
        &'a self,
        tx: &'a mut Transaction,
    ) -> BoxFuture<'a, Result<Arc<dyn AccessPolicy>>> {
        Box::pin(PolicySource::rebuild_private(self, tx))
    }
}

impl Refresh for PolicySource {
    fn refresh<'a>(&'a self, changes: &'a Changes) -> BoxFuture<'a, Result<Refreshed>> {
        Box::pin(async move {
            if changes.registry {
                return Err(Error::programming(
                    "the registry generation moved, and a policy source refreshes the policy of \
                     one registry only: install the server's refresh hook (a later release), which \
                     replaces the Orm and builds a policy source for the new registry",
                ));
            }
            if changes.policy {
                self.refresh_shared().await?;
            }
            Ok(Refreshed::InPlace)
        })
    }
}
