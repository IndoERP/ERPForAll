// SPDX-License-Identifier: MIT OR Apache-2.0
//! `erp db drop | dump | restore | neutralize` and the demo data choice of `erp db init` and
//! `erp module install` (spec DBLIFE), through the real `erp` binary on the test cluster.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration assertions"
)]
#[path = "support/native_http.rs"]
mod native_http;
#[path = "support/runtime_addons.rs"]
mod runtime_addons;

use clap::Parser;
use erp_cli::{Cli, Commands};
use erp_testkit::TestDb;
use erp_testkit::tokio_postgres::{Client, NoTls};
use std::ffi::OsStr;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// The job kinds keeps running on a neutralized database.
const KEPT_KINDS: [&str; 2] = ["core.jobs_dead_expiry", "core.jobs_history_retention"];

/// The PostgreSQL 18 client programs of the build host (`ERP_PG_BIN` names another directory).
fn pg_bin() -> PathBuf {
    if let Some(dir) = std::env::var_os("ERP_PG_BIN") {
        return PathBuf::from(dir);
    }
    [
        "/opt/homebrew/opt/postgresql@18/bin",
        "/usr/lib/postgresql/18/bin",
    ]
    .into_iter()
    .map(PathBuf::from)
    .find(|dir| dir.join("pg_dump").is_file())
    .expect(
        "PostgreSQL 18 client programs not found: install postgresql@18 (Homebrew) or \
             postgresql-client-18, or set ERP_PG_BIN",
    )
}

fn erp(args: &[&OsStr], env: &[(&str, &str)]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_erp"));
    command.args(args).env("ERP_PG_BIN", pg_bin());
    for (key, value) in env {
        command.env(key, value);
    }
    command.output().unwrap()
}

/// The outputs of every command carry no credential.
fn clean(output: &Output, secrets: &[&str]) -> String {
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    for secret in secrets {
        assert!(!text.contains(secret), "a secret was printed: {text}");
    }
    assert!(!text.contains("postgres://"), "a URL was printed: {text}");
    assert!(!text.contains("postgresql://"), "a URL was printed: {text}");
    text
}

fn ok(output: &Output, secrets: &[&str]) -> String {
    let text = clean(output, secrets);
    assert!(output.status.success(), "command failed: {text}");
    text
}

fn refused(output: &Output, secrets: &[&str], expected: &[&str]) {
    failed_with(output, 1, secrets, expected);
}

/// The command failed with exit `code` and said each of `expected`.
fn failed_with(output: &Output, code: i32, secrets: &[&str], expected: &[&str]) {
    let text = clean(output, secrets);
    assert_eq!(output.status.code(), Some(code), "not refused: {text}");
    for part in expected {
        assert!(text.contains(part), "refusal without {part:?}: {text}");
    }
}

fn os<'a>(values: &'a [&'a str]) -> Vec<&'a OsStr> {
    values.iter().map(OsStr::new).collect()
}

async fn connect(db: &TestDb, name: &str) -> Client {
    let mut config = db.config();
    config.dbname(name);
    let (client, connection) = config.connect(NoTls).await.unwrap();
    tokio::spawn(connection);
    client
}

fn addons_source() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("addons")
}

/// A running node in `client`'s database (the node table as the ORM creates it).
async fn live_node(client: &Client) {
    client.batch_execute(erp_orm::node::NODE_DDL).await.unwrap();
    client
        .batch_execute(
            "INSERT INTO erp_node (id, name, host, pid, role, dev, digest, config_digest, \
             build_digest, addons_digest, build_label, registered_at, state, heartbeat_at, \
             expires_at, beat, interval_ms, lease_ms) VALUES (gen_random_uuid(), 'fixture-node', \
             'localhost', 1, 'app', false, sha256('a'), sha256('b'), sha256('c'), sha256('d'), \
             'fixture', now(), 'ready', now(), now() + interval '1 hour', 1, 1000, 2000)",
        )
        .await
        .unwrap();
}

/// each other, and every other module operation refuses them before touching a database)
#[test]
fn demo_options_parse_on_init_and_install_and_nowhere_else() {
    let init = |flags: &[&str]| {
        let mut line = vec![
            "erp",
            "db",
            "init",
            "--config",
            "/private/server.toml",
            "--admin-login",
            "operator",
            "--admin-password-file",
            "/private/password",
        ];
        line.extend_from_slice(flags);
        Cli::try_parse_from(line)
    };
    init(&["--with-demo"]).unwrap();
    init(&["--without-demo"]).unwrap();
    init(&[]).unwrap();
    assert_eq!(
        init(&["--with-demo", "--without-demo"]).unwrap_err().kind(),
        clap::error::ErrorKind::ArgumentConflict
    );
    let module = |op: &str, flag: &str| {
        Cli::try_parse_from(["erp", "module", op, "-d", "erp_demo_check", flag, "base"]).unwrap()
    };
    module("install", "--with-demo");
    module("install", "--without-demo");
    for op in ["upgrade", "uninstall"] {
        let Commands::Module(command) = module(op, "--with-demo").command else {
            panic!("not a module command");
        };
        let error = erp_cli::module::run_module_command(&command.action).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("only the install command takes a demo data choice"),
            "{op}: {error}"
        );
    }
}

/// Whether database `name` exists, read through `client`.
async fn database_exists(client: &Client, name: &str) -> bool {
    client
        .query_one(
            "SELECT EXISTS (SELECT FROM pg_database WHERE datname = $1)",
            &[&name],
        )
        .await
        .unwrap()
        .get(0)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn drop_dump_restore_one_database_and_refuse_a_live_one() {
    let db = TestDb::create().await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let config = native_http::config(dir.path(), &db, 1, "dblife_unused", &addons_source());
    let config = config.to_str().unwrap();
    let owner_password = String::from_utf8(db.config().get_password().unwrap().to_vec()).unwrap();
    let secrets = [owner_password.as_str()];
    let a = db.name().to_owned();
    let copy = format!("{a}_c");
    let admin = connect(&db, &a).await;
    // DROP DATABASE refuses to run inside the implicit transaction of a multi-statement batch
    admin
        .batch_execute(&format!("DROP DATABASE IF EXISTS {copy}"))
        .await
        .unwrap();
    admin
        .batch_execute(
            "CREATE TABLE dblife_marker (v text); INSERT INTO dblife_marker VALUES ('kept')",
        )
        .await
        .unwrap();
    let marker = async |name: &str| -> Vec<String> {
        let client = connect(&db, name).await;
        let rows = client
            .query("SELECT v FROM dblife_marker", &[])
            .await
            .unwrap();
        rows.iter().map(|row| row.get(0)).collect()
    };
    let archive = dir.path().join("a.dump");
    let archive_s = archive.to_str().unwrap();

    // dump: a new private file, never over an existing one
    ok(
        &erp(
            &os(&[
                "db", "dump", "--config", config, "-d", &a, "--file", archive_s,
            ]),
            &[],
        ),
        &secrets,
    );
    let mode = std::fs::metadata(&archive).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    refused(
        &erp(
            &os(&[
                "db", "dump", "--config", config, "-d", &a, "--file", archive_s,
            ]),
            &[],
        ),
        &secrets,
        &[archive_s, "exists"],
    );

    // restore: a new database; an existing one only with --replace
    let restore = |extra: &[&str]| {
        let mut line = vec![
            "db", "restore", "--config", config, "-d", &copy, "--file", archive_s,
        ];
        line.extend_from_slice(extra);
        erp(&os(&line), &[])
    };
    ok(&restore(&[]), &secrets);
    assert_eq!(marker(&copy).await, ["kept"]);
    refused(&restore(&[]), &secrets, &[&copy, "--replace"]);
    connect(&db, &copy)
        .await
        .batch_execute("INSERT INTO dblife_marker VALUES ('later')")
        .await
        .unwrap();
    ok(&restore(&["--replace"]), &secrets);
    assert_eq!(marker(&copy).await, ["kept"]);
    // the replaced database is gone once the restored one has its name, and the restored one is
    // created like every ERPForAll database (C collation)
    let staging = format!("{copy}_erp_restore");
    let replaced = format!("{copy}_erp_replaced");
    assert!(
        !database_exists(&admin, &replaced).await,
        "{replaced} was left"
    );
    let collation: String = admin
        .query_one(
            "SELECT datcollate::text FROM pg_database WHERE datname = $1",
            &[&copy],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(collation, "C");

    // a database left under one of the restore's own names is refused by name, and nothing changes
    for (leftover, expected) in [
        (
            &staging,
            ["stopped before it finished", "running now", "erp db drop"],
        ),
        (
            &replaced,
            [
                "stopped before it finished",
                "running now",
                "rename it back",
            ],
        ),
    ] {
        admin
            .batch_execute(&format!("CREATE DATABASE {leftover}"))
            .await
            .unwrap();
        let mut parts = vec![leftover.as_str()];
        parts.extend_from_slice(&expected);
        refused(&restore(&["--replace"]), &secrets, &parts);
        admin
            .batch_execute(&format!("DROP DATABASE {leftover}"))
            .await
            .unwrap();
        assert_eq!(marker(&copy).await, ["kept"]);
    }

    // a restore stopped between its two renames: the target is missing, the original is under the
    // replaced name and the restored archive under the temporary one; the refusal says to rename the
    // original back and never to drop it (made by hand here)
    admin
        .batch_execute(&format!("ALTER DATABASE {copy} RENAME TO {replaced}"))
        .await
        .unwrap();
    admin
        .batch_execute(&format!("CREATE DATABASE {staging}"))
        .await
        .unwrap();
    let output = restore(&["--replace"]);
    refused(
        &output,
        &secrets,
        &[
            &format!("database {copy} does not exist and {replaced} holds what it was"),
            &format!("rename {replaced} back to {copy}"),
            &format!("{staging} holds the archive"),
        ],
    );
    assert!(
        !clean(&output, &secrets).contains(&format!("drop {replaced}")),
        "the only copy of the original must not be dropped"
    );
    assert!(!database_exists(&admin, &copy).await);
    assert!(database_exists(&admin, &replaced).await);
    // with the target present, each of the other two is named for what it holds
    admin
        .batch_execute(&format!("ALTER DATABASE {replaced} RENAME TO {copy}"))
        .await
        .unwrap();
    admin
        .batch_execute(&format!("CREATE DATABASE {replaced}"))
        .await
        .unwrap();
    refused(
        &restore(&["--replace"]),
        &secrets,
        &[
            &format!("{replaced} exists: it holds what {copy} was"),
            &format!("{staging} holds the archive"),
            "rename it back",
        ],
    );
    for leftover in [&staging, &replaced] {
        admin
            .batch_execute(&format!("DROP DATABASE {leftover}"))
            .await
            .unwrap();
    }
    assert_eq!(marker(&copy).await, ["kept"]);

    // a name whose restore names would be longer than PostgreSQL's limit is refused by name
    let long = "d".repeat(51);
    refused(
        &erp(
            &os(&[
                "db", "restore", "--config", config, "-d", &long, "--file", archive_s,
            ]),
            &[],
        ),
        &secrets,
        &[&long, "too long", "at most 50 bytes"],
    );

    // a file that is not an archive is refused before anything changes
    let junk = dir.path().join("junk.dump");
    std::fs::write(&junk, "not an archive").unwrap();
    let junk_s = junk.to_str().unwrap();
    connect(&db, &copy)
        .await
        .batch_execute("INSERT INTO dblife_marker VALUES ('current')")
        .await
        .unwrap();
    let fresh = format!("{a}_n");
    for target in [&copy, &fresh] {
        refused(
            &erp(
                &os(&[
                    "db",
                    "restore",
                    "--config",
                    config,
                    "-d",
                    target,
                    "--file",
                    junk_s,
                    "--replace",
                ]),
                &[],
            ),
            &secrets,
            &[junk_s, "not an archive", "nothing was changed", "--pg-bin"],
        );
    }
    let current = async || {
        let mut rows = marker(&copy).await;
        rows.sort();
        rows
    };
    assert_eq!(current().await, ["current", "kept"]);

    // a restore that fails after it began (a pg_restore that reads the archive's contents and
    // then fails): the existing database stays as it was, a new one is not created, and no
    // temporary database is left
    let failing_restore = dir.path().join("failing-restore-bin");
    std::fs::create_dir(&failing_restore).unwrap();
    std::fs::write(
        failing_restore.join("pg_restore"),
        "#!/bin/sh\nfor arg in \"$@\"; do [ \"$arg\" = --list ] && exit 0; done\n\
         echo 'fixture pg_restore failure' >&2\nexit 1\n",
    )
    .unwrap();
    std::fs::set_permissions(
        failing_restore.join("pg_restore"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    let stub_env = [("ERP_PG_BIN", failing_restore.to_str().unwrap())];
    failed_with(
        &erp(
            &os(&[
                "db",
                "restore",
                "--config",
                config,
                "-d",
                &copy,
                "--file",
                archive_s,
                "--replace",
            ]),
            &stub_env,
        ),
        2,
        &secrets,
        &[
            "fixture pg_restore failure",
            "--pg-bin",
            "temporary database",
            "dropped",
            "left untouched",
        ],
    );
    assert_eq!(current().await, ["current", "kept"]);
    failed_with(
        &erp(
            &os(&[
                "db", "restore", "--config", config, "-d", &fresh, "--file", archive_s,
            ]),
            &stub_env,
        ),
        2,
        &secrets,
        &[
            "fixture pg_restore failure",
            "temporary database",
            "dropped",
        ],
    );
    for name in [
        fresh.clone(),
        format!("{fresh}_erp_restore"),
        staging.clone(),
        replaced.clone(),
    ] {
        assert!(!database_exists(&admin, &name).await, "{name} was left");
    }

    // a pg_restore stub that restores nothing and makes one of the two renames fail, so the
    // failure messages say what happened to the existing database
    let psql = pg_bin().join("psql");
    let psql = psql.to_str().unwrap();
    let stub = |name: &str, body: &str| {
        let bin = dir.path().join(name);
        std::fs::create_dir(&bin).unwrap();
        std::fs::write(
            bin.join("pg_restore"),
            format!(
                "#!/bin/sh\nfor arg in \"$@\"; do [ \"$arg\" = --list ] && exit 0; done\n{body}\nexit 0\n"
            ),
        )
        .unwrap();
        std::fs::set_permissions(
            bin.join("pg_restore"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        bin
    };
    // the first rename (target to the replaced name) fails: a database already has that name
    let taken = stub(
        "rename-taken-bin",
        &format!(
            "\"{psql}\" -X -d postgres -c 'CREATE DATABASE \"{replaced}\"' </dev/null >/dev/null 2>&1 || exit 3"
        ),
    );
    failed_with(
        &erp(
            &os(&[
                "db",
                "restore",
                "--config",
                config,
                "-d",
                &copy,
                "--file",
                archive_s,
                "--replace",
                "--force-live",
            ]),
            &[("ERP_PG_BIN", taken.to_str().unwrap())],
        ),
        2,
        &secrets,
        &[
            &format!("could not rename database {copy} to {replaced}"),
            "temporary database",
            "dropped",
            &format!(
                "the existing database {copy} keeps its name and data, and --force-live ended its other sessions"
            ),
        ],
    );
    admin
        .batch_execute(&format!("DROP DATABASE {replaced}"))
        .await
        .unwrap();
    assert!(
        !database_exists(&admin, &staging).await,
        "{staging} was left"
    );
    assert_eq!(current().await, ["current", "kept"]);
    // the second rename (temporary to target) fails: a session holds the temporary database; the
    // target gets its name back, and the temporary database stays while the session holds it
    let held = stub(
        "rename-held-bin",
        &format!(
            "db=''; prev=''\nfor arg in \"$@\"; do [ \"$prev\" = --dbname ] && db=\"$arg\"; prev=\"$arg\"; done\n\
             \"{psql}\" -X -d \"$db\" -c 'SELECT pg_sleep(30)' </dev/null >/dev/null 2>&1 &\nsleep 2"
        ),
    );
    failed_with(
        &erp(
            &os(&[
                "db",
                "restore",
                "--config",
                config,
                "-d",
                &copy,
                "--file",
                archive_s,
                "--replace",
            ]),
            &[("ERP_PG_BIN", held.to_str().unwrap())],
        ),
        2,
        &secrets,
        &[
            &format!("could not rename database {staging} to {copy}"),
            &format!("the temporary database {staging} remains"),
            &format!(
                "the existing database was renamed to {replaced} and back to {copy} with its data unchanged"
            ),
        ],
    );
    let output = clean(
        &erp(
            &os(&[
                "db",
                "drop",
                "--config",
                config,
                "-d",
                &staging,
                "--force-live",
            ]),
            &[],
        ),
        &secrets,
    );
    assert!(!database_exists(&admin, &staging).await, "{output}");
    assert!(!database_exists(&admin, &replaced).await);
    assert_eq!(current().await, ["current", "kept"]);

    // drop: a database without a running node
    ok(
        &erp(&os(&["db", "drop", "--config", config, "-d", &copy]), &[]),
        &secrets,
    );
    assert!(!database_exists(&admin, &copy).await);
    refused(
        &erp(&os(&["db", "drop", "--config", config, "-d", &copy]), &[]),
        &secrets,
        &[&copy, "does not exist"],
    );
    refused(
        &erp(
            &os(&["db", "drop", "--config", config, "-d", "postgres"]),
            &[],
        ),
        &secrets,
        &["postgres", "system database"],
    );

    // a database without the base tables is not neutralized
    let password = dir.path().join("throwaway");
    native_http::private(&password, "fixture-throwaway-password");
    let password_s = password.to_str().unwrap();
    refused(
        &erp(
            &os(&[
                "db",
                "neutralize",
                "--config",
                config,
                "-d",
                &a,
                "--password-file",
                password_s,
            ]),
            &[],
        ),
        &secrets,
        &[&a, "no table ir_cron", "nothing was changed"],
    );

    // a running node: every command refuses by name, --force-live acts
    live_node(&admin).await;
    refused(
        &erp(&os(&["db", "drop", "--config", config, "-d", &a]), &[]),
        &secrets,
        &[&a, "fixture-node", "--force-live"],
    );
    let live_archive = dir.path().join("live.dump");
    let live_archive_s = live_archive.to_str().unwrap();
    refused(
        &erp(
            &os(&[
                "db",
                "dump",
                "--config",
                config,
                "-d",
                &a,
                "--file",
                live_archive_s,
            ]),
            &[],
        ),
        &secrets,
        &[&a, "fixture-node"],
    );
    assert!(!live_archive.exists(), "a refused dump leaves no file");
    ok(
        &erp(
            &os(&[
                "db",
                "dump",
                "--config",
                config,
                "-d",
                &a,
                "--file",
                live_archive_s,
                "--force-live",
            ]),
            // a libpq setting of the caller's environment does not reach pg_dump (this one would
            // make it fail)
            &[("PGSSLMODE", "no-such-mode")],
        ),
        &secrets,
    );
    let restore_live = |extra: &[&str]| {
        let mut line = vec![
            "db",
            "restore",
            "--config",
            config,
            "-d",
            &copy,
            "--file",
            live_archive_s,
        ];
        line.extend_from_slice(extra);
        erp(&os(&line), &[])
    };
    ok(&restore_live(&[]), &secrets);
    refused(
        &restore_live(&["--replace"]),
        &secrets,
        &[&copy, "fixture-node"],
    );
    ok(&restore_live(&["--replace", "--force-live"]), &secrets);
    refused(
        &erp(&os(&["db", "drop", "--config", config, "-d", &copy]), &[]),
        &secrets,
        &[&copy, "fixture-node"],
    );
    ok(
        &erp(
            &os(&[
                "db",
                "drop",
                "--config",
                config,
                "-d",
                &copy,
                "--force-live",
            ]),
            &[],
        ),
        &secrets,
    );
    db.cleanup().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_dump_leaves_no_file() {
    let db = TestDb::create().await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let config = native_http::config(dir.path(), &db, 1, "dblife_unused", &addons_source());
    let config = config.to_str().unwrap();
    let owner_password = String::from_utf8(db.config().get_password().unwrap().to_vec()).unwrap();
    let secrets = [owner_password.as_str()];
    let a = db.name().to_owned();
    // a failed pg_dump leaves no partial file and points at the client programs' directory
    let stub = dir.path().join("failing-bin");
    std::fs::create_dir(&stub).unwrap();
    std::fs::write(
        stub.join("pg_dump"),
        "#!/bin/sh\necho 'fixture pg_dump failure' >&2\nexit 1\n",
    )
    .unwrap();
    std::fs::set_permissions(stub.join("pg_dump"), std::fs::Permissions::from_mode(0o755)).unwrap();
    let failed_archive = dir.path().join("failed.dump");
    failed_with(
        &erp(
            &os(&[
                "db",
                "dump",
                "--config",
                config,
                "-d",
                &a,
                "--file",
                failed_archive.to_str().unwrap(),
            ]),
            &[("ERP_PG_BIN", stub.to_str().unwrap())],
        ),
        2,
        &secrets,
        &["pg_dump failed", "fixture pg_dump failure", "--pg-bin"],
    );
    assert!(!failed_archive.exists(), "a failed dump leaves no file");
    db.cleanup().await.unwrap();
}

async fn scalar_text(client: &Client, sql: &str) -> Option<String> {
    client
        .query_opt(sql, &[])
        .await
        .unwrap()
        .map(|row| row.get(0))
}

async fn count(client: &Client, sql: &str) -> i64 {
    client.query_one(sql, &[]).await.unwrap().get(0)
}

/// without demo data is not run here)
/// checked only as "none active")
/// identity names)
#[test]
fn demo_choice_is_stored_and_neutralize_makes_a_safe_copy() {
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .unwrap()
                .block_on(demo_and_neutralize());
        })
        .unwrap()
        .join()
        .unwrap();
}

async fn demo_and_neutralize() {
    let db = TestDb::create().await.unwrap();
    let role = format!("{}_web", db.name());
    let client = db.client().await.unwrap();
    let temporary = std::env::temp_dir().canonicalize().unwrap();
    let dir = tempfile::tempdir_in(temporary).unwrap();
    let addons = dir.path().join("runtime-addons");
    runtime_addons::stage_runtime_addons(&addons_source(), &addons).unwrap();
    let addons = addons.canonicalize().unwrap();
    let config = native_http::config(dir.path(), &db, 1, &role, &addons);
    let config = config.to_str().unwrap();
    client
        .batch_execute(&format!(
            "CREATE ROLE {} LOGIN PASSWORD 'fixture-runtime-password'; CREATE ROLE {} LOGIN PASSWORD 'fixture-runtime-password' NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION",
            erp_sql::quote(&role),
            erp_sql::quote(&format!("{role}_node"))
        ))
        .await
        .unwrap();
    let admin_password = "fixture-administrator-password";
    let throwaway = "fixture-throwaway-password";
    let owner_password = String::from_utf8(db.config().get_password().unwrap().to_vec()).unwrap();
    let secrets = [owner_password.as_str(), admin_password, throwaway];
    let password_file = dir.path().join("password");
    native_http::private(&password_file, admin_password);
    let password_file = password_file.to_str().unwrap();
    let init = |flag: &str| {
        erp(
            &os(&[
                "db",
                "init",
                "--config",
                config,
                "--admin-login",
                "fixture-admin",
                "--admin-password-file",
                password_file,
                flag,
            ]),
            &[],
        )
    };

    ok(&init("--with-demo"), &secrets);
    let demo = async |module: &str| -> bool {
        client
            .query_one(
                "SELECT demo FROM ir_module_module WHERE name = $1",
                &[&module],
            )
            .await
            .unwrap()
            .get(0)
    };
    assert!(demo("base").await);
    refused(
        &init("--without-demo"),
        &secrets,
        &[db.name(), "already initialized with demo data"],
    );
    let owner_url = native_http::url(&db.config(), None);
    let install = |flags: &[&str]| {
        let mut line = vec![
            "module",
            "install",
            "-d",
            db.name(),
            "--addons-path",
            addons.to_str().unwrap(),
        ];
        line.extend_from_slice(flags);
        erp(&os(&line), &[("DATABASE_URL", owner_url.as_str())])
    };
    ok(&install(&["contacts"]), &secrets);
    assert!(demo("contacts").await);
    let refusal = install(&["--without-demo", "product"]);
    refused(&refusal, &secrets, &[db.name(), "cannot be switched off"]);
    let state = scalar_text(
        &client,
        "SELECT state FROM ir_module_module WHERE name = 'product'",
    )
    .await;
    assert_ne!(state.as_deref(), Some("installed"));
    // an empty demo flag on base is an error by name, not a crash
    client
        .batch_execute("UPDATE ir_module_module SET demo = NULL WHERE name = 'base'")
        .await
        .unwrap();
    let empty = install(&["--without-demo", "product"]);
    let text = clean(&empty, &secrets);
    assert!(!empty.status.success(), "{text}");
    assert!(
        text.contains("base's demo flag is empty or not a boolean"),
        "{text}"
    );
    assert!(!text.contains("panicked"), "{text}");
    client
        .batch_execute("UPDATE ir_module_module SET demo = true WHERE name = 'base'")
        .await
        .unwrap();

    // what a lived-in database has: a session, a login throttle row, the administrator's password
    let user: i64 = client
        .query_one(
            "SELECT id FROM res_users WHERE login = 'fixture-admin'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    client
        .execute(
            "INSERT INTO erp_session (id, user_id, sid_digest, token, token_kid, state, expires_at) \
             VALUES (gen_random_uuid(), $1, sha256('sid'), sha256('token'), 1, 'active', now() + interval '1 hour')",
            &[&user],
        )
        .await
        .unwrap();
    client
        .batch_execute(
            "INSERT INTO erp_login_throttle (kind, key_hash, failures, last_failure_at) \
             VALUES ('login', sha256('key'), 1, now())",
        )
        .await
        .unwrap();
    let secret_before = scalar_text(
        &client,
        "SELECT value FROM ir_config_parameter WHERE key = 'database.secret'",
    )
    .await;
    let ready_before = count(
        &client,
        "SELECT count(*) FROM erp_job WHERE state = 'ready'",
    )
    .await;

    let throwaway_file = dir.path().join("throwaway");
    let throwaway_s = throwaway_file.to_str().unwrap().to_owned();
    std::fs::write(&throwaway_file, throwaway).unwrap();
    std::fs::set_permissions(&throwaway_file, std::fs::Permissions::from_mode(0o644)).unwrap();
    let neutralize_with = |extra: &[&str]| {
        let mut line = vec![
            "db",
            "neutralize",
            "--config",
            config,
            "-d",
            db.name(),
            "--password-file",
            &throwaway_s,
        ];
        line.extend_from_slice(extra);
        erp(&os(&line), &[])
    };
    let neutralize = || neutralize_with(&[]);
    refused(&neutralize(), &secrets, &["--password-file", "mode-600"]);
    std::fs::set_permissions(&throwaway_file, std::fs::Permissions::from_mode(0o600)).unwrap();

    live_node(&client).await;
    refused(&neutralize(), &secrets, &[db.name(), "fixture-node"]);
    assert_eq!(count(&client, "SELECT count(*) FROM erp_session").await, 1);
    client.batch_execute("DELETE FROM erp_node").await.unwrap();

    // from its original: refused by name, and nothing changes
    client
        .batch_execute("DELETE FROM ir_blob_owner_identity")
        .await
        .unwrap();
    let unrecorded = [
        db.name(),
        "no recorded identity",
        "--unrecorded-identity",
        "nothing was changed",
    ];
    refused(&neutralize(), &secrets, &unrecorded);
    client
        .batch_execute("ALTER TABLE ir_blob_owner_identity RENAME TO ir_blob_owner_identity_away")
        .await
        .unwrap();
    refused(&neutralize(), &secrets, &unrecorded);
    client
        .batch_execute("ALTER TABLE ir_blob_owner_identity_away RENAME TO ir_blob_owner_identity")
        .await
        .unwrap();
    assert_eq!(count(&client, "SELECT count(*) FROM erp_session").await, 1);

    // not a copy; nothing changes
    client
        .batch_execute(
            "INSERT INTO ir_blob_owner_identity (singleton, system_identifier, database_oid, incarnation) \
             SELECT true, p.system_identifier::text::numeric, d.oid::int8, gen_random_uuid() \
             FROM pg_control_system() p JOIN pg_database d ON d.datname = current_database() \
             ON CONFLICT (singleton) DO UPDATE SET system_identifier = EXCLUDED.system_identifier, \
             database_oid = EXCLUDED.database_oid",
        )
        .await
        .unwrap();
    refused(
        &neutralize(),
        &secrets,
        &[
            db.name(),
            "not a copy",
            "--physical-copy",
            "nothing was changed",
        ],
    );
    assert_eq!(count(&client, "SELECT count(*) FROM erp_session").await, 1);

    for run in 1..=2 {
        if run == 2 {
            client
                .batch_execute(
                    "UPDATE ir_blob_owner_identity SET database_oid = database_oid % 4294967295 + 1",
                )
                .await
                .unwrap();
        }
        let flags: &[&str] = if run == 1 { &["--physical-copy"] } else { &[] };
        ok(&neutralize_with(flags), &secrets);
        for table in ["erp_session", "res_users_apikeys", "erp_login_throttle"] {
            assert_eq!(
                count(&client, &format!("SELECT count(*) FROM {table}")).await,
                0,
                "run {run}: {table}"
            );
        }
        assert_eq!(
            count(&client, "SELECT count(*) FROM ir_cron WHERE active").await,
            0
        );
        let running = client
            .query(
                "SELECT kind FROM erp_job WHERE state IN ('ready', 'running') AND kind <> ALL($1::text[])",
                &[&KEPT_KINDS.as_slice()],
            )
            .await
            .unwrap();
        assert!(running.is_empty(), "run {run}: jobs still due");
        assert!(
            ready_before == 0
                || count(&client, "SELECT count(*) FROM erp_job WHERE state = 'held'").await > 0,
            "run {run}: the due jobs were not held"
        );
        let hashes = client
            .query(
                "SELECT DISTINCT password FROM res_users WHERE password IS NOT NULL",
                &[],
            )
            .await
            .unwrap();
        assert_eq!(hashes.len(), 1, "run {run}: one throwaway hash");
        let stored = erp_security::auth::password::StoredHash::parse(hashes[0].get(0)).unwrap();
        assert!(stored.verify(throwaway.as_bytes()).unwrap());
        assert!(!stored.verify(admin_password.as_bytes()).unwrap());
        let secret = scalar_text(
            &client,
            "SELECT value FROM ir_config_parameter WHERE key = 'database.secret'",
        )
        .await;
        assert!(
            secret.is_some() && secret != secret_before,
            "run {run}: secret replaced"
        );
        assert_eq!(
            scalar_text(
                &client,
                "SELECT value FROM ir_config_parameter WHERE key = 'database.neutralized'"
            )
            .await
            .as_deref(),
            Some("true")
        );
    }
    // the check was skipped
    client
        .batch_execute("DELETE FROM ir_blob_owner_identity")
        .await
        .unwrap();
    let text = ok(&neutralize_with(&["--unrecorded-identity"]), &secrets);
    assert!(
        text.contains("has no recorded identity; the check that it is a copy was skipped"),
        "{text}"
    );
    assert!(text.contains("neutralized"), "{text}");
    drop(client);
    db.cleanup().await.unwrap();
}

fn python311() -> PathBuf {
    let mut candidates: Vec<PathBuf> = [
        "python3",
        "python3.14",
        "python3.13",
        "python3.12",
        "python3.11",
    ]
    .into_iter()
    .map(PathBuf::from)
    .collect();
    if let Some(home) = std::env::var_os("HOME") {
        let uv = Path::new(&home).join(".local/share/uv/python");
        if let Ok(entries) = std::fs::read_dir(uv) {
            candidates.extend(
                entries
                    .flatten()
                    .map(|entry| entry.path().join("bin/python3")),
            );
        }
    }
    candidates
        .into_iter()
        .find(|python| {
            Command::new(python)
                .args([
                    "-I",
                    "-c",
                    "import sys; raise SystemExit(sys.version_info < (3, 11))",
                ])
                .output()
                .is_ok_and(|output| output.status.success())
        })
        .expect("no Python 3.11+ on this host for the upgrade gate script")
}

/// The upgrade gate (`scripts/upgrade-snapshot-gate.py`, design TOOL.DB-LIFECYCLE §3) on real
/// snapshots made with `erp db dump | restore | neutralize`: a neutralized snapshot is restored,
/// upgraded twice with the previous release's identity and passes; a snapshot whose upgrade the
/// candidate refuses fails with the upgrade's own message, printed once with the modules and no
/// credential; a dump without the neutralize marker is refused before any upgrade. Tooling only:
/// no product behaviour, no spec case.
#[test]
fn upgrade_gate_passes_a_neutralized_snapshot_and_refuses_a_failing_one() {
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .unwrap()
                .block_on(upgrade_gate());
        })
        .unwrap()
        .join()
        .unwrap();
}

async fn upgrade_gate() {
    let db = TestDb::create().await.unwrap();
    let role = format!("{}_web", db.name());
    let client = db.client().await.unwrap();
    let dir = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
    let staged = dir.path().join("runtime-addons");
    runtime_addons::stage_runtime_addons(&addons_source(), &staged).unwrap();
    let config = native_http::config(dir.path(), &db, 1, &role, &staged.canonicalize().unwrap());
    let config = config.to_str().unwrap();
    client
        .batch_execute(&format!(
            "CREATE ROLE {} LOGIN PASSWORD 'fixture-runtime-password'; CREATE ROLE {} LOGIN PASSWORD 'fixture-runtime-password' NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION",
            erp_sql::quote(&role),
            erp_sql::quote(&format!("{role}_node"))
        ))
        .await
        .unwrap();
    let owner_password = String::from_utf8(db.config().get_password().unwrap().to_vec()).unwrap();
    let secrets = [owner_password.as_str()];
    let admin = dir.path().join("admin");
    native_http::private(&admin, "fixture-administrator-password");
    let throwaway = dir.path().join("throwaway");
    native_http::private(&throwaway, "fixture-throwaway-password");
    let path = |name: &str| dir.path().join(name).to_str().unwrap().to_owned();
    let (admin, throwaway) = (path("admin"), throwaway.to_str().unwrap().to_owned());
    ok(
        &erp(
            &os(&[
                "db",
                "init",
                "--config",
                config,
                "--admin-login",
                "fixture-admin",
                "--admin-password-file",
                &admin,
            ]),
            &[],
        ),
        &secrets,
    );
    // what the dev/testing host does before a snapshot leaves it: a copy, neutralized, dumped
    let copy = format!("{}_g", db.name());
    let dump = |database: &str, file: &str| {
        ok(
            &erp(
                &os(&[
                    "db", "dump", "--config", config, "-d", database, "--file", file,
                ]),
                &[],
            ),
            &secrets,
        );
    };
    let original = path("original.dump");
    dump(db.name(), &original);
    ok(
        &erp(
            &os(&[
                "db", "restore", "--config", config, "-d", &copy, "--file", &original,
            ]),
            &[],
        ),
        &secrets,
    );
    ok(
        &erp(
            &os(&[
                "db",
                "neutralize",
                "--config",
                config,
                "-d",
                &copy,
                "--password-file",
                &throwaway,
                // a copy restored just above: `erp db init` records no identity
                "--unrecorded-identity",
            ]),
            &[],
        ),
        &secrets,
    );
    let neutralized = path("neutralized.dump");
    dump(&copy, &neutralized);

    let snapshots = dir.path().join("snapshots");
    let gate_script = addons_source()
        .parent()
        .unwrap()
        .join("scripts/upgrade-snapshot-gate.py");
    let digest = erp_modules::package::sha256_hex(db.name().as_bytes());
    let (release, revision) = (&digest[..40], &digest[24..64]);
    let owner_url = native_http::url(&db.config(), None);
    let python = python311();
    let gate = |args: &[&str]| {
        Command::new(&python)
            .arg("-I")
            .arg(&gate_script)
            .args(args)
            .env("ERP_SNAPSHOT_DIR", &snapshots)
            .env("ERP_PG_BIN", pg_bin())
            .env("DATABASE_URL", &owner_url)
            .output()
            .unwrap()
    };
    let addons = addons_source();
    let addons = addons.to_str().unwrap();
    let run = || {
        gate(&[
            "run",
            "--erp",
            env!("CARGO_BIN_EXE_erp"),
            "--addons",
            addons,
            "--previous-addons",
            addons,
            "--revision",
            revision,
        ])
    };
    let store = |file: &str| {
        ok(
            &gate(&["store", "--dump", file, "--release", release]),
            &secrets,
        );
        // a snapshot's directory is named by the second it was stored in
        std::thread::sleep(std::time::Duration::from_millis(1100));
    };

    store(&neutralized);
    let passed = ok(&run(), &secrets);
    assert!(
        passed.contains("\"status\": \"PASS\"") && passed.contains("\"upgrades\": 2"),
        "{passed}"
    );

    // a snapshot the candidate's upgrade refuses: the gate fails with the upgrade's own message
    let refusing = connect(&db, &copy).await;
    refusing
        .batch_execute(
            "CREATE FUNCTION gate_fixture_refuse() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN \
             RAISE EXCEPTION 'fixture: this snapshot refuses the upgrade'; END $$; \
             CREATE TRIGGER gate_fixture_refuse BEFORE UPDATE ON ir_module_module \
             FOR EACH ROW EXECUTE FUNCTION gate_fixture_refuse()",
        )
        .await
        .unwrap();
    drop(refusing);
    let failing = path("failing.dump");
    dump(&copy, &failing);
    store(&failing);
    let output = run();
    failed_with(
        &output,
        1,
        &secrets,
        &[
            "the upgrade of snapshot",
            "failed with exit",
            "Modules: base, web",
        ],
    );
    let text = String::from_utf8_lossy(&output.stderr);
    // the upgrade's standard error (whose one line names the message twice) is printed once
    for once in [
        "upgrade-snapshot-gate: REFUSED",
        "Error: fixture: this snapshot refuses the upgrade",
    ] {
        assert_eq!(text.matches(once).count(), 1, "{once}: {text}");
    }

    // a dump that was never neutralized is refused before any upgrade
    store(&original);
    failed_with(
        &run(),
        1,
        &secrets,
        &["is not neutralized (database.neutralized is absent)"],
    );

    let gate_database = format!("erpforall_erpcheck_{}", &revision[..8]);
    assert!(
        !database_exists(&client, &gate_database).await,
        "the gate left {gate_database}"
    );
    ok(
        &erp(&os(&["db", "drop", "--config", config, "-d", &copy]), &[]),
        &secrets,
    );
    drop(client);
    db.cleanup().await.unwrap();
}
