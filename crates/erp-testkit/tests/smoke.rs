//! They fail, rather than skip, when `DATABASE_URL` is missing. Every database here
//! is pinned to that server (no cluster selection), because the sweep and lookup checks below ask
//! the same server; cluster selection is tested in `clusters.rs`.
// clippy's allow-expect-in-tests covers #[test] functions only; the shared helpers below are test
// code too and must fail loudly.
#![allow(clippy::expect_used, reason = "integration-test helpers")]

use std::error::Error as _;
use std::time::Duration;

use erp_testkit::{
    SweepOptions, SweepReason, TestDb, TestDbBuilder, TestDbName, TestkitError, database_exists,
    database_url_from_env, is_test_db_name, sweep,
};

fn url() -> String {
    database_url_from_env().expect("DATABASE_URL must be set for erp-testkit tests")
}

/// A builder pinned to the server in `DATABASE_URL`.
fn pinned() -> TestDbBuilder {
    TestDb::builder().database_url(url())
}

fn unique_run_id(label: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after 1970")
        .as_nanos();
    format!("test-{label}-{}-{nanos}", std::process::id())
}

#[tokio::test]
async fn test_databases_use_the_c_collation() {
    let db = pinned().create().await.expect("create test database");
    {
        let client = db.client().await.expect("pooled connection");
        let row = client
            .query_one(
                "SELECT datcollate::text, datctype::text, pg_encoding_to_char(encoding)::text, \
                 'B' < 'a', lower('ÉA') FROM pg_database WHERE datname = current_database()",
                &[],
            )
            .await
            .expect("database locale");
        let (collate, ctype, encoding): (String, String, String) =
            (row.get(0), row.get(1), row.get(2));
        assert_eq!(collate.as_str(), "C");
        assert_ne!(
            ctype.as_str(),
            "C",
            "LC_CTYPE is not 'C' so Unicode case-folding works"
        );
        assert_eq!(encoding, "UTF8");
        assert!(row.get::<_, bool>(3), "code-point order");
        assert_eq!(row.get::<_, String>(4), "éa", "Unicode case folding works");
    }
    db.cleanup().await.expect("cleanup");
}

/// Smoke query in a throwaway database checks basic query execution and asserts
/// PostgreSQL 18+ (server_version_num >= 180_000) and task CI.PGMIN.
#[tokio::test]
async fn smoke_query_runs_in_a_throwaway_database() {
    let db = pinned().create().await.expect("create test database");
    let name = db.name().to_owned();
    assert!(is_test_db_name(&name), "unexpected name {name}");

    {
        let client = db.client().await.expect("pooled connection");
        let row = client
            .query_one(
                "SELECT current_database()::text, current_setting('server_version_num')::int",
                &[],
            )
            .await
            .expect("select current_database");
        let current: String = row.get(0);
        let version: i32 = row.get(1);
        assert_eq!(current, name);
        assert!(
            version >= 180_000,
            "PostgreSQL 18+ expected, server reports {version}"
        );

        client
            .batch_execute(
                "CREATE TABLE smoke (id bigint PRIMARY KEY, amount numeric NOT NULL); \
                 INSERT INTO smoke SELECT g, g * 2 FROM generate_series(1, 1000) AS g;",
            )
            .await
            .expect("create and fill table");
        let row = client
            .query_one("SELECT count(*), sum(amount)::bigint FROM smoke", &[])
            .await
            .expect("aggregate");
        let (count, total): (i64, i64) = (row.get(0), row.get(1));
        assert_eq!(count, 1000);
        assert_eq!(total, 1000 * 1001);
    }

    db.cleanup().await.expect("cleanup");
    assert!(
        !database_exists(&url(), &name).await.expect("lookup"),
        "{name} still exists"
    );
}

#[tokio::test]
async fn pool_hands_out_concurrent_connections() {
    let db = pinned()
        .pool_size(3)
        .create()
        .await
        .expect("create test database");
    let (a, b, c) = tokio::join!(db.client(), db.client(), db.client());
    let pids: Vec<i32> = futures_pids(&[a.expect("a"), b.expect("b"), c.expect("c")]).await;
    let mut distinct = pids.clone();
    distinct.sort_unstable();
    distinct.dedup();
    assert_eq!(distinct.len(), 3, "expected three backends, got {pids:?}");
    db.cleanup().await.expect("cleanup");
}

async fn futures_pids(clients: &[erp_testkit::deadpool_postgres::Object]) -> Vec<i32> {
    let mut pids = Vec::new();
    for client in clients {
        let row = client
            .query_one("SELECT pg_backend_pid()", &[])
            .await
            .expect("backend pid");
        pids.push(row.get(0));
    }
    pids
}

#[tokio::test]
async fn dropping_without_cleanup_removes_the_database() {
    let db = pinned().create().await.expect("create test database");
    let name = db.name().to_owned();
    let client = db.client().await.expect("pooled connection");
    client.batch_execute("SELECT 1").await.expect("query");
    // An open connection must not stop the drop: DROP … WITH (FORCE) ends it.
    drop(db);
    assert!(
        !database_exists(&url(), &name).await.expect("lookup"),
        "{name} still exists"
    );
    drop(client);
}

#[tokio::test]
async fn template_databases_are_cloned() {
    let source = pinned().create().await.expect("create source database");
    {
        let client = source.client().await.expect("connection");
        client
            .batch_execute("CREATE TABLE seeded (id int); INSERT INTO seeded VALUES (42);")
            .await
            .expect("seed");
    }
    // A template must have no other sessions while it is copied.
    source.pool().close();
    // Clone on the server that holds the source.
    let copy = TestDb::builder()
        .cluster(source.cluster().clone())
        .template(source.name())
        .create()
        .await
        .expect("clone template");
    let row = copy
        .client()
        .await
        .expect("connection")
        .query_one("SELECT id FROM seeded", &[])
        .await
        .expect("seeded row");
    assert_eq!(row.get::<_, i32>(0), 42);
    copy.cleanup().await.expect("cleanup copy");
    source.cleanup().await.expect("cleanup source");
}

#[tokio::test]
async fn sweep_drops_databases_of_its_run_only() {
    let mine = unique_run_id("mine");
    let other = unique_run_id("other");
    let a = pinned().run_id(&mine).create().await.expect("create a");
    let b = pinned().run_id(&other).create().await.expect("create b");
    let (a_name, b_name) = (a.name().to_owned(), b.name().to_owned());
    // Leak both so only the sweeper can remove them.
    std::mem::forget(a);
    std::mem::forget(b);

    let options = SweepOptions {
        max_age: Duration::from_secs(24 * 3600),
        run_id: Some(mine.clone()),
        dry_run: false,
    };
    let report = sweep(&url(), &options).await.expect("sweep");
    assert!(report.is_clean(), "sweep failures: {:?}", report.failed);
    assert!(
        report
            .dropped
            .contains(&(a_name.clone(), SweepReason::RunId)),
        "{report:?}"
    );
    assert!(!database_exists(&url(), &a_name).await.expect("lookup a"));
    assert!(
        database_exists(&url(), &b_name).await.expect("lookup b"),
        "other run was dropped"
    );

    let options = SweepOptions {
        run_id: Some(other),
        ..options
    };
    let report = sweep(&url(), &options).await.expect("sweep other");
    assert!(report.is_clean(), "sweep failures: {:?}", report.failed);
    assert!(!database_exists(&url(), &b_name).await.expect("lookup b"));
}

#[tokio::test]
async fn sweep_dry_run_drops_nothing() {
    let run = unique_run_id("dry");
    let db = pinned().run_id(&run).create().await.expect("create");
    let options = SweepOptions {
        max_age: Duration::from_secs(24 * 3600),
        run_id: Some(run),
        dry_run: true,
    };
    let report = sweep(&url(), &options).await.expect("sweep");
    assert!(
        report.dropped.iter().any(|(n, _)| n == db.name()),
        "{report:?}"
    );
    assert!(database_exists(&url(), db.name()).await.expect("lookup"));
    db.cleanup().await.expect("cleanup");
}

async fn admin_client() -> erp_testkit::tokio_postgres::Client {
    let admin: erp_testkit::tokio_postgres::Config = url().parse().expect("parse DATABASE_URL");
    let (client, connection) = admin
        .connect(erp_testkit::tokio_postgres::NoTls)
        .await
        .expect("connect");
    tokio::spawn(connection);
    client
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after 1970")
        .as_secs()
}

#[tokio::test]
async fn sweep_drops_old_untagged_orphans_but_not_young_ones() {
    let client = admin_client().await;
    let pid = std::process::id();
    let old = TestDbName {
        pid,
        n: 900_000_000,
        created_at: unix_now() - 3600,
    }
    .render();
    let young = TestDbName {
        pid,
        n: 900_000_001,
        created_at: unix_now(),
    }
    .render();
    for name in [&old, &young] {
        client
            .batch_execute(&format!("CREATE DATABASE \"{name}\""))
            .await
            .expect("create untagged database");
    }

    let options = SweepOptions {
        max_age: Duration::from_secs(24 * 3600),
        run_id: Some(unique_run_id("orphan")),
        dry_run: false,
    };
    let report = sweep(&url(), &options).await.expect("sweep");
    assert!(report.is_clean(), "sweep failures: {:?}", report.failed);
    // A concurrent sweep in another test may have dropped it first; either way it must be gone.
    assert!(
        !database_exists(&url(), &old).await.expect("lookup old"),
        "{old} survived"
    );
    assert!(
        database_exists(&url(), &young).await.expect("lookup young"),
        "{young} was dropped before MIN_ORPHAN_AGE"
    );
    client
        .batch_execute(&format!("DROP DATABASE \"{young}\""))
        .await
        .expect("drop young orphan");
}

#[tokio::test]
async fn sweep_never_touches_templates() {
    let client = admin_client().await;
    let name = TestDbName {
        pid: std::process::id(),
        n: 900_000_002,
        created_at: unix_now() - 3600,
    }
    .render();
    client
        .batch_execute(&format!("CREATE DATABASE \"{name}\" IS_TEMPLATE true"))
        .await
        .expect("create template");
    // Untagged and an hour old: an orphan in every respect except being a template. (A zero
    // max_age would also drop the databases of tests running concurrently.)
    let options = SweepOptions {
        max_age: Duration::from_secs(24 * 3600),
        run_id: None,
        dry_run: false,
    };
    let report = sweep(&url(), &options).await.expect("sweep");
    assert!(report.is_clean(), "sweep failures: {:?}", report.failed);
    let survived = database_exists(&url(), &name).await.expect("lookup");
    // Two calls: a multi-statement query is one implicit transaction, which DROP DATABASE refuses.
    client
        .batch_execute(&format!("ALTER DATABASE \"{name}\" IS_TEMPLATE false"))
        .await
        .expect("unmark template");
    client
        .batch_execute(&format!("DROP DATABASE \"{name}\""))
        .await
        .expect("drop template");
    assert!(survived, "sweeper dropped a template database");
}

#[test]
fn builder_debug_hides_the_url() {
    let builder = TestDb::builder().database_url("postgres://u:hunter2@127.0.0.1:5540/erpforall");
    let text = format!("{builder:?}");
    assert!(!text.contains("hunter2"), "{text}");
}

#[tokio::test]
async fn bad_inputs_are_rejected_before_connecting() {
    // Port 1 is never PostgreSQL: if validation did not come first, this would be a Connect error.
    let unreachable = "postgres://nobody@127.0.0.1:1/erpforall";
    let error = TestDb::builder()
        .database_url(unreachable)
        .run_id("x'; DROP DATABASE erpforall; --")
        .create()
        .await
        .expect_err("hostile run id must be rejected");
    assert!(
        matches!(error, TestkitError::InvalidName { what: "run id", .. }),
        "{error}"
    );

    let error = TestDb::builder()
        .database_url(unreachable)
        .template("a\"b")
        .create()
        .await
        .expect_err("hostile template must be rejected");
    assert!(
        matches!(
            error,
            TestkitError::InvalidName {
                what: "template database",
                ..
            }
        ),
        "{error}"
    );

    let error = TestDb::builder()
        .database_url("postgres://nobody@127.0.0.1:1")
        .create()
        .await
        .expect_err("missing admin database must be rejected");
    assert!(
        matches!(error, TestkitError::MissingAdminDatabase),
        "{error}"
    );
}

/// Verifies the deadpool post_create hook allows connections to PostgreSQL 18+.
#[tokio::test]
async fn post_create_version_gate_installed_and_accepts_pg18() {
    let db = TestDb::create().await.expect("create test database");
    let client = db.client().await.expect("pooled connection");
    let row = client
        .query_one("SELECT current_setting('server_version_num')::int", &[])
        .await
        .expect("server version");
    let version_num: i32 = row.get(0);
    assert!(
        version_num >= 180_000,
        "dev cluster must be PG18+, got {version_num}"
    );
    db.cleanup().await.expect("cleanup");
}

#[tokio::test]
async fn create_database_helper_installs_pg_trgm() {
    let base_url = url();
    let db_name = format!("test_create_db_{}", std::process::id());
    let pool = erp_testkit::create_database(&base_url, &db_name)
        .await
        .expect("create_database");
    {
        let client = pool.get().await.expect("client from pool");
        let row = client
            .query_one(
                "SELECT EXISTS(SELECT 1 FROM pg_extension WHERE extname = 'pg_trgm')",
                &[],
            )
            .await
            .expect("check pg_trgm");
        let has_trgm: bool = row.get(0);
        assert!(has_trgm, "pg_trgm must be installed by create_database");
    }
    pool.close();
    erp_testkit::drop_database_url(&base_url, &db_name)
        .await
        .expect("drop test database");
}

#[tokio::test]
async fn rejection_below_pg18_produces_unsupported_postgres_error() {
    let orm_err = erp_orm::pool::UnsupportedPostgresError::new(170002, "17.2");
    let hook_err = deadpool_postgres::HookError::message(orm_err.to_hook_message());
    let pool_err = deadpool_postgres::PoolError::PostCreateHook(hook_err);
    let testkit_err = TestkitError::from(pool_err);
    let msg = testkit_err.to_string();
    match testkit_err {
        TestkitError::UnsupportedPostgres {
            server_version_num,
            min_required,
            found_version,
        } => {
            assert_eq!(server_version_num, 170002);
            assert_eq!(min_required, 180000);
            assert_eq!(found_version, "17.2");
            assert!(msg.contains(""), "{msg}");
            assert!(msg.contains("180000"), "{msg}");
            assert!(msg.contains("170002"), "{msg}");
        }
        other => panic!("expected UnsupportedPostgres, got: {other:?}"),
    }
}

#[tokio::test]
async fn live_version_gate_refuses_when_version_below_threshold() {
    let db = TestDb::create().await.expect("create test database");
    let pool = erp_orm::pool::pool_builder_with_min_version(db.config(), 990_000)
        .max_size(1)
        .runtime(deadpool_postgres::Runtime::Tokio1)
        .build()
        .expect("pool build should succeed");

    let err = pool
        .get()
        .await
        .expect_err("live connection to cluster must be refused by post_create version hook");
    let testkit_err = TestkitError::from(err);
    let msg = testkit_err.to_string();
    match testkit_err {
        TestkitError::UnsupportedPostgres {
            server_version_num,
            min_required,
            found_version,
        } => {
            assert_eq!(min_required, 990_000);
            assert!(
                server_version_num >= 180_000,
                "must report real server version: {server_version_num}"
            );
            assert!(
                !found_version.is_empty(),
                "must report real found_version string"
            );
            assert!(msg.contains(""), "{msg}");
            assert!(msg.contains("below the minimum required version"), "{msg}");
        }
        other => panic!("expected UnsupportedPostgres, got: {other:?}"),
    }
    pool.close();
    db.cleanup().await.expect("cleanup");
}

#[tokio::test]
async fn create_database_failure_in_base_install_drops_database() {
    let base_url = url();
    static FAIL_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("unix time")
        .as_nanos();
    let count = FAIL_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let db_name = format!(
        "test_create_db_fail_{}_{}_{}",
        std::process::id(),
        count,
        nanos
    );
    let res = erp_testkit::create_database_with_base_install(&base_url, &db_name, 1, |_client| {
        Box::pin(async move { Err("simulated base install failure".to_string()) })
    })
    .await;
    match res {
        Err(TestkitError::Configuration(msg)) => {
            assert_eq!(msg, "simulated base install failure");
        }
        other => panic!("expected TestkitError::Configuration, got: {other:?}"),
    }
    let exists = erp_testkit::database_exists(&base_url, &db_name)
        .await
        .expect("check if database exists");
    assert!(
        !exists,
        "failed database must be automatically dropped to prevent leaks"
    );
}

#[tokio::test]
async fn orm_begin_with_returns_unsupported_postgres_error() {
    let db = TestDb::create().await.expect("create test database");
    let pool = erp_orm::pool::pool_builder_with_min_version(db.config(), 990_000)
        .max_size(1)
        .runtime(deadpool_postgres::Runtime::Tokio1)
        .build()
        .expect("pool build should succeed");

    static ADDON: erp_registry::AddonDescriptor = erp_registry::AddonDescriptor {
        name: "test_pg_gate",
        depends: &[],
        exact_only: false,
    };
    let registry = std::sync::Arc::new(
        erp_registry::RegistryBuilder::new([&ADDON])
            .build()
            .expect("build registry"),
    );
    let orm = erp_orm::Orm::builder(registry, pool.clone())
        .build()
        .expect("build orm");
    let err = match orm
        .begin_with(erp_orm::SUPERUSER_ID, erp_orm::Context::new())
        .await
    {
        Ok(_) => panic!("begin_with must fail when version gate rejects"),
        Err(e) => e,
    };
    assert_eq!(err.kind(), erp_fields::ErrorKind::Database);
    let unsupported = erp_orm::pool::UnsupportedPostgresError::from_pool_error(
        err.source().expect("pool error source must be present"),
    )
    .expect("should not fail parsing hook payload")
    .expect("should extract UnsupportedPostgresError from pool error");
    assert_eq!(err.to_string(), unsupported.to_string());
    pool.close();
    db.cleanup().await.expect("cleanup");
}
