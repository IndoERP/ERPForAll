//! Large-table rule in schema sync: on a table above the row
//! threshold, index builds, constraint validation, NULL fills and stored-compute backfills leave
//! the schema transaction and run as [`SchemaStep`]s after the commit; below it they run inline.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod support;

use std::future::Future;
use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::Duration;

use erp_fields::ErrorKind;
use erp_orm::{
    Orm, Replanned, Resume, SUPERUSER_ID, SchemaStep, SchemaStepFailures, SchemaStepKind,
    SchemaStepReport, SchemaSyncOptions, Signal, SignalKey, StaticPolicy, StepCursor, StepProgress,
};
use erp_registry::ident::index_name;
use erp_registry::{AddonDescriptor, Registry, RegistryBuilder};
use erp_sdk::Transaction;
use erp_sql::ddl::PENDING_BACKFILL_COMMENT_PREFIX;
use erp_test_models::base_users;
use erp_test_models::test_large::{
    test_large_base, test_large_ext, test_large_index, test_large_req, test_large_required,
};
use erp_testkit::Harness;
use support::{one, rows};

const TABLE: &str = "test_large_item";
/// Rows seeded before the extension is synced; the large tests use a threshold below it.
const ROWS: i64 = 12;
/// How long a test lets a step or a sync run before failing: far above what the work takes on a
/// table of [`ROWS`] rows, well under the long lock timeout the lock tests set ([`LONG`]), so a
/// wait that the short timeout should have cut, or a loop that never ends, fails the test.
const BOUND: Duration = Duration::from_secs(30);
/// The short lock timeout of the lock tests.
const SHORT: Duration = Duration::from_millis(200);
/// The long lock timeout of the lock tests.
const LONG: Duration = Duration::from_secs(60);
/// PostgreSQL's message for a lock wait cut by `lock_timeout` (SQLSTATE `55P03`).
const LOCK_TIMEOUT_MESSAGE: &str = "canceling statement due to lock timeout";

/// The base addon on a fresh database, with [`ROWS`] items: odd ones have a quantity, even ones
/// a NULL quantity; every item has a name and a reference.
async fn seeded() -> Harness {
    let h = Harness::builder([&base_users::ADDON, &test_large_base::ADDON])
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    tx.execute(
        "INSERT INTO test_large_item (name, qty, reference) \
         SELECT 'item ' || g, CASE WHEN g % 2 = 1 THEN g END, 'r' || g \
           FROM generate_series(1, $1::int8) g",
        &[&ROWS],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    h
}

/// The registry of the base addon with `ext` on top.
fn registry_with(ext: &'static AddonDescriptor) -> Arc<Registry> {
    Arc::new(
        RegistryBuilder::new([&base_users::ADDON, &test_large_base::ADDON, ext])
            .build()
            .unwrap(),
    )
}

/// An ORM on the registry with the extension, on the harness's database.
fn extended(h: &Harness) -> Orm {
    Orm::builder(registry_with(&test_large_ext::ADDON), h.db().pool().clone())
        .policy(StaticPolicy::AllowAll)
        .build()
        .unwrap()
}

/// An ORM with signals on the registry with `ext`, on the harness's database: the
/// signal table installed, the generations it has applied those of the table now.
async fn signal_orm(h: &Harness, ext: &'static AddonDescriptor) -> Orm {
    let mut tx = h.begin().await.unwrap();
    tx.install_signal().await.unwrap();
    tx.commit().await.unwrap();
    let client = h.db().client().await.unwrap();
    let applied = Signal::read(&client).await.unwrap();
    drop(client);
    Orm::builder(registry_with(ext), h.db().pool().clone())
        .policy(StaticPolicy::AllowAll)
        .signal(Signal::new(applied))
        .build()
        .unwrap()
}

/// The registry generation in the signal table.
async fn registry_generation(h: &Harness) -> u64 {
    let client = h.db().client().await.unwrap();
    Signal::read(&client)
        .await
        .unwrap()
        .get(&SignalKey::Registry)
}

/// Options making every table with rows large, with the lock tests' timeouts.
fn lock_test_options() -> SchemaSyncOptions {
    SchemaSyncOptions::new()
        .large_table_rows(0)
        .step_lock_timeouts(SHORT, LONG)
        .unwrap()
}

/// The names of the indexes the extension adds, from the registry.
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

fn fk_name() -> String {
    erp_sql::ddl::foreign_key_name(TABLE, "owner_id")
}

/// `(kind, key)` of the steps, sorted.
fn kinds(steps: &[SchemaStep]) -> Vec<(SchemaStepKind, String)> {
    let mut out: Vec<_> = steps
        .iter()
        .map(|s| (s.kind(), s.key().to_owned()))
        .collect();
    out.sort_by(|a, b| a.1.cmp(&b.1));
    out
}

/// Every step the extension's sync defers on a large table.
fn all_deferred(orm: &Orm) -> Vec<(SchemaStepKind, String)> {
    let [code, owner, unique] = new_indexes(orm);
    let mut out = vec![
        (SchemaStepKind::CreateIndex, format!("index.{code}")),
        (SchemaStepKind::CreateIndex, format!("index.{owner}")),
        (SchemaStepKind::CreateIndex, format!("index.{unique}")),
        (
            SchemaStepKind::ValidateForeignKey,
            format!("validate.{}", fk_name()),
        ),
        (SchemaStepKind::NotNull, format!("not_null.{TABLE}.qty")),
        (
            SchemaStepKind::NotNull,
            format!("not_null.{TABLE}.reference"),
        ),
        (SchemaStepKind::Backfill, format!("backfill.{TABLE}")),
    ];
    out.sort_by(|a, b| a.1.cmp(&b.1));
    out
}

/// The step with `key`.
fn step_of(steps: &[SchemaStep], key: &str) -> SchemaStep {
    steps
        .iter()
        .find(|s| s.key() == key)
        .unwrap_or_else(|| panic!("no step {key} in {:?}", kinds(steps)))
        .clone()
}

/// `future`, failing the test when it is still running after [`BOUND`].
async fn bounded<T>(what: &str, future: impl Future<Output = T>) -> T {
    tokio::time::timeout(BOUND, future)
        .await
        .unwrap_or_else(|_| panic!("{what} still running after {BOUND:?}"))
}

/// Run `step` to its end: it succeeds and reports nothing.
async fn run(orm: &Orm, step: &SchemaStep) {
    let report = bounded(
        step.key(),
        orm.run_schema_step(step, &SchemaSyncOptions::new()),
    )
    .await
    .unwrap();
    assert_eq!(report, SchemaStepReport::default(), "{}", step.key());
}

/// Run every step, in order.
async fn run_all(orm: &Orm, steps: &[SchemaStep]) {
    for step in steps {
        run(orm, step).await;
    }
}

/// The original error a step error wraps.
fn cause(error: &erp_fields::Error) -> &erp_fields::Error {
    std::error::Error::source(error)
        .and_then(|source| source.downcast_ref::<erp_fields::Error>())
        .expect("a step error wraps the original error")
}

/// `error` is the `Lock` error of `key`, a lock wait over the timeout.
fn assert_lock_timeout(error: &erp_fields::Error, key: &str) {
    assert_lock_timeout_text(error, key, LOCK_TIMEOUT_MESSAGE.to_owned());
}

/// [`assert_lock_timeout`] for a wait inside the ORM's own statements, which name the limit the
/// transaction began with and say the step changed it since.
fn assert_typed_lock_timeout(error: &erp_fields::Error, key: &str) {
    let began_with = erp_orm::OrmConfig::default().timeouts.lock().as_millis();
    assert_lock_timeout_text(
        error,
        key,
        format!(
            "{LOCK_TIMEOUT_MESSAGE} (lock_timeout was {began_with} ms when the transaction began; \
             changed inside the transaction since)"
        ),
    );
    assert_eq!(
        cause(error).condition(),
        Some(erp_fields::Condition::LockTimeout)
    );
}

fn assert_lock_timeout_text(error: &erp_fields::Error, key: &str, cause_message: String) {
    assert_eq!(error.kind(), ErrorKind::Lock);
    assert_eq!(error.sqlstate(), Some("55P03"));
    assert_eq!(error.message(), format!("schema step '{key}' failed"));
    let cause = cause(error);
    assert_eq!(cause.kind(), ErrorKind::Lock);
    assert_eq!(cause.message(), cause_message);
}

/// Statements of the captured DDL batches that touch the item table.
fn item_statements(tx: &Transaction) -> Vec<String> {
    tx.captured()
        .iter()
        .flat_map(|batch| batch.split(";\n"))
        .filter(|s| s.contains(&format!("\"{TABLE}\"")))
        .map(str::to_owned)
        .collect()
}

/// The statements of [`item_statements`] that change something (reads left out: the size count,
/// the required-column probe).
fn item_ddl(tx: &Transaction) -> Vec<String> {
    item_statements(tx)
        .into_iter()
        .filter(|s| !s.trim_start().starts_with("SELECT"))
        .collect()
}

/// The end state once all work is done: indexes valid (the unique one with its declaration as
/// comment), the foreign key and both not-null constraints validated, NULL quantities filled
/// with the default, `name_size` computed for every row.
async fn assert_complete(tx: &mut Transaction, orm: &Orm) {
    for index in new_indexes(orm) {
        assert_eq!(
            one(
                tx,
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
    let unique = &new_indexes(orm)[2];
    assert_eq!(
        one(
            tx,
            &format!("SELECT obj_description('{unique}'::regclass, 'pg_class')")
        )
        .await
        .as_deref(),
        Some("UNIQUE INDEX (code)")
    );
    assert_eq!(
        one(
            tx,
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
        rows(
            tx,
            "SELECT a.attname::text, a.attnotnull::text, \
                    coalesce(bool_and(k.convalidated), false)::text \
               FROM pg_attribute a \
               LEFT JOIN pg_constraint k ON k.conrelid = a.attrelid AND k.contype = 'n' \
                    AND k.conkey[1] = a.attnum \
              WHERE a.attrelid = 'test_large_item'::regclass AND a.attname IN ('qty', 'reference') \
              GROUP BY 1, 2 ORDER BY 1"
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
    // even rows had no quantity: they get the field's default
    assert_eq!(
        one(
            tx,
            "SELECT count(*)::text FROM test_large_item WHERE qty = 7 AND id % 2 = 0"
        )
        .await
        .as_deref(),
        Some("6")
    );
    assert_all_computed(tx).await;
    // the backfill is finished: the column's comment is its label again
    assert_eq!(name_size_comment(tx).await, Some(name_size_label(orm)));
}

/// `name_size` is computed for every row.
async fn assert_all_computed(tx: &mut Transaction) {
    assert_eq!(
        one(
            tx,
            "SELECT count(*)::text FROM test_large_item \
             WHERE name_size IS DISTINCT FROM char_length(name)"
        )
        .await
        .as_deref(),
        Some("0")
    );
}

/// The label of the `name_size` field, from the registry.
fn name_size_label(orm: &Orm) -> String {
    orm.registry()
        .model("test_large.item")
        .unwrap()
        .field("name_size")
        .unwrap()
        .string
        .clone()
}

/// The comment of the `name_size` column.
async fn name_size_comment(tx: &mut Transaction) -> Option<String> {
    one(
        tx,
        "SELECT col_description('test_large_item'::regclass, a.attnum) FROM pg_attribute a \
          WHERE a.attrelid = 'test_large_item'::regclass AND a.attname = 'name_size'",
    )
    .await
}

/// Whether the `name_size` column is still marked as waiting for its compute backfill.
async fn backfill_pending(tx: &mut Transaction) -> bool {
    name_size_comment(tx)
        .await
        .is_some_and(|c| c.starts_with(PENDING_BACKFILL_COMMENT_PREFIX))
}

/// Run `sql` (statements without parameters) in its own committed transaction.
async fn exec(h: &Harness, sql: &str) {
    let mut tx = h.begin().await.unwrap();
    tx.batch(sql).await.unwrap();
    tx.commit().await.unwrap();
}

/// A `BEFORE UPDATE` trigger `name` on the item table running `body` (PL/pgSQL statements)
/// before `RETURN NEW`.
async fn add_trigger(h: &Harness, name: &str, body: &str) {
    exec(
        h,
        &format!(
            "CREATE FUNCTION {name}() RETURNS trigger LANGUAGE plpgsql AS $$ \
             BEGIN {body} RETURN NEW; END $$; \
             CREATE TRIGGER {name} BEFORE UPDATE ON test_large_item \
             FOR EACH ROW EXECUTE FUNCTION {name}()"
        ),
    )
    .await;
}

async fn drop_trigger(h: &Harness, name: &str) {
    exec(
        h,
        &format!("DROP TRIGGER {name} ON test_large_item; DROP FUNCTION {name}()"),
    )
    .await;
}

/// A trigger failing the update of row `id` once `column` is set: an interruption half-way
/// through a batched step.
async fn fail_update_of(h: &Harness, id: i64, column: &str) {
    add_trigger(
        h,
        "test_large_interrupt",
        &format!(
            "IF NEW.id = {id} AND to_jsonb(NEW) ->> '{column}' IS NOT NULL THEN \
               RAISE EXCEPTION 'interrupted at row {id}'; \
             END IF;"
        ),
    )
    .await;
}

/// A transaction holding the row lock of row `id` (an uncommitted update), until committed.
async fn lock_row(orm: &Orm, id: i64) -> Transaction {
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    tx.execute(
        "UPDATE test_large_item SET name = name WHERE id = $1",
        &[&id],
    )
    .await
    .unwrap();
    tx
}

/// A transaction holding the item table's `SHARE UPDATE EXCLUSIVE` lock (the lock `COMMENT`
/// takes; reads and writes go on), until committed.
async fn lock_table_for_comments(orm: &Orm) -> Transaction {
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    tx.batch("LOCK TABLE test_large_item IN SHARE UPDATE EXCLUSIVE MODE")
        .await
        .unwrap();
    tx
}

/// on a table above the threshold the schema transaction only does catalog work
/// (new columns, `NOT VALID` constraints); index builds, validations, the fill and the compute
/// backfill come back as steps, which run after the commit and leave the same schema and data as
/// an inline sync. A second sync then has nothing left to do, and the steps can run again.
#[tokio::test]
async fn large_table_work_is_deferred_to_steps() {
    let h = seeded().await;
    let orm = extended(&h);
    let options = SchemaSyncOptions::new().large_table_rows(10);
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    tx.capture_statements(true);
    let sync = tx.init_models_with(&[], &options).await.unwrap();
    let inside = item_statements(&tx);
    for heavy in [
        "CREATE INDEX",
        "CREATE UNIQUE INDEX",
        "VALIDATE CONSTRAINT",
        "UPDATE ",
        "SET NOT NULL",
    ] {
        assert!(
            !inside.iter().any(|s| s.contains(heavy)),
            "{heavy} inside the transaction: {inside:#?}"
        );
    }
    let fk = fk_name();
    assert!(
        inside
            .iter()
            .any(|s| s.contains(&format!("ADD CONSTRAINT \"{fk}\"")) && s.ends_with(" NOT VALID")),
        "{inside:#?}"
    );
    assert!(
        inside
            .iter()
            .any(|s| s.contains("NOT NULL \"reference\" NOT VALID")),
        "{inside:#?}"
    );
    assert_eq!(kinds(sync.steps()), all_deferred(&orm));
    for step in sync.steps() {
        assert_eq!(step.table(), TABLE);
    }
    // nothing computed yet: the backfill is a step
    assert_eq!(
        one(
            &mut tx,
            "SELECT count(*)::text FROM test_large_item WHERE name_size IS NULL"
        )
        .await
        .as_deref(),
        Some("12")
    );
    tx.commit().await.unwrap();

    run_all(&orm, sync.steps()).await;
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    assert_complete(&mut tx, &orm).await;
    // converged: a second sync plans nothing and changes nothing
    tx.capture_statements(true);
    let again = tx.init_models_with(&[], &options).await.unwrap();
    assert_eq!(kinds(again.steps()), vec![]);
    assert_eq!(item_statements(&tx), Vec::<String>::new());
    tx.commit().await.unwrap();
    // steps are idempotent
    run_all(&orm, sync.steps()).await;
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    assert_complete(&mut tx, &orm).await;
    drop(tx);
    drop(orm);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn small_table_work_runs_inline() {
    let h = seeded().await;
    let orm = extended(&h);
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    tx.capture_statements(true);
    let sync = tx
        .init_models_with(&[], &SchemaSyncOptions::new().large_table_rows(12))
        .await
        .unwrap();
    assert_eq!(kinds(sync.steps()), vec![]);
    let inside = item_statements(&tx);
    for wanted in [
        "CREATE INDEX",
        "CREATE UNIQUE INDEX",
        "VALIDATE CONSTRAINT",
        "SET NOT NULL",
    ] {
        assert!(
            inside.iter().any(|s| s.contains(wanted)),
            "{wanted} missing: {inside:#?}"
        );
    }
    assert_complete(&mut tx, &orm).await;
    tx.commit().await.unwrap();
    drop(orm);
    h.cleanup().await.unwrap();
}

/// `init_models` keeps running all work inline, whatever the table size.
#[tokio::test]
async fn init_models_runs_everything_inline() {
    let h = seeded().await;
    let orm = extended(&h);
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    tx.init_models(&[]).await.unwrap();
    assert_complete(&mut tx, &orm).await;
    drop(tx);
    drop(orm);
    h.cleanup().await.unwrap();
}

/// `Orm::sync_schema_with` commits the catalog work, then runs the steps (the backfill in
/// batches smaller than the table); nothing to report.
#[tokio::test]
async fn sync_schema_runs_the_steps_after_commit() {
    let h = seeded().await;
    let orm = extended(&h);
    let report = orm
        .sync_schema_with(
            &SchemaSyncOptions::new()
                .large_table_rows(0)
                .backfill_batch(NonZeroU32::new(5).unwrap())
                .fill_batch(NonZeroU32::new(4).unwrap()),
        )
        .await
        .unwrap();
    assert_eq!(report, SchemaStepReport::default());
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    assert_complete(&mut tx, &orm).await;
    drop(tx);
    drop(orm);
    h.cleanup().await.unwrap();
}

/// Work the catalog shows as unfinished (missing or invalid indexes, `NOT VALID` constraints,
/// NULLs in a required column, a column marked as waiting for its compute backfill) is planned
/// again by the next sync when the steps of the last one never ran (a crash after the commit).
#[tokio::test]
async fn unfinished_work_is_planned_again() {
    let h = seeded().await;
    let orm = extended(&h);
    let options = SchemaSyncOptions::new().large_table_rows(10);
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let first = tx.init_models_with(&[], &options).await.unwrap();
    tx.commit().await.unwrap();
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    assert!(
        backfill_pending(&mut tx).await,
        "{:?}",
        name_size_comment(&mut tx).await
    );
    let second = tx.init_models_with(&[], &options).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(kinds(second.steps()), all_deferred(&orm));
    // the first plan's steps finish the job
    run_all(&orm, first.steps()).await;
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    assert_complete(&mut tx, &orm).await;
    drop(tx);
    drop(orm);
    h.cleanup().await.unwrap();
}

/// A concurrent unique index build that fails (duplicates) fails its step with the database's
/// error kept as the source; PostgreSQL leaves an invalid index, which the next sync plans again
/// and the step drops before building it anew.
#[tokio::test]
async fn failed_index_build_is_reported_and_retried() {
    let h = seeded().await;
    let orm = extended(&h);
    let options = SchemaSyncOptions::new().large_table_rows(10);
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let sync = tx.init_models_with(&[], &options).await.unwrap();
    tx.execute(
        "UPDATE test_large_item SET code = 'dup' WHERE id IN (1, 2)",
        &[],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let unique = new_indexes(&orm)[2].clone();
    let step = step_of(sync.steps(), &format!("index.{unique}"));
    let err = orm.run_schema_step(&step, &options).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.message(),
        format!("schema step 'index.{unique}' failed")
    );
    assert_eq!(err.sqlstate(), Some("23505"));
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    assert_eq!(
        one(
            &mut tx,
            &format!(
                "SELECT i.indisvalid::text FROM pg_index i \
                 JOIN pg_class c ON c.oid = i.indexrelid WHERE c.relname = '{unique}'"
            )
        )
        .await
        .as_deref(),
        Some("false")
    );
    tx.execute("UPDATE test_large_item SET code = NULL WHERE id = 2", &[])
        .await
        .unwrap();
    let again = tx.init_models_with(&[], &options).await.unwrap();
    tx.commit().await.unwrap();
    assert!(
        again.steps().iter().any(|s| s.key() == step.key()),
        "{:?}",
        kinds(again.steps())
    );
    run_all(&orm, sync.steps()).await;
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    assert_complete(&mut tx, &orm).await;
    drop(tx);
    drop(orm);
    h.cleanup().await.unwrap();
}

/// A required column whose fill and validation wait for a step stays writable meanwhile: old rows
/// with a NULL can still be edited (the not-null constraint is only added by the step), and the
/// ORM writes the default for new rows.
#[tokio::test]
async fn rows_stay_writable_while_a_fill_is_pending() {
    let h = seeded().await;
    let orm = extended(&h);
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let sync = tx
        .init_models_with(&[], &SchemaSyncOptions::new().large_table_rows(10))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let items = tx.env().records("test_large.item").unwrap();
    let even = items.browse([2]).unwrap();
    even.set_value(&mut tx, "name", erp_sdk::Value::str("renamed"))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    run_all(&orm, sync.steps()).await;
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    assert_complete(&mut tx, &orm).await;
    drop(tx);
    drop(orm);
    h.cleanup().await.unwrap();
}

/// A backfill interrupted half-way (here a failing batch; a crash leaves the same state): the
/// committed batches stay, the sync's error lists the failed step as data, the column stays
/// marked in the catalog, and the next sync plans the backfill again and finishes it.
#[tokio::test]
async fn interrupted_backfill_is_planned_again() {
    let h = seeded().await;
    let orm = extended(&h);
    fail_update_of(&h, 7, "name_size").await;
    let options = SchemaSyncOptions::new()
        .large_table_rows(0)
        .backfill_batch(NonZeroU32::new(3).unwrap());
    let err = bounded("sync", orm.sync_schema_with(&options))
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Database);
    assert_eq!(err.sqlstate(), Some("P0001"));
    let total = all_deferred(&orm).len();
    assert_eq!(
        err.message(),
        format!(
            "the schema change is committed, but 1 of its {total} post-commit steps failed: \
             backfill.{TABLE}"
        )
    );
    let failures = SchemaStepFailures::of(&err).expect("failed steps as data");
    assert_eq!(failures.total(), total);
    assert_eq!(failures.report(), &SchemaStepReport::default());
    let failed: Vec<_> = failures
        .failures()
        .iter()
        .map(|f| (f.step().kind(), f.step().key().to_owned()))
        .collect();
    assert_eq!(
        failed,
        vec![(SchemaStepKind::Backfill, format!("backfill.{TABLE}"))]
    );
    let only = &failures.failures()[0];
    assert_eq!(only.error().kind(), ErrorKind::Database);
    assert_eq!(
        only.error().message(),
        format!("schema step 'backfill.{TABLE}' failed")
    );
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    // batches 1-3 and 4-6 committed, the batch of row 7 rolled back
    assert_eq!(
        rows(
            &mut tx,
            "SELECT id::text FROM test_large_item WHERE name_size IS NOT NULL \
             ORDER BY test_large_item.id"
        )
        .await,
        (1..=6)
            .map(|i| vec![Some(i.to_string())])
            .collect::<Vec<_>>()
    );
    assert!(backfill_pending(&mut tx).await);
    drop(tx);
    drop_trigger(&h, "test_large_interrupt").await;
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let again = tx.init_models_with(&[], &options).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(
        kinds(again.steps()),
        vec![(SchemaStepKind::Backfill, format!("backfill.{TABLE}"))]
    );
    run_all(&orm, again.steps()).await;
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    assert_complete(&mut tx, &orm).await;
    let done = tx.init_models_with(&[], &options).await.unwrap();
    assert_eq!(kinds(done.steps()), vec![]);
    drop(tx);
    drop(orm);
    h.cleanup().await.unwrap();
}

/// A pending backfill on a table that is small by the next sync (or a sync with
/// [`SchemaSyncOptions::inline`]) runs in that sync's transaction, which also clears the mark.
/// The deferred step run afterwards finds the label already in place: nothing to replace,
/// nothing to report.
#[tokio::test]
async fn pending_backfill_runs_inline_when_the_table_is_small() {
    let h = seeded().await;
    let orm = extended(&h);
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let deferred = tx
        .init_models_with(&[], &SchemaSyncOptions::new().large_table_rows(0))
        .await
        .unwrap();
    assert!(!deferred.is_complete());
    tx.commit().await.unwrap();
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    tx.init_models(&[]).await.unwrap();
    assert_complete(&mut tx, &orm).await;
    tx.commit().await.unwrap();
    run(
        &orm,
        &step_of(deferred.steps(), &format!("backfill.{TABLE}")),
    )
    .await;
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    assert_complete(&mut tx, &orm).await;
    drop(tx);
    drop(orm);
    h.cleanup().await.unwrap();
}

/// The not-null fill moves a cursor forward instead of looking for NULLs from the start again,
/// and ends with one closing pass from the start: a row it cannot fill (a trigger keeps it NULL)
/// is passed, the closing pass finds it NULL, and the step fails with its own message before it
/// adds the constraint; the rows around it are filled and the backfill, which a constraint would
/// have made fail on that row, computes every row. A fill that searched from the start again
/// without a cursor would pick that row up for ever (the test's bound catches it). Once the row
/// can be filled, the next sync finishes the fill.
#[tokio::test]
async fn interrupted_fill_resumes() {
    let h = seeded().await;
    let orm = extended(&h);
    add_trigger(
        &h,
        "test_large_keep_null",
        "IF NEW.id = 8 THEN NEW.qty := NULL; END IF;",
    )
    .await;
    let options = SchemaSyncOptions::new()
        .large_table_rows(0)
        .fill_batch(NonZeroU32::new(2).unwrap());
    let err = bounded("sync", orm.sync_schema_with(&options))
        .await
        .unwrap_err();
    let fill = format!("not_null.{TABLE}.qty");
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(err.sqlstate(), Some("23502"));
    assert_eq!(
        err.message(),
        format!(
            "the schema change is committed, but 1 of its {} post-commit steps failed: {fill}",
            all_deferred(&orm).len()
        )
    );
    let failures = SchemaStepFailures::of(&err).expect("failed steps as data");
    let failed: Vec<_> = failures
        .failures()
        .iter()
        .map(|f| (f.step().kind(), f.step().key().to_owned()))
        .collect();
    assert_eq!(failed, vec![(SchemaStepKind::NotNull, fill.clone())]);
    // the closing pass of the fill still finds row 8 NULL: the constraint is not added, and the
    // backfill, which a constraint would have made fail on the same row, computed every row
    let error = failures.failures()[0].error();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(error.sqlstate(), Some("23502"));
    assert_eq!(error.message(), format!("schema step '{fill}' failed"));
    assert_eq!(
        cause(error).message(),
        format!(
            "column {TABLE}.qty still has rows with NULL after the closing pass over the whole \
             table; its not-null constraint is not added or validated"
        )
    );
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    // every even row but 8 filled with the default, past row 8 too; row 7 had its own 7
    assert_eq!(
        rows(
            &mut tx,
            "SELECT id::text FROM test_large_item WHERE qty = 7 \
             ORDER BY test_large_item.id"
        )
        .await,
        [2, 4, 6, 7, 10, 12]
            .map(|id| vec![Some(id.to_string())])
            .to_vec()
    );
    assert_eq!(
        one(
            &mut tx,
            "SELECT count(*)::text FROM test_large_item WHERE qty IS NULL"
        )
        .await
        .as_deref(),
        Some("1")
    );
    assert_all_computed(&mut tx).await;
    assert!(!backfill_pending(&mut tx).await);
    drop(tx);
    drop_trigger(&h, "test_large_keep_null").await;
    let report = bounded("sync", orm.sync_schema_with(&options))
        .await
        .unwrap();
    assert_eq!(report, SchemaStepReport::default());
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    assert_complete(&mut tx, &orm).await;
    drop(tx);
    drop(orm);
    h.cleanup().await.unwrap();
}

/// A statement taking `ACCESS EXCLUSIVE` (adding the not-null constraint) gives up after the
/// short lock timeout with a `Lock` error — not after the long one, which would fail the test's
/// bound — while a concurrent index build (`SHARE UPDATE EXCLUSIVE`, writes go on) waits for a
/// writer longer than the short timeout.
#[tokio::test]
async fn step_lock_timeouts_follow_what_a_wait_holds_up() {
    let h = seeded().await;
    let orm = extended(&h);
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let options = lock_test_options();
    let sync = tx.init_models_with(&[], &options).await.unwrap();
    tx.commit().await.unwrap();

    // a reader holds ACCESS SHARE: the ACCESS EXCLUSIVE statement gives up
    let mut reader = orm.begin(SUPERUSER_ID).await.unwrap();
    one(&mut reader, "SELECT count(*)::text FROM test_large_item").await;
    let key = format!("not_null.{TABLE}.qty");
    let not_null = step_of(sync.steps(), &key);
    let err = bounded(&key, orm.run_schema_step(&not_null, &options))
        .await
        .unwrap_err();
    assert_lock_timeout(&err, &key);
    reader.commit().await.unwrap();

    // a writer holds ROW EXCLUSIVE for longer than the short timeout: the index build waits
    let mut writer = lock_row(&orm, 1).await;
    let index = step_of(sync.steps(), &format!("index.{}", new_indexes(&orm)[0]));
    let release = async {
        tokio::time::sleep(SHORT * 5).await;
        writer.commit().await.unwrap();
    };
    let (built, ()) = tokio::join!(orm.run_schema_step(&index, &options), release);
    assert_eq!(built.unwrap(), SchemaStepReport::default());

    // a zero timeout would mean "wait forever": refused
    let err = SchemaSyncOptions::new()
        .step_lock_timeouts(Duration::ZERO, Duration::from_secs(1))
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "schema step lock timeouts must be at least 1 ms and at most 2147483647 ms, got 0 ms \
         (short) and 1000 ms (long)"
    );
    run_all(&orm, sync.steps()).await;
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    assert_complete(&mut tx, &orm).await;
    drop(tx);
    drop(orm);
    h.cleanup().await.unwrap();
}

/// A not-null fill batch that waits for a row lock holds the rows it already filled, so it
/// gives up after the short timeout: the batch rolls back, the batches before it stay, the step
/// fails with a `Lock` error, and a later run finishes the fill.
#[tokio::test]
async fn fill_batch_gives_up_on_a_row_lock_after_the_short_timeout() {
    let h = seeded().await;
    let orm = extended(&h);
    let options = lock_test_options().fill_batch(NonZeroU32::new(2).unwrap());
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let sync = tx.init_models_with(&[], &options).await.unwrap();
    tx.commit().await.unwrap();
    let key = format!("not_null.{TABLE}.qty");
    let not_null = step_of(sync.steps(), &key);

    // the second batch (rows 6 and 8) waits for row 8
    let mut writer = lock_row(&orm, 8).await;
    let err = bounded(&key, orm.run_schema_step(&not_null, &options))
        .await
        .unwrap_err();
    assert_lock_timeout(&err, &key);
    writer.rollback().await.unwrap();
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    // the first batch (rows 2 and 4) stays filled; row 7 had its own 7
    assert_eq!(
        rows(
            &mut tx,
            "SELECT id::text FROM test_large_item WHERE qty = 7 \
             ORDER BY test_large_item.id"
        )
        .await,
        [2, 4, 7].map(|id| vec![Some(id.to_string())]).to_vec()
    );
    drop(tx);
    run_all(&orm, sync.steps()).await;
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    assert_complete(&mut tx, &orm).await;
    drop(tx);
    drop(orm);
    h.cleanup().await.unwrap();
}

/// A backfill batch takes the step's short lock timeout: its flush waiting for a row lock gives
/// up after it, the batch rolls back, the batches before it stay, the column stays marked, and
/// a later run finishes the backfill.
#[tokio::test]
async fn backfill_batch_gives_up_on_a_row_lock_after_the_short_timeout() {
    let h = seeded().await;
    let orm = extended(&h);
    let options = lock_test_options().backfill_batch(NonZeroU32::new(3).unwrap());
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let sync = tx.init_models_with(&[], &options).await.unwrap();
    tx.commit().await.unwrap();
    let key = format!("backfill.{TABLE}");
    let backfill = step_of(sync.steps(), &key);

    // the second batch (rows 4 to 6) waits for row 5
    let mut writer = lock_row(&orm, 5).await;
    let err = bounded(&key, orm.run_schema_step(&backfill, &options))
        .await
        .unwrap_err();
    assert_typed_lock_timeout(&err, &key);
    writer.rollback().await.unwrap();
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    assert_eq!(
        rows(
            &mut tx,
            "SELECT id::text FROM test_large_item WHERE name_size IS NOT NULL \
             ORDER BY test_large_item.id"
        )
        .await,
        (1..=3)
            .map(|i| vec![Some(i.to_string())])
            .collect::<Vec<_>>()
    );
    assert!(backfill_pending(&mut tx).await);
    drop(tx);
    run_all(&orm, sync.steps()).await;
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    assert_complete(&mut tx, &orm).await;
    drop(tx);
    drop(orm);
    h.cleanup().await.unwrap();
}

/// Replacing the pending mark takes `SHARE UPDATE EXCLUSIVE` (the lock `COMMENT` takes), a wait
/// that holds no reader or writer up, so it waits up to the step's long timeout. Behind a session
/// holding that lock past the long timeout the step fails with a `Lock` error: every record is
/// computed, the column stays marked, the next sync plans the backfill again. A session that
/// releases the lock between the short and the long timeout only delays the step, which then
/// finishes (with the short timeout it would fail).
#[tokio::test]
async fn backfill_mark_replacement_waits_up_to_the_long_timeout() {
    let h = seeded().await;
    let orm = extended(&h);
    let long = Duration::from_secs(3);
    let options = SchemaSyncOptions::new()
        .large_table_rows(0)
        .backfill_batch(NonZeroU32::new(5).unwrap())
        .step_lock_timeouts(SHORT, long)
        .unwrap();
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let sync = tx.init_models_with(&[], &options).await.unwrap();
    tx.commit().await.unwrap();
    // a committed transaction keeps its connection: give it back (the test pool has four)
    drop(tx);
    let key = format!("backfill.{TABLE}");
    let backfill = step_of(sync.steps(), &key);

    // held past the long timeout: the step gives up
    let mut holder = lock_table_for_comments(&orm).await;
    let err = bounded(&key, orm.run_schema_step(&backfill, &options))
        .await
        .unwrap_err();
    assert_lock_timeout(&err, &key);
    holder.commit().await.unwrap();
    drop(holder);
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    assert_all_computed(&mut tx).await;
    assert!(backfill_pending(&mut tx).await);
    let again = tx.init_models_with(&[], &options).await.unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    assert!(
        again.steps().iter().any(|s| s.key() == key),
        "{:?}",
        kinds(again.steps())
    );

    // released after five short timeouts, well inside the long one: the step waits and finishes
    let mut holder = lock_table_for_comments(&orm).await;
    let release = async {
        tokio::time::sleep(SHORT * 5).await;
        holder.commit().await.unwrap();
    };
    let (replaced, ()) = bounded(&key, async {
        tokio::join!(orm.run_schema_step(&backfill, &options), release)
    })
    .await;
    assert_eq!(replaced.unwrap(), SchemaStepReport::default());
    drop(holder);
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    assert_eq!(
        name_size_comment(&mut tx).await,
        Some(name_size_label(&orm))
    );
    drop(tx);
    run_all(&orm, sync.steps()).await;
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    assert_complete(&mut tx, &orm).await;
    drop(tx);
    drop(orm);
    h.cleanup().await.unwrap();
}

/// A column comment changed outside the ORM while the backfill ran (the pending mark is gone) is
/// kept, never overwritten by the label, and reported; a comment removed altogether too. The
/// column is no longer marked, so the next sync plans no backfill for it.
#[tokio::test]
async fn backfill_keeps_a_comment_changed_meanwhile() {
    let h = seeded().await;
    let orm = extended(&h);
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let options = SchemaSyncOptions::new().large_table_rows(0);
    let sync = tx.init_models_with(&[], &options).await.unwrap();
    tx.commit().await.unwrap();
    let key = format!("backfill.{TABLE}");
    let backfill = step_of(sync.steps(), &key);

    for comment in [Some("kept by the DBA, ünïcode"), None] {
        let sql = match comment {
            Some(text) => format!(
                "COMMENT ON COLUMN test_large_item.name_size IS {}",
                erp_sql::literal(text)
            ),
            None => "COMMENT ON COLUMN test_large_item.name_size IS NULL".to_owned(),
        };
        exec(&h, &sql).await;
        let report = bounded(&key, orm.run_schema_step(&backfill, &options))
            .await
            .unwrap();
        let kept: Vec<_> = report
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
                comment.map(str::to_owned)
            )]
        );
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        assert_eq!(
            name_size_comment(&mut tx).await.as_deref(),
            comment,
            "the comment is kept"
        );
        assert_all_computed(&mut tx).await;
        drop(tx);
    }
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let again = tx.init_models_with(&[], &options).await.unwrap();
    assert!(
        again.steps().iter().all(|s| s.key() != key),
        "{:?}",
        kinds(again.steps())
    );
    drop(tx);
    drop(orm);
    h.cleanup().await.unwrap();
}

/// with the large-table rule: a sync whose only change is an index on a large table
/// runs no DDL in its transaction (the index is a step), yet bumps the registry generation there,
/// so other processes take the registry that declares the index before the step runs. A restart
/// once the schema is up to date bumps nothing.
#[tokio::test]
async fn index_step_alone_bumps_the_registry() {
    let h = seeded().await;
    let orm = signal_orm(&h, &test_large_index::ADDON).await;
    let options = SchemaSyncOptions::new().large_table_rows(0);
    assert_eq!(registry_generation(&h).await, 0);
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    tx.capture_statements(true);
    let sync = tx.init_models_with(&[], &options).await.unwrap();
    assert_eq!(item_ddl(&tx), Vec::<String>::new());
    assert_eq!(
        kinds(sync.steps()),
        vec![(
            SchemaStepKind::CreateIndex,
            format!("index.{}", index_name(TABLE, "name"))
        )]
    );
    tx.commit().await.unwrap();
    drop(tx);
    assert_eq!(registry_generation(&h).await, 1, "the change is signalled");
    run_all(&orm, sync.steps()).await;
    // up to date: a restart plans nothing and signals nothing
    let report = bounded("sync", orm.sync_schema_with(&options))
        .await
        .unwrap();
    assert_eq!(report, SchemaStepReport::default());
    assert_eq!(registry_generation(&h).await, 1);
    drop(orm);
    h.cleanup().await.unwrap();
}

/// with the large-table rule: a sync whose only change makes a column of a large table
/// required with a default runs no DDL in its transaction (fill, constraint and validation are a
/// step), yet bumps the registry generation there, so other processes write the default before
/// the step's constraint refuses NULLs. A step that failed before it changed the catalog is
/// planned again as a change (one more signal); a restart once up to date signals nothing.
#[tokio::test]
async fn not_null_step_alone_bumps_the_registry() {
    let h = seeded().await;
    let orm = signal_orm(&h, &test_large_required::ADDON).await;
    add_trigger(
        &h,
        "test_large_keep_null",
        "IF NEW.id = 8 THEN NEW.qty := NULL; END IF;",
    )
    .await;
    let options = SchemaSyncOptions::new().large_table_rows(0);
    assert_eq!(registry_generation(&h).await, 0);
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    tx.capture_statements(true);
    let sync = tx.init_models_with(&[], &options).await.unwrap();
    assert_eq!(item_ddl(&tx), Vec::<String>::new());
    let key = format!("not_null.{TABLE}.qty");
    assert_eq!(
        kinds(sync.steps()),
        vec![(SchemaStepKind::NotNull, key.clone())]
    );
    tx.commit().await.unwrap();
    drop(tx);
    assert_eq!(registry_generation(&h).await, 1, "the change is signalled");

    // the closing pass leaves the row kept NULL: the step fails before it adds the constraint
    let err = bounded(
        &key,
        orm.run_schema_step(&step_of(sync.steps(), &key), &options),
    )
    .await
    .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(err.sqlstate(), Some("23502"));
    assert_eq!(err.message(), format!("schema step '{key}' failed"));
    assert_eq!(cause(&err).kind(), ErrorKind::Validation);
    assert_eq!(cause(&err).sqlstate(), Some("23502"));
    assert_eq!(
        cause(&err).message(),
        format!(
            "column {TABLE}.qty still has rows with NULL after the closing pass over the whole \
             table; its not-null constraint is not added or validated"
        )
    );
    drop_trigger(&h, "test_large_keep_null").await;

    // nothing of the step is in the catalog (the constraint was not added), so planning it
    // again counts as a change: one more signal. Then it finishes.
    let report = bounded("sync", orm.sync_schema_with(&options))
        .await
        .unwrap();
    assert_eq!(report, SchemaStepReport::default());
    assert_eq!(registry_generation(&h).await, 2);
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    assert_eq!(
        rows(
            &mut tx,
            "SELECT a.attnotnull::text, k.convalidated::text FROM pg_attribute a \
               JOIN pg_constraint k ON k.conrelid = a.attrelid AND k.contype = 'n' \
                    AND k.conkey[1] = a.attnum \
              WHERE a.attrelid = 'test_large_item'::regclass AND a.attname = 'qty'"
        )
        .await,
        vec![vec![Some("true".to_owned()), Some("true".to_owned())]]
    );
    assert_eq!(
        one(
            &mut tx,
            "SELECT count(*)::text FROM test_large_item WHERE qty = 7 AND id % 2 = 0"
        )
        .await
        .as_deref(),
        Some("6")
    );
    drop(tx);
    // up to date: a restart signals nothing
    let report = bounded("sync", orm.sync_schema_with(&options))
        .await
        .unwrap();
    assert_eq!(report, SchemaStepReport::default());
    assert_eq!(registry_generation(&h).await, 2);
    drop(orm);
    h.cleanup().await.unwrap();
}

// ----: plan one step again, resume a batched step ---------------------------

/// A step planned again from the registry and the catalog after the schema transaction committed
/// equals the step the transaction returned (it is the same work: the lock timeouts that used to
/// be part of a step are options now), and once the work is done nothing is planned: a queue that
/// stores a step as a reference can run `plan_schema_step` and trust nothing it stored. A key the
/// registry does not declare is told apart from finished work.
#[tokio::test]
async fn a_step_planned_again_is_the_step_planned_first() {
    let h = seeded().await;
    let orm = extended(&h);
    let options = lock_test_options();
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let sync = tx.init_models_with(&[], &options).await.unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    assert_eq!(sync.steps().len(), all_deferred(&orm).len());
    for step in sync.steps() {
        // other timeouts, same step
        let planned = orm
            .plan_schema_step(
                step.table(),
                step.key(),
                &SchemaSyncOptions::new(),
                &step.backfill_fields(),
            )
            .await
            .unwrap();
        assert_eq!(planned, Replanned::Pending(step.clone()), "{}", step.key());
    }
    // keys the registry does not declare
    for (key, what) in [
        (
            "index.not_an_index",
            "no declared index not_an_index on table test_large_item",
        ),
        (
            "validate.not_a_key",
            "no declared foreign key not_a_key on table test_large_item",
        ),
        (
            "not_null.test_large_item.name",
            "column test_large_item.name is not a required field",
        ),
    ] {
        assert_eq!(
            orm.plan_schema_step(TABLE, key, &options, &[])
                .await
                .unwrap(),
            Replanned::NotDeclared {
                what: what.to_owned()
            },
            "{key}"
        );
    }
    run_all(&orm, sync.steps()).await;
    // finished work is told apart from a field the registry does not have
    assert_eq!(
        orm.plan_schema_step(TABLE, "backfill.test_large_item", &options, &["nothing"])
            .await
            .unwrap(),
        Replanned::NotDeclared {
            what: "none of the stored computed fields [nothing] is declared on table \
                   test_large_item"
                .to_owned()
        }
    );
    for step in sync.steps() {
        assert_eq!(
            orm.plan_schema_step(step.table(), step.key(), &options, &step.backfill_fields())
                .await
                .unwrap(),
            Replanned::Done,
            "{} is finished",
            step.key()
        );
    }
    drop(orm);
    h.cleanup().await.unwrap();
}

/// Planning a step on a table no model of the registry has is an error, not "nothing left": with
/// the registry from before the change the answer `None` would look like finished work.
#[tokio::test]
async fn planning_a_step_on_an_unknown_table_is_refused() {
    let h = seeded().await;
    let orm = extended(&h);
    let err = orm
        .plan_schema_step("no_such_table", "index.x", &SchemaSyncOptions::new(), &[])
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        format!(
            "schema step 'index.x' works on table 'no_such_table', which no model of this \
             registry ({} models) has: plan it with the ORM of the current module set",
            orm.registry().models().len()
        )
    );
    drop(orm);
    h.cleanup().await.unwrap();
}

/// A [`StepProgress`] that records the cursors it is told and fails on the `fail_at`th.
#[derive(Default)]
struct Recorder {
    cursors: Vec<StepCursor>,
    fail_at: Option<usize>,
}

impl StepProgress for Recorder {
    async fn committed(&mut self, cursor: StepCursor) -> erp_fields::Result<()> {
        self.cursors.push(cursor);
        if self.fail_at == Some(self.cursors.len()) {
            return Err(erp_fields::Error::new(
                ErrorKind::Database,
                format!("cannot store cursor {}", cursor.get()),
            ));
        }
        Ok(())
    }
}

/// Ids of the rows with a NULL in `column`, ascending.
async fn null_ids(orm: &Orm, column: &str) -> Vec<i64> {
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let ids = rows(
        &mut tx,
        &format!(
            "SELECT id::text FROM test_large_item WHERE {column} IS NULL \
         ORDER BY test_large_item.id"
        ),
    )
    .await
    .into_iter()
    .map(|r| r[0].as_deref().unwrap().parse().unwrap())
    .collect();
    drop(tx);
    ids
}

/// The cursors a run of batches of `size` over `ids` reports: the last id of each full batch.
fn expected_cursors(ids: &[i64], size: usize) -> Vec<i64> {
    ids.chunks(size)
        .filter(|chunk| chunk.len() == size)
        .map(|chunk| *chunk.last().unwrap())
        .collect()
}

/// A not-null fill and a compute backfill tell the progress sink the last id of each committed
/// batch. When the sink fails, the step stops with its error and the batches already committed
/// stay; started again with that cursor it continues from there. Ids are assigned at insert, so a
/// row can hold NULL behind the cursor: both steps end with a closing pass from the start that
/// repairs it. A cursor stored for other work (another fingerprint) is not resumed.
#[tokio::test]
async fn batched_steps_report_a_cursor_and_resume_from_it() {
    let h = seeded().await;
    let orm = extended(&h);
    let options = SchemaSyncOptions::new()
        .large_table_rows(0)
        .fill_batch(NonZeroU32::new(2).unwrap())
        .backfill_batch(NonZeroU32::new(3).unwrap());
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let sync = tx.init_models_with(&[], &options).await.unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let fill_key = format!("not_null.{TABLE}.qty");
    let fill = step_of(sync.steps(), &fill_key);
    let backfill_key = format!("backfill.{TABLE}");
    let backfill = step_of(sync.steps(), &backfill_key);
    let fill_ids = null_ids(&orm, "qty").await;
    let backfill_ids = null_ids(&orm, "name_size").await;
    assert!(fill_ids.len() > 4 && backfill_ids.len() > 6);

    // the sink fails after the second batch of the fill
    let mut recorder = Recorder {
        fail_at: Some(2),
        ..Recorder::default()
    };
    let err = bounded(
        &fill_key,
        orm.run_schema_step_from(&fill, &options, None, &mut recorder),
    )
    .await
    .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Database);
    assert_eq!(err.message(), format!("schema step '{fill_key}' failed"));
    let last = recorder.cursors.last().copied().unwrap();
    assert_eq!(
        cause(&err).message(),
        format!("cannot store cursor {}", last.get())
    );
    assert_eq!(
        recorder.cursors.iter().map(|c| c.get()).collect::<Vec<_>>(),
        expected_cursors(&fill_ids, 2)[..2]
    );
    // two batches committed: exactly the rows up to the last cursor are filled
    let still_null = null_ids(&orm, "qty").await;
    assert_eq!(
        still_null,
        fill_ids
            .iter()
            .copied()
            .filter(|id| *id > last.get())
            .collect::<Vec<_>>()
    );

    // a row behind the cursor lost its value again: the resumed run fills what is after the
    // cursor, and its closing pass fills that row and the constraint validates
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    tx.execute(
        "UPDATE test_large_item SET qty = NULL WHERE id = $1",
        &[&fill_ids[0]],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let resume = Resume::new(fill.fingerprint(), last);
    let mut resumed = Recorder::default();
    let report = bounded(
        &fill_key,
        orm.run_schema_step_from(&fill, &options, Some(&resume), &mut resumed),
    )
    .await
    .unwrap();
    assert_eq!(report, SchemaStepReport::default());
    let after_last: Vec<i64> = fill_ids
        .iter()
        .copied()
        .filter(|id| *id > last.get())
        .collect();
    assert_eq!(
        resumed.cursors.iter().map(|c| c.get()).collect::<Vec<_>>(),
        expected_cursors(&after_last, 2)
    );
    assert_eq!(null_ids(&orm, "qty").await, Vec::<i64>::new());

    // the backfill reports its batches the same way
    let mut recorder = Recorder {
        fail_at: Some(1),
        ..Recorder::default()
    };
    let err = bounded(
        &backfill_key,
        orm.run_schema_step_from(&backfill, &options, None, &mut recorder),
    )
    .await
    .unwrap_err();
    assert_eq!(
        err.message(),
        format!("schema step '{backfill_key}' failed")
    );
    let first = recorder.cursors[0];
    assert_eq!(first.get(), expected_cursors(&backfill_ids, 3)[0]);
    assert_eq!(
        null_ids(&orm, "name_size").await,
        backfill_ids
            .iter()
            .copied()
            .filter(|id| *id > first.get())
            .collect::<Vec<_>>()
    );
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    tx.execute(
        "UPDATE test_large_item SET name_size = NULL WHERE id = $1",
        &[&backfill_ids[0]],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let resume = Resume::new(backfill.fingerprint(), first);
    let mut resumed = Recorder::default();
    let report = bounded(
        &backfill_key,
        orm.run_schema_step_from(&backfill, &options, Some(&resume), &mut resumed),
    )
    .await
    .unwrap();
    assert_eq!(report, SchemaStepReport::default());
    let after_first: Vec<i64> = backfill_ids
        .iter()
        .copied()
        .filter(|id| *id > first.get())
        .collect();
    assert_eq!(
        resumed.cursors.iter().map(|c| c.get()).collect::<Vec<_>>(),
        expected_cursors(&after_first, 3)
    );
    // the closing pass computed the row behind the cursor
    assert_eq!(null_ids(&orm, "name_size").await, Vec::<i64>::new());

    // a cursor stored for other work is not resumed: every row is NULL again, the run starts at
    // the beginning although the cursor is past the middle
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    tx.batch("UPDATE test_large_item SET name_size = NULL")
        .await
        .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let foreign = Resume::new(fill.fingerprint(), StepCursor::new(9).unwrap());
    let mut restarted = Recorder::default();
    let report = bounded(
        &backfill_key,
        orm.run_schema_step_from(&backfill, &options, Some(&foreign), &mut restarted),
    )
    .await
    .unwrap();
    assert_eq!(report, SchemaStepReport::default());
    assert_eq!(
        restarted
            .cursors
            .iter()
            .map(|c| c.get())
            .collect::<Vec<_>>(),
        expected_cursors(&backfill_ids, 3)
    );
    assert_eq!(null_ids(&orm, "name_size").await, Vec::<i64>::new());
    drop(orm);
    h.cleanup().await.unwrap();
}

/// A not-null fill whose closing pass leaves NULLs (a trigger keeps one row NULL) fails with its
/// own message and neither adds nor validates the constraint.
#[tokio::test]
async fn a_not_null_fill_that_leaves_nulls_fails_before_the_constraint() {
    let h = seeded().await;
    let orm = extended(&h);
    let options = SchemaSyncOptions::new().large_table_rows(0);
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let sync = tx.init_models_with(&[], &options).await.unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    add_trigger(
        &h,
        "test_large_keep_null",
        "IF NEW.id = 8 THEN NEW.qty := NULL; END IF;",
    )
    .await;
    let key = format!("not_null.{TABLE}.qty");
    let err = bounded(
        &key,
        orm.run_schema_step(&step_of(sync.steps(), &key), &options),
    )
    .await
    .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(err.sqlstate(), Some("23502"));
    assert_eq!(err.message(), format!("schema step '{key}' failed"));
    assert_eq!(cause(&err).kind(), ErrorKind::Validation);
    assert_eq!(cause(&err).sqlstate(), Some("23502"));
    assert_eq!(
        cause(&err).message(),
        format!(
            "column {TABLE}.qty still has rows with NULL after the closing pass over the whole \
             table; its not-null constraint is not added or validated"
        )
    );
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    assert_eq!(
        one(
            &mut tx,
            "SELECT count(*)::text FROM pg_constraint WHERE contype = 'n' \
             AND conrelid = 'test_large_item'::regclass AND conkey[1] = \
             (SELECT attnum FROM pg_attribute WHERE attrelid = 'test_large_item'::regclass \
               AND attname = 'qty')"
        )
        .await
        .as_deref(),
        Some("0")
    );
    drop(tx);
    drop(orm);
    h.cleanup().await.unwrap();
}

/// Re-planning runs no DDL: another transaction that replaced the `unaccent` wrapper and stays
/// open does not make `plan_schema_step` wait (the sync's own `CREATE OR REPLACE FUNCTION` would).
#[tokio::test]
async fn planning_a_step_again_does_not_touch_the_unaccent_wrapper() {
    let h = seeded().await;
    h.db()
        .client()
        .await
        .unwrap()
        .batch_execute("CREATE EXTENSION IF NOT EXISTS unaccent")
        .await
        .unwrap();
    let orm = Orm::builder(registry_with(&test_large_ext::ADDON), h.db().pool().clone())
        .policy(StaticPolicy::AllowAll)
        .config(erp_orm::OrmConfig {
            unaccent: true,
            ..erp_orm::OrmConfig::default()
        })
        .build()
        .unwrap();
    let options = SchemaSyncOptions::new().large_table_rows(0);
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let sync = tx.init_models_with(&[], &options).await.unwrap();
    let schema = one(&mut tx, "SELECT current_schema()::text").await.unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    // another transaction replaces the wrapper and stays open
    let mut other = orm.begin(SUPERUSER_ID).await.unwrap();
    other
        .batch(&erp_sql::ddl::create_unaccent_wrapper(&schema))
        .await
        .unwrap();
    let step = step_of(sync.steps(), &format!("index.{}", new_indexes(&orm)[0]));
    let planned = bounded(
        "plan",
        orm.plan_schema_step(step.table(), step.key(), &options, &[]),
    )
    .await
    .unwrap();
    assert_eq!(planned, Replanned::Pending(step));
    other.rollback().await.unwrap();
    drop(other);
    drop(orm);
    h.cleanup().await.unwrap();
}

/// A step cursor is a record id.
#[test]
fn a_step_cursor_is_a_positive_record_id() {
    for bad in [0, -1, i64::MIN] {
        let err = StepCursor::new(bad).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Programming);
        assert_eq!(
            err.message(),
            format!("a step cursor is the id of a record, which is positive, not {bad}")
        );
    }
    assert_eq!(StepCursor::new(i64::MAX).unwrap().get(), i64::MAX);
}

/// A backfill whose closing pass leaves NULLs in a *required* stored computed field fails and keeps
/// the pending mark of the column comment. (A required column cannot be added to a table with rows,
/// and its constraint keeps NULLs out: the state is made by hand: the constraint dropped, rows
/// inserted without a value, the mark set, and a trigger that keeps the value NULL.)
#[tokio::test]
async fn a_backfill_that_leaves_nulls_in_a_required_field_fails_and_keeps_the_mark() {
    let h = Harness::builder([
        &base_users::ADDON,
        &test_large_base::ADDON,
        &test_large_req::ADDON,
    ])
    .build()
    .await
    .unwrap();
    let registry = Arc::new(
        RegistryBuilder::new([
            &base_users::ADDON,
            &test_large_base::ADDON,
            &test_large_req::ADDON,
        ])
        .build()
        .unwrap(),
    );
    let orm = Orm::builder(registry, h.db().pool().clone())
        .policy(StaticPolicy::AllowAll)
        .build()
        .unwrap();
    let label = orm
        .registry()
        .model("test_large.item")
        .unwrap()
        .field("name_size3")
        .unwrap()
        .string
        .clone();
    exec(
        &h,
        &format!(
            "ALTER TABLE test_large_item ALTER COLUMN name_size3 DROP NOT NULL; \
             INSERT INTO test_large_item (name) SELECT 'item ' || g FROM generate_series(1, 5) g; \
             {}",
            erp_sql::ddl::comment_pending_backfill(TABLE, "name_size3", &label)
        ),
    )
    .await;
    add_trigger(&h, "test_large_keep_null3", "NEW.name_size3 := NULL;").await;
    let options = SchemaSyncOptions::new().large_table_rows(0);
    let key = format!("backfill.{TABLE}");
    let Replanned::Pending(step) = orm
        .plan_schema_step(TABLE, &key, &options, &["name_size3"])
        .await
        .unwrap()
    else {
        panic!("the marked backfill is pending");
    };
    let err = bounded(&key, orm.run_schema_step(&step, &options))
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(err.sqlstate(), Some("23502"));
    assert_eq!(err.message(), format!("schema step '{key}' failed"));
    assert_eq!(cause(&err).kind(), ErrorKind::Validation);
    assert_eq!(cause(&err).sqlstate(), Some("23502"));
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let first = one(&mut tx, "SELECT min(id)::text FROM test_large_item")
        .await
        .unwrap();
    assert_eq!(
        cause(&err).message(),
        format!(
            "required stored computed field(s) name_size3 of {TABLE} still have NULL rows after \
             the closing pass over the whole table (first: id {first}); the pending marks of \
             their column comments are kept"
        )
    );
    let comment = one(
        &mut tx,
        "SELECT col_description('test_large_item'::regclass, a.attnum) FROM pg_attribute a \
          WHERE a.attrelid = 'test_large_item'::regclass AND a.attname = 'name_size3'",
    )
    .await
    .unwrap();
    assert!(
        comment.starts_with(PENDING_BACKFILL_COMMENT_PREFIX),
        "{comment}"
    );
    drop(tx);
    drop(orm);
    h.cleanup().await.unwrap();
}

/// The re-plan's NULL probe (for a recorded field whose pending mark is gone) fails when another
/// transaction holds the table (`ACCESS EXCLUSIVE`) for longer than the re-plan's lock timeout. The
/// error comes through as it is: restoring the statement timeout after a failed probe would only
/// fail with 25P02 ("current transaction is aborted") and hide it. (The step's statement timeout
/// cannot be lowered to make the probe time out with 57014, so a lock wait stands in for the
/// failing probe; it takes the 10 s of `SCHEMA_LOCK_TIMEOUT`.)
#[tokio::test]
async fn a_failing_null_probe_of_a_replan_comes_through() {
    let h = seeded().await;
    let orm = extended(&h);
    let options = SchemaSyncOptions::new().large_table_rows(0);
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let _sync = tx.init_models_with(&[], &options).await.unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    // the pending mark is gone, so the recorded field is looked for by its NULL rows
    exec(
        &h,
        "COMMENT ON COLUMN test_large_item.name_size IS 'changed outside the ORM'",
    )
    .await;
    let mut holder = orm.begin(SUPERUSER_ID).await.unwrap();
    holder
        .batch("LOCK TABLE test_large_item IN ACCESS EXCLUSIVE MODE")
        .await
        .unwrap();
    let key = format!("backfill.{TABLE}");
    let err = bounded(
        "plan",
        orm.plan_schema_step(TABLE, &key, &options, &["name_size"]),
    )
    .await
    .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Lock);
    assert_eq!(err.sqlstate(), Some("55P03"));
    // the re-plan set the schema limits inside its transaction: the typed error names the limit
    // the transaction began with and says it changed since (as for the schema lock)
    let began_with = erp_orm::OrmConfig::default().timeouts.lock().as_millis();
    assert_eq!(
        err.message(),
        format!(
            "{LOCK_TIMEOUT_MESSAGE} (lock_timeout was {began_with} ms when the transaction began; \
             changed inside the transaction since)"
        )
    );
    assert_eq!(err.condition(), Some(erp_fields::Condition::LockTimeout));
    holder.rollback().await.unwrap();
    drop(holder);
    // once the table is free the same call plans the backfill
    let planned = bounded(
        "plan",
        orm.plan_schema_step(TABLE, &key, &options, &["name_size"]),
    )
    .await
    .unwrap();
    assert!(matches!(planned, Replanned::Pending(_)), "{planned:?}");
    drop(orm);
    h.cleanup().await.unwrap();
}
