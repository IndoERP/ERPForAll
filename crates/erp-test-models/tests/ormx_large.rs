//! Business keys on large tables and conflict reporting during schema sync (
//! §11.1;
//! reviews D2 and D7).
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

mod support;

use erp_orm::{Orm, SchemaStep, SchemaStepFailures, SchemaStepKind, SchemaSyncOptions};
use erp_registry::{KeyDecl, ModelDecl, TableObjectDecl, TableObjectKind};
use erp_sdk::{ErrorKind, RecId, RecordSet, SUPERUSER_ID, Transaction, Upsert, Upserted, Vals};
use erp_test_models::base_users;
use erp_test_models::test_scale_excl;
use erp_test_models::test_scale_upsert::{
    self as scale, ScaleCompany, ScaleLocation, ScaleProduct, ScaleQuant,
};
use erp_testkit::{Harness, assert_query_count};
use support::one;

async fn harness() -> Harness {
    Harness::builder([&base_users::ADDON, &scale::ADDON])
        .build()
        .await
        .unwrap()
}

fn quants(tx: &Transaction) -> RecordSet<ScaleQuant> {
    tx.env().model::<ScaleQuant>().unwrap()
}

fn quant_key(registry: &erp_registry::Registry) -> erp_registry::BusinessKey {
    registry
        .model("scale.quant")
        .unwrap()
        .business_key("_quant_key")
        .unwrap()
        .clone()
}

fn key_row(product: RecId, location: RecId) -> Vals<ScaleQuant> {
    Vals::new()
        .set(ScaleQuant::PRODUCT_ID, product)
        .set(ScaleQuant::LOCATION_ID, location)
}

/// `n` products, one location, and a quant for each product at that location.
async fn seed_quants(tx: &mut Transaction, n: usize) -> (Vec<RecId>, RecId) {
    let env = tx.env();
    let company = env
        .model::<ScaleCompany>()
        .unwrap()
        .create(tx, vec![Vals::new().set(ScaleCompany::NAME, "C1")])
        .await
        .unwrap()
        .rec_ids()[0];
    let location = env
        .model::<ScaleLocation>()
        .unwrap()
        .create(
            tx,
            vec![
                Vals::new()
                    .set(ScaleLocation::NAME, "L0")
                    .set(ScaleLocation::COMPANY_ID, company),
            ],
        )
        .await
        .unwrap()
        .rec_ids()[0];
    let products = env
        .model::<ScaleProduct>()
        .unwrap()
        .create(
            tx,
            (0..n)
                .map(|i| Vals::new().set(ScaleProduct::NAME, format!("P{i}")))
                .collect(),
        )
        .await
        .unwrap()
        .rec_ids()
        .to_vec();
    quants(tx)
        .create(tx, products.iter().map(|p| key_row(*p, location)).collect())
        .await
        .unwrap();
    (products, location)
}

/// Take the key of `scale.quant` away (constraint and index), as in a database that does not have
/// it yet.
async fn remove_quant_key(h: &Harness) {
    let full = quant_key(h.registry()).full_name;
    let mut tx = h.begin().await.unwrap();
    tx.execute(
        &format!("ALTER TABLE scale_quant DROP CONSTRAINT \"{full}\""),
        &[],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
}

fn upsert_one(product: RecId, location: RecId) -> Upsert<ScaleQuant> {
    Upsert::on(ScaleQuant::QUANT_KEY)
        .rows([key_row(product, location).set(ScaleQuant::LOT_ID, None::<RecId>)])
}

/// Run `step` to its end with the default step options (every caller planned it with the default
/// lock timeouts): it succeeds and reports nothing.
async fn run(h: &Harness, step: &SchemaStep) {
    let report = h
        .orm()
        .run_schema_step(step, &SchemaSyncOptions::new())
        .await
        .unwrap();
    assert!(report.is_empty(), "{}", step.key());
}

/// Statements among `captured` that change a table, an index or a constraint's comment.
fn table_ddl(captured: &[String]) -> Vec<String> {
    captured
        .iter()
        .filter(|s| {
            s.contains("ALTER TABLE") || s.contains("INDEX") || s.contains("COMMENT ON CONSTRAINT")
        })
        .cloned()
        .collect()
}

fn kinds(steps: &[SchemaStep]) -> Vec<(SchemaStepKind, String)> {
    steps
        .iter()
        .map(|s| (s.kind(), s.key().to_owned()))
        .collect()
}

/// `(valid, comment)` of the index `name`, when it exists.
async fn index_state(h: &Harness, name: &str) -> Option<(bool, Option<String>)> {
    let mut tx = h.begin().await.unwrap();
    let rows = tx
        .query(
            "SELECT i.indisvalid, obj_description(i.indexrelid, 'pg_class') FROM pg_index i \
               JOIN pg_class c ON c.oid = i.indexrelid WHERE c.relname = $1",
            &[&name],
        )
        .await
        .unwrap();
    let state = rows.first().map(|r| (r.get(0), r.get(1)));
    tx.rollback().await.unwrap();
    state
}

/// The oid of the index `name`.
async fn index_oid(h: &Harness, name: &str) -> i64 {
    let mut tx = h.begin().await.unwrap();
    let rows = tx
        .query("SELECT indexrelid::oid::int8 FROM pg_index i JOIN pg_class c ON c.oid = i.indexrelid WHERE c.relname = $1", &[&name])
        .await
        .unwrap();
    let oid = rows[0].get(0);
    tx.rollback().await.unwrap();
    oid
}

/// The oid of the index behind the constraint `name` (`pg_constraint.conindid`).
async fn constraint_index(h: &Harness, name: &str) -> Option<i64> {
    let mut tx = h.begin().await.unwrap();
    let rows = tx
        .query(
            "SELECT conindid::oid::int8 FROM pg_constraint WHERE conname = $1",
            &[&name],
        )
        .await
        .unwrap();
    let oid = rows.first().map(|r| r.get(0));
    tx.rollback().await.unwrap();
    oid
}

/// The comment of the constraint `name`, when the constraint exists.
async fn constraint_comment(h: &Harness, name: &str) -> Option<Option<String>> {
    let mut tx = h.begin().await.unwrap();
    let rows = tx
        .query(
            "SELECT obj_description(oid, 'pg_constraint') FROM pg_constraint WHERE conname = $1",
            &[&name],
        )
        .await
        .unwrap();
    let comment = rows.first().map(|r| r.get(0));
    tx.rollback().await.unwrap();
    comment
}

/// The original error a step error wraps.
fn cause(error: &erp_fields::Error) -> &erp_fields::Error {
    std::error::Error::source(error)
        .and_then(|source| source.downcast_ref::<erp_fields::Error>())
        .expect("a step error wraps the original error")
}

const KEY_COLUMNS: [&str; 3] = ["product_id", "location_id", "lot_id"];

/// Keep the first quant of each product, remove the rest.
const REMOVE_DUPLICATE_QUANTS: &str =
    "DELETE FROM scale_quant WHERE id NOT IN (SELECT min(id) FROM scale_quant GROUP BY product_id)";

#[tokio::test]
async fn spec_ormx_031_a_business_key_on_a_large_table_is_built_in_two_post_commit_steps() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let (products, location) = seed_quants(&mut tx, 1001).await;
    tx.commit().await.unwrap();
    drop(tx);
    remove_quant_key(&h).await;
    let name = quant_key(h.registry()).full_name;
    let definition = erp_sql::upsert::key_definition(&KEY_COLUMNS, false);
    let options = SchemaSyncOptions::new().large_table_rows(1000);

    // the upgrade: nothing about the key in the transaction, two steps after it
    let mut tx = h.begin().await.unwrap();
    tx.capture_statements(true);
    let sync = tx.init_models_with(&[], &options).await.unwrap();
    assert!(
        table_ddl(tx.captured()).iter().all(|s| !s.contains(&name)),
        "{:?}",
        tx.captured()
    );
    let steps = sync.into_steps();
    assert_eq!(
        kinds(&steps),
        vec![
            (SchemaStepKind::CreateKeyIndex, format!("key_index.{name}")),
            (
                SchemaStepKind::AddKeyConstraint,
                format!("key_constraint.{name}")
            ),
        ]
    );
    tx.commit().await.unwrap();
    drop(tx);
    assert_eq!(index_state(&h, &name).await, None);
    assert_eq!(constraint_comment(&h, &name).await, None);

    // a restart before the steps ran plans the same two steps again
    let mut tx = h.begin().await.unwrap();
    let again = tx.init_models_with(&[], &options).await.unwrap();
    assert_eq!(kinds(again.steps()), kinds(&steps));
    tx.rollback().await.unwrap();
    drop(tx);

    // step 1: the unique index exists, valid; the constraint does not
    run(&h, &steps[0]).await;
    let index_oid = index_oid(&h, &name).await;
    assert_eq!(
        index_state(&h, &name).await,
        Some((true, Some(definition.clone())))
    );
    assert_eq!(constraint_comment(&h, &name).await, None);

    // an upsert in between: the arbiter does not exist yet
    let mut tx = h.begin().await.unwrap();
    let model = quants(&tx);
    // exactly one statement: the 42704 of the arbiter is the whole detection, no pre-check
    let err = assert_query_count!(tx, >= 1, <= 1, {
        model
            .upsert(&mut tx, upsert_one(products[0], location))
            .await
            .unwrap_err()
    });
    assert_eq!(
        (err.kind(), err.sqlstate(), err.message()),
        (
            ErrorKind::Database,
            Some("42704"),
            "business key '_quant_key' of 'scale.quant' is not usable yet: its post-commit steps have \
             not completed (pending or failed; see the schema step report and the module health \
             check)"
        )
    );
    tx.rollback().await.unwrap();
    drop(tx);

    // step 2, twice: the second run finds the constraint and does nothing; step 1 again finds
    // the constraint and does nothing
    run(&h, &steps[1]).await;
    run(&h, &steps[1]).await;
    run(&h, &steps[0]).await;
    assert_eq!(
        constraint_comment(&h, &name).await,
        Some(Some(definition.clone()))
    );
    // step 2 adopted the index step 1 built (`USING INDEX`): no second scan
    assert_eq!(
        constraint_index(&h, &name).await,
        Some(index_oid),
        "the constraint's index is the one step 1 built"
    );

    // after step 2 the upsert is one statement
    let mut tx = h.begin().await.unwrap();
    let model = quants(&tx);
    let out: Upserted<ScaleQuant> = assert_query_count!(tx, >= 1, <= 1, {
        model
            .upsert(&mut tx, upsert_one(products[0], location))
            .await
            .unwrap()
    });
    assert_eq!(out.rows().len(), 1);
    tx.rollback().await.unwrap();
    drop(tx);

    // the next synchronization finds the key up to date and plans nothing
    let mut tx = h.begin().await.unwrap();
    tx.capture_statements(true);
    let sync = tx.init_models_with(&[], &options).await.unwrap();
    assert!(sync.is_complete(), "{:?}", sync.steps());
    assert_eq!(table_ddl(tx.captured()), Vec::<String>::new());
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();

    // the duplicate fixture: two quants with the same key
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let (products, location) = seed_quants(&mut tx, 1001).await;
    tx.commit().await.unwrap();
    drop(tx);
    remove_quant_key(&h).await;
    let mut tx = h.begin().await.unwrap();
    quants(&tx)
        .create(&mut tx, vec![key_row(products[0], location)])
        .await
        .unwrap();
    let duplicates: Vec<i64> = tx
        .query(
            "SELECT id FROM scale_quant WHERE product_id = $1 ORDER BY id",
            &[&products[0].get()],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect();
    assert_eq!(duplicates.len(), 2);
    tx.commit().await.unwrap();
    drop(tx);
    let name = quant_key(h.registry()).full_name;
    let err = h.orm().sync_schema_with(&options).await.unwrap_err();
    // the failure is in the step report: the index step, and the constraint step that needed it
    let failures = SchemaStepFailures::of(&err).expect("the failed steps are the error's source");
    let failed: Vec<(String, ErrorKind)> = failures
        .failures()
        .iter()
        .map(|f| (f.step().key().to_owned(), f.error().kind()))
        .collect();
    assert_eq!(
        failed,
        vec![
            (format!("key_index.{name}"), ErrorKind::Validation),
            (format!("key_constraint.{name}"), ErrorKind::Database),
        ]
    );
    let index_failure = failures.failures()[0].error();
    assert_eq!(
        index_failure.message(),
        format!("schema step 'key_index.{name}' failed")
    );
    let conflict = cause(index_failure);
    assert_eq!(conflict.kind(), ErrorKind::Validation);
    assert_eq!(
        conflict.message(),
        format!(
            "cannot add key '_quant_key' to 'scale.quant': existing records conflict, e.g. ({}, {})",
            duplicates[0], duplicates[1]
        )
    );
    assert_eq!(
        failures.failures()[1].error().message(),
        format!(
            "schema step 'key_constraint.{name}' was not run: it needs step 'key_index.{name}', \
             which failed (see that step's error)"
        )
    );
    // it is its own error: no SQLSTATE copied from the conflict
    assert_eq!(failures.failures()[1].error().sqlstate(), None);
    // the invalid index of the failed concurrent build is gone, and no constraint was added
    assert_eq!(index_state(&h, &name).await, None);
    assert_eq!(constraint_comment(&h, &name).await, None);
    h.cleanup().await.unwrap();
}

/// A conflict names at most 20 id pairs, and the recorded steps finish once the duplicates are
/// gone.
#[tokio::test]
async fn key_conflicts_name_at_most_twenty_pairs_and_the_steps_run_again_after_a_fix() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let (products, location) = seed_quants(&mut tx, 3).await;
    tx.commit().await.unwrap();
    drop(tx);
    remove_quant_key(&h).await;
    let mut tx = h.begin().await.unwrap();
    // a second quant for each of the 3 products, then 22 more products with two quants each:
    // 25 groups of equal keys, one pair per group, 20 named
    let more = tx
        .env()
        .model::<ScaleProduct>()
        .unwrap()
        .create(
            &mut tx,
            (0..22)
                .map(|i| Vals::new().set(ScaleProduct::NAME, format!("Q{i}")))
                .collect(),
        )
        .await
        .unwrap()
        .rec_ids()
        .to_vec();
    let both: Vec<_> = products.iter().chain(more.iter()).collect();
    quants(&tx)
        .create(
            &mut tx,
            both.iter()
                .flat_map(|p| [key_row(**p, location), key_row(**p, location)])
                .collect(),
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let name = quant_key(h.registry()).full_name;
    let options = SchemaSyncOptions::new().large_table_rows(5);
    let mut tx = h.begin().await.unwrap();
    let steps = tx
        .init_models_with(&[], &options)
        .await
        .unwrap()
        .into_steps();
    tx.commit().await.unwrap();
    drop(tx);

    let err = h
        .orm()
        .run_schema_step(&steps[0], &options)
        .await
        .unwrap_err();
    let message = cause(&err).message().to_owned();
    let prefix = "cannot add key '_quant_key' to 'scale.quant': existing records conflict, e.g. ";
    let pairs = message.strip_prefix(prefix).expect(&message);
    assert_eq!(pairs.matches('(').count(), 20, "{message}");
    assert_eq!(index_state(&h, &name).await, None);

    // fix the data; the same recorded steps now finish the work
    let mut tx = h.begin().await.unwrap();
    tx.execute(REMOVE_DUPLICATE_QUANTS, &[]).await.unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    for step in &steps {
        run(&h, step).await;
    }
    assert!(constraint_comment(&h, &name).await.is_some());
    h.cleanup().await.unwrap();
}

/// An interrupted concurrent build leaves an invalid index: the next plan has the same two steps,
/// and step 1 drops the leftover before it builds.
#[tokio::test]
async fn an_invalid_index_left_by_an_interrupted_build_is_dropped_by_the_next_attempt() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let (products, location) = seed_quants(&mut tx, 3).await;
    tx.commit().await.unwrap();
    drop(tx);
    remove_quant_key(&h).await;
    let mut tx = h.begin().await.unwrap();
    quants(&tx)
        .create(&mut tx, vec![key_row(products[0], location)])
        .await
        .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let name = quant_key(h.registry()).full_name;
    // a concurrent build over the duplicate rows fails and leaves an invalid index
    let client = h.db().client().await.unwrap();
    client
        .batch_execute(&format!(
            "CREATE UNIQUE INDEX CONCURRENTLY \"{name}\" ON scale_quant \
             (product_id, location_id, lot_id) NULLS NOT DISTINCT"
        ))
        .await
        .unwrap_err();
    drop(client);
    assert_eq!(index_state(&h, &name).await.map(|s| s.0), Some(false));
    let mut tx = h.begin().await.unwrap();
    tx.execute(REMOVE_DUPLICATE_QUANTS, &[]).await.unwrap();
    tx.commit().await.unwrap();
    drop(tx);

    let options = SchemaSyncOptions::new().large_table_rows(2);
    let mut tx = h.begin().await.unwrap();
    let steps = tx
        .init_models_with(&[], &options)
        .await
        .unwrap()
        .into_steps();
    tx.commit().await.unwrap();
    drop(tx);
    assert_eq!(steps.len(), 2);
    for step in &steps {
        run(&h, step).await;
    }
    assert_eq!(index_state(&h, &name).await.map(|s| s.0), Some(true));
    assert!(constraint_comment(&h, &name).await.is_some());
    h.cleanup().await.unwrap();
}

/// An index built by step 1 and found by a sync of a table that is small by then is adopted in the
/// transaction, without a second scan.
#[tokio::test]
async fn a_built_key_index_is_adopted_by_an_inline_sync() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    seed_quants(&mut tx, 3).await;
    tx.commit().await.unwrap();
    drop(tx);
    remove_quant_key(&h).await;
    let name = quant_key(h.registry()).full_name;
    let mut tx = h.begin().await.unwrap();
    let steps = tx
        .init_models_with(&[], &SchemaSyncOptions::new().large_table_rows(2))
        .await
        .unwrap()
        .into_steps();
    tx.commit().await.unwrap();
    drop(tx);
    run(&h, &steps[0]).await;
    let mut tx = h.begin().await.unwrap();
    tx.capture_statements(true);
    tx.init_models(&[]).await.unwrap();
    assert!(
        tx.captured()
            .iter()
            .any(|s| s.contains("UNIQUE USING INDEX")),
        "{:?}",
        tx.captured()
    );
    assert!(
        tx.captured().iter().all(|s| !s.contains("CREATE UNIQUE")),
        "{:?}",
        tx.captured()
    );
    tx.commit().await.unwrap();
    drop(tx);
    assert!(constraint_comment(&h, &name).await.is_some());
    h.cleanup().await.unwrap();
}

/// A key of another definition on a large table is refused before any DDL (no online replacement
/// exists), and the refusal says what to do.
#[tokio::test]
async fn a_changed_business_key_on_a_large_table_is_refused_with_the_way_out() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    seed_quants(&mut tx, 3).await;
    let name = quant_key(h.registry()).full_name;
    // the same name with another definition, as an older release of the model left it
    for sql in [
        format!("ALTER TABLE scale_quant DROP CONSTRAINT \"{name}\""),
        format!(
            "ALTER TABLE scale_quant ADD CONSTRAINT \"{name}\" UNIQUE (product_id, location_id)"
        ),
        format!(
            "COMMENT ON CONSTRAINT \"{name}\" ON scale_quant IS \
             'UNIQUE NULLS NOT DISTINCT (\"product_id\", \"location_id\")'"
        ),
    ] {
        tx.execute(&sql, &[]).await.unwrap();
    }
    tx.commit().await.unwrap();
    drop(tx);

    let mut tx = h.begin().await.unwrap();
    tx.capture_statements(true);
    let err = tx
        .init_models_with(&[], &SchemaSyncOptions::new().large_table_rows(2))
        .await
        .unwrap_err();
    assert_eq!(table_ddl(tx.captured()), Vec::<String>::new());
    assert_eq!(err.kind(), ErrorKind::UserError);
    assert_eq!(
        err.message(),
        format!(
            "cannot change business key '_quant_key' of 'scale.quant' during the install: table \
             'scale_quant' has more than 2 rows (large_table_rows), and a changed key has no \
             online replacement: building the new unique index under ACCESS EXCLUSIVE scans the \
             whole table, and keeping the old constraint while it is built would leave upserts \
             naming the key on its old definition. Drop the old constraint in a maintenance \
             window (ALTER TABLE \"scale_quant\" DROP CONSTRAINT \"{name}\"; upserts naming the \
             key fail until the new one is built) and run the schema sync again: it adds the new \
             key in two steps after the commit"
        )
    );
    tx.rollback().await.unwrap();
    drop(tx);

    // the way out: drop it, sync again, the two steps build the new key
    let mut tx = h.begin().await.unwrap();
    tx.execute(
        &format!("ALTER TABLE scale_quant DROP CONSTRAINT \"{name}\""),
        &[],
    )
    .await
    .unwrap();
    let steps = tx
        .init_models_with(&[], &SchemaSyncOptions::new().large_table_rows(2))
        .await
        .unwrap()
        .into_steps();
    tx.commit().await.unwrap();
    drop(tx);
    assert_eq!(steps.len(), 2);
    for step in &steps {
        run(&h, step).await;
    }
    assert_eq!(
        constraint_comment(&h, &name).await,
        Some(Some(erp_sql::upsert::key_definition(&KEY_COLUMNS, false)))
    );
    h.cleanup().await.unwrap();
}

// ---- review D2: drift is compared by type and definition, not by the comment alone ------------

/// The catalog's definition of the constraint `name`.
async fn constraint_definition(h: &Harness, name: &str) -> String {
    let mut tx = h.begin().await.unwrap();
    let def = one(
        &mut tx,
        &format!("SELECT pg_get_constraintdef(oid) FROM pg_constraint WHERE conname = '{name}'"),
    )
    .await
    .unwrap();
    tx.rollback().await.unwrap();
    def
}

/// Review D2 (keys): a key replaced by hand under the same name, keeping the comment, is not up
/// to date; the sync re-creates it (and refuses on a large table).
#[tokio::test]
async fn a_hand_altered_business_key_with_the_comment_kept_is_re_created() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    seed_quants(&mut tx, 3).await;
    let name = quant_key(h.registry()).full_name;
    let definition = erp_sql::upsert::key_definition(&KEY_COLUMNS, false);
    tx.commit().await.unwrap();
    drop(tx);
    let declared = constraint_definition(&h, &name).await;
    assert_eq!(
        declared,
        "UNIQUE NULLS NOT DISTINCT (product_id, location_id, lot_id)"
    );
    // the same columns, but NULLs distinct: another definition under the same comment
    let mut tx = h.begin().await.unwrap();
    for sql in [
        format!("ALTER TABLE scale_quant DROP CONSTRAINT \"{name}\""),
        format!(
            "ALTER TABLE scale_quant ADD CONSTRAINT \"{name}\" \
             UNIQUE (product_id, location_id, lot_id)"
        ),
        format!(
            "COMMENT ON CONSTRAINT \"{name}\" ON scale_quant IS '{}'",
            definition.replace('\'', "''")
        ),
    ] {
        tx.execute(&sql, &[]).await.unwrap();
    }
    tx.commit().await.unwrap();
    drop(tx);
    assert_eq!(
        constraint_definition(&h, &name).await,
        "UNIQUE (product_id, location_id, lot_id)"
    );

    // on a large table the changed key is refused
    let mut tx = h.begin().await.unwrap();
    let err = tx
        .init_models_with(&[], &SchemaSyncOptions::new().large_table_rows(2))
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::UserError);
    assert_eq!(
        err.message(),
        format!(
            "cannot change business key '_quant_key' of 'scale.quant' during the install: table \
             'scale_quant' has more than 2 rows (large_table_rows), and a changed key has no \
             online replacement: building the new unique index under ACCESS EXCLUSIVE scans the \
             whole table, and keeping the old constraint while it is built would leave upserts \
             naming the key on its old definition. Drop the old constraint in a maintenance \
             window (ALTER TABLE \"scale_quant\" DROP CONSTRAINT \"{name}\"; upserts naming the \
             key fail until the new one is built) and run the schema sync again: it adds the new \
             key in two steps after the commit"
        )
    );
    tx.rollback().await.unwrap();
    drop(tx);

    // otherwise the sync puts the declared definition back
    let mut tx = h.begin().await.unwrap();
    tx.init_models(&[]).await.unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    assert_eq!(constraint_definition(&h, &name).await, declared);
    // and a sync of an unchanged key plans nothing
    let mut tx = h.begin().await.unwrap();
    tx.capture_statements(true);
    tx.init_models(&[]).await.unwrap();
    assert_eq!(table_ddl(tx.captured()), Vec::<String>::new());
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

async fn excl_harness() -> Harness {
    Harness::builder([&base_users::ADDON, &test_scale_excl::ADDON])
        .build()
        .await
        .unwrap()
}

/// Review D2 (exclusions): an exclusion replaced by hand, keeping the comment, with another
/// definition or another type, is re-created by the sync; on a large table the exclusion refusal
/// applies.
#[tokio::test]
async fn a_hand_altered_exclusion_with_the_comment_kept_is_re_created() {
    let h = excl_harness().await;
    let registry = std::sync::Arc::clone(h.registry());
    let booking = registry.model("scale.booking").unwrap();
    let name = booking
        .table_objects
        .iter()
        .find(|o| o.name == "_no_double_booking")
        .unwrap()
        .full_name
        .clone();
    let declared = constraint_definition(&h, &name).await;
    assert!(declared.contains("resource_id WITH ="), "{declared}");
    let mut tx = h.begin().await.unwrap();
    let comment = one(
        &mut tx,
        &format!(
            "SELECT obj_description(oid, 'pg_constraint') FROM pg_constraint \
             WHERE conname = '{name}'"
        ),
    )
    .await
    .unwrap();
    // more rows than the large-table threshold below, disjoint periods
    tx.execute(
        "INSERT INTO scale_resource (name) SELECT 'r' || g FROM generate_series(1, 12) g",
        &[],
    )
    .await
    .unwrap();
    tx.execute(
        "INSERT INTO scale_booking (resource_id, start, stop, state) \
         SELECT id, timestamp '2026-01-05' + id * interval '1 hour', \
                timestamp '2026-01-05' + id * interval '1 hour' + interval '30 minutes', \
                'confirmed' FROM scale_resource",
        &[],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let replacements = [
        // another exclusion: the resource is no longer compared
        "EXCLUDE USING gist (tsrange(start, stop, '[)') WITH &&)",
        // an equality element added by hand (the number of elements is compared)
        "EXCLUDE USING gist (resource_id WITH =, state WITH =, tsrange(start, stop, '[)') WITH &&) \
         WHERE (state <> 'cancel')",
        // another type altogether
        "CHECK (true)",
    ];
    for replacement in replacements {
        let mut tx = h.begin().await.unwrap();
        for sql in [
            format!("ALTER TABLE scale_booking DROP CONSTRAINT \"{name}\""),
            format!("ALTER TABLE scale_booking ADD CONSTRAINT \"{name}\" {replacement}"),
            format!(
                "COMMENT ON CONSTRAINT \"{name}\" ON scale_booking IS '{}'",
                comment.replace('\'', "''")
            ),
        ] {
            tx.execute(&sql, &[]).await.unwrap();
        }
        tx.commit().await.unwrap();
        drop(tx);
        assert_ne!(constraint_definition(&h, &name).await, declared);

        // above the threshold the exclusion has no online build
        let mut tx = h.begin().await.unwrap();
        let err = tx
            .init_models_with(
                &[booking.id],
                &SchemaSyncOptions::new().large_table_rows(10),
            )
            .await
            .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::UserError, "{replacement}");
        assert_eq!(
            err.message(),
            "cannot add exclusion '_no_double_booking' to 'scale.booking' during the install: \
             table 'scale_booking' has more than 10 rows and an exclusion constraint has no \
             online build. Run `erp db build-object -d <db> --model scale.booking --object \
             _no_double_booking` in a maintenance window (it holds ACCESS EXCLUSIVE on the table \
             for the build), then retry the install",
            "{replacement}"
        );
        tx.rollback().await.unwrap();
        drop(tx);

        let mut tx = h.begin().await.unwrap();
        tx.init_models(&[booking.id]).await.unwrap();
        tx.commit().await.unwrap();
        drop(tx);
        assert_eq!(
            constraint_definition(&h, &name).await,
            declared,
            "{replacement}"
        );
    }
    // everything up to date again: a sync of all models plans no table change
    let mut tx = h.begin().await.unwrap();
    tx.capture_statements(true);
    tx.init_models(&[]).await.unwrap();
    assert_eq!(table_ddl(tx.captured()), Vec::<String>::new());
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

// ---- review D7: schema sync failing on existing records says so ---------------------------

static LATE_KEY: erp_sdk::AddonDescriptor = erp_sdk::AddonDescriptor {
    name: "test_scale_late_key",
    depends: &["test_scale_upsert"],
    exact_only: false,
};

static LOT_NAME_KEY: [TableObjectDecl; 1] = [TableObjectDecl {
    name: "_name_key",
    kind: TableObjectKind::Key(KeyDecl::new(
        &["name"],
        "A lot with this name exists already.",
    )),
}];

static LOT_NAME_KEY_DECL: ModelDecl =
    ModelDecl::extension("test_scale_late_key", "scale.lot", &["scale.lot"])
        .table_objects(&LOT_NAME_KEY);

fn orm_with(
    h: &Harness,
    addon: &'static erp_sdk::AddonDescriptor,
    decl: &'static ModelDecl,
) -> Orm {
    let registry = erp_registry::RegistryBuilder::new([&base_users::ADDON, &scale::ADDON, addon])
        .with_decls([decl])
        .build()
        .unwrap();
    Orm::builder(std::sync::Arc::new(registry), h.db().pool().clone())
        .policy(erp_sdk::StaticPolicy::AllowAll)
        .build()
        .unwrap()
}

/// Review D7 (keys): `ADD CONSTRAINT` over duplicate rows during the sync is the sync failing, with
/// the model, the object and the ids; an ordinary duplicate write keeps the declared message.
#[tokio::test]
async fn a_key_added_over_duplicate_rows_fails_the_sync_naming_object_model_and_ids() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    tx.execute(
        "INSERT INTO scale_lot (name) VALUES ('a'), ('b'), ('a'), ('a')",
        &[],
    )
    .await
    .unwrap();
    let ids: Vec<i64> = tx
        .query("SELECT id FROM scale_lot WHERE name = 'a' ORDER BY id", &[])
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect();
    tx.commit().await.unwrap();
    drop(tx);
    let orm = orm_with(&h, &LATE_KEY, &LOT_NAME_KEY_DECL);
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let err = tx.init_models(&[]).await.unwrap_err();
    assert_eq!(
        (err.kind(), err.sqlstate()),
        (ErrorKind::Validation, Some("23505"))
    );
    assert_eq!(
        err.message(),
        format!(
            "cannot add key '_name_key' to 'scale.lot': existing records conflict, e.g. \
             ({0}, {1})",
            ids[0], ids[2]
        )
    );
    // no row value reaches any text the error can print (PostgreSQL's DETAIL line quotes it):
    // the message, the debug form, or the display of any error in the chain (review DB10)
    let mut shown = vec![
        err.message().to_owned(),
        err.to_string(),
        format!("{err:?}"),
    ];
    let mut source = std::error::Error::source(&err);
    while let Some(cause) = source {
        // every ORM error in the chain prints its message only (the command line prints the
        // top error's, the step report and the logs the messages of the chain); the driver
        // error at the end of the chain holds the DETAIL line by design
        // and is not printed by any of them
        let Some(orm) = cause.downcast_ref::<erp_fields::Error>() else {
            break;
        };
        shown.push(orm.to_string());
        shown.push(format!("{orm:?}"));
        source = cause.source();
    }
    for text in &shown {
        assert!(!text.contains("=(a)") && !text.contains("(name)"), "{text}");
    }
    // the diagnostic's read-only mode and timeout ended with its savepoint (review SF-M1): a
    // write in the same transaction works
    assert_eq!(
        one(&mut tx, "SHOW transaction_read_only").await.as_deref(),
        Some("off")
    );
    // the schema step's own bound (set before the failure) is what is in force, not the
    // diagnostic's 30 s: the failed sync leaves its bounds set until the rollback
    assert_eq!(
        one(&mut tx, "SHOW statement_timeout").await.as_deref(),
        Some(erp_orm::SCHEMA_STATEMENT_TIMEOUT)
    );
    tx.execute(
        "INSERT INTO scale_lot (name) VALUES ('written after the conflict')",
        &[],
    )
    .await
    .unwrap();
    tx.rollback().await.unwrap();
    drop(tx);

    // without duplicates the sync adds the key, and an ordinary duplicate write gets the
    // declared message
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    tx.execute("DELETE FROM scale_lot WHERE name = 'a'", &[])
        .await
        .unwrap();
    tx.init_models(&[]).await.unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let err = tx
        .execute("INSERT INTO scale_lot (name) VALUES ('b')", &[])
        .await
        .unwrap_err();
    assert_eq!(
        (err.kind(), err.message()),
        (
            ErrorKind::Validation,
            "The operation cannot be completed: A lot with this name exists already."
        )
    );
    drop(tx);
    drop(orm);
    h.cleanup().await.unwrap();
}

/// Review D7 (exclusions and temporal keys): the same for a `23P01` and a temporal key.
#[tokio::test]
async fn an_exclusion_or_temporal_key_added_over_conflicting_rows_fails_the_sync_naming_the_ids() {
    let h = excl_harness().await;
    let registry = std::sync::Arc::clone(h.registry());
    let booking = registry.model("scale.booking").unwrap();
    let rate = registry.model("scale.rate").unwrap();
    let object = |m: &erp_registry::ModelSpec, n: &str| {
        m.table_objects
            .iter()
            .find(|o| o.name == n)
            .unwrap()
            .full_name
            .clone()
    };
    let excl = object(booking, "_no_double_booking");
    let check = erp_sql::exclusion::period_check_name(&excl);
    let key = object(rate, "_rate_key");
    let mut tx = h.begin().await.unwrap();
    for (table, name) in [
        ("scale_booking", &check),
        ("scale_booking", &excl),
        ("scale_rate", &key),
    ] {
        tx.execute(
            &format!("ALTER TABLE {table} DROP CONSTRAINT \"{name}\""),
            &[],
        )
        .await
        .unwrap();
    }
    tx.execute(
        "INSERT INTO scale_resource (name) VALUES ('r1'), ('r2')",
        &[],
    )
    .await
    .unwrap();
    // two overlapping bookings of r1, a disjoint one, and an overlapping pair of r2 where one is
    // cancelled (the exclusion's filter leaves it out)
    tx.execute(
        "INSERT INTO scale_booking (resource_id, start, stop, state) \
         SELECT r.id, v.s, v.e, v.st FROM scale_resource r JOIN (VALUES \
            ('r1', timestamp '2026-01-05 10:00', timestamp '2026-01-05 11:00', 'confirmed'), \
            ('r1', timestamp '2026-01-05 10:30', timestamp '2026-01-05 12:00', 'confirmed'), \
            ('r1', timestamp '2026-01-05 13:00', timestamp '2026-01-05 14:00', 'confirmed'), \
            ('r2', timestamp '2026-01-05 10:00', timestamp '2026-01-05 11:00', 'confirmed'), \
            ('r2', timestamp '2026-01-05 10:30', timestamp '2026-01-05 12:00', 'cancel')) \
            AS v(n, s, e, st) ON r.name = v.n ORDER BY v.n, v.s",
        &[],
    )
    .await
    .unwrap();
    tx.execute(
        "INSERT INTO scale_rate (product_id, validity, amount) \
         SELECT r.id, v.d, 1 FROM scale_resource r, (VALUES \
            (daterange(date '2026-01-01', date '2026-02-01')), \
            (daterange(date '2026-01-15', date '2026-03-01'))) AS v(d) WHERE r.name = 'r1'",
        &[],
    )
    .await
    .unwrap();
    let ids = async |tx: &mut Transaction, sql: &str| -> String {
        let ids: Vec<i64> = tx
            .query(sql, &[])
            .await
            .unwrap()
            .iter()
            .map(|r| r.get(0))
            .collect();
        assert_eq!(ids.len(), 2, "{sql}");
        format!("({}, {})", ids[0], ids[1])
    };
    let overlapping = ids(
        &mut tx,
        "SELECT b.id FROM scale_booking b JOIN scale_resource r ON r.id = b.resource_id \
          WHERE r.name = 'r1' AND b.start < timestamp '2026-01-05 12:00' ORDER BY b.id",
    )
    .await;
    let rates = ids(&mut tx, "SELECT id FROM scale_rate ORDER BY id").await;
    tx.commit().await.unwrap();
    drop(tx);

    for (model, what, expected) in [
        (booking, "exclusion '_no_double_booking'", overlapping),
        (rate, "key '_rate_key'", rates),
    ] {
        let mut tx = h.begin().await.unwrap();
        let err = tx.init_models(&[model.id]).await.unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Validation, "{what}");
        assert_eq!(
            err.message(),
            format!(
                "cannot add {what} to '{}': existing records conflict, e.g. {expected}",
                model.name
            )
        );
        assert!(
            matches!(err.sqlstate(), Some("23P01" | "23505")),
            "{:?}",
            err.sqlstate()
        );
        tx.rollback().await.unwrap();
        drop(tx);
    }
    h.cleanup().await.unwrap();
}

/// A conflict diagnostic that times out leaves the conflict as the error, with no pair listed, on
/// both paths: the step-1 build of a large table and the in-transaction `ADD CONSTRAINT`. The
/// diagnostic's `statement_timeout` is forced to 1 ms over a table too big for the self-join to
/// finish in that time.
#[tokio::test]
async fn a_conflict_diagnostic_that_times_out_still_reports_the_conflict() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    tx.execute(
        "INSERT INTO scale_lot (name) SELECT 'lot ' || g FROM generate_series(1, 100000) g",
        &[],
    )
    .await
    .unwrap();
    tx.execute("INSERT INTO scale_lot (name) VALUES ('lot 1')", &[])
        .await
        .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let orm = orm_with(&h, &LATE_KEY, &LOT_NAME_KEY_DECL);
    let unlisted = "cannot add key '_name_key' to 'scale.lot': existing records conflict (no pair \
                    could be listed: the diagnostic query failed, timed out or found none)";
    let tiny = std::time::Duration::from_millis(1);

    // in the install transaction (the table is not large)
    let options = SchemaSyncOptions::new()
        .large_table_rows(1_000_000)
        .diagnostic_timeout(tiny)
        .unwrap();
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let err = tx.init_models_with(&[], &options).await.unwrap_err();
    assert_eq!(
        (err.kind(), err.message()),
        (ErrorKind::Validation, unlisted)
    );
    tx.rollback().await.unwrap();
    drop(tx);

    // in step 1 of a large table
    let options = SchemaSyncOptions::new()
        .large_table_rows(1000)
        .diagnostic_timeout(tiny)
        .unwrap();
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let steps = tx
        .init_models_with(&[], &options)
        .await
        .unwrap()
        .into_steps();
    tx.commit().await.unwrap();
    drop(tx);
    let err = orm.run_schema_step(&steps[0], &options).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(cause(&err).message(), unlisted);
    // the invalid index is gone all the same
    let mut tx = h.begin().await.unwrap();
    let left = tx
        .query(
            "SELECT 1 FROM pg_class WHERE relname = 'scale_lot_name_key'",
            &[],
        )
        .await
        .unwrap();
    assert!(left.is_empty());
    tx.rollback().await.unwrap();
    drop(tx);

    assert!(
        SchemaSyncOptions::new()
            .diagnostic_timeout(std::time::Duration::ZERO)
            .is_err()
    );
    drop(orm);
    h.cleanup().await.unwrap();
}

// ---- review DB1: an index left by a crash between the build and its comment ----------------

/// A valid unique index of the key's exact shape, without the comment step 1 writes.
async fn create_uncommented_key_index(h: &Harness, name: &str) {
    let mut tx = h.begin().await.unwrap();
    tx.execute(
        &format!(
            "CREATE UNIQUE INDEX \"{name}\" ON scale_quant (product_id, location_id, lot_id) \
             NULLS NOT DISTINCT"
        ),
        &[],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
}

/// Step path: a crash after `CREATE UNIQUE INDEX CONCURRENTLY` and before its comment leaves a
/// valid index of the declared shape; step 1 checks the shape, writes the comment, and step 2
/// adopts that very index.
#[tokio::test]
async fn an_uncommented_index_of_the_declared_shape_is_adopted_by_the_steps() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    seed_quants(&mut tx, 3).await;
    tx.commit().await.unwrap();
    drop(tx);
    remove_quant_key(&h).await;
    let name = quant_key(h.registry()).full_name;
    create_uncommented_key_index(&h, &name).await;
    let oid = index_oid(&h, &name).await;
    let mut tx = h.begin().await.unwrap();
    let steps = tx
        .init_models_with(&[], &SchemaSyncOptions::new().large_table_rows(2))
        .await
        .unwrap()
        .into_steps();
    tx.commit().await.unwrap();
    drop(tx);
    assert_eq!(steps.len(), 2);
    run(&h, &steps[0]).await;
    assert_eq!(
        index_state(&h, &name).await,
        Some((
            true,
            Some(erp_sql::upsert::key_definition(&KEY_COLUMNS, false))
        ))
    );
    run(&h, &steps[1]).await;
    assert_eq!(constraint_index(&h, &name).await, Some(oid));
    h.cleanup().await.unwrap();
}

/// Inline path: the same index is adopted in the transaction, with no `42P07` (relation exists)
/// from a second `CREATE`.
#[tokio::test]
async fn an_uncommented_index_of_the_declared_shape_is_adopted_by_an_inline_sync() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    seed_quants(&mut tx, 3).await;
    tx.commit().await.unwrap();
    drop(tx);
    remove_quant_key(&h).await;
    let name = quant_key(h.registry()).full_name;
    create_uncommented_key_index(&h, &name).await;
    let oid = index_oid(&h, &name).await;
    let mut tx = h.begin().await.unwrap();
    tx.init_models(&[]).await.unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    assert_eq!(constraint_index(&h, &name).await, Some(oid));
    assert_eq!(
        constraint_comment(&h, &name).await,
        Some(Some(erp_sql::upsert::key_definition(&KEY_COLUMNS, false)))
    );
    h.cleanup().await.unwrap();
}

/// An index of the key's name that is not the key's unique index (here: other columns) is never
/// adopted or replaced, on either path, and the message says so.
#[tokio::test]
async fn an_index_of_another_shape_under_the_keys_name_is_refused_on_both_paths() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    seed_quants(&mut tx, 3).await;
    tx.commit().await.unwrap();
    drop(tx);
    remove_quant_key(&h).await;
    let name = quant_key(h.registry()).full_name;
    let mut tx = h.begin().await.unwrap();
    tx.execute(
        &format!("CREATE UNIQUE INDEX \"{name}\" ON scale_quant (product_id)"),
        &[],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let expected = format!(
        "cannot add key '_quant_key' to 'scale.quant': an index named '{name}' exists that is not \
         this key's unique index (another shape, columns, comment or uniqueness); it is not \
         replaced, drop it or rename the key"
    );
    for options in [
        SchemaSyncOptions::new().large_table_rows(2),
        SchemaSyncOptions::inline(),
    ] {
        let mut tx = h.begin().await.unwrap();
        let err = tx.init_models_with(&[], &options).await.unwrap_err();
        assert_eq!(
            (err.kind(), err.message()),
            (ErrorKind::Programming, expected.as_str())
        );
        tx.rollback().await.unwrap();
        drop(tx);
    }
    assert_eq!(index_state(&h, &name).await.map(|s| s.0), Some(true));
    h.cleanup().await.unwrap();
}

// ---- review DB8: the inline path does not pull an index from under a running build ----------

/// An invalid key index is dropped by the inline sync only when no step holds the build's
/// per-step lock; while one does, the sync fails with a `Lock` error saying a build is running.
#[tokio::test]
async fn an_inline_sync_does_not_drop_the_invalid_index_of_a_running_build() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let (products, location) = seed_quants(&mut tx, 3).await;
    tx.commit().await.unwrap();
    drop(tx);
    remove_quant_key(&h).await;
    let mut tx = h.begin().await.unwrap();
    quants(&tx)
        .create(&mut tx, vec![key_row(products[0], location)])
        .await
        .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let name = quant_key(h.registry()).full_name;
    let client = h.db().client().await.unwrap();
    client
        .batch_execute(&format!(
            "CREATE UNIQUE INDEX CONCURRENTLY \"{name}\" ON scale_quant \
             (product_id, location_id, lot_id) NULLS NOT DISTINCT"
        ))
        .await
        .unwrap_err();
    assert_eq!(index_state(&h, &name).await.map(|s| s.0), Some(false));
    let mut tx = h.begin().await.unwrap();
    tx.execute(REMOVE_DUPLICATE_QUANTS, &[]).await.unwrap();
    tx.commit().await.unwrap();
    drop(tx);

    // a step runner holds the per-step lock of step 1
    let step_key = format!("key_index.{name}");
    let holder = h.db().client().await.unwrap();
    holder
        .query_one(
            "SELECT pg_advisory_lock(1702, hashtext($1::text))::text",
            &[&step_key],
        )
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    let err = tx.init_models(&[]).await.unwrap_err();
    assert_eq!(
        (err.kind(), err.message()),
        (
            ErrorKind::Lock,
            format!(
                "cannot replace the invalid index '{name}' of key '_quant_key' of 'scale.quant': \
                 its post-commit build step is running (the per-step lock is held); wait for it \
                 to finish and run the schema sync again"
            )
            .as_str()
        )
    );
    tx.rollback().await.unwrap();
    drop(tx);
    assert_eq!(index_state(&h, &name).await.map(|s| s.0), Some(false));

    // released: the sync drops the leftover and adds the key
    holder
        .query_one(
            "SELECT pg_advisory_unlock(1702, hashtext($1::text))::text",
            &[&step_key],
        )
        .await
        .unwrap();
    drop(holder);
    let mut tx = h.begin().await.unwrap();
    tx.init_models(&[]).await.unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    assert!(constraint_comment(&h, &name).await.is_some());
    drop(client);
    h.cleanup().await.unwrap();
}

// ---- review DB2: a business key on a partitioned table ---------------------------------------

/// The registry refuses a key on a partitioned model when it is built. A table that is
/// partitioned in the database only (hand-made, or adopted) never reaches the key planning with
/// rows: the partitioning plan refuses it first ("changing its partitioning is not supported"),
/// and an empty one is not large, so no path needs a partitioned-key refusal of its own.
#[tokio::test]
async fn a_hand_partitioned_table_with_rows_is_refused_before_any_key_is_planned() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    for sql in [
        "ALTER TABLE scale_quant DROP CONSTRAINT IF EXISTS scale_quant_lot_id_fkey",
        "ALTER TABLE scale_lot RENAME TO scale_lot_plain",
        "CREATE TABLE scale_lot (LIKE scale_lot_plain INCLUDING DEFAULTS) PARTITION BY RANGE (id)",
        "CREATE TABLE scale_lot_all PARTITION OF scale_lot DEFAULT",
        "INSERT INTO scale_lot (id, name) SELECT g, 'lot ' || g FROM generate_series(1, 11) g",
    ] {
        tx.execute(sql, &[]).await.unwrap();
    }
    tx.commit().await.unwrap();
    drop(tx);
    let orm = orm_with(&h, &LATE_KEY, &LOT_NAME_KEY_DECL);
    let lot = orm.registry().model("scale.lot").unwrap().id;
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let err = tx
        .init_models_with(&[lot], &SchemaSyncOptions::new().large_table_rows(10))
        .await
        .unwrap_err();
    assert_eq!(
        (err.kind(), err.message()),
        (
            ErrorKind::Programming,
            "table 'scale_lot' has rows; changing its partitioning is not supported (see the documentation; business tables: a later release)"
        )
    );
    tx.rollback().await.unwrap();
    drop(tx);
    drop(orm);
    h.cleanup().await.unwrap();
}
