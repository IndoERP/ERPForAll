//! Fixtures shared by the database tests of module operations.
//!
//! The recording statics (`LOG`) are per test binary and shared by its tests: the suite runs under
//! nextest (`scripts/ci.sh`), which gives every test its own process. Under plain `cargo test`
//! the tests of one binary share a process and the logs would interleave.
#![allow(dead_code, reason = "each test binary uses a subset")]
#![allow(clippy::panic, clippy::unwrap_used, reason = "test fixtures")]

use std::fs;
use std::path::Path;
use std::sync::Mutex;

use erp_modules::AddonIndex;
use erp_modules::error::{Error, ModuleStep};
use erp_modules::loader::{DataFile, FileLoader, Operations};
use erp_modules::manifest::{Manifest, ModuleName};
use erp_modules::system::{ModuleSystem, ModuleSystemBuilder, OperationOutcome};
use erp_orm::{BoxFuture, Context, Env, Orm, SUPERUSER_ID, Transaction};
use erp_registry::{FieldDecl, ModelDecl};
use erp_testkit::TestDb;
use tempfile::TempDir;

// ---- fixtures ------------------------------------------------------------------------------

/// Addons in a dev folder, `base` included.
pub struct Addons {
    root: TempDir,
}

impl Addons {
    pub fn new() -> Self {
        let addons = Self {
            // macOS exposes its temporary directory through /var; resolve that trusted
            // host prefix before creating fixtures so error paths and no-link admission agree.
            root: tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap(),
        };
        addons.add(
            "base",
            "name = 'Base'\nauto_install = true\ncategory = 'Hidden'\n",
        );
        addons
    }

    /// Replace the minimal `base` with the test kit's stand-in, for fixtures that link base's code.
    pub fn stand_in_base(&self) -> &Self {
        erp_testkit::stand_in_base::write_stand_in_base(self.root.path()).unwrap();
        self
    }

    pub fn add(&self, name: &str, manifest: &str) -> &Self {
        let dir = self.root.path().join(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("manifest.toml"), manifest).unwrap();
        self
    }

    pub fn file(&self, module: &str, path: &str, content: &str) -> &Self {
        let full = self.root.path().join(module).join(path);
        fs::create_dir_all(full.parent().unwrap()).unwrap();
        fs::write(full, content).unwrap();
        self
    }

    pub fn index(&self) -> AddonIndex {
        let mut index = AddonIndex::new(true);
        index.add_dev_path(self.root.path().canonicalize().unwrap());
        index
    }

    pub fn manifest(&self, name: &str) -> Manifest {
        let text = fs::read_to_string(self.root.path().join(name).join("manifest.toml")).unwrap();
        Manifest::from_toml(name, &text).unwrap()
    }

    pub fn path(&self) -> &Path {
        self.root.path()
    }
}

/// The five-module fixture of the spec: a (base), b (a), c (a), d (b, c), e (b, d).
pub fn spec_fixture(addons: &Addons) {
    addons
        .add("a", "name = 'A'\ndepends = ['base']\n")
        .add("b", "name = 'B'\ndepends = ['a']\n")
        .add("c", "name = 'C'\ndepends = ['a']\n")
        .add("d", "name = 'D'\ndepends = ['b', 'c']\n")
        .add("e", "name = 'E'\ndepends = ['b', 'd']\n");
}

/// A post-commit key.
pub fn pk(key: &str) -> erp_modules::PostCommitKey {
    erp_modules::PostCommitKey::parse(key).unwrap()
}

pub fn n(name: &str) -> ModuleName {
    ModuleName::new(name).unwrap()
}

pub fn builder(db: &TestDb, addons: &Addons) -> ModuleSystemBuilder {
    let mut identities = std::collections::BTreeMap::new();
    for entry in fs::read_dir(addons.path()).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name().into_string().unwrap();
        // A deliberately invalid filesystem fixture remains invalid at production admission;
        // only readable test artifacts receive trusted native provenance from this test root.
        match erp_modules::artifact_identity::ArtifactSnapshot::capture(
            &name,
            &entry.path().canonicalize().unwrap(),
        ) {
            Ok(snapshot) => {
                identities.insert(n(&name), snapshot.digest().to_owned());
            }
            Err(error) => {
                eprintln!("fixture cannot attest native source: {error}");
            }
        }
    }
    ModuleSystem::builder(db.pool().clone(), addons.index())
        .linked_hooks(false)
        .native_artifact_identities(identities)
}

/// Explicit operator recovery for a synthetic fixture whose source deliberately changes.
/// Never called by the production lifecycle; tests of receipt refusal do not use this helper.
pub async fn enroll_fixture_transition(system: &ModuleSystem, names: &[&str]) {
    let names: Vec<_> = names.iter().map(|name| n(name)).collect();
    system.identity_enroll(&names, "synthetic-fixture-operator", "Explicitly reviewed synthetic fixture source transition; identity behavior is tested separately").await.unwrap();
}

/// A model `name` declared by `addon` with a `name` char field (leaked: registries hold
/// `'static` declarations).
pub fn model(addon: &'static str, name: &'static str) -> &'static ModelDecl {
    let fields: &'static [FieldDecl] = Box::leak(vec![FieldDecl::char("name")].into_boxed_slice());
    Box::leak(Box::new(
        ModelDecl::new(addon, name)
            .description("Test model")
            .fields(fields)
            .at(file!(), line!()),
    ))
}

/// An extension of `name` by `addon` adding the char field `field`.
pub fn extension(
    addon: &'static str,
    name: &'static str,
    field: &'static str,
) -> &'static ModelDecl {
    let fields: &'static [FieldDecl] = Box::leak(vec![FieldDecl::char(field)].into_boxed_slice());
    let inherit: &'static [&'static str] = Box::leak(vec![name].into_boxed_slice());
    Box::leak(Box::new(
        ModelDecl::extension(addon, name, inherit)
            .fields(fields)
            .at(file!(), line!()),
    ))
}

pub fn su(orm: &Orm) -> Env {
    orm.env(SUPERUSER_ID, Context::new())
}

pub async fn rows(orm: &Orm, sql: &str) -> Vec<Vec<Option<String>>> {
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let result = tx.query(sql, &[]).await.unwrap();
    tx.rollback().await.unwrap();
    result
        .iter()
        .map(|r| {
            (0..r.len())
                .map(|i| r.get::<_, Option<String>>(i))
                .collect()
        })
        .collect()
}

pub async fn states(orm: &Orm) -> Vec<(String, String)> {
    rows(
        orm,
        "SELECT name, state FROM ir_module_module ORDER BY name",
    )
    .await
    .into_iter()
    .map(|r| (r[0].clone().unwrap(), r[1].clone().unwrap()))
    .collect()
}

pub async fn state(orm: &Orm, module: &str) -> String {
    rows(
        orm,
        &format!("SELECT state FROM ir_module_module WHERE name = '{module}'"),
    )
    .await[0][0]
        .clone()
        .unwrap()
}

pub async fn table_exists(orm: &Orm, table: &str) -> bool {
    rows(
        orm,
        &format!("SELECT (to_regclass('{table}') IS NOT NULL)::text"),
    )
    .await[0][0]
        .as_deref()
        == Some("true")
}

/// An operation that committed with a clean post-commit phase.
#[track_caller]
pub fn committed(outcome: OperationOutcome) {
    assert!(outcome.post_commit.failed.is_empty(), "{outcome:?}");
    assert!(outcome.post_commit.stopped.is_none(), "{outcome:?}");
}

// ---- recording ---------------------------------------------------------------------------

pub static LOG: Mutex<Vec<String>> = Mutex::new(Vec::new());

pub fn log(entry: impl Into<String>) {
    LOG.lock().unwrap().push(entry.into());
}

pub fn take_log() -> Vec<String> {
    std::mem::take(&mut *LOG.lock().unwrap())
}

/// A data loader recording files and operation counts; parse errors abort.
pub struct RecordingLoader;

impl FileLoader for RecordingLoader {
    fn purge<'a>(
        &'a self,
        _tx: &'a mut Transaction,
        modules: &'a [ModuleName],
    ) -> BoxFuture<'a, erp_modules::Result<Vec<erp_data::load::DataNotice>>> {
        Box::pin(async move {
            let names: Vec<&str> = modules.iter().map(ModuleName::as_str).collect();
            log(format!("purge {}", names.join(",")));
            Ok(Vec::new())
        })
    }

    fn load<'a, 'o: 'a>(
        &'a self,
        _tx: &'a mut Transaction,
        file: &'a DataFile,
        operations: &'a mut Operations<'o>,
    ) -> BoxFuture<'a, erp_modules::Result<()>> {
        Box::pin(async move {
            let mut count = 0;
            for op in operations {
                op?;
                count += 1;
            }
            log(format!(
                "{}:{}:{}:{}:{count}",
                file.kind().as_str(),
                file.module(),
                file.path(),
                file.mode().as_str()
            ));
            Ok(())
        })
    }
}

pub const ONE_RECORD_XML: &str =
    "<odoo><record id=\"r1\" model=\"test.x\"><field name=\"name\">one</field></record></odoo>";

/// Run one batch of SQL in its own committed transaction.
pub async fn exec(orm: &Orm, sql: &str) {
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    tx.batch(sql).await.unwrap();
    tx.commit().await.unwrap();
}

/// The messages of an error and of each of its sources, outermost first.
pub fn chain(err: &(dyn std::error::Error + 'static)) -> Vec<String> {
    let mut out = vec![err.to_string()];
    let mut next = err.source();
    while let Some(source) = next {
        out.push(source.to_string());
        next = source.source();
    }
    out
}

/// Whether advisory lock `key` (two-int4 form) is held by any session of the database.
pub async fn advisory_held(orm: &Orm, key: (i32, i32)) -> bool {
    rows(
        orm,
        &format!(
            "SELECT (count(*) > 0)::text FROM pg_locks WHERE locktype = 'advisory' \
             AND granted AND classid::bigint = {} AND objid::bigint = {} AND objsubid = 2 \
             AND database = (SELECT oid FROM pg_database WHERE datname = current_database())",
            key.0, key.1
        ),
    )
    .await[0][0]
        .as_deref()
        == Some("true")
}

/// The advisory lock the gated post-commit step waits for (see [`close_gate`]).
pub const GATE: (i32, i32) = (7_001, 1);

/// The table the gated post-commit step analyzes (see [`close_step_gate`]).
pub const STEP_GATE_TABLE: &str = "erp_test_step_gate";

/// A post-commit step that blocks until the step gate is opened: `ANALYZE` of
/// [`STEP_GATE_TABLE`], which waits for the `ACCESS EXCLUSIVE` lock the test holds while the gate
/// is closed. Tests that need a step to be "running" synchronize on this gate instead of on how
/// long a statement takes.
pub fn gate_action() -> erp_modules::PostCommitAction {
    analyze(STEP_GATE_TABLE)
}

/// `ANALYZE <table>` as a post-commit action.
pub fn analyze(table: &str) -> erp_modules::PostCommitAction {
    erp_modules::PostCommitAction::analyze(erp_modules::TableName::parse(table).unwrap())
}

/// Close the step gate: create [`STEP_GATE_TABLE`] (committed) and hold an `ACCESS EXCLUSIVE`
/// lock on it in a transaction until [`open_gate`].
pub async fn close_step_gate(orm: &Orm) -> Transaction {
    let mut create = orm.begin(SUPERUSER_ID).await.unwrap();
    create
        .batch(&format!(
            "CREATE TABLE IF NOT EXISTS {STEP_GATE_TABLE} (id int)"
        ))
        .await
        .unwrap();
    create.commit().await.unwrap();
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    tx.batch(&format!(
        "LOCK TABLE {STEP_GATE_TABLE} IN ACCESS EXCLUSIVE MODE"
    ))
    .await
    .unwrap();
    tx
}

/// Whether a session of the database waits for a lock on [`STEP_GATE_TABLE`].
pub async fn step_gate_waiting(orm: &Orm) -> bool {
    rows(
        orm,
        &format!(
            "SELECT (count(*) > 0)::text FROM pg_locks WHERE locktype = 'relation' \
             AND NOT granted AND relation = '{STEP_GATE_TABLE}'::regclass \
             AND database = (SELECT oid FROM pg_database WHERE datname = current_database())"
        ),
    )
    .await[0][0]
        .as_deref()
        == Some("true")
}

/// Close the gate: hold [`GATE`] in a transaction until [`open_gate`].
pub async fn close_gate(orm: &Orm) -> Transaction {
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    tx.query("SELECT pg_advisory_xact_lock($1, $2)", &[&GATE.0, &GATE.1])
        .await
        .unwrap();
    tx
}

/// Open the gate: release the lock the steps wait for.
pub async fn open_gate(mut gate: Transaction) {
    gate.rollback().await.unwrap();
}

/// Whether a session of the database waits for advisory lock `key` (two-int4 form).
pub async fn advisory_waiting(orm: &Orm, key: (i32, i32)) -> bool {
    rows(
        orm,
        &format!(
            "SELECT (count(*) > 0)::text FROM pg_locks WHERE locktype = 'advisory' \
             AND NOT granted AND classid::bigint = {} AND objid::bigint = {} AND objsubid = 2 \
             AND database = (SELECT oid FROM pg_database WHERE datname = current_database())",
            key.0, key.1
        ),
    )
    .await[0][0]
        .as_deref()
        == Some("true")
}

/// Poll `probe` every 20 ms until it holds, at most `limit`; whether it held. For observing state
/// that only the database can report (a session waiting on a lock, a lock released): the test
/// itself never depends on how long an operation takes, only on a condition becoming true.
pub async fn eventually<F, Fut>(limit: std::time::Duration, mut probe: F) -> bool
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let start = std::time::Instant::now();
    loop {
        if probe().await {
            return true;
        }
        if start.elapsed() > limit {
            return false;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

/// The parts of an [`Error::StepFailed`]: module, step, file and the step's own error.
#[track_caller]
pub fn step_failed(err: &Error) -> (&str, ModuleStep, Option<&str>, &Error) {
    match err {
        Error::StepFailed {
            module,
            step,
            file,
            source,
        } => (module.as_str(), *step, file.as_deref(), source),
        other => panic!("expected StepFailed, got {other:?}"),
    }
}

/// The whole database as text, for "nothing changed" checks: every table of the public schema
/// with its rows (as JSON, ordered), the columns of every table, and every relation (tables,
/// views, indexes, sequences) by name.
pub async fn snapshot(orm: &Orm) -> Vec<(String, String)> {
    let tables: Vec<String> = rows(
        orm,
        "SELECT tablename::text FROM pg_tables WHERE schemaname = 'public' ORDER BY 1",
    )
    .await
    .into_iter()
    .map(|r| r[0].clone().unwrap())
    .collect();
    let mut out = Vec::new();
    for table in tables {
        let content = rows(
            orm,
            &format!(
                "SELECT coalesce(json_agg(t ORDER BY t::text)::text, '[]') FROM \"{table}\" t"
            ),
        )
        .await;
        out.push((table, content[0][0].clone().unwrap()));
    }
    for (label, sql) in [
        (
            "columns",
            "SELECT coalesce(json_agg(c ORDER BY c.t, c.n)::text, '[]') FROM \
             (SELECT table_name::text AS t, column_name::text AS n, data_type::text AS ty, \
             is_nullable::text AS nullable FROM information_schema.columns \
             WHERE table_schema = 'public') c",
        ),
        (
            "relations",
            "SELECT coalesce(json_agg(relname::text || ':' || relkind::text ORDER BY relname)::text, '[]') \
             FROM pg_class WHERE relnamespace = 'public'::regnamespace",
        ),
    ] {
        let content = rows(orm, sql).await;
        out.push((label.to_owned(), content[0][0].clone().unwrap()));
    }
    out
}

/// The models of a registry with their field names, sorted.
pub fn registry_shape(orm: &Orm) -> Vec<String> {
    let mut shape: Vec<String> = orm
        .registry()
        .models()
        .iter()
        .map(|m| {
            let mut fields: Vec<&str> = m.fields.iter().map(|f| f.name).collect();
            fields.sort_unstable();
            format!("{}: {}", m.name, fields.join(","))
        })
        .collect();
    shape.sort();
    shape
}

/// Schedule a post-commit step the way a hook does, from the managed transaction of a module
/// operation, and commit it. `schedule_post_commit` refuses any other transaction.
pub async fn schedule_managed(
    orm: &Orm,
    module: &ModuleName,
    key: &erp_modules::PostCommitKey,
    action: &erp_modules::PostCommitAction,
) -> erp_fields::Result<()> {
    let mut tx = orm.begin(SUPERUSER_ID).await?;
    let control = tx.manage_operation(&erp_orm::OperationKey::for_tests())?;
    let result = erp_modules::schedule_post_commit(&mut tx, module, key, action).await;
    match &result {
        Ok(()) => tx.operation_commit(&control).await?,
        Err(_) => tx.operation_rollback(&control).await?,
    }
    result
}

/// A superuser environment made by `system`'s own ORM lineage (a second system on the same
/// database cannot use the environments of the first: `ForeignEnvironment`).
pub fn su_of(system: &ModuleSystem) -> Env {
    system.system_env(Context::new())
}
