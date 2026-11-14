//! Addon migration tests (§7).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod common;

use common::*;
use erp_modules::artifact_identity::ArtifactSnapshot;
use erp_modules::error::{Error, ModuleStep};
use erp_modules::graph::DropReason;
use erp_modules::migrations::{MigrationImpl, MigrationSqlRefusal, MigrationStage};
use erp_modules::system::{InitOptions, ModuleSystemBuilder};
use erp_modules::version::SemverVersion;
use erp_orm::{BoxFuture, Transaction};
use erp_testkit::TestDb;
use std::collections::BTreeMap;
use std::sync::Mutex;

/// Explicit trusted previous-release provenance for these forward-migration fixtures.
/// Capture the installed source before initialization/install and before the test changes its
/// manifest or scripts. Candidate migration may validate this preimage without enrolling or
/// re-stamping replacement content; the tests still exercise the original migration boundaries.
fn migration_builder(db: &TestDb, addons: &Addons) -> ModuleSystemBuilder {
    let previous = std::fs::read_dir(addons.path())
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            let name = entry.file_name().into_string().unwrap();
            let snapshot =
                ArtifactSnapshot::capture(&name, &entry.path().canonicalize().unwrap()).unwrap();
            (n(&name), snapshot.digest().to_owned())
        })
        .collect::<BTreeMap<_, _>>();
    builder(db, addons).identity_previous(previous)
}

// ----: migration script selection ---------------------------------------------

#[tokio::test]
async fn spec_mod_036_migration_script_selection() {
    let addons = Addons::new();
    addons
        .add("x", "name = 'X'\nversion = '20.0.1.0'\n")
        .file(
            "x",
            "migrations/1.0/post-a.sql",
            "INSERT INTO migration_log VALUES ('1.0', 'post', 'post-a');",
        )
        .file(
            "x",
            "migrations/1.1/post-a.sql",
            "INSERT INTO migration_log VALUES ('1.1', 'post', 'post-a');",
        )
        .file(
            "x",
            "migrations/1.3/post-a.sql",
            "INSERT INTO migration_log VALUES ('1.3', 'post', 'post-a');",
        )
        .file(
            "x",
            "migrations/1.4/post-a.sql",
            "INSERT INTO migration_log VALUES ('1.4', 'post', 'post-a');",
        )
        .file(
            "x",
            "migrations/20.0.1.2/post-a.sql",
            "INSERT INTO migration_log VALUES ('20.0.1.2', 'post', 'post-a');",
        )
        .file(
            "x",
            "migrations/0.0.0/post-a.sql",
            "INSERT INTO migration_log VALUES ('0.0.0', 'post', 'post-a');",
        )
        .file(
            "x",
            "migrations/tests/post-a.sql",
            "INSERT INTO migration_log VALUES ('tests', 'post', 'post-a');",
        )
        .file(
            "x",
            "migrations/bogus/post-a.sql",
            "INSERT INTO migration_log VALUES ('bogus', 'post', 'post-a');",
        );

    let db = TestDb::create().await.unwrap();
    let system = migration_builder(&db, &addons).build().unwrap();

    // Initialize database: only base is installed
    let outcome = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    let admin = su(&outcome.orm);

    // Install module x at version 20.0.1.0 (semver 1.0.0)
    let outcome = system
        .button_immediate_install(&admin, &[n("x")])
        .await
        .unwrap();
    assert_eq!(outcome.installed, vec![n("x")]);

    // Create the test migration_log table
    db.pool()
        .get()
        .await
        .unwrap()
        .batch_execute("CREATE TABLE migration_log (version text, stage text, name text);")
        .await
        .unwrap();

    // Now update x's manifest to version 1.3
    addons.add("x", "name = 'X'\nversion = '1.3'\n");

    // Upgrade x
    let outcome = system
        .button_immediate_upgrade(&admin, &[n("x")])
        .await
        .unwrap();
    assert_eq!(outcome.upgraded, vec![n("x")]);

    // Check which scripts ran:
    // 1.1, 20.0.1.2 (maps to 1.2.0), 1.3, and 0.0.0 run; 1.0, 1.4, tests do not; bogus does not.
    let ran = rows(
        &outcome.orm,
        "SELECT version, stage, name FROM migration_log ORDER BY version, name",
    )
    .await;

    let ran_versions: Vec<String> = ran.iter().map(|r| r[0].clone().unwrap()).collect();
    // 0.0.0, 1.1, 1.3, 20.0.1.2 ran
    assert_eq!(ran_versions, vec!["0.0.0", "1.1", "1.3", "20.0.1.2"]);

    // Verify 1.0, 1.4, tests, bogus did not run
    assert!(!ran_versions.contains(&"1.0".to_string()));
    assert!(!ran_versions.contains(&"1.4".to_string()));
    assert!(!ran_versions.contains(&"tests".to_string()));
    assert!(!ran_versions.contains(&"bogus".to_string()));

    db.cleanup().await.unwrap();
}

// ----: migration stage and file order -----------------------------------------

/// What the native migrations received (stage, installed version), in call order.
static NATIVE_RECORD: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());

async fn log_native(
    tx: &mut Transaction,
    stage: &str,
    name: &str,
    installed: &SemverVersion,
) -> erp_fields::Result<()> {
    let installed = installed.to_string();
    NATIVE_RECORD
        .lock()
        .unwrap()
        .push((stage.to_owned(), installed.clone()));
    tx.execute(
        "INSERT INTO stage_order_log (module, stage, version, name, installed_version) \
         VALUES ('x', $1, '1.1', $2, $3)",
        &[&stage, &name, &installed],
    )
    .await?;
    Ok(())
}

fn native_pre<'a>(
    tx: &'a mut Transaction,
    installed: &'a SemverVersion,
) -> BoxFuture<'a, erp_fields::Result<()>> {
    Box::pin(log_native(tx, "pre", "pre-c", installed))
}

fn native_post<'a>(
    tx: &'a mut Transaction,
    installed: &'a SemverVersion,
) -> BoxFuture<'a, erp_fields::Result<()>> {
    Box::pin(async move {
        // post runs on the updated registry: x's model exists
        assert!(tx.registry().model("x.thing").is_some());
        log_native(tx, "post", "post-b", installed).await
    })
}

fn native_end<'a>(
    tx: &'a mut Transaction,
    installed: &'a SemverVersion,
) -> BoxFuture<'a, erp_fields::Result<()>> {
    Box::pin(log_native(tx, "end", "end-b", installed))
}

/// The SQL of a script that logs itself.
fn log_sql(module: &str, stage: &str, version: &str, name: &str) -> String {
    format!(
        "INSERT INTO stage_order_log (module, stage, version, name) \
         VALUES ('{module}', '{stage}', '{version}', '{name}');"
    )
}

#[tokio::test]
async fn spec_mod_037_migration_stage_and_file_order() {
    NATIVE_RECORD.lock().unwrap().clear();

    let addons = Addons::new();
    addons
        .add("x", "name = 'X'\nversion = '20.0.1.0'\n")
        .add("y", "name = 'Y'\nversion = '20.0.1.0'\ndepends = ['x']\n");
    // x: selected versions 1.1 and 0.0.0, each with pre-b, pre-a, post-a, end-a (written in
    // that order, so the directory order is not the run order)
    for version in ["1.1", "0.0.0"] {
        for (stage, name) in [
            ("pre", "pre-b"),
            ("pre", "pre-a"),
            ("post", "post-a"),
            ("end", "end-a"),
        ] {
            addons.file(
                "x",
                &format!("migrations/{version}/{name}.sql"),
                &log_sql("x", stage, version, name),
            );
        }
    }
    // y, upgraded after x in the same operation
    for (stage, name) in [("pre", "pre-a"), ("post", "post-a"), ("end", "end-a")] {
        addons.file(
            "y",
            &format!("migrations/1.1/{name}.sql"),
            &log_sql("y", stage, "1.1", name),
        );
    }

    static NATIVE_PRE: MigrationImpl =
        MigrationImpl::new("x", "1.1", MigrationStage::Pre, "pre-c", native_pre)
            .at(file!(), line!());
    static NATIVE_POST: MigrationImpl =
        MigrationImpl::new("x", "1.1", MigrationStage::Post, "post-b", native_post)
            .at(file!(), line!());
    static NATIVE_END: MigrationImpl =
        MigrationImpl::new("x", "1.1", MigrationStage::End, "end-b", native_end)
            .at(file!(), line!());

    let db = TestDb::create().await.unwrap();
    let system = migration_builder(&db, &addons)
        .decls([model("x", "x.thing")])
        .migrations([&NATIVE_PRE, &NATIVE_POST, &NATIVE_END])
        .build()
        .unwrap();

    let outcome = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    let admin = su(&outcome.orm);
    let outcome = system
        .button_immediate_install(&admin, &[n("y")])
        .await
        .unwrap();
    assert_eq!(outcome.installed, vec![n("x"), n("y")]);

    db.pool()
        .get()
        .await
        .unwrap()
        .batch_execute(
            "CREATE TABLE stage_order_log (seq serial PRIMARY KEY, module text, stage text, \
             version text, name text, installed_version text);",
        )
        .await
        .unwrap();

    addons
        .add("x", "name = 'X'\nversion = '1.1'\n")
        .add("y", "name = 'Y'\nversion = '1.1'\ndepends = ['x']\n");
    // upgrading x upgrades its dependent y in the same operation
    let outcome = system
        .button_immediate_upgrade(&admin, &[n("x")])
        .await
        .unwrap();
    assert_eq!(outcome.upgraded, vec![n("x"), n("y")]);

    let logged: Vec<String> = rows(
        &outcome.orm,
        "SELECT module || ' ' || stage || ' ' || version || '/' || name FROM stage_order_log \
         ORDER BY seq",
    )
    .await
    .into_iter()
    .map(|r| r[0].clone().unwrap())
    .collect();
    assert_eq!(
        logged,
        vec![
            // x pre: 0.0.0 first, then 1.1; by name within each (pre-c is native)
            "x pre 0.0.0/pre-a",
            "x pre 0.0.0/pre-b",
            "x pre 1.1/pre-a",
            "x pre 1.1/pre-b",
            "x pre 1.1/pre-c",
            // x post: 1.1, then 0.0.0 last (post-b is native)
            "x post 1.1/post-a",
            "x post 1.1/post-b",
            "x post 0.0.0/post-a",
            // y loads after x's post scripts
            "y pre 1.1/pre-a",
            "y post 1.1/post-a",
            // end scripts after every module was loaded: x (1.1, then 0.0.0), then y
            "x end 1.1/end-a",
            "x end 1.1/end-b",
            "x end 0.0.0/end-a",
            "y end 1.1/end-a",
        ]
    );

    // every native migration received the previous version (its semver form, divergent)
    let native = NATIVE_RECORD.lock().unwrap().clone();
    let expected: Vec<(String, String)> = ["pre", "post", "end"]
        .into_iter()
        .map(|stage| (stage.to_owned(), SemverVersion::new(1, 0, 0).to_string()))
        .collect();
    assert_eq!(native, expected);
    assert_eq!(
        rows(
            &outcome.orm,
            "SELECT DISTINCT installed_version FROM stage_order_log \
             WHERE installed_version IS NOT NULL",
        )
        .await,
        vec![vec![Some("1.0.0".to_owned())]]
    );

    db.cleanup().await.unwrap();
}

// ----: no migration on fresh install ------------------------------------------

static FRESH_INSTALL_RAN: Mutex<bool> = Mutex::new(false);

fn record_fresh_install<'a>(
    _tx: &'a mut Transaction,
    _version: &'a SemverVersion,
) -> BoxFuture<'a, erp_fields::Result<()>> {
    Box::pin(async move {
        *FRESH_INSTALL_RAN.lock().unwrap() = true;
        Ok(())
    })
}

#[tokio::test]
async fn spec_mod_038_no_migration_on_fresh_install() {
    *FRESH_INSTALL_RAN.lock().unwrap() = false;

    let addons = Addons::new();
    addons.add("x", "name = 'X'\n").file(
        "x",
        "migrations/0.0.0/pre-x.sql",
        "CREATE TABLE IF NOT EXISTS should_not_exist (id int);",
    );

    static NATIVE_FRESH: MigrationImpl = MigrationImpl::new(
        "x",
        "0.0.0",
        MigrationStage::Pre,
        "pre-fresh",
        record_fresh_install,
    );

    let db = TestDb::create().await.unwrap();
    let system = builder(&db, &addons)
        .migrations([&NATIVE_FRESH])
        .build()
        .unwrap();

    let outcome = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    let admin = su(&outcome.orm);

    // Fresh install of x
    let outcome = system
        .button_immediate_install(&admin, &[n("x")])
        .await
        .unwrap();
    assert_eq!(outcome.installed, vec![n("x")]);

    // Assert the migration SQL script did NOT run
    let table_exists = db
        .pool()
        .get()
        .await
        .unwrap()
        .query("SELECT to_regclass('should_not_exist') IS NOT NULL;", &[])
        .await
        .unwrap();
    assert!(!table_exists[0].get::<_, bool>(0));

    // Assert native migration did NOT run
    assert!(!*FRESH_INSTALL_RAN.lock().unwrap());

    db.cleanup().await.unwrap();
}

// ----: migration failure aborts and resets -------------------------------------

fn failing_migration<'a>(
    _tx: &'a mut Transaction,
    _version: &'a SemverVersion,
) -> BoxFuture<'a, erp_fields::Result<()>> {
    Box::pin(async move { Err(erp_fields::Error::validation("migration forced failure")) })
}

static FAILING_POST: MigrationImpl = MigrationImpl::new(
    "x",
    "1.1",
    MigrationStage::Post,
    "post-fail",
    failing_migration,
)
.at(file!(), line!());

/// The module system of an upgrade test, with `x` installed at 1.0 (its manifest says 1.0) and
/// `migrations` given to the builder; the ORM after the install and its superuser environment.
async fn installed_x(
    addons: &Addons,
    db: &TestDb,
    migrations: &[&'static MigrationImpl],
) -> (
    erp_modules::system::ModuleSystem,
    erp_orm::Orm,
    erp_orm::Env,
) {
    addons.add("x", "name = 'X'\nversion = '1.0'\n");
    let system = migration_builder(db, addons)
        .migrations(migrations.iter().copied())
        .build()
        .unwrap();
    let outcome = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    let admin = su(&outcome.orm);
    let outcome = system
        .button_immediate_install(&admin, &[n("x")])
        .await
        .unwrap();
    assert_eq!(outcome.installed, vec![n("x")]);
    let admin = su(&outcome.orm);
    (system, outcome.orm, admin)
}

/// The `StepFailed` of a failed migration step: its step and file, and the inner error.
fn migration_step_failed(err: Error, stage: MigrationStage, file: &str) -> Error {
    let message = err.to_string();
    match err {
        Error::StepFailed {
            module,
            step,
            file: Some(failed_file),
            source,
        } => {
            assert_eq!(module, n("x"));
            assert_eq!(step, ModuleStep::Migration { stage });
            assert_eq!(failed_file, file);
            assert_eq!(
                message,
                format!("Module \"x\": {stage} migration '{file}' failed")
            );
            *source
        }
        other => panic!("expected StepFailed with a file, got {other:?}"),
    }
}

/// the Python contract errors are n/a-by-construction, their native counterpart is the build-time
/// refusal in erp-macros tests/compile_fail.rs)
#[tokio::test]
async fn spec_mod_039_migration_script_failure_aborts_and_resets() {
    let addons = Addons::new();
    addons.file(
        "x",
        "migrations/1.1/pre-a.sql",
        "CREATE TABLE migration_before_fail (id int);",
    );
    let db = TestDb::create().await.unwrap();
    let (system, orm, admin) = installed_x(&addons, &db, &[&FAILING_POST]).await;

    addons.add("x", "name = 'X'\nversion = '1.1'\n");
    let err = system
        .button_immediate_upgrade(&admin, &[n("x")])
        .await
        .unwrap_err();

    let inner = migration_step_failed(err, MigrationStage::Post, &FAILING_POST.location());
    let inner_message = inner.to_string();
    match inner {
        Error::MigrationFailed {
            module,
            version,
            stage,
            name,
            source,
        } => {
            assert_eq!(module, n("x"));
            assert_eq!(version, SemverVersion::new(1, 1, 0));
            assert_eq!(stage, MigrationStage::Post);
            assert_eq!(name, "post-fail");
            assert_eq!(source.kind(), erp_fields::ErrorKind::Validation);
            assert_eq!(source.message(), "migration forced failure");
            assert_eq!(
                inner_message,
                "Module 'x': migration 'post-fail' (version 1.1.0, stage post) failed"
            );
        }
        other => panic!("expected MigrationFailed, got {other:?}"),
    }

    // the pre script ran before the failure; its table was rolled back with the operation
    let table_exists = db
        .pool()
        .get()
        .await
        .unwrap()
        .query(
            "SELECT to_regclass('migration_before_fail') IS NOT NULL;",
            &[],
        )
        .await
        .unwrap();
    assert!(!table_exists[0].get::<_, bool>(0));

    // the transient state was reset: x is still installed at 1.0.0
    assert_eq!(state(&orm, "x").await, "installed");
    assert_eq!(
        rows(
            &orm,
            "SELECT latest_version FROM ir_module_module WHERE name = 'x'"
        )
        .await,
        vec![vec![Some("1.0.0".to_owned())]]
    );

    db.cleanup().await.unwrap();
}

// ---- error paths (review r2 MUST 4, 7) ----------------------------------------------------

fn noop_migration<'a>(
    _tx: &'a mut Transaction,
    _version: &'a SemverVersion,
) -> BoxFuture<'a, erp_fields::Result<()>> {
    Box::pin(async move { Ok(()) })
}

/// Two native migrations of the same addon, version (`1.1` and `1.1.0` are the same) stage and
/// name are refused when the module system is built, naming both locations.
#[tokio::test]
async fn duplicate_native_migration_is_refused_at_build() {
    static FIRST: MigrationImpl =
        MigrationImpl::new("x", "1.1", MigrationStage::Pre, "pre-a", noop_migration)
            .at(file!(), line!());
    static SECOND: MigrationImpl =
        MigrationImpl::new("x", "1.1.0", MigrationStage::Pre, "pre-a", noop_migration)
            .at(file!(), line!());
    // a different stage with the same name is no duplicate
    static OTHER_STAGE: MigrationImpl =
        MigrationImpl::new("x", "1.1", MigrationStage::Post, "pre-a", noop_migration)
            .at(file!(), line!());

    let addons = Addons::new();
    addons.add("x", "name = 'X'\n");
    let db = TestDb::create().await.unwrap();
    let err = builder(&db, &addons)
        .migrations([&FIRST, &OTHER_STAGE, &SECOND])
        .build()
        .map(|_| ())
        .unwrap_err();
    let message = err.to_string();
    match err {
        Error::MigrationDuplicate {
            module,
            version,
            stage,
            name,
            first,
            second,
        } => {
            assert_eq!(module, n("x"));
            assert_eq!(version, SemverVersion::new(1, 1, 0));
            assert_eq!(stage, MigrationStage::Pre);
            assert_eq!(name, "pre-a");
            assert_eq!(first, FIRST.location());
            assert_eq!(second, SECOND.location());
            assert_eq!(
                message,
                format!(
                    "Module 'x': migration 'pre-a' for version 1.1.0 (pre) is implemented twice: \
                     {} and {}",
                    FIRST.location(),
                    SECOND.location()
                )
            );
        }
        other => panic!("expected MigrationDuplicate, got {other:?}"),
    }
    db.cleanup().await.unwrap();
}

/// A native migration whose version is not a migration version is refused when the module
/// system is built, not when its module is upgraded.
#[tokio::test]
async fn invalid_native_migration_version_is_refused_at_build() {
    static BAD: MigrationImpl =
        MigrationImpl::new("x", "one.two", MigrationStage::Pre, "pre-a", noop_migration)
            .at(file!(), line!());
    let addons = Addons::new();
    let db = TestDb::create().await.unwrap();
    let err = builder(&db, &addons)
        .migrations([&BAD])
        .build()
        .map(|_| ())
        .unwrap_err();
    let message = err.to_string();
    match err {
        Error::InvalidVersion {
            name,
            version,
            message: reason,
        } => {
            assert_eq!(name, n("x"));
            assert_eq!(version, "one.two");
            assert_eq!(
                reason,
                format!(
                    "migration 'pre-a' at {} is not for a migration version (0.0.0, x.y, x.y.z or \
                     20.0.x.y)",
                    BAD.location()
                )
            );
            assert_eq!(
                message,
                format!("Module x: invalid manifest: invalid version 'one.two': {reason}")
            );
        }
        other => panic!("expected InvalidVersion, got {other:?}"),
    }
    db.cleanup().await.unwrap();
}

/// A failing statement in a `.sql` script fails the step with the statement's error (its
/// SQLSTATE kept) and aborts the upgrade.
#[tokio::test]
async fn sql_script_statement_error_fails_the_upgrade() {
    let addons = Addons::new();
    addons.file(
        "x",
        "migrations/1.1/post-a.sql",
        "UPDATE migration_no_such_table SET id = 1;",
    );
    let db = TestDb::create().await.unwrap();
    let (system, orm, admin) = installed_x(&addons, &db, &[]).await;

    addons.add("x", "name = 'X'\nversion = '1.1'\n");
    let err = system
        .button_immediate_upgrade(&admin, &[n("x")])
        .await
        .unwrap_err();
    let inner = migration_step_failed(err, MigrationStage::Post, "migrations/1.1/post-a.sql");
    let inner_message = inner.to_string();
    match inner {
        Error::MigrationFailed {
            module,
            version,
            stage,
            name,
            source,
        } => {
            assert_eq!(module, n("x"));
            assert_eq!(version, SemverVersion::new(1, 1, 0));
            assert_eq!(stage, MigrationStage::Post);
            assert_eq!(name, "post-a");
            // undefined_table
            assert_eq!(source.sqlstate(), Some("42P01"));
            assert_eq!(source.kind(), erp_fields::ErrorKind::Database);
            assert_eq!(
                inner_message,
                "Module 'x': migration 'post-a' (version 1.1.0, stage post) failed"
            );
        }
        other => panic!("expected MigrationFailed, got {other:?}"),
    }
    assert_eq!(state(&orm, "x").await, "installed");
    db.cleanup().await.unwrap();
}

/// An upgraded module with migrations whose row stores no installed version is refused: the
/// migrations cannot be selected (no default version is assumed).
#[tokio::test]
async fn upgrade_with_migrations_and_no_installed_version_is_refused() {
    let addons = Addons::new();
    addons.file(
        "x",
        "migrations/1.1/pre-a.sql",
        "CREATE TABLE migration_no_version (id int);",
    );
    let db = TestDb::create().await.unwrap();
    let (system, _orm, admin) = installed_x(&addons, &db, &[]).await;
    db.pool()
        .get()
        .await
        .unwrap()
        .batch_execute("UPDATE ir_module_module SET latest_version = NULL WHERE name = 'x';")
        .await
        .unwrap();

    addons.add("x", "name = 'X'\nversion = '1.1'\n");
    let err = system
        .button_immediate_upgrade(&admin, &[n("x")])
        .await
        .unwrap_err();
    let message = err.to_string();
    match err {
        Error::InvalidModuleRow {
            module,
            message: reason,
        } => {
            assert_eq!(module, n("x"));
            assert_eq!(
                reason,
                "the module is upgraded and has migrations, but its row stores no installed \
                 version (latest_version), so they cannot be selected"
            );
            assert_eq!(
                message,
                format!("Module \"x\": inconsistent stored row: {reason}")
            );
        }
        other => panic!("expected InvalidModuleRow, got {other:?}"),
    }
    let table_exists = db
        .pool()
        .get()
        .await
        .unwrap()
        .query(
            "SELECT to_regclass('migration_no_version') IS NOT NULL;",
            &[],
        )
        .await
        .unwrap();
    assert!(!table_exists[0].get::<_, bool>(0));
    db.cleanup().await.unwrap();
}

/// An upgrade to a manifest whose version is incompatible is refused before any migration runs:
/// the module is not installable and the update graph drops it (the upgrade runner has no
/// "target = installed" fallback; it treats a missing target version as an invariant violation).
#[tokio::test]
async fn upgrade_to_incompatible_manifest_version_runs_no_migration() {
    let addons = Addons::new();
    addons.file(
        "x",
        "migrations/0.0.0/pre-a.sql",
        "CREATE TABLE migration_incompatible (id int);",
    );
    let db = TestDb::create().await.unwrap();
    let (system, _orm, admin) = installed_x(&addons, &db, &[]).await;

    addons.add("x", "name = 'X'\nversion = '19.0.1.1'\n");
    let err = system
        .button_immediate_upgrade(&admin, &[n("x")])
        .await
        .unwrap_err();
    let message = err.to_string();
    match err {
        Error::DroppedUpdateModules { modules } => {
            assert_eq!(modules, vec![(n("x"), DropReason::NotInstallable)]);
            assert_eq!(
                message,
                format!(
                    "Modules marked to install, upgrade or remove cannot be loaded:\n  - x: {}",
                    DropReason::NotInstallable
                )
            );
        }
        other => panic!("expected DroppedUpdateModules, got {other:?}"),
    }
    let table_exists = db
        .pool()
        .get()
        .await
        .unwrap()
        .query(
            "SELECT to_regclass('migration_incompatible') IS NOT NULL;",
            &[],
        )
        .await
        .unwrap();
    assert!(!table_exists[0].get::<_, bool>(0));
    db.cleanup().await.unwrap();
}

/// Only the selected version folders are read: a script that would be refused (not UTF-8) in a
/// folder that does not run does not fail the upgrade, the same script in a selected folder does.
#[tokio::test]
async fn only_selected_version_folders_are_read() {
    let addons = Addons::new();
    addons.file(
        "x",
        "migrations/1.1/post-a.sql",
        "CREATE TABLE migration_selected (id int);",
    );
    let bad = addons.path().join("x/migrations/1.0/post-a.sql");
    std::fs::create_dir_all(bad.parent().unwrap()).unwrap();
    std::fs::write(&bad, b"SELECT '\xff';").unwrap();
    let db = TestDb::create().await.unwrap();
    let (system, _orm, admin) = installed_x(&addons, &db, &[]).await;

    // 1.0 is the installed version: its folder is not selected and not read
    addons.add("x", "name = 'X'\nversion = '1.1'\n");
    let outcome = system
        .button_immediate_upgrade(&admin, &[n("x")])
        .await
        .unwrap();
    assert_eq!(outcome.upgraded, vec![n("x")]);
    assert!(table_exists(&outcome.orm, "migration_selected").await);

    // the same bytes in a selected folder (1.2) are refused
    let selected = addons.path().join("x/migrations/1.2/post-a.sql");
    std::fs::create_dir_all(selected.parent().unwrap()).unwrap();
    std::fs::write(&selected, b"SELECT '\xff';").unwrap();
    addons.add("x", "name = 'X'\nversion = '1.2'\n");
    let admin = su(&outcome.orm);
    let err = system
        .button_immediate_upgrade(&admin, &[n("x")])
        .await
        .unwrap_err();
    match err {
        Error::Io { path, source } => {
            assert_eq!(path, selected);
            assert_eq!(source.kind(), std::io::ErrorKind::InvalidData);
        }
        other => panic!("expected Io, got {other:?}"),
    }
    db.cleanup().await.unwrap();
}

// ---- transaction control and operation budgets (review r3 R3-M1, R3-M2) ----------------------

/// The module system of a two-module upgrade test: `y` and `x` (which depends on `y`, so `y`
/// upgrades first) installed at 1.0, the builder adjusted by `limits`; the ORM after the install
/// and its superuser environment.
async fn installed_y_x(
    addons: &Addons,
    db: &TestDb,
    limits: impl FnOnce(
        erp_modules::system::ModuleSystemBuilder,
    ) -> erp_modules::system::ModuleSystemBuilder,
) -> (
    erp_modules::system::ModuleSystem,
    erp_orm::Orm,
    erp_orm::Env,
) {
    addons.add("y", "name = 'Y'\nversion = '1.0'\n");
    addons.add("x", "name = 'X'\nversion = '1.0'\ndepends = ['y']\n");
    let system = limits(migration_builder(db, addons)).build().unwrap();
    let outcome = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    let admin = su(&outcome.orm);
    let outcome = system
        .button_immediate_install(&admin, &[n("x")])
        .await
        .unwrap();
    assert_eq!(outcome.installed, vec![n("y"), n("x")]);
    let admin = su(&outcome.orm);
    (system, outcome.orm, admin)
}

/// Upgrade `y` and `x` to 1.1.
async fn upgrade_y_x(
    addons: &Addons,
    system: &erp_modules::system::ModuleSystem,
    admin: &erp_orm::Env,
) -> erp_modules::Result<erp_modules::system::OperationOutcome> {
    addons.add("y", "name = 'Y'\nversion = '1.1'\n");
    addons.add("x", "name = 'X'\nversion = '1.1'\ndepends = ['y']\n");
    system
        .button_immediate_upgrade(admin, &[n("y"), n("x")])
        .await
}

/// Whether `table` exists, read on a fresh connection outside every operation transaction.
async fn committed_table(db: &TestDb, table: &str) -> bool {
    let rows = db
        .pool()
        .get()
        .await
        .unwrap()
        .query("SELECT to_regclass($1) IS NOT NULL;", &[&table])
        .await
        .unwrap();
    rows[0].get::<_, bool>(0)
}

/// `y` and `x` are still installed at 1.0.0: the failed upgrade changed no module row.
async fn y_x_unchanged(orm: &erp_orm::Orm) {
    assert_eq!(state(orm, "y").await, "installed");
    assert_eq!(state(orm, "x").await, "installed");
    assert_eq!(
        rows(
            orm,
            "SELECT name, latest_version FROM ir_module_module WHERE name IN ('x', 'y') \
             ORDER BY name"
        )
        .await,
        vec![
            vec![Some("x".to_owned()), Some("1.0.0".to_owned())],
            vec![Some("y".to_owned()), Some("1.0.0".to_owned())],
        ]
    );
}

/// A SQL migration that commits the operation transaction is refused before any of it is sent:
/// the early `COMMIT` followed by a failing statement leaves neither the script's own table nor
/// the earlier module's migration write behind. Without the refusal the `COMMIT` would persist
/// both before `1/0` fails.
#[tokio::test]
async fn sql_transaction_control_is_refused_before_it_is_sent() {
    let addons = Addons::new();
    addons.file(
        "y",
        "migrations/1.1/pre-a.sql",
        "CREATE TABLE migration_tc_earlier (id int);",
    );
    addons.file(
        "x",
        "migrations/1.1/pre-a.sql",
        "CREATE TABLE migration_tc_escape (id int);\n-- now persist it\nCOMMIT;\nSELECT 1/0;\n",
    );
    let db = TestDb::create().await.unwrap();
    let (system, orm, admin) = installed_y_x(&addons, &db, |b| b).await;

    let err = upgrade_y_x(&addons, &system, &admin).await.unwrap_err();
    let message = err.to_string();
    match err {
        Error::MigrationSqlRefused {
            module,
            path,
            refusal,
        } => {
            assert_eq!(module, n("x"));
            assert_eq!(path, "migrations/1.1/pre-a.sql");
            assert_eq!(
                refusal,
                MigrationSqlRefusal::TransactionControl {
                    statement: 2,
                    keyword: "COMMIT".to_owned(),
                }
            );
            assert_eq!(
                message,
                "Module \"x\": SQL migration 'migrations/1.1/pre-a.sql' is refused: statement 2 \
                 is transaction control (COMMIT); a migration runs inside the upgrade's \
                 transaction and must not begin, commit or roll back one"
            );
        }
        other => panic!("expected MigrationSqlRefused, got {other:?}"),
    }
    assert!(!committed_table(&db, "migration_tc_escape").await);
    assert!(!committed_table(&db, "migration_tc_earlier").await);
    y_x_unchanged(&orm).await;

    // PostgreSQL ends a line comment at CR as well as LF: a `COMMIT` after a CR- or
    // CRLF-terminated comment is a statement and is refused the same way
    for ending in ["\r", "\r\n"] {
        addons.file(
            "x",
            "migrations/1.1/pre-a.sql",
            &format!(
                "CREATE TABLE migration_tc_escape (id int); -- now persist it{ending}COMMIT;\
                 {ending}SELECT 1/0;{ending}"
            ),
        );
        let err = upgrade_y_x(&addons, &system, &admin).await.unwrap_err();
        match err {
            Error::MigrationSqlRefused {
                module,
                path,
                refusal,
            } => {
                assert_eq!(module, n("x"), "{ending:?}");
                assert_eq!(path, "migrations/1.1/pre-a.sql", "{ending:?}");
                assert_eq!(
                    refusal,
                    MigrationSqlRefusal::TransactionControl {
                        statement: 2,
                        keyword: "COMMIT".to_owned(),
                    },
                    "{ending:?}"
                );
            }
            other => panic!("expected MigrationSqlRefused for {ending:?}, got {other:?}"),
        }
        assert!(!committed_table(&db, "migration_tc_escape").await);
        assert!(!committed_table(&db, "migration_tc_earlier").await);
        y_x_unchanged(&orm).await;
    }

    // PostgreSQL continues `E'...'` on the next line with backslash escapes kept: the `\'` does
    // not close the continuation, so the `COMMIT` after it is a statement and is refused
    // (without the refusal it would persist both tables before `1/0` fails)
    for gap in ["\n", "\r\n", " -- c '\n  "] {
        addons.file(
            "x",
            "migrations/1.1/pre-a.sql",
            &format!(
                "CREATE TABLE migration_tc_escape (id int);\n\
                 SELECT E'a'{gap}'x\\'foo'; COMMIT; SELECT 1/0; -- '\n"
            ),
        );
        let err = upgrade_y_x(&addons, &system, &admin).await.unwrap_err();
        match err {
            Error::MigrationSqlRefused {
                module,
                path,
                refusal,
            } => {
                assert_eq!(module, n("x"), "{gap:?}");
                assert_eq!(path, "migrations/1.1/pre-a.sql", "{gap:?}");
                assert_eq!(
                    refusal,
                    MigrationSqlRefusal::TransactionControl {
                        statement: 3,
                        keyword: "COMMIT".to_owned(),
                    },
                    "{gap:?}"
                );
            }
            other => panic!("expected MigrationSqlRefused for {gap:?}, got {other:?}"),
        }
        assert!(!committed_table(&db, "migration_tc_escape").await);
        assert!(!committed_table(&db, "migration_tc_earlier").await);
        y_x_unchanged(&orm).await;
    }

    // transaction control inside a procedural block is refused by PostgreSQL itself in the
    // operation's transaction block (invalid_transaction_termination); nothing persists either
    addons.file(
        "x",
        "migrations/1.1/pre-a.sql",
        "CREATE TABLE migration_tc_escape (id int);\nDO $$ BEGIN COMMIT; END $$;\n",
    );
    let err = upgrade_y_x(&addons, &system, &admin).await.unwrap_err();
    let inner = migration_step_failed(err, MigrationStage::Pre, "migrations/1.1/pre-a.sql");
    match inner {
        Error::MigrationFailed { module, source, .. } => {
            assert_eq!(module, n("x"));
            assert_eq!(source.sqlstate(), Some("2D000"));
        }
        other => panic!("expected MigrationFailed, got {other:?}"),
    }
    assert!(!committed_table(&db, "migration_tc_escape").await);
    assert!(!committed_table(&db, "migration_tc_earlier").await);
    y_x_unchanged(&orm).await;
    db.cleanup().await.unwrap();
}

/// Transaction-control words inside strings, quoted identifiers, comments and dollar-quoted
/// bodies are not statements: such a script runs.
#[tokio::test]
async fn sql_transaction_words_in_literals_and_comments_run() {
    let addons = Addons::new();
    addons.file(
        "x",
        "migrations/1.1/pre-a.sql",
        "-- COMMIT; ROLLBACK;\n\
         /* BEGIN; /* nested; COMMIT; */ END; */\n\
         CREATE TABLE migration_tc_words (\"commit\" text, note text);\n\
         INSERT INTO migration_tc_words VALUES ('COMMIT; ROLLBACK', 'it''s; BEGIN');\n\
         INSERT INTO migration_tc_words VALUES (E'a\\'; COMMIT; \\'', $tag$; END; $tag$);\n\
         DO $body$ BEGIN PERFORM 1; END $body$;\n\
         CREATE FUNCTION migration_tc_fn() RETURNS int LANGUAGE plpgsql AS $$\n\
         BEGIN RETURN 1; END;\n\
         $$;\n",
    );
    let db = TestDb::create().await.unwrap();
    let (system, _orm, admin) = installed_y_x(&addons, &db, |b| b).await;

    let outcome = upgrade_y_x(&addons, &system, &admin).await.unwrap();
    assert_eq!(outcome.upgraded, vec![n("y"), n("x")]);
    assert_eq!(
        rows(
            &outcome.orm,
            "SELECT \"commit\", note FROM migration_tc_words ORDER BY note COLLATE \"C\""
        )
        .await,
        vec![
            vec![Some("a'; COMMIT; '".to_owned()), Some("; END; ".to_owned())],
            vec![
                Some("COMMIT; ROLLBACK".to_owned()),
                Some("it's; BEGIN".to_owned())
            ],
        ]
    );
    db.cleanup().await.unwrap();
}

/// A SQL migration is sent only while the session reads strings the way the scan did: an
/// earlier migration that turns `standard_conforming_strings` off makes the next script refused
/// before it is sent, and the operation rolls back.
#[tokio::test]
async fn sql_migration_with_non_standard_strings_is_refused() {
    let addons = Addons::new();
    addons.file(
        "y",
        "migrations/1.1/pre-a.sql",
        "CREATE TABLE migration_tc_earlier (id int);\nSET standard_conforming_strings = off;\n",
    );
    addons.file(
        "x",
        "migrations/1.1/pre-a.sql",
        "CREATE TABLE migration_tc_escape (id int);",
    );
    let db = TestDb::create().await.unwrap();
    let (system, orm, admin) = installed_y_x(&addons, &db, |b| b).await;

    let err = upgrade_y_x(&addons, &system, &admin).await.unwrap_err();
    // refused when the step is to run: the step error names the stage and the file
    let inner = migration_step_failed(err, MigrationStage::Pre, "migrations/1.1/pre-a.sql");
    let message = inner.to_string();
    match inner {
        Error::MigrationSqlRefused {
            module,
            path,
            refusal,
        } => {
            assert_eq!(module, n("x"));
            assert_eq!(path, "migrations/1.1/pre-a.sql");
            assert_eq!(refusal, MigrationSqlRefusal::NonStandardStrings);
            assert_eq!(
                message,
                "Module \"x\": SQL migration 'migrations/1.1/pre-a.sql' is refused: \
                 standard_conforming_strings is off in the upgrade's session; SQL migrations are \
                 checked with standard strings and run only while it is on"
            );
        }
        other => panic!("expected MigrationSqlRefused, got {other:?}"),
    }
    assert!(!committed_table(&db, "migration_tc_escape").await);
    assert!(!committed_table(&db, "migration_tc_earlier").await);
    y_x_unchanged(&orm).await;
    db.cleanup().await.unwrap();
}

/// The fixture of the budget tests: `y` selects two SQL files (1.1) and has an unselected
/// folder (1.0) whose file would be refused if it were read, and a non-script file; `x` selects
/// one file. Each selected file has [`BUDGET_FILE_BYTES`] bytes.
fn budget_fixture(addons: &Addons) {
    addons.file(
        "y",
        "migrations/1.1/pre-a.sql",
        "CREATE TABLE migration_b_y1 (i int);",
    );
    addons.file(
        "y",
        "migrations/1.1/post-b.sql",
        "CREATE TABLE migration_b_y2 (i int);",
    );
    addons.file("y", "migrations/1.1/README.txt", "not a script");
    let unselected = addons.path().join("y/migrations/1.0/pre-a.sql");
    std::fs::create_dir_all(unselected.parent().unwrap()).unwrap();
    std::fs::write(&unselected, b"SELECT '\xff';").unwrap();
    addons.file(
        "x",
        "migrations/1.1/pre-a.sql",
        "CREATE TABLE migration_b_x1 (i int);",
    );
}

/// Bytes of each selected file of [`budget_fixture`].
const BUDGET_FILE_BYTES: u64 = 36;

/// Exactly `max_migration_files` selected SQL files in one operation (all modules together)
/// run; unselected folders and non-script files do not count.
#[tokio::test]
async fn migration_file_count_at_the_limit_runs() {
    let addons = Addons::new();
    budget_fixture(&addons);
    let db = TestDb::create().await.unwrap();
    let (system, _orm, admin) = installed_y_x(&addons, &db, |b| b.max_migration_files(3)).await;

    let outcome = upgrade_y_x(&addons, &system, &admin).await.unwrap();
    assert_eq!(outcome.upgraded, vec![n("y"), n("x")]);
    for table in ["migration_b_y1", "migration_b_y2", "migration_b_x1"] {
        assert!(committed_table(&db, table).await, "{table}");
    }
    db.cleanup().await.unwrap();
}

/// One selected SQL file past `max_migration_files` refuses the operation before that file is
/// read; the earlier module's migrations are rolled back with it.
#[tokio::test]
async fn migration_file_count_past_the_limit_is_refused() {
    let addons = Addons::new();
    budget_fixture(&addons);
    let db = TestDb::create().await.unwrap();
    let (system, orm, admin) = installed_y_x(&addons, &db, |b| b.max_migration_files(2)).await;

    let err = upgrade_y_x(&addons, &system, &admin).await.unwrap_err();
    let message = err.to_string();
    match err {
        Error::MigrationFilesTooMany {
            module,
            path,
            limit,
        } => {
            assert_eq!(module, n("x"));
            assert_eq!(path, "migrations/1.1/pre-a.sql");
            assert_eq!(limit, 2);
            assert_eq!(
                message,
                "Module \"x\": SQL migration 'migrations/1.1/pre-a.sql' passes the 2 migration \
                 files one operation selects (ModuleSystemBuilder::max_migration_files)"
            );
        }
        other => panic!("expected MigrationFilesTooMany, got {other:?}"),
    }
    for table in ["migration_b_y1", "migration_b_y2", "migration_b_x1"] {
        assert!(!committed_table(&db, table).await, "{table}");
    }
    y_x_unchanged(&orm).await;
    db.cleanup().await.unwrap();
}

/// A `migrations/` directory with more version folders than `max_migration_files` is refused
/// when it is listed, before any file in it is read.
#[tokio::test]
async fn migration_folder_count_past_the_limit_is_refused() {
    let addons = Addons::new();
    budget_fixture(&addons);
    // y has two version folders (1.0, 1.1) and two selected files: within a limit of 2. x has
    // three folders (1.1, and the unselected 1.2 and 1.3): one past it, refused when listed
    addons.file("x", "migrations/1.2/pre-a.sql", "SELECT 1;");
    addons.file("x", "migrations/1.3/pre-a.sql", "SELECT 1;");
    let db = TestDb::create().await.unwrap();
    let (system, orm, admin) = installed_y_x(&addons, &db, |b| b.max_migration_files(2)).await;

    let err = upgrade_y_x(&addons, &system, &admin).await.unwrap_err();
    let message = err.to_string();
    match err {
        Error::MigrationFoldersTooMany { module, limit } => {
            assert_eq!(module, n("x"));
            assert_eq!(limit, 2);
            assert_eq!(
                message,
                "Module \"x\": migrations/ holds more than 2 version folders \
                 (ModuleSystemBuilder::max_migration_files)"
            );
        }
        other => panic!("expected MigrationFoldersTooMany, got {other:?}"),
    }
    for table in ["migration_b_y1", "migration_b_y2", "migration_b_x1"] {
        assert!(!committed_table(&db, table).await, "{table}");
    }
    y_x_unchanged(&orm).await;
    db.cleanup().await.unwrap();
}

/// The selected SQL files of one operation are charged to `max_operation_data_bytes` together:
/// exactly the limit runs.
#[tokio::test]
async fn migration_sql_bytes_at_the_operation_limit_run() {
    let addons = Addons::new();
    budget_fixture(&addons);
    let db = TestDb::create().await.unwrap();
    let (system, _orm, admin) = installed_y_x(&addons, &db, |b| {
        b.max_operation_data_bytes(3 * BUDGET_FILE_BYTES)
    })
    .await;

    let outcome = upgrade_y_x(&addons, &system, &admin).await.unwrap();
    assert_eq!(outcome.upgraded, vec![n("y"), n("x")]);
    assert!(committed_table(&db, "migration_b_x1").await);
    db.cleanup().await.unwrap();
}

/// One byte less than the selected SQL files of the operation refuses it on the file that passes
/// the limit; the earlier module's migrations are rolled back with it.
#[tokio::test]
async fn migration_sql_bytes_past_the_operation_limit_are_refused() {
    let addons = Addons::new();
    budget_fixture(&addons);
    let db = TestDb::create().await.unwrap();
    let limit = 3 * BUDGET_FILE_BYTES - 1;
    let (system, orm, admin) =
        installed_y_x(&addons, &db, |b| b.max_operation_data_bytes(limit)).await;

    let err = upgrade_y_x(&addons, &system, &admin).await.unwrap_err();
    let message = err.to_string();
    match err {
        Error::OperationDataTooLarge {
            module,
            path,
            limit: refused_at,
        } => {
            assert_eq!(module, n("x"));
            assert_eq!(path, "migrations/1.1/pre-a.sql");
            assert_eq!(refused_at, limit);
            assert_eq!(
                message,
                format!(
                    "Module \"x\": data file 'migrations/1.1/pre-a.sql' passes the {limit} bytes \
                     of data files one operation reads (ModuleSystemBuilder::max_operation_data_bytes)"
                )
            );
        }
        other => panic!("expected OperationDataTooLarge, got {other:?}"),
    }
    for table in ["migration_b_y1", "migration_b_y2", "migration_b_x1"] {
        assert!(!committed_table(&db, table).await, "{table}");
    }
    y_x_unchanged(&orm).await;
    db.cleanup().await.unwrap();
}

/// `max_migration_files` is between 1 and `MAX_MIGRATION_FILES`.
#[tokio::test]
async fn max_migration_files_out_of_range_is_refused_at_build() {
    let addons = Addons::new();
    let db = TestDb::create().await.unwrap();
    for value in [0, erp_modules::system::MAX_MIGRATION_FILES + 1] {
        let err = builder(&db, &addons)
            .max_migration_files(value)
            .build()
            .err()
            .unwrap();
        let message = err.to_string();
        match err {
            Error::InvalidLimit {
                name,
                value: got,
                max,
            } => {
                assert_eq!(name, erp_modules::error::LimitKind::MaxMigrationFiles);
                assert_eq!(got, value as u64);
                assert_eq!(max, erp_modules::system::MAX_MIGRATION_FILES as u64);
                assert_eq!(
                    message,
                    format!("max_migration_files must be between 1 and {max}, got {value}")
                );
            }
            other => panic!("expected InvalidLimit, got {other:?}"),
        }
    }
    db.cleanup().await.unwrap();
}

// ---- schema steps of one run wait for a later module's pre-migration --------------------------

/// `y` declares `mig.item` with rows; `x` 1.1 extends it with a required column that its
/// pre-migration adds and fills. Upgraded together, `y`'s schema step comes first and must leave
/// the column to `x`'s step: syncing it there would refuse a required column without default on
/// a table with rows before `x`'s pre-migration ran.
/// changes its table, when the module only extends the model)
#[tokio::test]
async fn pre_migration_runs_before_an_earlier_module_syncs_its_column() {
    let addons = Addons::new();
    addons.file(
        "y",
        "models/item.toml",
        "[[model]]\nname = \"mig.item\"\ndescription = \"Item\"\n\n\
         [[model.field]]\nname = \"name\"\ntype = \"char\"\n",
    );
    let db = TestDb::create().await.unwrap();
    let (system, orm, admin) = installed_y_x(&addons, &db, |b| b).await;
    exec(
        &orm,
        "INSERT INTO mig_item (name) VALUES ('first'), ('second')",
    )
    .await;
    addons.file(
        "x",
        "models/item.toml",
        "[[model]]\nextend = \"mig.item\"\n\n\
         [[model.field]]\nname = \"code\"\ntype = \"char\"\nrequired = true\n",
    );
    addons.file(
        "x",
        "migrations/1.1/pre-a.sql",
        "ALTER TABLE mig_item ADD COLUMN code varchar;\n\
         UPDATE mig_item SET code = 'legacy-' || name;\n",
    );

    let outcome = upgrade_y_x(&addons, &system, &admin)
        .await
        .unwrap_or_else(|e| panic!("upgrade of y and x: {e:?}"));
    assert_eq!(outcome.upgraded, vec![n("y"), n("x")]);
    assert_eq!(
        rows(&outcome.orm, "SELECT code FROM mig_item ORDER BY name").await,
        vec![
            vec![Some("legacy-first".to_owned())],
            vec![Some("legacy-second".to_owned())],
        ]
    );
    let nullable = rows(
        &outcome.orm,
        "SELECT is_nullable::text FROM information_schema.columns \
         WHERE table_name = 'mig_item' AND column_name = 'code'",
    )
    .await;
    assert_eq!(nullable, vec![vec![Some("NO".to_owned())]]);
    drop(system);
    db.cleanup().await.unwrap();
}

/// Whether the column `table.column` is NOT NULL, read from the catalog.
async fn not_null(orm: &erp_orm::Orm, table: &str, column: &str) -> bool {
    let found = rows(
        orm,
        &format!(
            "SELECT is_nullable::text FROM information_schema.columns \
             WHERE table_name = '{table}' AND column_name = '{column}'"
        ),
    )
    .await;
    assert_eq!(found.len(), 1, "{table}.{column} exists once");
    found[0][0].as_deref() == Some("NO")
}

/// Two later modules of the run extend `y`'s model, each with a required column its own `pre`
/// migration adds and fills; `z`'s fill reads the column `x`'s migration filled. The model waits
/// at `y`'s and `x`'s steps and is synced at `z`'s, after both `pre` stages.
/// makes their columns required, in one run)
#[tokio::test]
async fn two_later_pre_migrations_hold_the_model_until_the_last() {
    let addons = Addons::new();
    addons
        .add("y", "name = 'Y'\nversion = '1.0'\n")
        .add("x", "name = 'X'\nversion = '1.0'\ndepends = ['y']\n")
        .add("z", "name = 'Z'\nversion = '1.0'\ndepends = ['x']\n")
        .file(
            "y",
            "models/item.toml",
            "[[model]]\nname = \"mig.item\"\ndescription = \"Item\"\n\n\
             [[model.field]]\nname = \"name\"\ntype = \"char\"\n",
        );
    let db = TestDb::create().await.unwrap();
    let system = migration_builder(&db, &addons).build().unwrap();
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    let installed = system
        .button_immediate_install(&su(&init.orm), &[n("z")])
        .await
        .unwrap();
    assert_eq!(installed.installed, vec![n("y"), n("x"), n("z")]);
    exec(
        &installed.orm,
        "INSERT INTO mig_item (name) VALUES ('first')",
    )
    .await;
    addons
        .add("y", "name = 'Y'\nversion = '1.1'\n")
        .add("x", "name = 'X'\nversion = '1.1'\ndepends = ['y']\n")
        .add("z", "name = 'Z'\nversion = '1.1'\ndepends = ['x']\n")
        .file(
            "x",
            "models/item.toml",
            "[[model]]\nextend = \"mig.item\"\n\n\
             [[model.field]]\nname = \"code\"\ntype = \"char\"\nrequired = true\n",
        )
        .file(
            "x",
            "migrations/1.1/pre-a.sql",
            "ALTER TABLE mig_item ADD COLUMN code varchar;\n\
             UPDATE mig_item SET code = 'x-' || name;\n",
        )
        .file(
            "z",
            "models/item.toml",
            "[[model]]\nextend = \"mig.item\"\n\n\
             [[model.field]]\nname = \"tag\"\ntype = \"char\"\nrequired = true\n",
        )
        .file(
            "z",
            "migrations/1.1/pre-a.sql",
            "ALTER TABLE mig_item ADD COLUMN tag varchar;\n\
             UPDATE mig_item SET tag = 'z-' || code;\n",
        );

    let outcome = system
        .button_immediate_upgrade(&su(&installed.orm), &[n("y"), n("x"), n("z")])
        .await
        .unwrap_or_else(|e| panic!("upgrade of y, x and z: {e:?}"));
    assert_eq!(outcome.upgraded, vec![n("y"), n("x"), n("z")]);
    assert_eq!(
        rows(&outcome.orm, "SELECT code, tag FROM mig_item").await,
        vec![vec![
            Some("x-first".to_owned()),
            Some("z-x-first".to_owned())
        ]]
    );
    assert!(not_null(&outcome.orm, "mig_item", "code").await);
    assert!(not_null(&outcome.orm, "mig_item", "tag").await);
    drop(system);
    db.cleanup().await.unwrap();
}

/// A later module of the run that owns a column of the model but has no `pre` stage selected
/// holds nothing back: `y`'s new column exists when `y`'s own data file writes it. `x` stays at
/// its installed version, so its `1.1` script is not selected (a version runs only when it is
/// above the installed one and at most the target; equal versions select nothing).
#[tokio::test]
async fn a_later_module_without_pre_stage_does_not_hold_the_model() {
    let addons = Addons::new();
    addons
        .file(
            "y",
            "models/item.toml",
            "[[model]]\nname = \"mig.item\"\ndescription = \"Item\"\n\n\
             [[model.field]]\nname = \"name\"\ntype = \"char\"\n",
        )
        .file(
            "x",
            "models/item.toml",
            "[[model]]\nextend = \"mig.item\"\n\n\
             [[model.field]]\nname = \"flag\"\ntype = \"char\"\n",
        )
        .file("x", "migrations/1.1/pre-a.sql", "SELECT 1/0;\n");
    let db = TestDb::create().await.unwrap();
    let (system, _orm, admin) = installed_y_x(&addons, &db, |b| b).await;
    addons
        .add(
            "y",
            "name = 'Y'\nversion = '1.1'\ndata = ['data/item.xml']\n",
        )
        .file(
            "y",
            "models/item.toml",
            "[[model]]\nname = \"mig.item\"\ndescription = \"Item\"\n\n\
             [[model.field]]\nname = \"name\"\ntype = \"char\"\n\n\
             [[model.field]]\nname = \"note\"\ntype = \"char\"\n",
        )
        .file(
            "y",
            "data/item.xml",
            "<odoo><record id=\"r1\" model=\"mig.item\"><field name=\"name\">one</field>\
             <field name=\"note\">from y</field></record></odoo>",
        );

    let outcome = system
        .button_immediate_upgrade(&admin, &[n("y"), n("x")])
        .await
        .unwrap_or_else(|e| panic!("upgrade of y and x: {e:?}"));
    assert_eq!(outcome.upgraded, vec![n("y"), n("x")]);
    assert_eq!(
        rows(&outcome.orm, "SELECT name, note FROM mig_item").await,
        vec![vec![Some("one".to_owned()), Some("from y".to_owned())]]
    );
    assert_eq!(
        rows(
            &outcome.orm,
            "SELECT latest_version FROM ir_module_module WHERE name = 'x'"
        )
        .await,
        vec![vec![Some("1.0.0".to_owned())]]
    );
    drop(system);
    db.cleanup().await.unwrap();
}
