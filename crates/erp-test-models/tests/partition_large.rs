//! Partitioned tables above the large-table threshold (design §7.10): PostgreSQL
//! refuses `CREATE INDEX CONCURRENTLY` and `ADD CONSTRAINT … USING INDEX` on a partitioned parent,
//! so an index is built `ON ONLY` the parent, per partition, then attached, and a unique
//! constraint object is refused in the install path.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod support;

use std::sync::Arc;

use erp_fields::ErrorKind;
use erp_orm::{
    Orm, SUPERUSER_ID, SchemaStep, SchemaStepKind, SchemaStepReport, SchemaSyncOptions,
    StaticPolicy,
};
use erp_registry::ident::index_name;
use erp_registry::{AddonDescriptor, RegistryBuilder};
use erp_sdk::Transaction;
use erp_test_models::base_users;
use erp_test_models::test_partition_large::{
    test_partition_large_base, test_partition_large_excl, test_partition_large_index,
    test_partition_large_unique,
};
use erp_testkit::Harness;
use support::{one, rows};

/// Rows seeded before the extension is synced; the large tests use a threshold below it.
const ROWS: i64 = 12;

/// The base addon on a fresh database with [`ROWS`] rows in `scale_big` and `scale_slot`.
async fn seeded() -> Harness {
    let h = Harness::builder([&base_users::ADDON, &test_partition_large_base::ADDON])
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    tx.execute(
        "INSERT INTO scale_big (name, create_date) \
         SELECT 'n' || g, now() AT TIME ZONE 'UTC' FROM generate_series(1, $1::int8) g",
        &[&ROWS],
    )
    .await
    .unwrap();
    tx.execute(
        "INSERT INTO scale_slot (recorded_at, resource, start, stop) \
         SELECT now() AT TIME ZONE 'UTC', g, now() AT TIME ZONE 'UTC', \
                (now() AT TIME ZONE 'UTC') + interval '1 hour' FROM generate_series(1, $1::int8) g",
        &[&ROWS],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    h
}

/// An ORM on the registry of the base addon with `ext` on top, on the harness's database.
fn extended(h: &Harness, ext: &'static AddonDescriptor) -> Orm {
    let registry = Arc::new(
        RegistryBuilder::new([&base_users::ADDON, &test_partition_large_base::ADDON, ext])
            .build()
            .unwrap(),
    );
    Orm::builder(registry, h.db().pool().clone())
        .policy(StaticPolicy::AllowAll)
        .build()
        .unwrap()
}

fn large() -> SchemaSyncOptions {
    SchemaSyncOptions::new().large_table_rows(10)
}

/// `(kind, key)` of the steps, sorted by key.
fn kinds(steps: &[SchemaStep]) -> Vec<(SchemaStepKind, String)> {
    let mut out: Vec<_> = steps
        .iter()
        .map(|s| (s.kind(), s.key().to_owned()))
        .collect();
    out.sort_by(|a, b| a.1.cmp(&b.1));
    out
}

/// Statements of the captured DDL batches that touch table `table`.
fn statements_on(tx: &Transaction, table: &str) -> Vec<String> {
    tx.captured()
        .iter()
        .flat_map(|batch| batch.split(";\n"))
        .filter(|s| s.contains(&format!("\"{table}\"")))
        .map(str::to_owned)
        .collect()
}

/// `(valid, comment)` of index `name` on the parent, or `None` when it does not exist.
async fn index_state(tx: &mut Transaction, name: &str) -> Option<(String, Option<String>)> {
    let found = rows(
        tx,
        &format!(
            "SELECT i.indisvalid::text, obj_description(i.indexrelid, 'pg_class') \
             FROM pg_index i JOIN pg_class c ON c.oid = i.indexrelid WHERE c.relname = '{name}'"
        ),
    )
    .await;
    found
        .into_iter()
        .next()
        .map(|r| (r[0].clone().unwrap(), r[1].clone()))
}

/// Number of partitions of `table` that have an index attached to the parent's index `name`.
async fn covered(tx: &mut Transaction, table: &str, name: &str) -> i64 {
    one(
        tx,
        &format!(
            "SELECT count(DISTINCT x.indrelid)::text FROM pg_index x \
             JOIN pg_inherits xi ON xi.inhrelid = x.indexrelid \
             WHERE xi.inhparent = '{name}'::regclass AND x.indrelid <> '{table}'::regclass"
        ),
    )
    .await
    .unwrap()
    .parse()
    .unwrap()
}

/// Number of partitions of `table`.
async fn partitions(tx: &mut Transaction, table: &str) -> i64 {
    one(
        tx,
        &format!("SELECT count(*)::text FROM pg_inherits WHERE inhparent = '{table}'::regclass"),
    )
    .await
    .unwrap()
    .parse()
    .unwrap()
}

/// `large_table_rows` are no DDL of the schema transaction; steps build the parent index `ON ONLY`,
/// one index per partition concurrently, and attach them; the end state matches an inline sync,
/// the steps are idempotent and a second sync plans nothing)
#[tokio::test]
async fn large_partitioned_indexes_are_built_per_partition() {
    let h = seeded().await;
    let orm = extended(&h, &test_partition_large_index::ADDON);
    let code = index_name("scale_big", "code");
    let unique = orm
        .registry()
        .model("scale.big")
        .unwrap()
        .table_objects
        .iter()
        .find(|o| o.full_name.ends_with("_code_uniq"))
        .unwrap()
        .full_name
        .clone();
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    tx.capture_statements(true);
    let sync = tx.init_models_with(&[], &large()).await.unwrap();
    let inside = statements_on(&tx, "scale_big");
    assert!(
        !inside.iter().any(|s| s.contains("CREATE")),
        "an index build inside the transaction: {inside:#?}"
    );
    let mut want = vec![
        (
            SchemaStepKind::CreatePartitionedIndex,
            format!("index.{code}"),
        ),
        (
            SchemaStepKind::CreatePartitionedIndex,
            format!("index.{unique}"),
        ),
    ];
    want.sort_by(|a, b| a.1.cmp(&b.1));
    assert_eq!(kinds(sync.steps()), want);
    for step in sync.steps() {
        assert_eq!(step.table(), "scale_big");
    }
    // nothing built yet
    assert_eq!(index_state(&mut tx, &code).await, None);
    tx.commit().await.unwrap();

    for _ in 0..2 {
        // the second pass: a step that already ran does nothing more
        for step in sync.steps() {
            let report = orm.run_schema_step(step, &large()).await.unwrap();
            assert_eq!(report, SchemaStepReport::default(), "{}", step.key());
        }
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let parts = partitions(&mut tx, "scale_big").await;
        assert!(parts >= 2, "{parts} partitions");
        for name in [&code, &unique] {
            let (valid, _) = index_state(&mut tx, name).await.expect(name);
            assert_eq!(valid, "true", "{name}");
            assert_eq!(covered(&mut tx, "scale_big", name).await, parts, "{name}");
        }
        assert_eq!(
            index_state(&mut tx, &code).await.map(|s| s.1),
            Some(None),
            "a field index carries no comment once complete"
        );
        let (_, comment) = index_state(&mut tx, &unique).await.unwrap();
        assert_eq!(
            comment.as_deref(),
            Some("UNIQUE INDEX (code, create_date)"),
            "the declaration is the comment"
        );
        // the unique index holds: the same code at the same create_date twice is refused
        let insert = "INSERT INTO scale_big (name, code, create_date) \
                      VALUES ('a', 'k', now() AT TIME ZONE 'UTC')";
        tx.execute(insert, &[]).await.unwrap();
        let error = tx.execute(insert, &[]).await.unwrap_err();
        assert_eq!(error.sqlstate(), Some("23505"), "{error:?}");
        assert!(error.message().contains(&unique), "{}", error.message());
        drop(tx);
    }
    // converged: a second sync plans nothing and sends no DDL on the table
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    tx.capture_statements(true);
    let again = tx.init_models_with(&[], &large()).await.unwrap();
    assert_eq!(kinds(again.steps()), vec![]);
    let touched: Vec<String> = statements_on(&tx, "scale_big")
        .into_iter()
        .filter(|s| !s.trim_start().starts_with("SELECT"))
        .collect();
    assert_eq!(touched, Vec::<String>::new());
    tx.commit().await.unwrap();
    drop(orm);
    h.cleanup().await.unwrap();
}

/// The index statement the declaration of the `code` field index plans for `scale_big`.
fn declared(name: &str) -> String {
    format!("CREATE INDEX \"{name}\" ON \"scale_big\" USING btree (\"code\")")
}

/// the same declaration keeps the partition indexes already attached, the step builds only the
/// partitions without one, and a sync that finds it plans the step again without a schema change)
#[tokio::test]
async fn an_interrupted_partitioned_index_build_resumes() {
    let h = seeded().await;
    let orm = extended(&h, &test_partition_large_index::ADDON);
    let code = index_name("scale_big", "code");
    // what an earlier run left: the parent index and the index of the first partition, attached
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let parts = rows(
        &mut tx,
        "SELECT c.relname::text FROM pg_inherits i JOIN pg_class c ON c.oid = i.inhrelid \
         WHERE i.inhparent = 'scale_big'::regclass ORDER BY 1",
    )
    .await;
    assert!(parts.len() >= 2, "{parts:?}");
    let first = parts[0][0].clone().unwrap();
    let kept = format!("{first}__{code}");
    // the building marker of this declaration, which a field index carries until it is complete
    let marker = erp_sql::ddl::partitioned_build_comment(&code, &declared(&code));
    tx.batch(&format!(
        "ALTER TABLE scale_big ADD COLUMN code varchar; \
         CREATE INDEX \"{code}\" ON ONLY scale_big USING btree (\"code\"); {marker}; \
         CREATE INDEX \"{kept}\" ON \"{first}\" USING btree (\"code\"); \
         ALTER INDEX \"{code}\" ATTACH PARTITION \"{kept}\""
    ))
    .await
    .unwrap();
    assert_eq!(
        index_state(&mut tx, &code).await.map(|s| s.0).as_deref(),
        Some("false")
    );
    let kept_oid = one(&mut tx, &format!("SELECT '{kept}'::regclass::oid::text")).await;
    tx.commit().await.unwrap();

    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let sync = tx.init_models_with(&[], &large()).await.unwrap();
    let step = sync
        .steps()
        .iter()
        .find(|s| s.key() == format!("index.{code}"))
        .expect("the unfinished build is planned again")
        .clone();
    assert_eq!(step.kind(), SchemaStepKind::CreatePartitionedIndex);
    tx.commit().await.unwrap();
    assert_eq!(
        orm.run_schema_step(&step, &large()).await.unwrap(),
        SchemaStepReport::default()
    );
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    assert_eq!(
        index_state(&mut tx, &code).await.map(|s| s.0).as_deref(),
        Some("true")
    );
    let total = partitions(&mut tx, "scale_big").await;
    assert_eq!(covered(&mut tx, "scale_big", &code).await, total);
    assert_eq!(
        one(&mut tx, &format!("SELECT '{kept}'::regclass::oid::text")).await,
        kept_oid,
        "the partition index attached by the earlier run was kept"
    );
    assert_eq!(
        index_state(&mut tx, &code).await.map(|s| s.1),
        Some(None),
        "the building marker is removed when the index is complete"
    );
    drop(tx);
    drop(orm);
    h.cleanup().await.unwrap();
}

/// comment, is not resumed: it is dropped with the partition indexes attached to it and built
/// again as declared)
#[tokio::test]
async fn a_stale_unfinished_partitioned_index_is_rebuilt_as_declared() {
    let h = seeded().await;
    let orm = extended(&h, &test_partition_large_index::ADDON);
    let code = index_name("scale_big", "code");
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let first = rows(
        &mut tx,
        "SELECT c.relname::text FROM pg_inherits i JOIN pg_class c ON c.oid = i.inhrelid \
         WHERE i.inhparent = 'scale_big'::regclass ORDER BY 1",
    )
    .await[0][0]
        .clone()
        .unwrap();
    let stale = format!("{first}__{code}");
    // a hash index of the same name, no comment: another declaration, of unknown origin
    tx.batch(&format!(
        "ALTER TABLE scale_big ADD COLUMN code varchar; \
         CREATE INDEX \"{code}\" ON ONLY scale_big USING hash (\"code\"); \
         CREATE INDEX \"{stale}\" ON \"{first}\" USING hash (\"code\"); \
         ALTER INDEX \"{code}\" ATTACH PARTITION \"{stale}\""
    ))
    .await
    .unwrap();
    let mut stale_oid = one(&mut tx, &format!("SELECT '{stale}'::regclass::oid::text"))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    for comment in [None, Some("UNIQUE INDEX (code)")] {
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        if let Some(comment) = comment {
            // the same stale index, now with a comment that is not this declaration's
            tx.batch(&format!("COMMENT ON INDEX \"{code}\" IS '{comment}'"))
                .await
                .unwrap();
        }
        let sync = tx.init_models_with(&[], &large()).await.unwrap();
        let step = sync
            .steps()
            .iter()
            .find(|s| s.key() == format!("index.{code}"))
            .expect("the unfinished build is planned again")
            .clone();
        tx.commit().await.unwrap();
        assert_eq!(
            orm.run_schema_step(&step, &large()).await.unwrap(),
            SchemaStepReport::default()
        );
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        assert_eq!(
            index_state(&mut tx, &code).await.map(|s| s.0).as_deref(),
            Some("true")
        );
        let def = one(
            &mut tx,
            &format!("SELECT pg_get_indexdef('{code}'::regclass)"),
        )
        .await
        .unwrap();
        assert!(def.contains("USING btree (code)"), "{def}");
        let total = partitions(&mut tx, "scale_big").await;
        assert_eq!(covered(&mut tx, "scale_big", &code).await, total);
        let gone = rows(
            &mut tx,
            &format!("SELECT oid::text FROM pg_class WHERE oid = {stale_oid}"),
        )
        .await;
        assert!(
            gone.is_empty(),
            "the stale partition index is gone: {gone:?}"
        );
        // put the stale state back for the second pass (drop what the step built)
        if comment.is_none() {
            tx.batch(&format!(
                "DROP INDEX \"{code}\"; \
                 CREATE INDEX \"{code}\" ON ONLY scale_big USING hash (\"code\"); \
                 CREATE INDEX \"{stale}\" ON \"{first}\" USING hash (\"code\"); \
                 ALTER INDEX \"{code}\" ATTACH PARTITION \"{stale}\""
            ))
            .await
            .unwrap();
            stale_oid = one(&mut tx, &format!("SELECT '{stale}'::regclass::oid::text"))
                .await
                .unwrap();
        }
        tx.commit().await.unwrap();
    }
    drop(orm);
    h.cleanup().await.unwrap();
}

/// `large_table_rows` is refused in the install path, naming the table and the gate, before any
/// DDL; at or below it the constraint is added in the transaction)
#[tokio::test]
async fn a_unique_constraint_on_a_large_partitioned_table_is_refused() {
    let h = seeded().await;
    let orm = extended(&h, &test_partition_large_unique::ADDON);
    let constraint = orm
        .registry()
        .model("scale.big")
        .unwrap()
        .table_objects
        .iter()
        .find(|o| o.full_name.ends_with("_name_uniq"))
        .unwrap()
        .full_name
        .clone();
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let error = tx.init_models_with(&[], &large()).await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming, "{error:?}");
    assert_eq!(
        error.message(),
        format!(
            "cannot add or change unique constraint '{constraint}' of 'scale.big' during the \
             install: partitioned table 'scale_big' has more than 10 rows (large_table_rows), ADD \
             CONSTRAINT would build an index on every partition under ACCESS EXCLUSIVE, and \
             PostgreSQL has no ADD CONSTRAINT … USING INDEX on a partitioned table, so no online \
             build exists; declare a unique index instead (built partition by partition after \
             the commit)"
        )
    );
    tx.rollback().await.unwrap();
    // below the threshold the constraint is a statement of the transaction
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let sync = tx
        .init_models_with(&[], &SchemaSyncOptions::new().large_table_rows(1_000))
        .await
        .unwrap();
    assert!(sync.is_complete());
    assert_eq!(
        one(
            &mut tx,
            &format!(
                "SELECT count(*)::text FROM pg_constraint \
                 WHERE conrelid = 'scale_big'::regclass AND conname = '{constraint}'"
            )
        )
        .await
        .as_deref(),
        Some("1")
    );
    tx.commit().await.unwrap();
    drop(orm);
    h.cleanup().await.unwrap();
}

/// in the install path like on any large table, and the table is left as it was; at or below the
/// threshold it is added in the transaction)
#[tokio::test]
async fn an_exclusion_on_a_large_partitioned_table_is_refused() {
    let h = seeded().await;
    let orm = extended(&h, &test_partition_large_excl::ADDON);
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let error = tx.init_models_with(&[], &large()).await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::UserError, "{error:?}");
    assert_eq!(
        error.message(),
        "cannot add exclusion '_no_overlap' to 'scale.slot' during the install: table \
         'scale_slot' has more than 10 rows and an exclusion constraint has no online build. Run \
         `erp db build-object -d <db> --model scale.slot --object _no_overlap` in a maintenance \
         window (it holds ACCESS EXCLUSIVE on the table for the build), then retry the install"
    );
    tx.rollback().await.unwrap();
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let sync = tx
        .init_models_with(&[], &SchemaSyncOptions::new().large_table_rows(1_000))
        .await
        .unwrap();
    assert!(sync.is_complete());
    assert_eq!(
        one(
            &mut tx,
            "SELECT count(*)::text FROM pg_constraint \
             WHERE conrelid = 'scale_slot'::regclass AND contype = 'x'"
        )
        .await
        .as_deref(),
        Some("1")
    );
    tx.commit().await.unwrap();
    drop(orm);
    h.cleanup().await.unwrap();
}

/// The error chain of `error` as text: its message and its sources.
fn chain(error: &(dyn std::error::Error + 'static)) -> String {
    let mut out = error.to_string();
    let mut source = error.source();
    while let Some(next) = source {
        out.push_str(": ");
        out.push_str(&next.to_string());
        source = next.source();
    }
    out
}

/// Leaves the stale unfinished hash parent index of `a_stale_unfinished_partitioned_index_is_rebuilt_as_declared`
/// behind on `scale_big` and returns the step that builds `code`.
async fn stale_step(orm: &Orm, options: &SchemaSyncOptions) -> (String, SchemaStep) {
    let code = index_name("scale_big", "code");
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let first = rows(
        &mut tx,
        "SELECT c.relname::text FROM pg_inherits i JOIN pg_class c ON c.oid = i.inhrelid \
         WHERE i.inhparent = 'scale_big'::regclass ORDER BY 1",
    )
    .await[0][0]
        .clone()
        .unwrap();
    let stale = format!("{first}__{code}");
    tx.batch(&format!(
        "ALTER TABLE scale_big ADD COLUMN code varchar; \
         CREATE INDEX \"{code}\" ON ONLY scale_big USING hash (\"code\"); \
         CREATE INDEX \"{stale}\" ON \"{first}\" USING hash (\"code\"); \
         ALTER INDEX \"{code}\" ATTACH PARTITION \"{stale}\""
    ))
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let sync = tx.init_models_with(&[], options).await.unwrap();
    let step = sync
        .steps()
        .iter()
        .find(|s| s.key() == format!("index.{code}"))
        .expect("the unfinished build is planned")
        .clone();
    tx.commit().await.unwrap();
    (code, step)
}

/// EXCLUSIVE` on the parent and every partition, waits the short lock timeout and not the long
/// one: a long reader of the table cuts the step in about the short wait, with a `Lock` error
/// that a later run resumes)
#[tokio::test]
async fn a_stale_parent_index_is_dropped_under_the_short_lock_timeout() {
    let h = seeded().await;
    let orm = extended(&h, &test_partition_large_index::ADDON);
    let short = std::time::Duration::from_millis(300);
    let long = std::time::Duration::from_secs(20);
    let options = large().step_lock_timeouts(short, long).unwrap();
    let (code, step) = stale_step(&orm, &options).await;
    // a long reader of the table: the drop cannot get ACCESS EXCLUSIVE
    let reader = h.db().client().await.unwrap();
    reader
        .batch_execute("BEGIN; SELECT count(*) FROM scale_big")
        .await
        .unwrap();
    let started = std::time::Instant::now();
    let error = orm.run_schema_step(&step, &options).await.unwrap_err();
    let waited = started.elapsed();
    assert_eq!(error.kind(), ErrorKind::Lock, "{error:?}");
    assert_eq!(error.sqlstate(), Some("55P03"));
    assert!(
        waited < long / 4,
        "the drop waited {waited:?}, not the short {short:?}"
    );
    reader.batch_execute("COMMIT").await.unwrap();
    // resumed: the stale index is dropped and the build completes
    assert_eq!(
        orm.run_schema_step(&step, &options).await.unwrap(),
        SchemaStepReport::default()
    );
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    assert_eq!(
        index_state(&mut tx, &code).await.map(|s| s.0).as_deref(),
        Some("true")
    );
    drop(tx);
    drop(orm);
    h.cleanup().await.unwrap();
}

/// (a bounded try-lock loop); a run that cannot take it within the long lock wait fails with a `Lock` error naming the table
/// and the lock, and a later run, once the lock is free, finishes the build)
#[tokio::test]
async fn a_partitioned_index_build_waits_for_the_partition_upkeep_lock() {
    let h = seeded().await;
    let orm = extended(&h, &test_partition_large_index::ADDON);
    let short = std::time::Duration::from_millis(300);
    let long = std::time::Duration::from_millis(1_000);
    let options = large().step_lock_timeouts(short, long).unwrap();
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let sync = tx.init_models_with(&[], &options).await.unwrap();
    let code = index_name("scale_big", "code");
    let step = sync
        .steps()
        .iter()
        .find(|s| s.key() == format!("index.{code}"))
        .expect("the index build is a step")
        .clone();
    tx.commit().await.unwrap();
    // an upkeep call (another session) holds the table's partition lock
    let upkeep = h.db().client().await.unwrap();
    upkeep
        .query(erp_sql::partition::SESSION_LOCK, &[&"scale_big"])
        .await
        .unwrap();
    let error = orm.run_schema_step(&step, &options).await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Lock, "{error:?}");
    assert_eq!(error.sqlstate(), Some("55P03"));
    assert_eq!(
        error.message(),
        format!("schema step '{}' failed", step.key())
    );
    assert_eq!(
        chain(&error),
        format!(
            "schema step '{}' failed: index {code}: could not take the partition upkeep lock of \
             table 'scale_big' within 1000ms (an upkeep call or a schema synchronization holds \
             it); the step resumes at the next synchronization",
            step.key()
        )
    );
    // the lock is free again: the build resumes and completes
    upkeep
        .query(erp_sql::partition::SESSION_UNLOCK, &[&"scale_big"])
        .await
        .unwrap();
    assert_eq!(
        orm.run_schema_step(&step, &options).await.unwrap(),
        SchemaStepReport::default()
    );
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    assert_eq!(
        index_state(&mut tx, &code).await.map(|s| s.0).as_deref(),
        Some("true")
    );
    drop(tx);
    drop(orm);
    h.cleanup().await.unwrap();
}

/// Wait until a `CREATE INDEX CONCURRENTLY` is waiting for older snapshots (the gate below holds
/// one), polling the activity view; no fixed sleep stands in for it.
async fn wait_for_the_build_to_block(h: &Harness) {
    let client = h.db().client().await.unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let waiting: i64 = client
            .query_one(
                "SELECT count(*) FROM pg_stat_activity \
                  WHERE datname = current_database() AND pid <> pg_backend_pid() \
                    AND (query LIKE 'CREATE INDEX CONCURRENTLY%' OR query LIKE 'CREATE UNIQUE INDEX CONCURRENTLY%') \
                    AND wait_event_type = 'Lock' AND wait_event = 'virtualxid'",
                &[],
            )
            .await
            .unwrap()
            .get(0);
        if waiting > 0 {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the concurrent index build never started waiting for older snapshots"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

/// index build: while the build waits for an older snapshot, upkeep calls take the lock, finish
/// their work and report complete, instead of waiting on it while holding the snapshot the build
/// waits for, which PostgreSQL ends as a deadlock; the build then completes)
#[tokio::test]
async fn upkeep_runs_while_a_partition_index_is_built() {
    let h = seeded().await;
    let orm = extended(&h, &test_partition_large_index::ADDON);
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let sync = tx.init_models_with(&[], &large()).await.unwrap();
    let code = index_name("scale_big", "code");
    let step = sync
        .steps()
        .iter()
        .find(|s| s.key() == format!("index.{code}"))
        .expect("the index build is a step")
        .clone();
    tx.commit().await.unwrap();
    // the gate: a transaction with an older snapshot holds every partition's build waiting
    let gate = h.db().client().await.unwrap();
    gate.batch_execute("BEGIN ISOLATION LEVEL REPEATABLE READ; SELECT count(*) FROM scale_big")
        .await
        .unwrap();
    let owner = h.db().pool().clone();
    let options = large();
    let (built, (maintained, ensured)) =
        tokio::join!(orm.run_schema_step(&step, &options), async {
            wait_for_the_build_to_block(&h).await;
            let now = {
                let client = h.db().client().await.unwrap();
                client
                    .query_one("SELECT (now() AT TIME ZONE 'UTC')", &[])
                    .await
                    .unwrap()
                    .get(0)
            };
            let maintained = orm.maintain_partitions(&owner, now).await;
            let ensured = orm
                .ensure_partitions(&owner, "scale.big", erp_orm::Horizon::Declared)
                .await;
            gate.batch_execute("COMMIT").await.unwrap();
            (maintained, ensured)
        });
    let maintained = maintained.unwrap();
    let ensured = ensured.unwrap();
    assert!(maintained.is_complete(), "{maintained:?}");
    assert!(ensured.is_complete(), "{ensured:?}");
    assert_eq!(built.unwrap(), SchemaStepReport::default());
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    assert_eq!(
        index_state(&mut tx, &code).await.map(|s| s.0).as_deref(),
        Some("true")
    );
    let total = partitions(&mut tx, "scale_big").await;
    assert_eq!(covered(&mut tx, "scale_big", &code).await, total);
    drop(tx);
    drop(orm);
    h.cleanup().await.unwrap();
}

/// definition under the name the build uses, with no parent index or with a stale unfinished one,
/// does not make every attach fail on the definition mismatch: the build drops the leftover and
/// builds the partition index as declared)
#[tokio::test]
async fn an_unattached_partition_index_of_another_definition_is_rebuilt() {
    let h = seeded().await;
    let orm = extended(&h, &test_partition_large_index::ADDON);
    let code = index_name("scale_big", "code");
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let first = rows(
        &mut tx,
        "SELECT c.relname::text FROM pg_inherits i JOIN pg_class c ON c.oid = i.inhrelid \
         WHERE i.inhparent = 'scale_big'::regclass ORDER BY 1",
    )
    .await[0][0]
        .clone()
        .unwrap();
    let leftover = format!("{first}__{code}");
    tx.batch(&format!(
        "ALTER TABLE scale_big ADD COLUMN code varchar; \
         CREATE INDEX \"{leftover}\" ON \"{first}\" USING hash (\"code\")"
    ))
    .await
    .unwrap();
    tx.commit().await.unwrap();
    for stale_parent in [false, true] {
        if stale_parent {
            let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
            tx.batch(&format!(
                "DROP INDEX \"{code}\"; \
                 CREATE INDEX \"{code}\" ON ONLY scale_big USING hash (\"code\"); \
                 CREATE INDEX \"{leftover}\" ON \"{first}\" USING hash (\"code\")"
            ))
            .await
            .unwrap();
            tx.commit().await.unwrap();
        }
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let sync = tx.init_models_with(&[], &large()).await.unwrap();
        let step = sync
            .steps()
            .iter()
            .find(|s| s.key() == format!("index.{code}"))
            .expect("the index build is a step")
            .clone();
        tx.commit().await.unwrap();
        assert_eq!(
            orm.run_schema_step(&step, &large()).await.unwrap(),
            SchemaStepReport::default(),
            "stale parent: {stale_parent}"
        );
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        assert_eq!(
            index_state(&mut tx, &code).await.map(|s| s.0).as_deref(),
            Some("true")
        );
        let def = one(
            &mut tx,
            &format!("SELECT pg_get_indexdef('{leftover}'::regclass)"),
        )
        .await
        .unwrap();
        assert!(def.contains("USING btree (code)"), "{def}");
        let total = partitions(&mut tx, "scale_big").await;
        assert_eq!(covered(&mut tx, "scale_big", &code).await, total);
        drop(tx);
    }
    drop(orm);
    h.cleanup().await.unwrap();
}

/// An old daily partition of `scale_big` (2020-01-01) with a row, left pending detach by a
/// concurrent detach an open snapshot keeps waiting past its lock timeout; returns its name.
async fn pending_old_partition(h: &Harness) -> String {
    let name = "scale_big_p20200101";
    let client = h.db().client().await.unwrap();
    client
        .batch_execute(&format!(
            "CREATE TABLE \"{name}\" (LIKE scale_big INCLUDING DEFAULTS INCLUDING CONSTRAINTS); \
             ALTER TABLE scale_big ATTACH PARTITION \"{name}\" FOR VALUES FROM ('2020-01-01') \
             TO ('2020-01-02')"
        ))
        .await
        .unwrap();
    let reader = h.db().client().await.unwrap();
    reader
        .batch_execute("BEGIN ISOLATION LEVEL REPEATABLE READ; SELECT count(*) FROM scale_big")
        .await
        .unwrap();
    let detacher = h.db().client().await.unwrap();
    detacher
        .batch_execute("SET lock_timeout = '300ms'")
        .await
        .unwrap();
    let error = detacher
        .batch_execute(&format!(
            "ALTER TABLE scale_big DETACH PARTITION \"{name}\" CONCURRENTLY"
        ))
        .await
        .unwrap_err();
    assert_eq!(error.code().map(|c| c.code()), Some("55P03"));
    reader.batch_execute("COMMIT").await.unwrap();
    assert!(pending(h, name).await, "the cut detach left it pending");
    name.to_owned()
}

/// Whether `name` is a partition pending detach.
async fn pending(h: &Harness, name: &str) -> bool {
    h.db()
        .client()
        .await
        .unwrap()
        .query_opt(
            "SELECT inhdetachpending FROM pg_inherits WHERE inhrelid = $1::text::regclass",
            &[&name],
        )
        .await
        .unwrap()
        .is_some_and(|r| r.get::<_, bool>(0))
}

/// Whether `name` is still a partition (pending or not).
async fn is_partition(h: &Harness, name: &str) -> bool {
    h.db()
        .client()
        .await
        .unwrap()
        .query_opt(
            "SELECT 1 FROM pg_inherits WHERE inhrelid = $1::text::regclass",
            &[&name],
        )
        .await
        .unwrap()
        .is_some()
}

/// Wait until the stepwise build is in its try-lock loop for the partition lock (its statement is
/// the last one of an idle or running session).
async fn wait_for_the_build_to_try_the_lock(h: &Harness) {
    let client = h.db().client().await.unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let trying: i64 = client
            .query_one(
                "SELECT count(*) FROM pg_stat_activity \
                  WHERE datname = current_database() AND pid <> pg_backend_pid() \
                    AND query LIKE 'SELECT pg_try_advisory_lock(1701%'",
                &[],
            )
            .await
            .unwrap()
            .get(0);
        if trying > 0 {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the build never tried the partition lock"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
}

/// The step that builds the `code` index of `scale_big`, from a sync with the index addon.
async fn code_step(orm: &Orm, options: &SchemaSyncOptions) -> (String, SchemaStep) {
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let sync = tx.init_models_with(&[], options).await.unwrap();
    let code = index_name("scale_big", "code");
    let step = sync
        .steps()
        .iter()
        .find(|s| s.key() == format!("index.{code}"))
        .expect("the index build is a step")
        .clone();
    tx.commit().await.unwrap();
    (code, step)
}

/// detach is not held up by a build waiting for that lock: the build polls with `pg_try_advisory_
/// lock` and holds no snapshot between tries, so the finalize, which waits for older snapshots,
/// completes inside its short lock timeout; the build then takes the lock and completes)
#[tokio::test]
async fn a_pending_detach_is_finalized_while_the_build_waits_for_the_lock() {
    let h = seeded().await;
    let orm = extended(&h, &test_partition_large_index::ADDON);
    let options = large()
        .step_lock_timeouts(
            std::time::Duration::from_millis(300),
            std::time::Duration::from_secs(30),
        )
        .unwrap();
    let (code, step) = code_step(&orm, &options).await;
    let stale = pending_old_partition(&h).await;
    // the upkeep call: holds the table's partition lock
    let upkeep = h.db().client().await.unwrap();
    upkeep
        .query(erp_sql::partition::SESSION_LOCK, &[&"scale_big"])
        .await
        .unwrap();
    upkeep
        .batch_execute("SET lock_timeout = '300ms'")
        .await
        .unwrap();
    let (built, finalized) = tokio::join!(orm.run_schema_step(&step, &options), async {
        wait_for_the_build_to_try_the_lock(&h).await;
        let finalized = upkeep
            .batch_execute(&erp_sql::partition::detach_finalize("scale_big", &stale))
            .await;
        let released = upkeep
            .query_one(erp_sql::partition::SESSION_UNLOCK, &[&"scale_big"])
            .await
            .unwrap()
            .get::<_, bool>(0);
        (finalized, released)
    });
    let (finalized, released) = finalized;
    finalized.unwrap();
    assert!(released);
    assert_eq!(built.unwrap(), SchemaStepReport::default());
    assert!(
        !is_partition(&h, &stale).await,
        "the finalize was not cut: the table is standalone"
    );
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    assert_eq!(
        index_state(&mut tx, &code).await.map(|s| s.0).as_deref(),
        Some("true")
    );
    let total = partitions(&mut tx, "scale_big").await;
    assert_eq!(covered(&mut tx, "scale_big", &code).await, total);
    drop(tx);
    drop(orm);
    h.cleanup().await.unwrap();
}

/// is refused with a `Lock` error naming the index, and does not touch the first runner's
/// in-progress concurrent build, which completes)
#[tokio::test]
async fn a_second_runner_of_the_same_index_step_is_refused() {
    let h = seeded().await;
    let orm = extended(&h, &test_partition_large_index::ADDON);
    let options = large()
        .step_lock_timeouts(
            std::time::Duration::from_millis(300),
            std::time::Duration::from_secs(30),
        )
        .unwrap();
    let (code, step) = code_step(&orm, &options).await;
    let gate = h.db().client().await.unwrap();
    gate.batch_execute("BEGIN ISOLATION LEVEL REPEATABLE READ; SELECT count(*) FROM scale_big")
        .await
        .unwrap();
    let (first, second) = tokio::join!(orm.run_schema_step(&step, &options), async {
        wait_for_the_build_to_block(&h).await;
        let second = orm.run_schema_step(&step, &options).await;
        gate.batch_execute("COMMIT").await.unwrap();
        second
    });
    let error = second.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Lock, "{error:?}");
    assert_eq!(
        error.message(),
        format!("schema step '{}' failed", step.key())
    );
    assert_eq!(
        chain(&error),
        format!(
            "schema step '{}' failed: index {code}: another runner is building this index (its \
             per-step lock is held); run the step again when that runner has finished",
            step.key()
        )
    );
    assert_eq!(first.unwrap(), SchemaStepReport::default());
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    assert_eq!(
        index_state(&mut tx, &code).await.map(|s| s.0).as_deref(),
        Some("true")
    );
    let total = partitions(&mut tx, "scale_big").await;
    assert_eq!(covered(&mut tx, "scale_big", &code).await, total);
    drop(tx);
    // the lock is gone with the step: the step can run again
    assert_eq!(
        orm.run_schema_step(&step, &options).await.unwrap(),
        SchemaStepReport::default()
    );
    drop(orm);
    h.cleanup().await.unwrap();
}

/// Run the build of `step` until the first partition's concurrent build has finished and its
/// index waits to be attached, with the test holding the partition lock on `holder`; then call
/// `during` (the lock is still held) and release the lock. Returns the build's result and the
/// name of the partition whose index was waiting.
async fn build_held_at_the_attach<F, Fut>(
    h: &Harness,
    orm: &Orm,
    step: &SchemaStep,
    options: &SchemaSyncOptions,
    code: &str,
    holder: &erp_testkit::tokio_postgres::Client,
    during: F,
) -> (erp_fields::Result<SchemaStepReport>, String)
where
    F: FnOnce(String) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let gate = h.db().client().await.unwrap();
    gate.batch_execute("BEGIN ISOLATION LEVEL REPEATABLE READ; SELECT count(*) FROM scale_big")
        .await
        .unwrap();
    let probe = h.db().client().await.unwrap();
    tokio::join!(orm.run_schema_step(step, options), async {
        wait_for_the_build_to_block(h).await;
        // between the two lock takings of the partition: the build released the lock before the
        // concurrent build
        holder
            .query(erp_sql::partition::SESSION_LOCK, &[&"scale_big"])
            .await
            .unwrap();
        gate.batch_execute("COMMIT").await.unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        let partition = loop {
            let found = probe
                .query_opt(
                    &format!(
                        "SELECT t.relname::text FROM pg_index x \
                         JOIN pg_class c ON c.oid = x.indexrelid \
                         JOIN pg_class t ON t.oid = x.indrelid \
                         WHERE c.relname LIKE 'scale\\_big\\_p%\\_\\_{code}' AND x.indisvalid \
                           AND NOT EXISTS (SELECT FROM pg_inherits i WHERE i.inhrelid = c.oid)"
                    ),
                    &[],
                )
                .await
                .unwrap();
            if let Some(row) = found {
                break row.get::<_, String>(0);
            }
            assert!(
                std::time::Instant::now() < deadline,
                "no partition index waited for its attach"
            );
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        };
        during(partition.clone()).await;
        let released: bool = holder
            .query_one(erp_sql::partition::SESSION_UNLOCK, &[&"scale_big"])
            .await
            .unwrap()
            .get(0);
        assert!(released);
        partition
    })
}

/// build waited for the partition lock for the attach, is found by the second check: the build
/// fails with a `Database` error naming the partition and the index, and the next run resumes)
#[tokio::test]
async fn a_partition_detached_between_the_two_checks_fails_the_attach() {
    let h = seeded().await;
    let orm = extended(&h, &test_partition_large_index::ADDON);
    let options = large()
        .step_lock_timeouts(
            std::time::Duration::from_millis(300),
            std::time::Duration::from_secs(30),
        )
        .unwrap();
    let (code, step) = code_step(&orm, &options).await;
    let holder = h.db().client().await.unwrap();
    let hr = &h;
    let (built, partition) = build_held_at_the_attach(
        &h,
        &orm,
        &step,
        &options,
        &code,
        &holder,
        |partition| async move {
            let client = hr.db().client().await.unwrap();
            client
                .batch_execute(&format!(
                    "ALTER TABLE scale_big DETACH PARTITION \"{partition}\""
                ))
                .await
                .unwrap();
        },
    )
    .await;
    let error = built.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Database, "{error:?}");
    assert_eq!(
        error.message(),
        format!("schema step '{}' failed", step.key())
    );
    let index = erp_registry::ident::shorten_identifier(&format!("{partition}__{code}"));
    assert_eq!(
        chain(&error),
        format!(
            "schema step '{}' failed: index {code}: partition {partition} of table 'scale_big' \
             changed while its index {index} was built (it was detached, dropped or indexed by \
             another run); the step resumes at the next synchronization",
            step.key()
        )
    );
    drop(orm);
    h.cleanup().await.unwrap();
}

/// the build waits for the partition lock to attach its first index, the upkeep call finishes a
/// pending detach and creates a partition; the build then attaches and completes, and the new
/// partition has its index)
#[tokio::test]
async fn the_attach_phase_meets_an_upkeep_with_work() {
    let h = seeded().await;
    let orm = extended(&h, &test_partition_large_index::ADDON);
    let options = large()
        .step_lock_timeouts(
            std::time::Duration::from_millis(300),
            std::time::Duration::from_secs(30),
        )
        .unwrap();
    let (code, step) = code_step(&orm, &options).await;
    let stale = pending_old_partition(&h).await;
    let holder = h.db().client().await.unwrap();
    holder
        .batch_execute("SET lock_timeout = '300ms'")
        .await
        .unwrap();
    let (built, _) =
        build_held_at_the_attach(&h, &orm, &step, &options, &code, &holder, |_| async {
            holder
                .batch_execute(&erp_sql::partition::detach_finalize("scale_big", &stale))
                .await
                .unwrap();
            holder
                .batch_execute(
                    "CREATE TABLE scale_big_p20990101 PARTITION OF scale_big \
                 FOR VALUES FROM ('2099-01-01') TO ('2099-01-02')",
                )
                .await
                .unwrap();
        })
        .await;
    assert_eq!(built.unwrap(), SchemaStepReport::default());
    assert!(!is_partition(&h, &stale).await);
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    assert_eq!(
        index_state(&mut tx, &code).await.map(|s| s.0).as_deref(),
        Some("true")
    );
    let total = partitions(&mut tx, "scale_big").await;
    assert_eq!(covered(&mut tx, "scale_big", &code).await, total);
    drop(tx);
    drop(orm);
    h.cleanup().await.unwrap();
}

/// indexes of the partition itself: an index of another table that carries the name a partition
/// index would get is never dropped; the build then fails loudly, at the attach, because the foreign index is not an index of
/// a partition)
#[tokio::test]
async fn a_same_named_index_of_another_table_is_not_dropped() {
    let h = seeded().await;
    let orm = extended(&h, &test_partition_large_index::ADDON);
    let (code, step) = code_step(&orm, &large()).await;
    let client = h.db().client().await.unwrap();
    let first: String = client
        .query_one(
            "SELECT c.relname::text FROM pg_inherits i JOIN pg_class c ON c.oid = i.inhrelid \
             WHERE i.inhparent = 'scale_big'::regclass ORDER BY 1 LIMIT 1",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    let clash = erp_registry::ident::shorten_identifier(&format!("{first}__{code}"));
    client
        .batch_execute(&format!(
            "CREATE TABLE unrelated (id int8); CREATE INDEX \"{clash}\" ON unrelated (id)"
        ))
        .await
        .unwrap();
    let error = orm.run_schema_step(&step, &large()).await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Database, "{error:?}");
    assert_eq!(error.sqlstate(), Some("55000"));
    assert_eq!(
        chain(&error),
        format!(
            "schema step '{key}' failed: cannot attach index \"{clash}\" as a partition of \
             index \"{code}\": db error: ERROR: cannot attach index \"{clash}\" as a partition \
             of index \"{code}\"\nDETAIL: Index \"{clash}\" is not an index on any partition \
             of table \"scale_big\".",
            key = step.key()
        )
    );
    let owner: String = client
        .query_one(
            "SELECT t.relname::text FROM pg_index x JOIN pg_class c ON c.oid = x.indexrelid \
             JOIN pg_class t ON t.oid = x.indrelid WHERE c.relname = $1",
            &[&clash],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(owner, "unrelated", "the other table's index is untouched");
    drop(orm);
    h.cleanup().await.unwrap();
}
