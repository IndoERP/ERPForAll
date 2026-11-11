//! The large-table schema work of a module operation (Amendment 2,
//! ): on a table above the row threshold, an install or upgrade commits without
//! building indexes, validating constraints or filling columns; the work is queued, one row per
//! step, and the post-commit phase plans each step again from the registry and the catalog and runs
//! it, a batched step resuming after the cursor stored in its row.
//!
//! The fixtures are `test_large_base` (a model with rows) and `test_large_ext` (an index, a unique
//! index, a foreign key, two required columns, a new stored computed column), as in
//! `large_tables.rs`, installed through the module system. A tiny `large_table_rows` makes the
//! table large.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use std::fs;
use std::num::NonZeroU32;
use std::time::Duration;

use erp_modules::lifecycle::InstallOptions;
use erp_modules::manifest::ModuleName;
use erp_modules::system::{InitOptions, LoadOutcome, ModuleSystem, PendingState, PostCommitReport};
use erp_modules::{AddonIndex, PostCommitStep};
use erp_orm::{
    Context, Orm, Replanned, SUPERUSER_ID, SchemaSyncOptions, StaticPolicy, StepFingerprint,
};
use erp_registry::ident::index_name;
use erp_test_models::test_large::{
    test_large_base, test_large_ext, test_large_ext2, test_large_key,
};
use erp_testkit::TestDb;
use tempfile::TempDir;

const TABLE: &str = "test_large_item";
/// Rows seeded before the extension: odd ones have a quantity, even ones a NULL quantity.
const ROWS: i64 = 12;
/// Tables with more rows than this are large.
const THRESHOLD: u64 = 5;
const SHORT: Duration = Duration::from_millis(200);
const LONG: Duration = Duration::from_secs(60);
/// How long a test lets the post-commit phase run before failing.
const BOUND: Duration = Duration::from_secs(120);

fn n(name: &str) -> ModuleName {
    ModuleName::new(name).unwrap()
}

/// Options making the seeded table large, with small batches and short lock waits.
fn large_options() -> SchemaSyncOptions {
    SchemaSyncOptions::new()
        .large_table_rows(THRESHOLD)
        .fill_batch(NonZeroU32::new(2).unwrap())
        .backfill_batch(NonZeroU32::new(3).unwrap())
        .step_lock_timeouts(SHORT, LONG)
        .unwrap()
}

struct Fixture {
    db: TestDb,
    _dir: TempDir,
    system: ModuleSystem,
    /// The ORM after `test_large_base` is installed and the rows are seeded.
    orm: Orm,
}

impl Fixture {
    async fn finish(self) {
        drop(self.orm);
        drop(self.system);
        self.db.cleanup().await.unwrap();
    }
}

/// A database with the module system and `test_large_base` installed, [`ROWS`] items seeded.
async fn fixture(options: SchemaSyncOptions) -> Fixture {
    fixture_on(TestDb::create().await.unwrap(), options).await
}

/// [`fixture`] on a database made by the caller.
async fn fixture_on(db: TestDb, options: SchemaSyncOptions) -> Fixture {
    let dir = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
    for (name, manifest) in [
        (
            "base",
            "name = 'Base'\nauto_install = true\ncategory = 'Hidden'\n",
        ),
        ("test_large_base", "name = 'Large base'\n"),
        (
            "test_large_ext",
            "name = 'Large ext'\ndepends = ['base', 'test_large_base']\n",
        ),
        (
            "test_large_ext2",
            "name = 'Large ext 2'\ndepends = ['base', 'test_large_base']\n",
        ),
        (
            "test_large_key",
            "name = 'Large key'\ndepends = ['base', 'test_large_base']\n",
        ),
    ] {
        let module = dir.path().join(name);
        fs::create_dir_all(&module).unwrap();
        fs::write(module.join("manifest.toml"), manifest).unwrap();
    }
    let mut index = AddonIndex::new(true);
    index.add_dev_path(dir.path());
    // This binary links code for these private fixture artifacts. Bind its provenance to
    // the explicit physical fixture bytes rather than exempting tests from admission.
    let native_artifact_identities = index
        .discover_entries()
        .unwrap()
        .into_iter()
        .map(|(name, entry)| {
            let snapshot = erp_modules::artifact_identity::ArtifactSnapshot::capture(
                name.as_str(),
                std::path::Path::new(entry.source_location()),
            )
            .unwrap();
            (name, snapshot.digest().to_owned())
        })
        .collect();
    let system = ModuleSystem::builder(db.pool().clone(), index)
        .native_artifact_identities(native_artifact_identities)
        .linked_hooks(false)
        .code([
            &test_large_base::ADDON,
            &test_large_ext::ADDON,
            &test_large_ext2::ADDON,
            &test_large_key::ADDON,
        ])
        .schema_sync_options(options)
        .build()
        .unwrap();
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    let env = init.orm.env(SUPERUSER_ID, Context::new());
    let outcome = system
        .button_immediate_install(&env, &[n("test_large_base")])
        .await
        .unwrap();
    assert_eq!(outcome.post_commit, PostCommitReport::default());
    let orm = outcome.orm;
    exec(
        &orm,
        &format!(
            "INSERT INTO {TABLE} (name, qty, reference) \
             SELECT 'item ' || g, CASE WHEN g % 2 = 1 THEN g END, 'r' || g \
               FROM generate_series(1, {ROWS}) g"
        ),
    )
    .await;
    Fixture {
        db,
        _dir: dir,
        system,
        orm,
    }
}

async fn exec(orm: &Orm, sql: &str) {
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    tx.batch(sql).await.unwrap();
    tx.commit().await.unwrap();
}

async fn q(orm: &Orm, sql: &str) -> Vec<Vec<Option<String>>> {
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let rows = tx.query(sql, &[]).await.unwrap();
    tx.rollback().await.unwrap();
    rows.iter()
        .map(|r| {
            (0..r.len())
                .map(|i| r.get::<_, Option<String>>(i))
                .collect()
        })
        .collect()
}

async fn q1(orm: &Orm, sql: &str) -> Option<String> {
    q(orm, sql).await.remove(0).remove(0)
}

/// Install `test_large_ext` the way a plain load does: scheduled, then the load's forced update
/// runs the operation and leaves its post-commit steps pending.
async fn install_ext_by_load(f: &Fixture) -> LoadOutcome {
    let env = f.system.system_env(Context::new());
    f.system
        .button_install(&env, &[n("test_large_ext")], &InstallOptions::new())
        .await
        .unwrap();
    let loaded = f.system.load().await.unwrap();
    let forced = loaded.forced_update.as_ref().expect("a forced update");
    assert_eq!(forced.installed, vec![n("test_large_ext")]);
    loaded
}

fn fk_name() -> String {
    erp_sql::ddl::foreign_key_name(TABLE, "owner_id")
}

/// The names of the three indexes the extension adds, from the registry.
fn new_indexes(orm: &Orm) -> [String; 3] {
    let spec = orm.registry().model("test_large.item").unwrap();
    let unique = spec
        .table_objects
        .iter()
        .find(|o| o.full_name.ends_with("_code_uniq"))
        .unwrap()
        .full_name
        .clone();
    [
        index_name(TABLE, "code"),
        index_name(TABLE, "owner_id"),
        unique,
    ]
}

/// Every step key the extension leaves on the large table, from the registry, sorted.
fn expected_step_keys(orm: &Orm) -> Vec<String> {
    let [code, owner, unique] = new_indexes(orm);
    let mut keys = vec![
        format!("index.{code}"),
        format!("index.{owner}"),
        format!("index.{unique}"),
        format!("validate.{}", fk_name()),
        format!("not_null.{TABLE}.qty"),
        format!("not_null.{TABLE}.reference"),
        format!("backfill.{TABLE}"),
    ];
    keys.sort();
    keys
}

/// The queue key of a step key (short enough to be used as it is).
fn queue_key(step_key: &str) -> String {
    format!("schema.{step_key}")
}

/// The queue keys of every step the extension leaves, sorted.
fn expected_queue_keys(orm: &Orm) -> Vec<String> {
    let mut keys: Vec<String> = expected_step_keys(orm)
        .iter()
        .map(|k| queue_key(k))
        .collect();
    keys.sort();
    keys
}

/// `(module, state, step key)` of the schema rows of the queue, in scheduling order.
async fn schema_rows(orm: &Orm) -> Vec<(String, String, String)> {
    q(
        orm,
        "SELECT module, state, spec FROM ir_module_post_commit WHERE kind = 'schema_step' \
          ORDER BY id",
    )
    .await
    .into_iter()
    .map(|r| {
        let spec: serde_json::Value = serde_json::from_str(r[2].as_deref().unwrap()).unwrap();
        (
            r[0].clone().unwrap(),
            r[1].clone().unwrap(),
            spec["key"].as_str().unwrap().to_owned(),
        )
    })
    .collect()
}

fn sorted_keys(rows: &[(String, String, String)]) -> Vec<String> {
    let mut keys: Vec<String> = rows.iter().map(|r| r.2.clone()).collect();
    keys.sort();
    keys
}

/// What the operation left undone on the large table: no index, the foreign key and the
/// `reference` constraint not validated, no fill, no compute.
async fn assert_nothing_scanned(orm: &Orm) {
    for index in new_indexes(orm) {
        assert_eq!(
            q1(orm, &format!("SELECT to_regclass('{index}')::text")).await,
            None,
            "{index} is built after the commit"
        );
    }
    assert_eq!(
        q1(
            orm,
            &format!(
                "SELECT convalidated::text FROM pg_constraint WHERE conname = '{}'",
                fk_name()
            )
        )
        .await
        .as_deref(),
        Some("false")
    );
    // the NULL quantities are filled by their step, `reference` was added NOT VALID
    assert_eq!(
        q1(
            orm,
            &format!("SELECT count(*)::text FROM {TABLE} WHERE qty IS NULL")
        )
        .await,
        Some((ROWS / 2).to_string())
    );
    assert_eq!(
        q1(
            orm,
            &format!("SELECT count(*)::text FROM {TABLE} WHERE name_size IS NULL")
        )
        .await,
        Some(ROWS.to_string())
    );
    assert_eq!(
        q1(
            orm,
            &format!(
                "SELECT convalidated::text FROM pg_constraint WHERE contype = 'n' \
                 AND conrelid = '{TABLE}'::regclass AND conkey[1] = \
                 (SELECT attnum FROM pg_attribute WHERE attrelid = '{TABLE}'::regclass \
                   AND attname = 'reference')"
            )
        )
        .await
        .as_deref(),
        Some("false")
    );
}

/// The end state once every step ran: indexes valid, the foreign key and both not-null
/// constraints validated, NULL quantities filled with the default, `name_size` computed, the
/// column's comment its label again.
async fn assert_complete(orm: &Orm) {
    for index in new_indexes(orm) {
        assert_eq!(
            q1(
                orm,
                &format!(
                    "SELECT i.indisvalid::text FROM pg_index i \
                     JOIN pg_class c ON c.oid = i.indexrelid WHERE c.relname = '{index}'"
                )
            )
            .await
            .as_deref(),
            Some("true"),
            "{index}"
        );
    }
    assert_eq!(
        q1(
            orm,
            &format!(
                "SELECT convalidated::text FROM pg_constraint WHERE conname = '{}'",
                fk_name()
            )
        )
        .await
        .as_deref(),
        Some("true")
    );
    assert_eq!(
        q(
            orm,
            &format!(
                "SELECT a.attname::text, a.attnotnull::text, \
                        coalesce(bool_and(k.convalidated), false)::text \
                   FROM pg_attribute a \
                   LEFT JOIN pg_constraint k ON k.conrelid = a.attrelid AND k.contype = 'n' \
                        AND k.conkey[1] = a.attnum \
                  WHERE a.attrelid = '{TABLE}'::regclass AND a.attname IN ('qty', 'reference') \
                  GROUP BY 1, 2 ORDER BY 1"
            )
        )
        .await,
        vec![
            vec![Some("qty".into()), Some("true".into()), Some("true".into())],
            vec![
                Some("reference".into()),
                Some("true".into()),
                Some("true".into())
            ],
        ]
    );
    assert_eq!(
        q1(
            orm,
            &format!("SELECT count(*)::text FROM {TABLE} WHERE qty IS NULL")
        )
        .await
        .as_deref(),
        Some("0")
    );
    assert_eq!(
        q1(
            orm,
            &format!(
                "SELECT count(*)::text FROM {TABLE} \
                 WHERE name_size IS DISTINCT FROM char_length(name)"
            )
        )
        .await
        .as_deref(),
        Some("0")
    );
    let label = orm
        .registry()
        .model("test_large.item")
        .unwrap()
        .field("name_size")
        .unwrap()
        .string
        .clone();
    assert_eq!(
        q1(
            orm,
            &format!(
                "SELECT col_description('{TABLE}'::regclass, attnum) FROM pg_attribute \
                  WHERE attrelid = '{TABLE}'::regclass AND attname = 'name_size'"
            )
        )
        .await,
        Some(label)
    );
}

/// `future` with the phase bound.
async fn bounded<T>(what: &str, future: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(BOUND, future)
        .await
        .unwrap_or_else(|_| panic!("{what} still running after {BOUND:?}"))
}

/// A trigger refusing, on row `id`, the update that fills `qty` or computes `name_size`: the fill
/// and the backfill fail when their batch reaches that row, without waiting for anything (an open
/// transaction holding the row would also make `CREATE INDEX CONCURRENTLY` wait for it).
async fn refuse_fill_of_row(orm: &Orm, id: i64) {
    exec(
        orm,
        &format!(
            "CREATE FUNCTION wire_refuse() RETURNS trigger LANGUAGE plpgsql AS $$ \
             BEGIN \
               IF NEW.id = {id} AND ((OLD.qty IS NULL AND NEW.qty IS NOT NULL) \
                  OR (OLD.name_size IS NULL AND NEW.name_size IS NOT NULL)) THEN \
                 RAISE EXCEPTION 'refused at row {id}'; \
               END IF; \
               RETURN NEW; \
             END $$; \
             CREATE TRIGGER wire_refuse BEFORE UPDATE ON {TABLE} \
             FOR EACH ROW EXECUTE FUNCTION wire_refuse()"
        ),
    )
    .await;
}

/// Lift [`refuse_fill_of_row`].
async fn allow_fill(orm: &Orm) {
    exec(
        orm,
        &format!("DROP TRIGGER wire_refuse ON {TABLE}; DROP FUNCTION wire_refuse()"),
    )
    .await;
}

fn keys_of(steps: &[PostCommitStep]) -> Vec<String> {
    let mut keys: Vec<String> = steps.iter().map(|s| s.key.to_string()).collect();
    keys.sort();
    keys
}

/// builds, constraint validation, fills or backfills; they are queued, one row per step, and the
/// post-commit phase runs them; the order of the module pipeline is
/// `spec_mod_029_per_module_pipeline_order`)
#[tokio::test]
async fn large_table_work_leaves_the_operation_and_runs_from_the_queue() {
    let f = fixture(large_options()).await;
    let loaded = install_ext_by_load(&f).await;
    let orm = &loaded.orm;

    // the operation committed the catalog work only, and queued one row per step
    assert_nothing_scanned(orm).await;
    let queued = schema_rows(orm).await;
    assert_eq!(sorted_keys(&queued), expected_step_keys(orm));
    assert!(
        queued
            .iter()
            .all(|(module, state, _)| module == "test_large_ext" && state == "pending"),
        "{queued:?}"
    );
    // each row names the table and holds no index name and no cursor yet
    let shape = q(
        orm,
        "SELECT DISTINCT table_name, kind, (index_name IS NULL)::text, (\"cursor\" IS NULL)::text \
           FROM ir_module_post_commit",
    )
    .await;
    assert_eq!(
        shape,
        vec![vec![
            Some(TABLE.to_owned()),
            Some("schema_step".to_owned()),
            Some("true".to_owned()),
            Some("true".to_owned())
        ]]
    );
    let expected_pending = expected_queue_keys(orm);
    let mut pending: Vec<String> = loaded
        .post_commit_pending
        .iter()
        .map(|p| match &p.state {
            PendingState::Pending { step } => step.key.to_string(),
            other => panic!("{other:?}"),
        })
        .collect();
    pending.sort();
    assert_eq!(pending, expected_pending);

    // the phase runs them all, and records what each reported
    let report = bounded("phase", f.system.run_post_commit(orm)).await;
    assert!(report.is_clean(), "{report:?}");
    assert_eq!(keys_of(&report.done), expected_pending);
    assert_eq!(report.schema.len(), report.done.len());
    assert!(report.schema.iter().all(|s| s.report.is_empty()));
    assert_complete(orm).await;
    let after = schema_rows(orm).await;
    assert!(
        after.iter().all(|(_, state, _)| state == "done"),
        "{after:?}"
    );

    // nothing is left: a second phase runs nothing
    assert_eq!(
        bounded("phase", f.system.run_post_commit(orm)).await,
        PostCommitReport::default()
    );
    f.finish().await;
}

/// The immediate install runs the same steps in its own post-commit phase: the outcome is clean,
/// lists the steps and leaves the end state.
#[tokio::test]
async fn an_immediate_install_runs_its_schema_steps() {
    let f = fixture(large_options()).await;
    let env = f.system.system_env(Context::new());
    let outcome = bounded(
        "install",
        f.system
            .button_immediate_install(&env, &[n("test_large_ext")]),
    )
    .await
    .unwrap();
    assert!(outcome.post_commit.is_clean(), "{:?}", outcome.post_commit);
    let expected = expected_queue_keys(&outcome.orm);
    assert_eq!(keys_of(&outcome.post_commit.done), expected);
    assert_eq!(outcome.post_commit.schema.len(), expected.len());
    assert_complete(&outcome.orm).await;
    f.finish().await;
}

/// operation's transaction, which commits or rolls back as a whole; the queue gets no row)
#[tokio::test]
async fn small_table_work_stays_in_the_operation() {
    let f = fixture(SchemaSyncOptions::new().large_table_rows(ROWS.unsigned_abs())).await;
    let loaded = install_ext_by_load(&f).await;
    assert!(loaded.post_commit_pending.is_empty());
    assert_eq!(schema_rows(&loaded.orm).await, vec![]);
    assert_complete(&loaded.orm).await;
    f.finish().await;
}

/// A step that fails does not stop the phase: the others finish and are recorded; the failed ones
/// stay queued with their error and run again in the next phase, which does not run the finished
/// ones again (the index keeps its oid).
#[tokio::test]
async fn a_failed_step_leaves_the_others_done_and_runs_again_alone() {
    let f = fixture(large_options()).await;
    let loaded = install_ext_by_load(&f).await;
    let orm = &loaded.orm;
    // the fill and the backfill fail when their first batch reaches row 4
    refuse_fill_of_row(orm, 4).await;
    let report = bounded("phase", f.system.run_post_commit(orm)).await;
    let fill = format!("not_null.{TABLE}.qty");
    let backfill = format!("backfill.{TABLE}");
    let mut failed: Vec<(String, Option<String>, String)> = report
        .failed
        .iter()
        .map(|x| (x.step.key.to_string(), x.sqlstate.clone(), x.error.clone()))
        .collect();
    failed.sort();
    let mut expected_failed: Vec<(String, Option<String>, String)> = [&fill, &backfill]
        .iter()
        .map(|k| {
            (
                queue_key(k),
                Some("P0001".to_owned()),
                format!(
                    "schema step '{k}' failed: refused at row 4: db error: ERROR: refused at row 4"
                ),
            )
        })
        .collect();
    expected_failed.sort();
    assert_eq!(failed, expected_failed);
    assert!(report.stopped.is_none());
    let mut expected_done: Vec<String> = expected_step_keys(orm)
        .into_iter()
        .filter(|k| *k != fill && *k != backfill)
        .map(|k| queue_key(&k))
        .collect();
    expected_done.sort();
    assert_eq!(keys_of(&report.done), expected_done);

    let rows = schema_rows(orm).await;
    for (_, state, key) in &rows {
        let expected = if *key == fill || *key == backfill {
            "failed"
        } else {
            "done"
        };
        assert_eq!(state, expected, "{key}");
    }
    let code_index = new_indexes(orm)[0].clone();
    let oid = q1(orm, &format!("SELECT '{code_index}'::regclass::oid::text")).await;

    allow_fill(orm).await;
    let again = bounded("phase", f.system.run_post_commit(orm)).await;
    assert!(again.is_clean(), "{again:?}");
    let mut expected_again = vec![queue_key(&fill), queue_key(&backfill)];
    expected_again.sort();
    assert_eq!(keys_of(&again.done), expected_again);
    assert_complete(orm).await;
    assert_eq!(
        q1(orm, &format!("SELECT '{code_index}'::regclass::oid::text")).await,
        oid,
        "a finished step is not run again"
    );
    f.finish().await;
}

/// A batched step stores the last id of each committed batch in its queue row, with the
/// fingerprint of the work, and the next run starts after it. The batch refused at a row fails
/// the step; the cursor is the last row the committed batches reached. A row emptied behind a
/// cursor is repaired by the closing pass of the resumed run: the constraint validates and the
/// field is computed.
#[tokio::test]
async fn a_batched_step_resumes_after_the_cursor_stored_in_its_row() {
    let f = fixture(large_options()).await;
    let loaded = install_ext_by_load(&f).await;
    let orm = &loaded.orm;
    let fill = format!("not_null.{TABLE}.qty");
    let backfill = format!("backfill.{TABLE}");
    let cursor_of = |key: String| async move {
        q1(
            orm,
            &format!(
                "SELECT \"cursor\"::text FROM ir_module_post_commit WHERE key = '{}'",
                queue_key(&key)
            ),
        )
        .await
    };
    assert_eq!(cursor_of(fill.clone()).await, None);

    // row 8 is refused: the fill (batches of 2 NULL rows: 2 4 | 6 8) stops in its second batch,
    // the backfill (batches of 3: 1-3 | 4-6 | 7-9) in its third
    refuse_fill_of_row(orm, 8).await;
    let report = bounded("phase", f.system.run_post_commit(orm)).await;
    let failed: Vec<String> = report
        .failed
        .iter()
        .map(|x| x.step.key.to_string())
        .collect();
    assert!(failed.contains(&queue_key(&fill)), "{report:?}");
    assert!(failed.contains(&queue_key(&backfill)), "{report:?}");
    let fill_cursor = q1(
        orm,
        &format!("SELECT max(id)::text FROM {TABLE} WHERE qty = 7 AND id % 2 = 0"),
    )
    .await;
    assert!(fill_cursor.is_some());
    assert_eq!(cursor_of(fill.clone()).await, fill_cursor);
    let backfill_cursor = q1(
        orm,
        &format!(
            "SELECT max(id)::text FROM {TABLE} WHERE name_size IS NOT NULL \
             AND id < (SELECT min(id) FROM {TABLE} WHERE name_size IS NULL)"
        ),
    )
    .await;
    assert!(backfill_cursor.is_some());
    assert_eq!(cursor_of(backfill.clone()).await, backfill_cursor);
    // the row also says which work the cursor belongs to: what the ORM fingerprints that step as
    for (key, fields) in [(&fill, vec![]), (&backfill, vec!["name_size"])] {
        let Replanned::Pending(planned) = orm
            .plan_schema_step(TABLE, key, &large_options(), &fields)
            .await
            .unwrap()
        else {
            panic!("{key} is pending");
        };
        assert_eq!(
            q1(
                orm,
                &format!(
                    "SELECT cursor_fingerprint FROM ir_module_post_commit WHERE key = '{}'",
                    queue_key(key)
                )
            )
            .await
            .as_deref(),
            Some(planned.fingerprint().as_str()),
            "{key}"
        );
    }

    // a row before each cursor loses its value again (ids are assigned at insert, a NULL can
    // appear behind any cursor): the runs resume after their cursors, and their closing passes
    // repair row 2 for both columns, so the constraint validates and the field is computed
    allow_fill(orm).await;
    exec(
        orm,
        &format!("UPDATE {TABLE} SET qty = NULL, name_size = NULL WHERE id = 2"),
    )
    .await;
    let report = bounded("phase", f.system.run_post_commit(orm)).await;
    assert!(report.is_clean(), "{report:?}");
    let mut expected = vec![queue_key(&fill), queue_key(&backfill)];
    expected.sort();
    assert_eq!(keys_of(&report.done), expected);
    assert_complete(orm).await;
    f.finish().await;
}

/// An interrupted concurrent index build leaves an invalid index: the step plans a rebuild, drops
/// it and ends with a valid index.
#[tokio::test]
async fn an_invalid_index_is_rebuilt() {
    let f = fixture(large_options()).await;
    let loaded = install_ext_by_load(&f).await;
    let orm = &loaded.orm;
    let code_index = new_indexes(orm)[0].clone();
    // a unique build that fails on duplicates, under the field index's name
    exec(
        orm,
        &format!("UPDATE {TABLE} SET code = 'dup' WHERE id IN (1, 2)"),
    )
    .await;
    {
        let client = f.db.client().await.unwrap();
        client
            .batch_execute(&format!(
                "CREATE UNIQUE INDEX CONCURRENTLY \"{code_index}\" ON {TABLE} (code)"
            ))
            .await
            .unwrap_err();
    }
    exec(orm, &format!("UPDATE {TABLE} SET code = NULL")).await;
    assert_eq!(
        q1(
            orm,
            &format!(
                "SELECT i.indisvalid::text FROM pg_index i \
                 JOIN pg_class c ON c.oid = i.indexrelid WHERE c.relname = '{code_index}'"
            )
        )
        .await
        .as_deref(),
        Some("false")
    );
    let report = bounded("phase", f.system.run_post_commit(orm)).await;
    assert!(report.is_clean(), "{report:?}");
    assert_complete(orm).await;
    assert_eq!(
        q1(
            orm,
            &format!("SELECT indexdef FROM pg_indexes WHERE indexname = '{code_index}'")
        )
        .await
        .map(|d| d.contains("UNIQUE")),
        Some(false),
        "the rebuilt index is the declared, non-unique one"
    );
    f.finish().await;
}

/// A step whose work is already done (the schema was synchronized in between) is planned as
/// nothing: its row is marked done with an empty report, and nothing is changed.
#[tokio::test]
async fn a_step_with_nothing_left_is_marked_done() {
    let f = fixture(large_options()).await;
    let loaded = install_ext_by_load(&f).await;
    let orm = &loaded.orm;
    // the work is done by an inline synchronization (the default threshold is far above the rows)
    let report = orm.sync_schema().await.unwrap();
    assert!(report.is_empty());
    assert_complete(orm).await;
    let report = bounded("phase", f.system.run_post_commit(orm)).await;
    assert!(report.is_clean(), "{report:?}");
    assert_eq!(keys_of(&report.done), expected_queue_keys(orm));
    assert!(report.schema.iter().all(|s| s.report.is_empty()));
    let rows = schema_rows(orm).await;
    assert!(rows.iter().all(|(_, state, _)| state == "done"), "{rows:?}");
    f.finish().await;
}

/// A schema row that was edited into something the queue could not have written is listed as
/// invalid with the reason, and never run: the key must fit the kind and table.
#[tokio::test]
async fn an_invalid_schema_row_is_listed_and_never_run() {
    let f = fixture(large_options()).await;
    let orm = &f.orm;
    let cases = [
        (
            "wrong",
            r#"{"work":"backfill","key":"backfill.other_table"}"#,
        ),
        (
            "kind",
            r#"{"work":"rebuild","key":"index.test_large_item_x"}"#,
        ),
        ("chars", r#"{"work":"create_index","key":"index.x y"}"#),
        (
            "extra",
            r#"{"work":"backfill","key":"backfill.test_large_item","fields":["a"],"sql":"x"}"#,
        ),
    ];
    for (key, spec) in cases {
        exec(
            orm,
            &format!(
                "INSERT INTO ir_module_post_commit \
                 (module, key, kind, table_name, spec, state, revision, attempts) \
                 VALUES ('base', '{key}', 'schema_step', '{TABLE}', '{spec}', 'pending', 1, 0)"
            ),
        )
        .await;
    }
    let listed = f.system.pending_post_commit().await.unwrap();
    let reasons: Vec<(String, String)> = listed
        .iter()
        .map(|p| match &p.state {
            PendingState::Invalid(row) => (row.key.clone(), row.reason.clone()),
            other => panic!("{other:?}"),
        })
        .collect();
    let expected = [
        (
            "wrong",
            "invalid post-commit action: schema step key \"backfill.other_table\" does not fit a \
             backfill step on table test_large_item: it must be \"backfill.test_large_item\"",
        ),
        (
            "kind",
            "invalid post-commit action: unknown schema step work \"rebuild\"",
        ),
        (
            "chars",
            "invalid post-commit action: invalid schema step key \"index.x y\": 1 to 160 bytes \
             of a-z, 0-9, '_', '.'",
        ),
    ];
    assert_eq!(reasons.len(), 4, "{reasons:?}");
    for ((key, reason), (want_key, want)) in reasons.iter().zip(expected) {
        assert_eq!((key.as_str(), reason.as_str()), (want_key, want));
    }
    // serde's own message ends with the position in the text, which is not ours to pin
    assert_eq!(reasons[3].0, "extra");
    assert!(
        reasons[3].1.starts_with(
            "invalid post-commit action: invalid spec: unknown field `sql`, expected one of \
             `work`, `key`, `fields`"
        ),
        "{}",
        reasons[3].1
    );
    let report = bounded("phase", f.system.run_post_commit(orm)).await;
    assert_eq!(report.invalid.len(), 4, "{report:?}");
    assert!(
        report.done.is_empty() && report.schema.is_empty(),
        "{report:?}"
    );
    f.finish().await;
}

/// Uninstalling removes the schema rows of the module and the schema rows of any module on a table
/// the uninstall drops.
#[tokio::test]
async fn uninstall_removes_the_schema_rows_of_the_module_and_of_dropped_tables() {
    let f = fixture(large_options()).await;
    let loaded = install_ext_by_load(&f).await;
    let orm = &loaded.orm;
    assert_eq!(schema_rows(orm).await.len(), expected_step_keys(orm).len());
    // another module's row on the same table
    let spec = r#"{"work":"backfill","key":"backfill.test_large_item","fields":["name_size"]}"#;
    exec(
        orm,
        &format!(
            "INSERT INTO ir_module_post_commit \
             (module, key, kind, table_name, spec, state, revision, attempts) \
             VALUES ('test_large_base', 'schema.backfill.test_large_item', 'schema_step', \
                     '{TABLE}', '{spec}', 'pending', 1, 0)"
        ),
    )
    .await;
    let env = f.system.system_env(Context::new());
    let outcome = f
        .system
        .button_immediate_uninstall(&env, &[n("test_large_ext")])
        .await
        .unwrap();
    let rows = schema_rows(&outcome.orm).await;
    assert_eq!(
        rows,
        vec![(
            "test_large_base".to_owned(),
            "failed".to_owned(),
            "backfill.test_large_item".to_owned()
        )],
        "the extension's rows went with it; the base row stayed, and its phase could not plan a \
         field the registry no longer has"
    );
    let outcome = f
        .system
        .button_immediate_uninstall(&env, &[n("test_large_base")])
        .await
        .unwrap();
    assert_eq!(schema_rows(&outcome.orm).await, vec![]);
    f.finish().await;
}

/// The message of an older queue table (the const of the module system, spelled out).
const OLDER_BASE: &str = "ir_module_post_commit is from an older base: upgrade module base first";

/// The messages of an error and of each of its sources, outermost first.
fn chain(err: &(dyn std::error::Error + 'static)) -> Vec<String> {
    let mut out = vec![err.to_string()];
    let mut next = err.source();
    while let Some(source) = next {
        out.push(source.to_string());
        next = source.source();
    }
    out
}

/// A second module adds a stored computed field to the table while the first one's backfill holds
/// a cursor for its single field. The backfill the queue then runs is planned for both fields, so
/// the stored cursor (and its fingerprint) belongs to other work: it is not resumed, and every row
/// of the new field is computed, which resuming would have skipped for the rows before it.
#[tokio::test]
async fn a_cursor_stored_for_fewer_backfill_fields_is_not_resumed_by_a_larger_set() {
    let f = fixture(large_options()).await;
    let loaded = install_ext_by_load(&f).await;
    let orm = &loaded.orm;
    refuse_fill_of_row(orm, 8).await;
    let report = bounded("phase", f.system.run_post_commit(orm)).await;
    assert!(
        report
            .failed
            .iter()
            .any(|x| x.step.key.to_string() == queue_key(&format!("backfill.{TABLE}"))),
        "{report:?}"
    );
    let first_cursor = q1(
        orm,
        "SELECT \"cursor\"::text FROM ir_module_post_commit WHERE key = 'schema.backfill.test_large_item'",
    )
    .await;
    assert!(
        first_cursor.is_some(),
        "a cursor was stored for {{name_size}}"
    );
    allow_fill(orm).await;

    // the second module: its field is new, its rows NULL
    let env = f.system.system_env(Context::new());
    f.system
        .button_install(&env, &[n("test_large_ext2")], &InstallOptions::new())
        .await
        .unwrap();
    let loaded = f.system.load().await.unwrap();
    assert!(loaded.forced_update.is_some());
    let orm = &loaded.orm;
    let report = bounded("phase", f.system.run_post_commit(orm)).await;
    assert!(report.is_clean(), "{report:?}");
    assert_complete(orm).await;
    assert_eq!(
        q1(
            orm,
            &format!(
                "SELECT count(*)::text FROM {TABLE} \
                 WHERE name_size2 IS DISTINCT FROM 2 * char_length(name)"
            )
        )
        .await
        .as_deref(),
        Some("0"),
        "every row of the new field is computed"
    );
    f.finish().await;
}

/// An ORM that is not of this module system's lineage (here: one built on the same database from
/// the same registry, outside the module system) is refused when the phase starts, and no row is
/// touched.
#[tokio::test]
async fn an_orm_of_another_lineage_is_refused_at_phase_start() {
    let f = fixture(large_options()).await;
    let loaded = install_ext_by_load(&f).await;
    let foreign = Orm::builder(
        std::sync::Arc::clone(loaded.orm.registry()),
        f.db.pool().clone(),
    )
    .policy(StaticPolicy::AllowAll)
    .build()
    .unwrap();
    let report = bounded("phase", f.system.run_post_commit(&foreign)).await;
    assert_eq!(
        report.stopped,
        Some(erp_modules::system::PostCommitStop::Failed {
            message: "run_post_commit: the ORM is not of this module system's lineage (it \
                      belongs to another module system or another database); pass \
                      OperationOutcome::orm or LoadOutcome::orm"
                .to_owned(),
            sqlstate: None
        })
    );
    assert!(
        report.done.is_empty() && report.failed.is_empty(),
        "{report:?}"
    );
    let rows = schema_rows(&loaded.orm).await;
    assert_eq!(rows.len(), expected_step_keys(&loaded.orm).len());
    assert!(
        rows.iter().all(|(_, state, _)| state == "pending"),
        "{rows:?}"
    );
    drop(foreign);
    f.finish().await;
}

/// The ORM from before the operation is of the right lineage but its registry does not declare the
/// index, key, required columns and field the rows name: each step fails naming its key and none
/// is recorded as done (a re-plan that found nothing would have looked like finished work). With
/// the ORM of the current module set the next phase finishes them.
#[tokio::test]
async fn an_orm_from_before_the_operation_cannot_say_a_step_is_done() {
    let f = fixture(large_options()).await;
    let loaded = install_ext_by_load(&f).await;
    let report = bounded("phase", f.system.run_post_commit(&f.orm)).await;
    assert!(report.done.is_empty(), "{report:?}");
    let [code, owner, unique] = new_indexes(&loaded.orm);
    let mut expected: Vec<(String, String)> = Vec::new();
    for (key, what) in [
        (
            format!("index.{code}"),
            format!("no declared index {code} on table {TABLE}"),
        ),
        (
            format!("index.{owner}"),
            format!("no declared index {owner} on table {TABLE}"),
        ),
        (
            format!("index.{unique}"),
            format!("no declared index {unique} on table {TABLE}"),
        ),
        (
            format!("validate.{}", fk_name()),
            format!("no declared foreign key {} on table {TABLE}", fk_name()),
        ),
        (
            format!("not_null.{TABLE}.qty"),
            format!("column {TABLE}.qty is not a required field"),
        ),
        (
            format!("not_null.{TABLE}.reference"),
            format!("column {TABLE}.reference is not a required field"),
        ),
        (
            format!("backfill.{TABLE}"),
            format!("none of the stored computed fields [name_size] is declared on table {TABLE}"),
        ),
    ] {
        expected.push((
            queue_key(&key),
            format!(
                "schema step '{key}' is not declared by the registry of the ORM given to the \
                 post-commit phase ({what}): pass the ORM of the current module set \
                 (OperationOutcome::orm, LoadOutcome::orm)"
            ),
        ));
    }
    expected.sort();
    let mut failed: Vec<(String, String)> = report
        .failed
        .iter()
        .map(|x| (x.step.key.to_string(), x.error.clone()))
        .collect();
    failed.sort();
    assert_eq!(failed, expected);
    assert!(report.failed.iter().all(|x| x.sqlstate.is_none()));
    let rows = schema_rows(&loaded.orm).await;
    assert!(
        rows.iter().all(|(_, state, _)| state == "failed"),
        "{rows:?}"
    );

    let report = bounded("phase", f.system.run_post_commit(&loaded.orm)).await;
    assert!(report.is_clean(), "{report:?}");
    assert_complete(&loaded.orm).await;
    f.finish().await;
}

/// A column comment changed outside the ORM before the phase leaves the backfill's pending mark
/// gone, yet the queued backfill recorded its field: the re-plan still finds rows without a value,
/// the column is computed, and the comment is kept and reported.
#[tokio::test]
async fn a_comment_changed_before_the_phase_still_gets_the_column_computed() {
    let f = fixture(large_options()).await;
    let loaded = install_ext_by_load(&f).await;
    let orm = &loaded.orm;
    exec(
        orm,
        &format!("COMMENT ON COLUMN {TABLE}.name_size IS 'changed outside the ORM'"),
    )
    .await;
    let report = bounded("phase", f.system.run_post_commit(orm)).await;
    assert!(report.is_clean(), "{report:?}");
    assert_eq!(
        q1(
            orm,
            &format!("SELECT count(*)::text FROM {TABLE} WHERE name_size IS NULL")
        )
        .await
        .as_deref(),
        Some("0")
    );
    let backfill = queue_key(&format!("backfill.{TABLE}"));
    let done = report
        .schema
        .iter()
        .find(|d| d.step.key.to_string() == backfill)
        .expect("the backfill ran");
    assert_eq!(done.outcome, erp_modules::system::SchemaStepOutcome::Ran);
    let kept: Vec<(String, String, Option<String>)> = done
        .report
        .kept_comments()
        .iter()
        .map(|k| {
            (
                k.table().to_owned(),
                k.column().to_owned(),
                k.comment().map(str::to_owned),
            )
        })
        .collect();
    assert_eq!(
        kept,
        vec![(
            TABLE.to_owned(),
            "name_size".to_owned(),
            Some("changed outside the ORM".to_owned())
        )]
    );
    f.finish().await;
}

/// The phase holds a connection while a schema step plans and computes with others: with a pool of
/// one connection it stops with the reason (not a wait for itself), leaves the row pending and
/// untried.
#[tokio::test]
async fn a_pool_of_one_connection_stops_the_phase_loudly() {
    let db = TestDb::builder().pool_size(1).create().await.unwrap();
    let f = bounded("fixture", fixture_on(db, large_options())).await;
    let spec = r#"{"work":"backfill","key":"backfill.test_large_item","fields":["name"]}"#;
    exec(
        &f.orm,
        &format!(
            "INSERT INTO ir_module_post_commit \
             (module, key, kind, table_name, spec, state, revision, attempts) \
             VALUES ('base', 'schema.one', 'schema_step', '{TABLE}', '{spec}', 'pending', 1, 0)"
        ),
    )
    .await;
    let report = bounded("phase", f.system.run_post_commit(&f.orm)).await;
    assert_eq!(
        report.stopped,
        Some(erp_modules::system::PostCommitStop::Failed {
            message: "schema steps need a connection pool of at least 2 connections (the phase \
                      holds one while a step plans and computes with others); this pool has 1: \
                      raise the pool size and run the phase again"
                .to_owned(),
            sqlstate: None
        })
    );
    assert!(
        report.done.is_empty() && report.failed.is_empty(),
        "{report:?}"
    );
    assert_eq!(
        q(
            &f.orm,
            "SELECT state, attempts::text FROM ir_module_post_commit"
        )
        .await,
        vec![vec![Some("pending".to_owned()), Some("0".to_owned())]]
    );
    f.finish().await;
}

/// A queue table made by a `base` older than the schema steps (no cursor column) names the cause:
/// the phase stops with it, and an operation that has to queue a schema step fails with it.
#[tokio::test]
async fn an_older_queue_table_without_the_cursor_column_names_the_cause() {
    let f = fixture(large_options()).await;
    exec(
        &f.orm,
        "ALTER TABLE ir_module_post_commit DROP COLUMN \"cursor\", DROP COLUMN cursor_fingerprint",
    )
    .await;
    let report = bounded("phase", f.system.run_post_commit(&f.orm)).await;
    assert_eq!(
        report.stopped,
        Some(erp_modules::system::PostCommitStop::Failed {
            message: format!(
                "reading ir_module_post_commit: {OLDER_BASE}: column \"cursor\" does not exist"
            ),
            sqlstate: Some("42703".to_owned())
        })
    );
    let env = f.system.system_env(Context::new());
    let err = f
        .system
        .button_immediate_install(&env, &[n("test_large_ext")])
        .await
        .map(drop)
        .unwrap_err();
    assert!(
        chain(&err).iter().any(|m| m.starts_with(OLDER_BASE)),
        "{:?}",
        chain(&err)
    );
    f.finish().await;
}

/// A queue table whose kind check does not know schema steps (an older `base`) names the cause
/// when the operation tries to queue one.
#[tokio::test]
async fn an_older_queue_table_with_the_old_kind_check_names_the_cause() {
    let f = fixture(large_options()).await;
    let checks: Vec<String> = q(
        &f.orm,
        "SELECT conname::text FROM pg_constraint WHERE conrelid = 'ir_module_post_commit'::regclass \
           AND contype = 'c' AND pg_get_constraintdef(oid) LIKE '%schema_step%'",
    )
    .await
    .into_iter()
    .map(|r| r[0].clone().unwrap())
    .collect();
    assert!(!checks.is_empty());
    for check in checks {
        exec(
            &f.orm,
            &format!("ALTER TABLE ir_module_post_commit DROP CONSTRAINT \"{check}\""),
        )
        .await;
    }
    exec(
        &f.orm,
        "ALTER TABLE ir_module_post_commit ADD CONSTRAINT ir_module_post_commit_kind_check \
         CHECK (kind IN ('create_index', 'analyze'))",
    )
    .await;
    let env = f.system.system_env(Context::new());
    let err = f
        .system
        .button_immediate_install(&env, &[n("test_large_ext")])
        .await
        .map(drop)
        .unwrap_err();
    assert!(
        chain(&err).iter().any(|m| m.starts_with(OLDER_BASE)),
        "{:?}",
        chain(&err)
    );
    f.finish().await;
}

/// A queue key already held by a different schema step (two step keys that hash to one queue key)
/// is refused instead of replaced: the operation fails and changes nothing.
#[tokio::test]
async fn a_queue_key_held_by_another_schema_step_is_refused() {
    let f = fixture(large_options()).await;
    let spec = r#"{"work":"backfill","key":"backfill.some_other_table","fields":["x"]}"#;
    exec(
        &f.orm,
        &format!(
            "INSERT INTO ir_module_post_commit \
             (module, key, kind, table_name, spec, state, revision, attempts) \
             VALUES ('test_large_ext', 'schema.backfill.test_large_item', 'schema_step', \
                     'some_other_table', '{spec}', 'pending', 1, 0)"
        ),
    )
    .await;
    let env = f.system.system_env(Context::new());
    let err = f
        .system
        .button_immediate_install(&env, &[n("test_large_ext")])
        .await
        .map(drop)
        .unwrap_err();
    let messages = chain(&err);
    let wanted = "post-commit queue key \"schema.backfill.test_large_item\" of module \
                  test_large_ext already holds another schema step: the two step keys map to \
                  the same queue key and the first one is kept";
    assert!(messages.iter().any(|m| m == wanted), "{messages:?}");
    assert_eq!(state_of(&f.orm, "test_large_ext").await, "uninstalled");
    f.finish().await;
}

/// The state of a module.
async fn state_of(orm: &Orm, module: &str) -> String {
    q1(
        orm,
        &format!("SELECT state FROM ir_module_module WHERE name = '{module}'"),
    )
    .await
    .unwrap()
}

/// A stored cursor fingerprint that is not one the module system could have written is listed as an
/// invalid row with the reason (and never run).
#[tokio::test]
async fn a_corrupt_cursor_fingerprint_lists_the_row_as_invalid() {
    let f = fixture(large_options()).await;
    let spec = r#"{"work":"backfill","key":"backfill.test_large_item","fields":["name"]}"#;
    exec(
        &f.orm,
        &format!(
            "INSERT INTO ir_module_post_commit \
             (module, key, kind, table_name, spec, state, revision, attempts, \"cursor\", \
              cursor_fingerprint) \
             VALUES ('base', 'schema.bad', 'schema_step', '{TABLE}', '{spec}', 'pending', 1, 0, \
                     3, 'NOT A FINGERPRINT')"
        ),
    )
    .await;
    let listed = f.system.pending_post_commit().await.unwrap();
    let reasons: Vec<String> = listed
        .iter()
        .map(|p| match &p.state {
            PendingState::Invalid(row) => row.reason.clone(),
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(
        reasons,
        vec![format!(
            "invalid cursor fingerprint: {}",
            StepFingerprint::parse("NOT A FINGERPRINT")
                .unwrap_err()
                .message()
        )]
    );
    let report = bounded("phase", f.system.run_post_commit(&f.orm)).await;
    assert_eq!(report.invalid.len(), 1, "{report:?}");
    assert!(
        report.done.is_empty() && report.schema.is_empty(),
        "{report:?}"
    );
    f.finish().await;
}

/// A check of the queue table that a current `base` has too (here the table-name check, made to
/// refuse every table) is not an older `base`: the operation fails with the check's own error and
/// the older-base message is not used.
#[tokio::test]
async fn a_current_queue_check_is_not_reported_as_an_older_base() {
    let f = fixture(large_options()).await;
    exec(
        &f.orm,
        "ALTER TABLE ir_module_post_commit DROP CONSTRAINT ir_module_post_commit_table_name_check; \
         ALTER TABLE ir_module_post_commit ADD CONSTRAINT ir_module_post_commit_table_name_check \
         CHECK (table_name = 'nothing')",
    )
    .await;
    let env = f.system.system_env(Context::new());
    let err = f
        .system
        .button_immediate_install(&env, &[n("test_large_ext")])
        .await
        .map(drop)
        .unwrap_err();
    let messages = chain(&err);
    assert!(
        !messages.iter().any(|m| m.contains(OLDER_BASE)),
        "{messages:?}"
    );
    assert!(
        messages
            .iter()
            .any(|m| m.contains("ir_module_post_commit_table_name_check")
                || m.contains("The table of a post-commit step")),
        "{messages:?}"
    );
    f.finish().await;
}

/// The constraint name of the business key `test_large_key` declares, from the registry.
fn reference_key(orm: &Orm) -> String {
    orm.registry()
        .model("test_large.item")
        .unwrap()
        .business_key("_reference_key")
        .unwrap()
        .full_name
        .clone()
}

/// `(index valid, constraint present)` of the business key `name` on the large table.
async fn key_state(orm: &Orm, name: &str) -> (Option<String>, Option<String>) {
    let row = q(
        orm,
        &format!(
            "SELECT (SELECT i.indisvalid::text FROM pg_index i \
                       JOIN pg_class c ON c.oid = i.indexrelid \
                      WHERE c.relname = '{name}'), \
                    (SELECT k.contype::text FROM pg_constraint k \
                       JOIN pg_class t ON t.oid = k.conrelid \
                      WHERE t.relname = '{TABLE}' AND k.conname = '{name}')"
        ),
    )
    .await
    .remove(0);
    (row[0].clone(), row[1].clone())
}

/// A business key added on a large table by a module operation ('s two steps, merged
/// with this queue): the operation commits without the index or the constraint, the queue holds the
/// two steps in build order under their own work names, and the post-commit phase plans each again
/// and runs it, the index before the constraint that adopts it. A second phase runs nothing.
#[tokio::test]
async fn a_business_key_on_a_large_table_is_queued_as_its_two_steps() {
    let f = fixture(large_options()).await;
    let env = f.system.system_env(Context::new());
    f.system
        .button_install(&env, &[n("test_large_key")], &InstallOptions::new())
        .await
        .unwrap();
    let loaded = f.system.load().await.unwrap();
    let forced = loaded.forced_update.as_ref().expect("a forced update");
    assert_eq!(forced.installed, vec![n("test_large_key")]);
    let orm = &loaded.orm;
    let name = reference_key(orm);

    // nothing was built in the operation; the two steps are queued, the index first
    assert_eq!(key_state(orm, &name).await, (None, None));
    let queued = schema_rows(orm).await;
    assert_eq!(
        queued,
        vec![
            (
                "test_large_key".to_owned(),
                "pending".to_owned(),
                format!("key_index.{name}")
            ),
            (
                "test_large_key".to_owned(),
                "pending".to_owned(),
                format!("key_constraint.{name}")
            ),
        ]
    );
    let works = q(
        orm,
        "SELECT spec::json->>'work' FROM ir_module_post_commit WHERE kind = 'schema_step' \
          ORDER BY id",
    )
    .await;
    assert_eq!(
        works,
        vec![
            vec![Some("create_key_index".to_owned())],
            vec![Some("add_key_constraint".to_owned())],
        ]
    );

    // the phase builds the index, then adds the constraint over it
    let report = bounded("phase", f.system.run_post_commit(orm)).await;
    assert!(report.is_clean(), "{report:?}");
    assert_eq!(
        keys_of(&report.done),
        vec![
            queue_key(&format!("key_constraint.{name}")),
            queue_key(&format!("key_index.{name}")),
        ]
    );
    assert_eq!(
        key_state(orm, &name).await,
        (Some("true".to_owned()), Some("u".to_owned()))
    );
    // the key is the step's work: re-planned now, nothing is left of either step
    for key in [
        format!("key_index.{name}"),
        format!("key_constraint.{name}"),
    ] {
        assert_eq!(
            orm.plan_schema_step(TABLE, &key, &large_options(), &[])
                .await
                .unwrap(),
            Replanned::Done,
            "{key}"
        );
    }
    assert_eq!(
        bounded("phase", f.system.run_post_commit(orm)).await,
        PostCommitReport::default()
    );
    f.finish().await;
}
