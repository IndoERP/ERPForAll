// SPDX-License-Identifier: MIT OR Apache-2.0
//! Encrypted fields (`encrypted = true`) against a database: the ORM reads and writes the clear
//! value, the column holds only envelopes, SQL conditions, ordering and grouping on the field are
//! refused, and a field that becomes encrypted has its existing clear values sealed by schema sync.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod support;

use std::sync::Arc;

use erp_orm::field_encryption::{FieldKeys, install};
use erp_orm::{ReadGroup, SearchOptions};
use erp_registry::{AddonDescriptor, FieldDecl, ModelDecl, RegistryBuilder};
use erp_sdk::{Cell, Domain, ErrorKind, Operator, Orm, RawVals, SUPERUSER_ID, Transaction};
use erp_testkit::Harness;
use support::one;

static ADDON: AddonDescriptor = AddonDescriptor {
    name: "test_crypt",
    depends: &["base"],
    exact_only: false,
};

/// A fixed test key, never a deployment's.
const KEYS: &str = "t1 BwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwc=\n";
const CANARY: &str = "3201-CANARY-4567";

fn keys() {
    install(FieldKeys::parse(KEYS).unwrap()).unwrap();
}

fn person(encrypted: bool) -> ModelDecl {
    let fields = vec![
        FieldDecl::char("name"),
        FieldDecl::char("national_id").size(20).encrypted(encrypted),
    ];
    ModelDecl::new("test_crypt", "test_crypt.person").fields(Vec::leak(fields))
}

async fn harness(encrypted: bool) -> Harness {
    keys();
    Harness::builder([&ADDON])
        .with_decls([person(encrypted)])
        .build()
        .await
        .unwrap()
}

fn orm(h: &Harness, decl: ModelDecl) -> Orm {
    let registry = RegistryBuilder::new([&ADDON])
        .with_decls([&*Box::leak(Box::new(decl))])
        .build()
        .unwrap();
    h.orm().rebuild(Arc::new(registry)).build().unwrap()
}

fn str_cell(text: &str) -> Cell {
    Cell::Str(text.into())
}

/// Whether any stored byte sequence of the column contains `needle`.
async fn column_contains(tx: &mut Transaction, needle: &str) -> bool {
    one(
        tx,
        &format!(
            "SELECT bool_or(position(convert_to('{needle}', 'UTF8') IN national_id) > 0)::text \
             FROM test_crypt_person"
        ),
    )
    .await
    .as_deref()
        == Some("true")
}

#[tokio::test]
async fn values_round_trip_and_the_column_holds_only_envelopes() {
    let h = harness(true).await;
    let mut tx = h.begin().await.unwrap();
    assert_eq!(
        one(
            &mut tx,
            "SELECT udt_name::text FROM information_schema.columns \
             WHERE table_name = 'test_crypt_person' AND column_name = 'national_id'"
        )
        .await
        .as_deref(),
        Some("bytea")
    );
    let people = tx.env().records("test_crypt.person").unwrap();
    let created = people
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", str_cell("a"))
                    .set_cell("national_id", str_cell(CANARY)),
                RawVals::new().set_cell("name", str_cell("b")),
            ],
        )
        .await
        .unwrap();
    tx.flush().await.unwrap();
    let ids = created.ids();
    assert!(!column_contains(&mut tx, CANARY).await);
    assert_eq!(
        one(
            &mut tx,
            "SELECT count(*)::text FROM test_crypt_person WHERE national_id IS NULL"
        )
        .await
        .as_deref(),
        Some("1"),
        "an empty value stays NULL"
    );
    tx.commit().await.unwrap();

    // a fresh transaction reads from the database, not from the cache of the create
    let mut tx = h.begin().await.unwrap();
    let people = tx.env().records("test_crypt.person").unwrap();
    let first = people.browse([ids[0]]).unwrap();
    assert_eq!(
        first.cell(&mut tx, "national_id").await.unwrap(),
        str_cell(CANARY)
    );
    first
        .write(
            &mut tx,
            RawVals::new().set_cell("national_id", str_cell("NEW-CANARY")),
        )
        .await
        .unwrap();
    tx.flush().await.unwrap();
    assert!(!column_contains(&mut tx, "NEW-CANARY").await);
    tx.commit().await.unwrap();

    let mut tx = h.begin().await.unwrap();
    let people = tx.env().records("test_crypt.person").unwrap();
    assert_eq!(
        people
            .browse([ids[0]])
            .unwrap()
            .cell(&mut tx, "national_id")
            .await
            .unwrap(),
        str_cell("NEW-CANARY")
    );
    // an envelope copied to another row of the same column still opens (row binding is a
    // follow-up), but a value that is not an envelope is refused by name
    tx.batch(&format!(
        "UPDATE test_crypt_person SET national_id = convert_to('plain', 'UTF8') WHERE id = {}",
        ids[1]
    ))
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let mut tx = h.begin().await.unwrap();
    let people = tx.env().records("test_crypt.person").unwrap();
    let error = people
        .browse([ids[1]])
        .unwrap()
        .cell(&mut tx, "national_id")
        .await
        .unwrap_err();
    assert!(
        error
            .message()
            .contains("test_crypt.person.national_id is not an encrypted value"),
        "{}",
        error.message()
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn conditions_ordering_and_grouping_are_refused() {
    let h = harness(true).await;
    let mut tx = h.begin().await.unwrap();
    let people = tx.env().records("test_crypt.person").unwrap();
    for operator in [Operator::Eq, Operator::ILike] {
        let domain = Domain::condition("national_id", operator, CANARY).unwrap();
        let error = people.search(&mut tx, &domain).await.unwrap_err();
        assert!(
            error
                .message()
                .contains("national_id of test_crypt.person is encrypted"),
            "{}",
            error.message()
        );
    }
    let error = people
        .search_with(
            &mut tx,
            &Domain::TRUE,
            &SearchOptions::new().order("national_id"),
        )
        .await
        .unwrap_err();
    assert!(error.message().contains("ordering"), "{}", error.message());
    let error = people
        .read_group(
            &mut tx,
            ReadGroup::new(Domain::TRUE).groupby(["national_id"]),
        )
        .await
        .unwrap_err();
    assert!(error.message().contains("grouping"), "{}", error.message());
    // other fields of the model search as before
    let domain = Domain::condition("name", Operator::Eq, "x").unwrap();
    people.search(&mut tx, &domain).await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn a_field_that_becomes_encrypted_has_its_clear_values_sealed() {
    let h = harness(false).await;
    let mut tx = h.begin().await.unwrap();
    tx.batch(&format!(
        "INSERT INTO test_crypt_person (name, national_id) \
         VALUES ('a', '{CANARY}'), ('b', NULL), ('c', 'back\\slash é')"
    ))
    .await
    .unwrap();
    tx.commit().await.unwrap();

    let encrypted = orm(&h, person(true));
    let mut tx = encrypted.begin(SUPERUSER_ID).await.unwrap();
    tx.init_models(&[]).await.unwrap();
    tx.commit().await.unwrap();

    let mut tx = encrypted.begin(SUPERUSER_ID).await.unwrap();
    assert!(!column_contains(&mut tx, CANARY).await);
    let people = tx.env().records("test_crypt.person").unwrap();
    let all = people
        .search_with(&mut tx, &Domain::TRUE, &SearchOptions::new().order("name"))
        .await
        .unwrap();
    let mut values = Vec::new();
    for id in all.ids() {
        values.push(
            people
                .browse([id])
                .unwrap()
                .cell(&mut tx, "national_id")
                .await
                .unwrap(),
        );
    }
    assert_eq!(
        values,
        [str_cell(CANARY), Cell::Null, str_cell("back\\slash é")]
    );
    drop(tx);

    // declaring it plain again is refused before any DDL
    let plain = orm(&h, person(false));
    let mut tx = plain.begin(SUPERUSER_ID).await.unwrap();
    let error = tx.init_models(&[]).await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert!(
        error.message().contains("no longer declared encrypted"),
        "{}",
        error.message()
    );
    drop(tx);
    h.cleanup().await.unwrap();
}
