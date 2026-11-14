//! One node of a database: the module system with the runtime of WebAssembly package addons, as
//! the server and the operator commands (`erp addon install | upgrade | uninstall`) compose it.
//!
//! A node holds the compiled addons linked into this binary, the addon directories, the module
//! system and the lifecycle of package addons. [`Node::load`] is what a starting server and a
//! server whose registry generation moved run: it reads the installed packages of the database,
//! verifies them, loads their components and builds the registry and ORM that include them.
//! Nothing here keeps a reference to a previous ORM: the caller swaps the result in, and requests
//! still running on the previous one finish on it.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use erp_modules::error::WasmReason;
use erp_modules::package_runtime::PackageRuntime;
use erp_modules::wasm_lifecycle::{HostLoader, WasmLifecycle, WasmPackages};
use erp_modules::{AddonIndex, LoadOutcome, ModuleSystem};
use erp_orm::{Generations, Orm, Signal, WasmMethodImpl};
use erp_wasm_host::{Limits, WasmHost};

use crate::error::{CliError, Result};

/// How often [`Node::load`] tries again when the packages it read moved under it.
const LOAD_ATTEMPTS: u32 = 3;
/// Pause between two attempts.
const LOAD_PAUSE: Duration = Duration::from_millis(250);

/// The owner-side signal hook of a node. The module system of a node only needs the signal to bump
/// the registry generation in the operation's transaction; the registry of the requests is
/// rebuilt by the web layer when it sees the generation move.
pub(crate) struct OwnerRefresh;

impl erp_orm::Refresh for OwnerRefresh {
    fn refresh<'a>(
        &'a self,
        _: &'a erp_orm::Changes,
    ) -> erp_orm::BoxFuture<'a, erp_fields::Result<erp_orm::Refreshed>> {
        Box::pin(async { Ok(erp_orm::Refreshed::InPlace) })
    }
}

/// A refusal of the installed registry keeps its own kind for the node's admission (a
/// [`Concurrency`](erp_fields::ErrorKind::Concurrency) refusal stays one); every other module
/// error is a module operation error.
fn admission_error(error: erp_modules::Error) -> CliError {
    match error {
        erp_modules::Error::Orm(error) if error.kind() == erp_fields::ErrorKind::Concurrency => {
            CliError::from(error)
        }
        other => CliError::ModuleOperation(Box::new(other)),
    }
}

/// The module system of one database with the lifecycle of its WebAssembly package addons.
pub struct Node {
    system: Arc<ModuleSystem>,
    packages: Arc<WasmPackages>,
    life: Arc<WasmLifecycle>,
}

impl std::fmt::Debug for Node {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Node").finish_non_exhaustive()
    }
}

impl Node {
    /// A node over `pool` (the owner connection of the database) that discovers addons from
    /// `addons`. `baseline` is the signal generations read before the registry is built; `None`
    /// for the initialization of a database, which creates the signal table itself.
    ///
    /// # Errors
    /// [`CliError::Execution`] when the WebAssembly host cannot start;
    /// [`CliError::ModuleOperation`] when the module system refuses its configuration.
    pub fn open(
        pool: deadpool_postgres::Pool,
        addons: &[PathBuf],
        baseline: Option<Generations>,
    ) -> Result<Self> {
        Self::open_composed(
            pool,
            addons,
            baseline,
            true,
            erp_orm::SchemaSyncOptions::new(),
        )
    }

    /// Compose a node with the operator's SQL-object role bindings.
    pub(crate) fn open_configured(
        config: &erp_server::config::ServerConfig,
        baseline: Option<Generations>,
    ) -> Result<Self> {
        Self::open_composed(
            crate::access::pool_with_size(config.owner_connection(), 4)?,
            config.addons(),
            baseline,
            true,
            crate::sql_object_publication::options(config)?,
        )
    }

    /// Read the installed set without loading hooks or recovering pending work.
    pub(crate) async fn registry_for_publication(
        &self,
        tx: &mut erp_orm::Transaction,
    ) -> Result<Arc<erp_registry::Registry>> {
        let rows = tx
            .query(
                "SELECT pg_try_advisory_xact_lock($1,$2)",
                &[
                    &erp_modules::OPERATION_LOCK.0,
                    &erp_modules::OPERATION_LOCK.1,
                ],
            )
            .await?;
        let locked: bool = rows
            .first()
            .ok_or_else(|| erp_fields::Error::programming("publication lock state missing"))?
            .try_get(0)
            .map_err(|e| erp_fields::Error::db("invalid publication lock state").with_source(e))?;
        if !locked {
            return Err(CliError::ModuleOperation(Box::new(
                erp_modules::Error::OperationInProgress,
            )));
        }
        self.life
            .admit_installed_packages()
            .await
            .map_err(|e| CliError::ModuleOperation(Box::new(e)))?;
        self.system
            .registry_for_publication(tx)
            .await
            .map_err(|e| CliError::ModuleOperation(Box::new(e)))
    }

    /// The installed registry for a serving node's admission: as [`Self::registry_for_publication`]
    /// but without the operation lock, so nodes that start or refresh together, or during a module
    /// operation, do not refuse each other.
    pub(crate) async fn registry_for_admission(
        &self,
        tx: &mut erp_orm::Transaction,
    ) -> Result<Arc<erp_registry::Registry>> {
        self.life
            .admit_installed_packages()
            .await
            .map_err(|e| CliError::ModuleOperation(Box::new(e)))?;
        self.system
            .installed_registry(tx)
            .await
            .map_err(admission_error)
    }

    /// Owner-only enrollment composition: admit old folder/package bytes without claiming that
    /// candidate native descriptors attest them. It is never loaded or used to execute hooks.
    pub(crate) fn open_legacy_enrollment(
        pool: deadpool_postgres::Pool,
        addons: &[PathBuf],
    ) -> Result<Self> {
        Self::open_composed(pool, addons, None, false, erp_orm::SchemaSyncOptions::new())
    }

    fn open_composed(
        pool: deadpool_postgres::Pool,
        addons: &[PathBuf],
        baseline: Option<Generations>,
        native: bool,
        options: erp_orm::SchemaSyncOptions,
    ) -> Result<Self> {
        let mut index = AddonIndex::new(true);
        for path in addons {
            index.add_dev_path(crate::identity::configured_addon_path(path)?);
        }
        let host = WasmHost::new().map_err(|e| {
            CliError::Execution(format!("could not start the WebAssembly host: {e}"))
        })?;
        let packages =
            WasmPackages::new(Arc::new(HostLoader::new(Arc::new(host), Limits::default())));
        let builder = ModuleSystem::builder(pool, index)
            .require_core_tables(erp_security::auth::core_tables())
            .package_runtime(packages.clone())
            .schema_sync_options(options);
        let builder = if native {
            builder
                .require_sql_objects(crate::native_addons::SQL_MANIFESTS.iter().copied())
                .native_artifact_identities(crate::identity::native_artifact_identities()?)
                .code(crate::native_addons::DESCRIPTORS.iter().copied())
                .not_linked(crate::native_addons::NOT_LINKED.iter().copied())
                .view_types(erp_addon_web::view_type_contributions())
                .lifecycle_hook(Arc::new(crate::ui_assets::AssetLifecycle::new(
                    crate::ui_assets::host_options()?,
                )))
                .lifecycle_hook(Arc::new(
                    crate::attachment_schedules::AttachmentScheduleLifecycle,
                ))
        } else {
            builder.linked_hooks(false).linked_migrations(false)
        };
        let builder = match baseline {
            Some(baseline) => builder.signal(Signal::new(baseline).with_refresh(OwnerRefresh)),
            None => builder,
        };
        let system = Arc::new(
            builder
                .build()
                .map_err(|e| CliError::ModuleOperation(Box::new(e)))?,
        );
        let life = Arc::new(WasmLifecycle::new(
            Arc::clone(&system),
            Arc::clone(&packages),
        ));
        Ok(Self {
            system,
            packages,
            life,
        })
    }

    /// The module system.
    pub fn system(&self) -> &Arc<ModuleSystem> {
        &self.system
    }

    /// The lifecycle of the package addons (install, upgrade, uninstall).
    pub fn lifecycle(&self) -> &Arc<WasmLifecycle> {
        &self.life
    }

    /// Load the installed set: the packages stored in the database are verified and loaded first,
    /// then the registry and ORM are built with them.
    ///
    /// A package another node installed between the read of the stored packages and the build of
    /// the registry is reported by the module system as "not loaded in this process"; that is a
    /// race, not damage, so it is tried again (at most [`LOAD_ATTEMPTS`] times). Every other error,
    /// a stored package that no longer matches its row included, is returned at once.
    ///
    /// # Errors
    /// The errors of [`WasmLifecycle::load`].
    pub async fn load(&self) -> erp_modules::Result<LoadOutcome> {
        let mut attempt = 1;
        loop {
            match self.life.load().await {
                Err(erp_modules::Error::WasmAddon {
                    reason: WasmReason::State(text),
                    addon,
                }) if attempt < LOAD_ATTEMPTS => {
                    tracing::warn!(module = %addon, attempt, reason = %text, "installed packages moved while loading; trying again");
                    attempt += 1;
                    tokio::time::sleep(LOAD_PAUSE).await;
                }
                other => return other,
            }
        }
    }

    /// Admit UI assets from the exact immutable addon snapshots used for this load.
    /// `observed` is this database's durable signal baseline read before the complete load.
    /// Plain request refresh uses committed publication only and never calls this method.
    /// # Errors
    /// Changed registry generation (Concurrency), invalid declaration, incompatible runtime
    /// or assembly/storage failure.
    pub async fn admit_ui_assets(
        &self,
        loaded: &LoadOutcome,
        observed: &Generations,
    ) -> erp_fields::Result<()> {
        let active: Vec<_> = loaded.admitted_addons().iter().collect();
        crate::ui_assets::admit_startup(
            &loaded.orm,
            &active,
            &crate::ui_assets::host_options()?,
            observed,
        )
        .await?;
        Ok(())
    }

    /// The WebAssembly method layers of the packages loaded in this node, for an ORM built from
    /// the same registry as `orm` (for example the request ORM, which runs under a different
    /// database role than the owner-side ORM [`Node::load`] returns). `orm` supplies the exposure
    /// table the components' callable lists are read from.
    ///
    /// # Errors
    /// The errors of the components' `method_impls`.
    pub fn wasm_methods(&self, orm: &Orm) -> erp_modules::Result<Vec<WasmMethodImpl>> {
        self.packages
            .wasm_methods(orm, &self.packages.live_modules())
    }

    /// The computes and constraints of the packages loaded in this node, for an ORM built from the
    /// same registry (the request ORM): the registry holds the computed fields of the packages, so
    /// every ORM built from it needs the computes.
    ///
    /// # Errors
    /// A name or count the ORM refuses.
    pub fn package_checks(&self) -> erp_modules::Result<erp_modules::PackageChecks> {
        self.packages.checks(&self.packages.live_modules())
    }
}

/// Spaces out the rebuilds of a node's registry after one failed, so a stored package that does
/// not verify (or a component that does not compile) is not verified and compiled again by every
/// request: the delay doubles from [`RefreshBackoff::BASE`] up to [`RefreshBackoff::CAP`] and
/// resets on the first success. The clock is passed in, so the schedule is testable.
#[derive(Debug, Default)]
pub struct RefreshBackoff {
    state: Mutex<BackoffState>,
}

#[derive(Debug, Default)]
struct BackoffState {
    failures: u32,
    retry_at: Option<Instant>,
}

impl RefreshBackoff {
    /// The delay after the first failure.
    pub const BASE: Duration = Duration::from_secs(1);
    /// The longest delay.
    pub const CAP: Duration = Duration::from_secs(60);

    /// No failure recorded.
    pub fn new() -> Self {
        Self::default()
    }

    /// The delay after `failures` consecutive failures (at least one).
    pub fn delay_after(failures: u32) -> Duration {
        let doublings = failures.saturating_sub(1).min(16);
        Self::BASE.saturating_mul(1u32 << doublings).min(Self::CAP)
    }

    fn state(&self) -> std::sync::MutexGuard<'_, BackoffState> {
        // the state is replaced whole, a panic while it was held leaves it consistent
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// `Ok` when a rebuild may run at `now`; `Err(remaining)` while the last failure's delay runs.
    ///
    /// # Errors
    /// The time left until the next attempt is allowed.
    pub fn check(&self, now: Instant) -> std::result::Result<(), Duration> {
        match self.state().retry_at {
            Some(at) if now < at => Err(at - now),
            _ => Ok(()),
        }
    }

    /// Record a failed rebuild at `now`; returns the delay before the next attempt.
    pub fn failed(&self, now: Instant) -> Duration {
        let mut state = self.state();
        state.failures = state.failures.saturating_add(1);
        let delay = Self::delay_after(state.failures);
        state.retry_at = Some(now + delay);
        delay
    }

    /// Record a successful rebuild: the next failure starts from the base delay again.
    pub fn succeeded(&self) {
        *self.state() = BackoffState::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A node's admission refusal keeps the Concurrency kind and its sentence; another module
    /// error stays a module operation error (cycle 13 r3).
    #[test]
    fn an_admission_refusal_keeps_its_kind_and_sentence() {
        let refusal = erp_fields::Error::new(
            erp_fields::ErrorKind::Concurrency,
            "Pending module operations require operator recovery",
        );
        let CliError::Access(kept) = admission_error(erp_modules::Error::Orm(refusal)) else {
            panic!("not passed through");
        };
        assert_eq!(kept.kind(), erp_fields::ErrorKind::Concurrency);
        assert_eq!(
            kept.message(),
            "Pending module operations require operator recovery"
        );
        assert!(matches!(
            admission_error(erp_modules::Error::OperationInProgress),
            CliError::ModuleOperation(_)
        ));
    }

    /// A failed rebuild blocks the next one for a delay that doubles up to the cap, a rebuild
    /// after the delay is allowed, and a success resets the schedule (review r1 M3).
    #[test]
    fn a_failed_refresh_backs_off_exponentially_up_to_a_cap_and_resets_on_success() {
        let backoff = RefreshBackoff::new();
        let t0 = Instant::now();
        assert!(backoff.check(t0).is_ok(), "nothing failed yet");

        let mut now = t0;
        let mut previous = Duration::ZERO;
        for _ in 0..10 {
            let delay = backoff.failed(now);
            assert!(
                delay >= previous && delay <= RefreshBackoff::CAP,
                "{delay:?}"
            );
            let blocked = backoff
                .check(now)
                .expect_err("blocked right after a failure");
            assert_eq!(blocked, delay);
            assert!(
                backoff
                    .check(now + delay - Duration::from_millis(1))
                    .is_err(),
                "still blocked just before the delay ends"
            );
            now += delay;
            assert!(
                backoff.check(now).is_ok(),
                "allowed again once the delay ended"
            );
            previous = delay;
        }
        assert_eq!(
            previous,
            RefreshBackoff::CAP,
            "the delay stops growing at the cap"
        );
        assert_eq!(RefreshBackoff::delay_after(1), RefreshBackoff::BASE);
        assert_eq!(RefreshBackoff::delay_after(2), RefreshBackoff::BASE * 2);

        backoff.succeeded();
        assert!(backoff.check(now).is_ok());
        assert_eq!(backoff.failed(now), RefreshBackoff::BASE, "starts over");
    }
}
