//! Live `erp module install | upgrade | uninstall` through the `erp` binary against a throwaway
//! database (credentials in `DATABASE_URL`, never on the command line), and the exit codes of
//! module-system errors.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::process::{Command, ExitCode, Output};

use erp_cli::{CliError, node::Node};
use erp_modules::error::ModuleStep;
use erp_modules::{Error, InitOptions, ModuleName};
use erp_testkit::TestDb;
use erp_testkit::tokio_postgres::config::Host;
use tempfile::TempDir;

/// A byte-for-byte source copy keeps this fixture's base identity bound to the native binary.
fn copy_dir(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let kind = entry.file_type().unwrap();
        assert!(
            !kind.is_symlink(),
            "fixture source must not contain links: {}",
            entry.path().display()
        );
        let target = to.join(entry.file_name());
        if kind.is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            assert!(
                kind.is_file(),
                "fixture source must contain regular files: {}",
                entry.path().display()
            );
            fs::copy(entry.path(), target).unwrap();
        }
    }
}

/// Addons in a folder: the real linked `base`, `live_demo` (base) and `live_child` (live_demo).
fn addons() -> TempDir {
    // Resolve the trusted temporary search prefix once; addon roots themselves remain real
    // directories, so this fixture does not rely on following `/var` or addon symlinks.
    let temporary = std::env::temp_dir().canonicalize().unwrap();
    let root = tempfile::tempdir_in(temporary).unwrap();
    let workspace_base = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("addons/base");
    copy_dir(&workspace_base, &root.path().join("base"));
    for (name, manifest) in [
        ("live_demo", "name = 'Live Demo'\ndepends = ['base']\n"),
        (
            "live_child",
            "name = 'Live Child'\ndepends = ['live_demo']\n",
        ),
    ] {
        let dir = root.path().join(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("manifest.toml"), manifest).unwrap();
    }
    root
}

/// A database initialized with the same linked native composition as the CLI binary, base only.
async fn initialized(addons: &TempDir) -> TestDb {
    let db = TestDb::create().await.unwrap();
    let node = Node::open(db.pool().clone(), &[addons.path().to_path_buf()], None).unwrap();
    let outcome = node
        .system()
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    assert_eq!(outcome.installed, [ModuleName::new("base").unwrap()]);
    db
}

/// A key=value connection string for the database's cluster; the database is `postgres` because
/// `-d` decides.
fn conninfo(db: &TestDb) -> String {
    let config = db.config();
    let quote = |v: &str| format!("'{}'", v.replace('\\', "\\\\").replace('\'', "\\'"));
    let mut parts = Vec::new();
    for host in config.get_hosts() {
        match host {
            Host::Tcp(name) => parts.push(format!("host={}", quote(name))),
            Host::Unix(path) => parts.push(format!("host={}", quote(&path.to_string_lossy()))),
        }
    }
    for port in config.get_ports() {
        parts.push(format!("port={port}"));
    }
    if let Some(user) = config.get_user() {
        parts.push(format!("user={}", quote(user)));
    }
    if let Some(password) = config.get_password() {
        parts.push(format!(
            "password={}",
            quote(std::str::from_utf8(password).unwrap())
        ));
    }
    parts.push("dbname='postgres'".to_owned());
    parts.join(" ")
}

/// `erp module <action> -d <db> <modules> --addons-path <addons>` with the credentials in
/// `DATABASE_URL`.
fn erp(db: &TestDb, addons: &TempDir, action: &str, modules: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_erp"))
        .args(["module", action, "-d", db.name()])
        .args(modules)
        .arg("--addons-path")
        .arg(addons.path())
        .env("DATABASE_URL", conninfo(db))
        .output()
        .unwrap()
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8(bytes.to_vec()).unwrap()
}

async fn states(db: &TestDb) -> Vec<(String, String)> {
    db.client()
        .await
        .unwrap()
        .query(
            "SELECT name, state FROM ir_module_module WHERE name LIKE 'live_%' ORDER BY name",
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|row| (row.get(0), row.get(1)))
        .collect()
}

fn pairs(list: &[(&str, &str)]) -> Vec<(String, String)> {
    list.iter()
        .map(|(a, b)| ((*a).to_owned(), (*b).to_owned()))
        .collect()
}

/// An install brings the dependencies of the requested module, an upgrade keeps the states, and
/// an uninstall cascades to the installed dependents; each prints what it did.
#[tokio::test]
async fn install_upgrade_and_uninstall_run_through_the_module_system() {
    let addons = addons();
    let db = initialized(&addons).await;
    assert_eq!(
        states(&db).await,
        pairs(&[("live_child", "uninstalled"), ("live_demo", "uninstalled")]),
        "initialize lists the addons of the folder"
    );

    let out = erp(&db, &addons, "install", &["live_child"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(
        text(&out.stdout),
        "Install completed:\n  installed: live_demo, live_child\n  post-commit steps run: 0\n"
    );
    assert_eq!(
        states(&db).await,
        pairs(&[("live_child", "installed"), ("live_demo", "installed")])
    );

    let out = erp(&db, &addons, "upgrade", &["live_demo"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let stdout = text(&out.stdout);
    assert!(
        stdout.starts_with("Upgrade completed:\n  upgraded: "),
        "{stdout}"
    );
    let upgraded: BTreeSet<&str> = stdout
        .lines()
        .find_map(|l| l.strip_prefix("  upgraded: "))
        .unwrap()
        .split(", ")
        .collect();
    assert_eq!(upgraded, BTreeSet::from(["live_child", "live_demo"]));
    assert_eq!(
        states(&db).await,
        pairs(&[("live_child", "installed"), ("live_demo", "installed")])
    );

    // the dependent `live_child` is installed, so it goes with `live_demo`
    let out = erp(&db, &addons, "uninstall", &["live_demo"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let stdout = text(&out.stdout);
    assert!(
        stdout.starts_with("Uninstall completed:\n  uninstalled: "),
        "{stdout}"
    );
    let removed: BTreeSet<&str> = stdout
        .lines()
        .find_map(|l| l.strip_prefix("  uninstalled: "))
        .unwrap()
        .split(", ")
        .collect();
    assert_eq!(removed, BTreeSet::from(["live_child", "live_demo"]));
    assert_eq!(
        states(&db).await,
        pairs(&[("live_child", "uninstalled"), ("live_demo", "uninstalled")])
    );
    db.cleanup().await.unwrap();
}

/// `--dry-run` resolves the plan without a connection: no `DATABASE_URL` needed.
#[tokio::test]
async fn dry_run_needs_no_connection() {
    let addons = addons();
    let out = Command::new(env!("CARGO_BIN_EXE_erp"))
        .args([
            "module",
            "install",
            "-d",
            "nowhere",
            "live_child",
            "--dry-run",
        ])
        .arg("--addons-path")
        .arg(addons.path())
        .env_remove("DATABASE_URL")
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(
        text(&out.stdout),
        "Install plan resolved successfully (3 modules):\n  1. base\n  2. live_demo\n  3. live_child\n"
    );
}

/// Refusals of the module system are exit code 1 with its message; nothing changes.
#[tokio::test]
async fn refusals_exit_with_code_one_and_the_module_systems_message() {
    let addons = addons();
    let db = initialized(&addons).await;

    // upgrading what is not installed
    let out = erp(&db, &addons, "upgrade", &["live_demo"]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        text(&out.stderr),
        "Error: Cannot upgrade module \u{201c}live_demo\u{201d}. It is not installed.\n"
    );
    assert!(out.stdout.is_empty());

    // a module no addon folder provides: refused before any connection
    let out = erp(&db, &addons, "install", &["ghost"]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        text(&out.stderr),
        "Error: Module 'ghost' not found in addon search paths\n"
    );

    // the server-wide modules cannot be uninstalled
    let out = erp(&db, &addons, "uninstall", &["base"]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        text(&out.stderr),
        "Error: Those modules cannot be uninstalled: base\n"
    );
    assert_eq!(
        states(&db).await,
        pairs(&[("live_child", "uninstalled"), ("live_demo", "uninstalled")])
    );
    db.cleanup().await.unwrap();
}

/// A database the module system never initialized is refused, not initialized behind the
/// operator's back.
#[tokio::test]
async fn uninitialized_database_is_refused() {
    let addons = addons();
    let db = TestDb::create().await.unwrap();
    let out = erp(&db, &addons, "install", &["live_demo"]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        text(&out.stderr),
        format!(
            "Error: Database {} not initialized: initialize it with ModuleSystem::initialize (create_database makes the empty database); `erp module` never initializes one\n",
            db.name()
        )
    );
    db.cleanup().await.unwrap();
}

/// The credentials come from the environment only: without `DATABASE_URL` a live run is an
/// input error naming the variable, never the connection string.
#[tokio::test]
async fn live_run_needs_database_url() {
    let addons = addons();
    let out = Command::new(env!("CARGO_BIN_EXE_erp"))
        .args(["module", "install", "-d", "nowhere", "live_demo"])
        .arg("--addons-path")
        .arg(addons.path())
        .env_remove("DATABASE_URL")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        text(&out.stderr),
        "Error: Invalid argument 'DATABASE_URL': not set: the connection credentials come from the environment\n"
    );
}

/// Exit codes tell a refusal (1) from a busy database (3), a rolled-back step (4) and a
/// post-commit phase that did not finish (5); the message carries the causes.
#[test]
fn module_system_errors_map_to_distinct_exit_codes() {
    let module = ModuleName::new("live_demo").unwrap();
    let cases: Vec<(Error, u8)> = vec![
        (
            Error::UnknownModule {
                names: vec![module.clone()],
            },
            1,
        ),
        (Error::OperationInProgress, 3),
        (
            Error::StepFailed {
                module: module.clone(),
                step: ModuleStep::Schema,
                file: None,
                source: Box::new(Error::InvariantViolation {
                    message: "boom".to_owned(),
                }),
            },
            4,
        ),
        (
            Error::InvariantViolation {
                message: "boom".to_owned(),
            },
            2,
        ),
    ];
    for (error, code) in cases {
        let cli = CliError::ModuleOperation(Box::new(error));
        assert_eq!(cli.exit_code(), ExitCode::from(code), "{cli}");
    }
    let step = CliError::ModuleOperation(Box::new(Error::StepFailed {
        module,
        step: ModuleStep::Schema,
        file: None,
        source: Box::new(Error::InvariantViolation {
            message: "boom".to_owned(),
        }),
    }));
    assert_eq!(
        step.to_string(),
        "Module \"live_demo\": schema update failed: Internal invariant broken: boom"
    );

    let post = CliError::PostCommitIncomplete {
        failed: 2,
        stopped: Some("no connection".to_owned()),
    };
    assert_eq!(post.exit_code(), ExitCode::from(5));
    assert_eq!(
        post.to_string(),
        "The operation committed, but its post-commit phase did not finish cleanly: 2 step(s) failed; the phase stopped early: no connection"
    );
}
