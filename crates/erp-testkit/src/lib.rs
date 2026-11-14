//! Test support for ERPForAll: throwaway PostgreSQL databases.
//!
//! [`TestDb`] creates `erpforall_test_<pid>_<n>_<created>` on the first cluster in
//! `ERP_TEST_DATABASE_URLS` with connection headroom, or on the server named by `DATABASE_URL`
//! ([`cluster`] module), hands out a deadpool-postgres pool, and drops the database on
//! [`TestDb::cleanup`] or on drop.
//! [`alloc`] counts heap allocations for the allocation-budget tests of standards §5b.
//! [`serve`] measures a server under test: allocations per request, heap per idle connection,
//! future sizes (PERF.SERVE-HARNESS); [`WireCounter`] counts round trips and client message types
//! at the PostgreSQL wire.
//! [`sweep`] removes leftovers from crashed runs; `scripts/ci.sh` calls it through the
//! `erp-testkit` binary. The creation time in the name is what lets the sweeper judge age without
//! any lock against concurrent creators (see the `tag` module docs).
//!
//! ```no_run
//! # async fn example() -> erp_testkit::Result<()> {
//! let db = erp_testkit::TestDb::create().await?;
//! let client = db.client().await.expect("pooled connection");
//! client.batch_execute("CREATE TABLE t (id int)").await.expect("create table");
//! drop(client);
//! db.cleanup().await?;
//! # Ok(())
//! # }
//! ```

pub mod ai;
/// Allocation counting for the budget tests of standards §5b: the `erp-alloc-budget` crate,
/// re-exported so tests that use the test kit keep `erp_testkit::alloc`.
pub use erp_alloc_budget as alloc;

pub mod base_install;
pub mod cluster;
mod concurrency;
mod db;
pub mod driver;
mod error;
mod field_policy;
pub mod fixtures;
mod harness;
mod policy;
mod rpc;
pub mod serve;
mod snapshot;
pub mod stand_in_base;
mod sweep;
mod tag;
mod timeout_check;
mod wire;

pub use ai::{MockEmbeddingProvider, MockModelProvider, ScriptedModelProvider, ScriptedReply};
pub use cluster::{
    CLUSTER_LOG_ENV, Cluster, ClusterProbe, ClusterState, DATABASE_URLS_ENV, DEFAULT_MAX_USAGE_PCT,
    MAX_USAGE_ENV, PROBED_UNREACHABLE_ENV, SkippedCluster, Unreachable, UnreachableKind, Usage,
    UsageThreshold, clusters_from_env,
};
pub use concurrency::{backend_pid, wait_until_blocked};
pub use db::{
    TestDb, TestDbBuilder, create_database, create_database_with_base_install,
    create_database_with_size, database_exists, database_url_from_env, drop_database_url,
};
pub use error::{Result, TestkitError, error_chain};
pub use field_policy::FieldPolicy;
pub use harness::{Harness, HarnessBuilder, HarnessError};
pub use policy::{DomainPolicy, DomainPolicyBuilder, Otherwise, RulesPolicy, TestRules};
pub use rpc::{RpcHarness, RpcHarnessBuilder};
pub use snapshot::{BLESS_ENV, assert_snapshot};
pub use sweep::{MIN_ORPHAN_AGE, SweepOptions, SweepReason, SweepReport, sweep};
pub use tag::{Comment, RUN_ID_ENV, TEST_DB_PREFIX, Tag, TestDbName, is_test_db_name};
pub use timeout_check::{ATTEMPTS as TIMEOUT_CHECK_ATTEMPTS, MAX_TIMEOUT_LATENESS, TimeoutCheck};
pub use wire::{MessageCounts, WireCounter};

/// Re-exported so tests use the same driver and pool versions as the testkit.
pub use deadpool_postgres;
pub use tokio_postgres;

/// Schema options for an explicitly authorized lifecycle installation fixture.
pub use erp_orm::SchemaSyncOptions;
