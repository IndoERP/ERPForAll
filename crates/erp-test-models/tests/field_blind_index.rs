// SPDX-License-Identifier: MIT OR Apache-2.0
//! Blind indexes of encrypted fields (`encrypted = true, blind_index = true`) against a database:
//! equality conditions find the records, a typed key on the field refuses duplicates, the
//! companion column holds only keyed hashes, schema sync fills it for existing rows (and
//! recomputes stale ones) before the key is built, and other operators stay refused.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod support;

use std::sync::Arc;

use erp_domain::Value;
use erp_orm::UpsertCall;
use erp_orm::field_encryption::{FieldKeys, install};
use erp_registry::{
    AddonDescriptor, FieldDecl, KeyDecl, ModelDecl, RegistryBuilder, TableObjectDecl,
    TableObjectKind,
};
use erp_sdk::{Cell, Domain, ErrorKind, Operator, Orm, RawVals, SUPERUSER_ID, Transaction};
use erp_testkit::Harness;
use support::one;

static ADDON: AddonDescriptor = AddonDescriptor {
    name: "test_bidx",
    depends: &["base"],
    exact_only: false,
};

/// A fixed test key, never a deployment's.
const KEYS: &str = "t1 BwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwc=\n";
const MODEL: &str = "test_bidx.person";
const TABLE: &str = "test_bidx_person";
const NIK_A: &str = "3201010101010001";
const NIK_B: &str = "3201010101010002";

static KEY: &[TableObjectDecl] = &[TableObjectDecl {
    name: "_nid_key",
    kind: TableObjectKind::Key(
        KeyDecl::new(&["national_id"], "This national id is already used.").nulls_distinct(true),
    ),
}];

#[derive(Clone, Copy)]
enum Shape {
    Plain,
    Encrypted,
    Blind,
    BlindKeyed,
}

fn keys() {
    install(FieldKeys::parse(KEYS).unwrap()).unwrap();
}

fn person(shape: Shape) -> ModelDecl {
    let id = FieldDecl::char("national_id").size(20);
    let id = match shape {
        Shape::Plain => id,
        Shape::Encrypted => id.encrypted(true),
        Shape::Blind | Shape::BlindKeyed => id.encrypted(true).blind_index(true),
    };
    let fields = vec![FieldDecl::char("name"), id];
    let decl = ModelDecl::new("test_bidx", MODEL).fields(Vec::leak(fields));
    match shape {
        Shape::BlindKeyed => decl.table_objects(KEY),
        _ => decl,
    }
}

async fn harness(shape: Shape) -> Harness {
    keys();
    Harness::builder([&ADDON])
        .with_decls([person(shape)])
        .build()
        .await
        .unwrap()
}

fn orm(h: &Harness, shape: Shape) -> Orm {
    let registry = RegistryBuilder::new([&ADDON])
        .with_decls([&*Box::leak(Box::new(person(shape)))])
        .build()
        .unwrap();
    h.orm().rebuild(Arc::new(registry)).build().unwrap()
}

fn str_cell(text: &str) -> Cell {
    Cell::Str(text.into())
}

async fn create(tx: &mut Transaction, rows: &[(&str, Option<&str>)]) {
    let people = tx.env().records(MODEL).unwrap();
    let vals = rows
        .iter()
        .map(|(name, nid)| {
            let v = RawVals::new().set_cell("name", str_cell(name));
            match nid {
                Some(nid) => v.set_cell("national_id", str_cell(nid)),
                None => v,
            }
        })
        .collect::<Vec<_>>();
    people.create(tx, vals).await.unwrap();
    tx.flush().await.unwrap();
}

/// The names of the records `domain` finds, sorted.
async fn names(tx: &mut Transaction, domain: &Domain) -> Vec<String> {
    let people = tx.env().records(MODEL).unwrap();
    let found = people.search(tx, domain).await.unwrap();
    let mut out = Vec::new();
    for id in found.ids() {
        match people.browse([id]).unwrap().cell(tx, "name").await.unwrap() {
            Cell::Str(name) => out.push(name.to_string()),
            other => panic!("name is {other:?}"),
        }
    }
    out.sort();
    out
}

fn condition(operator: Operator, value: impl Into<Value>) -> Domain {
    Domain::condition("national_id", operator, value).unwrap()
}

async fn count(tx: &mut Transaction, sql: &str) -> String {
    one(tx, sql).await.unwrap_or_default()
}

#[tokio::test]
async fn equality_conditions_find_records_through_the_blind_index() {
    let h = harness(Shape::Blind).await;
    let mut tx = h.begin().await.unwrap();
    create(
        &mut tx,
        &[
            ("a", Some(NIK_A)),
            ("b", Some(NIK_B)),
            ("c", None),
            ("d", Some(NIK_A)),
        ],
    )
    .await;
    // the companion column holds `<key id>:<hex>`, never the value
    assert_eq!(
        count(
            &mut tx,
            &format!(
                "SELECT count(*)::text FROM {TABLE} WHERE national_id__bidx LIKE 't1:%' \
                 AND length(national_id__bidx) = 67"
            )
        )
        .await,
        "3"
    );
    assert_eq!(
        count(
            &mut tx,
            &format!("SELECT count(*)::text FROM {TABLE} WHERE national_id__bidx LIKE '%{NIK_A}%'")
        )
        .await,
        "0"
    );
    assert_eq!(
        names(&mut tx, &condition(Operator::Eq, NIK_A)).await,
        ["a", "d"]
    );
    assert_eq!(
        names(
            &mut tx,
            &condition(
                Operator::In,
                Value::List(vec![Value::from(NIK_B), Value::from("none")])
            )
        )
        .await,
        ["b"]
    );
    assert_eq!(
        names(&mut tx, &condition(Operator::Ne, NIK_A)).await,
        ["b", "c"],
        "`!=` keeps the records without a value, as on a clear column"
    );
    assert_eq!(
        names(&mut tx, &condition(Operator::Eq, Value::Bool(false))).await,
        ["c"]
    );
    assert_eq!(
        names(&mut tx, &condition(Operator::Ne, Value::Bool(false))).await,
        ["a", "b", "d"]
    );
    // a write changes the hash with the value; clearing the value clears the hash
    let people = tx.env().records(MODEL).unwrap();
    let b = people
        .search(&mut tx, &condition(Operator::Eq, NIK_B))
        .await
        .unwrap();
    b.write(
        &mut tx,
        RawVals::new().set_cell("national_id", str_cell(NIK_A)),
    )
    .await
    .unwrap();
    let c = people
        .search(&mut tx, &condition(Operator::Eq, Value::Bool(false)))
        .await
        .unwrap();
    c.write(
        &mut tx,
        RawVals::new().set_cell("national_id", str_cell(NIK_B)),
    )
    .await
    .unwrap();
    tx.flush().await.unwrap();
    assert_eq!(
        names(&mut tx, &condition(Operator::Eq, NIK_A)).await,
        ["a", "b", "d"]
    );
    assert_eq!(names(&mut tx, &condition(Operator::Eq, NIK_B)).await, ["c"]);
    c.write(&mut tx, RawVals::new().set_cell("national_id", Cell::Null))
        .await
        .unwrap();
    tx.flush().await.unwrap();
    assert_eq!(
        count(
            &mut tx,
            &format!("SELECT count(*)::text FROM {TABLE} WHERE national_id__bidx IS NULL")
        )
        .await,
        "1"
    );
    // other operators stay refused
    for operator in [Operator::ILike, Operator::Like, Operator::Gt] {
        let error = people
            .search(&mut tx, &condition(operator, NIK_A))
            .await
            .unwrap_err();
        assert!(
            error
                .message()
                .contains("national_id of test_bidx.person is encrypted"),
            "{operator:?}: {}",
            error.message()
        );
    }
    // the companion column is not a field: it is never read nor described
    assert!(people.spec().field("national_id__bidx").is_none());
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn a_typed_key_on_a_blind_indexed_field_refuses_duplicates() {
    let h = harness(Shape::BlindKeyed).await;
    let mut tx = h.begin().await.unwrap();
    create(&mut tx, &[("a", Some(NIK_A)), ("b", None), ("c", None)]).await;
    assert_eq!(
        count(
            &mut tx,
            "SELECT string_agg(pg_get_constraintdef(oid), ',') FROM pg_constraint \
             WHERE conrelid = 'test_bidx_person'::regclass AND contype = 'u'",
        )
        .await,
        "UNIQUE (national_id__bidx)"
    );
    tx.commit().await.unwrap();

    let mut tx = h.begin().await.unwrap();
    let people = tx.env().records(MODEL).unwrap();
    let created = people
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", str_cell("dup"))
                    .set_cell("national_id", str_cell(NIK_A)),
            ],
        )
        .await;
    let error = match created {
        Ok(_) => tx.flush().await.unwrap_err(),
        Err(error) => error,
    };
    assert!(
        error
            .message()
            .contains("This national id is already used."),
        "{}",
        error.message()
    );
    drop(tx);

    // an upsert would not maintain the companion column: refused by name
    let mut tx = h.begin().await.unwrap();
    let people = tx.env().records(MODEL).unwrap();
    let error = people
        .upsert(
            &mut tx,
            UpsertCall::new("_nid_key").with_rows([RawVals::new()
                .set_cell("name", str_cell("u"))
                .set_cell("national_id", str_cell(NIK_B))]),
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert!(
        error.message().contains("blind-indexed"),
        "{}",
        error.message()
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn schema_sync_fills_the_blind_index_of_existing_rows() {
    // clear values, then encrypted, blind-indexed and keyed in one upgrade
    let h = harness(Shape::Plain).await;
    let mut tx = h.begin().await.unwrap();
    tx.batch(&format!(
        "INSERT INTO {TABLE} (name, national_id) \
         VALUES ('a', '{NIK_A}'), ('b', NULL), ('c', '{NIK_B}')"
    ))
    .await
    .unwrap();
    tx.commit().await.unwrap();

    let keyed = orm(&h, Shape::BlindKeyed);
    let mut tx = keyed.begin(SUPERUSER_ID).await.unwrap();
    tx.init_models(&[]).await.unwrap();
    tx.commit().await.unwrap();
    let mut tx = keyed.begin(SUPERUSER_ID).await.unwrap();
    assert_eq!(names(&mut tx, &condition(Operator::Eq, NIK_A)).await, ["a"]);
    assert_eq!(names(&mut tx, &condition(Operator::Eq, NIK_B)).await, ["c"]);
    assert_eq!(
        names(&mut tx, &condition(Operator::Eq, Value::Bool(false))).await,
        ["b"]
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn schema_sync_recomputes_stale_blind_indexes() {
    let h = harness(Shape::Blind).await;
    let mut tx = h.begin().await.unwrap();
    create(&mut tx, &[("a", Some(NIK_A)), ("b", None)]).await;
    // a hash of another key id (the last key of the file changed) and a hash left on a cleared
    // value
    tx.batch(&format!(
        "UPDATE {TABLE} SET national_id__bidx = 'old:00' WHERE name = 'a'; \
         UPDATE {TABLE} SET national_id__bidx = 't1:ff' WHERE name = 'b'"
    ))
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let mut tx = h.begin().await.unwrap();
    assert!(
        names(&mut tx, &condition(Operator::Eq, NIK_A))
            .await
            .is_empty()
    );
    drop(tx);

    let again = orm(&h, Shape::Blind);
    let mut tx = again.begin(SUPERUSER_ID).await.unwrap();
    tx.init_models(&[]).await.unwrap();
    tx.commit().await.unwrap();
    let mut tx = again.begin(SUPERUSER_ID).await.unwrap();
    assert_eq!(names(&mut tx, &condition(Operator::Eq, NIK_A)).await, ["a"]);
    assert_eq!(
        count(
            &mut tx,
            &format!("SELECT (national_id__bidx IS NULL)::text FROM {TABLE} WHERE name = 'b'")
        )
        .await,
        "true"
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn a_field_no_longer_blind_indexed_loses_its_companion_column() {
    let h = harness(Shape::Blind).await;
    let mut tx = h.begin().await.unwrap();
    create(&mut tx, &[("a", Some(NIK_A))]).await;
    tx.commit().await.unwrap();

    let encrypted = orm(&h, Shape::Encrypted);
    let mut tx = encrypted.begin(SUPERUSER_ID).await.unwrap();
    tx.init_models(&[]).await.unwrap();
    tx.commit().await.unwrap();
    let mut tx = encrypted.begin(SUPERUSER_ID).await.unwrap();
    assert_eq!(
        count(
            &mut tx,
            &format!(
                "SELECT count(*)::text FROM information_schema.columns \
                 WHERE table_name = '{TABLE}' AND column_name = 'national_id__bidx'"
            )
        )
        .await,
        "0"
    );
    // the value itself is untouched
    let people = tx.env().records(MODEL).unwrap();
    let all = people.search(&mut tx, &Domain::TRUE).await.unwrap();
    assert_eq!(
        all.cell(&mut tx, "national_id").await.unwrap(),
        str_cell(NIK_A)
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn duplicates_in_existing_rows_stop_the_key() {
    let h = harness(Shape::Blind).await;
    let mut tx = h.begin().await.unwrap();
    create(&mut tx, &[("a", Some(NIK_A)), ("b", Some(NIK_A))]).await;
    tx.commit().await.unwrap();

    let keyed = orm(&h, Shape::BlindKeyed);
    let mut tx = keyed.begin(SUPERUSER_ID).await.unwrap();
    let error = tx.init_models(&[]).await.unwrap_err();
    assert!(
        error
            .message()
            .contains("This national id is already used.")
            || error.message().contains("_nid_key"),
        "{}",
        error.message()
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn optimizer_does_not_fold_disallowed_operators_on_encrypted_fields() {
    let h = harness(Shape::Blind).await;
    let mut tx = h.begin().await.unwrap();
    create(&mut tx, &[("a", Some(NIK_A)), ("none", None)]).await;
    let people = tx.env().records(MODEL).unwrap();
    for (op, val) in [
        (Operator::ILike, Value::from("")),
        (Operator::EqLike, Value::from("")),
        (Operator::EqIfSet, Value::Bool(false)),
    ] {
        let error = people
            .search(&mut tx, &condition(op, val))
            .await
            .unwrap_err();
        assert!(
            error
                .message()
                .contains("national_id of test_bidx.person is encrypted"),
            "{op:?}: {}",
            error.message()
        );
    }
    // `= false` must check only `IS NULL` on `__bidx`, never `OR __bidx = ''`.
    tx.batch(&format!(
        "UPDATE {TABLE} SET national_id__bidx = '' WHERE name = 'a'"
    ))
    .await
    .unwrap();
    assert_eq!(
        names(&mut tx, &condition(Operator::Eq, Value::Bool(false))).await,
        ["none"]
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn removing_key_and_blind_index_together_drops_both_cleanly() {
    let h = harness(Shape::BlindKeyed).await;
    let mut tx = h.begin().await.unwrap();
    create(&mut tx, &[("a", Some(NIK_A))]).await;
    tx.commit().await.unwrap();

    // Upgrading from BlindKeyed -> Encrypted drops the unique constraint in Phase 2 before
    // dropping `national_id__bidx` (avoids PostgreSQL 2BP01 dependent-object failure).
    let encrypted = orm(&h, Shape::Encrypted);
    let mut tx = encrypted.begin(SUPERUSER_ID).await.unwrap();
    tx.init_models(&[]).await.unwrap();
    tx.commit().await.unwrap();
    let mut tx = encrypted.begin(SUPERUSER_ID).await.unwrap();
    assert_eq!(
        count(
            &mut tx,
            &format!(
                "SELECT count(*)::text FROM information_schema.columns \
                 WHERE table_name = '{TABLE}' AND column_name = 'national_id__bidx'"
            )
        )
        .await,
        "0"
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[test]
fn blind_index_companion_column_over_63_bytes_is_refused() {
    let long_name: &'static str = Box::leak("a".repeat(58).into_boxed_str());
    let fields = vec![
        FieldDecl::char("name"),
        FieldDecl::char(long_name).encrypted(true).blind_index(true),
    ];
    let decl = ModelDecl::new("test_bidx", "test_bidx.longcol").fields(Vec::leak(fields));
    let error = RegistryBuilder::new([&ADDON])
        .with_decls([&*Box::leak(Box::new(decl))])
        .build()
        .unwrap_err();
    assert!(
        error.to_string().contains("63"),
        "expected 63-byte identifier error, got: {error}"
    );
}
