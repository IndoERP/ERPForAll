// SPDX-License-Identifier: MIT OR Apache-2.0
//! Declared SQL conditions against PostgreSQL, slice B5 (contract
//! ): a server error whose SQLSTATE and CONSTRAINT
//! diagnostic are a registered pair becomes a `Validation` error with the declared public message;
//! everything else keeps its mapping. Raising needs no installed object: a `DO` block raises the
//! error the way a native function declared by the addon would.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

use std::sync::Arc;

use erp_fields::Value;
use erp_registry::{
    AddonDescriptor, Registry, RegistryBuilder, SqlConditionDecl, SqlObjectDecl, SqlObjectManifest,
};
use erp_sdk::{Error, ErrorKind, Orm, SUPERUSER_ID, StaticPolicy};
use erp_test_models::base_users;
use erp_testkit::TestDb;

static ADDON: AddonDescriptor = AddonDescriptor {
    name: "test_sqcond",
    depends: &["base_users"],
    exact_only: false,
};
static MANIFEST: SqlObjectManifest = SqlObjectManifest::new(
    "test_sqcond",
    &["unbalanced", "locked_period", "other_state"],
);
static UNBALANCED: SqlObjectDecl = SqlObjectDecl::condition(
    "test_sqcond",
    "unbalanced",
    SqlConditionDecl::new("ERP01", "erp01_unbalanced", "The move is not balanced.")
        .kind(erp_registry::SqlConditionKind::Validation),
);
static LOCKED: SqlObjectDecl = SqlObjectDecl::condition(
    "test_sqcond",
    "locked_period",
    SqlConditionDecl::new("ERP01", "erp01_locked_period", "The period is locked.")
        .kind(erp_registry::SqlConditionKind::Validation),
);
static OTHER: SqlObjectDecl = SqlObjectDecl::condition(
    "test_sqcond",
    "other_state",
    SqlConditionDecl::new("ERP02", "erp02_other", "Another declared state.")
        .kind(erp_registry::SqlConditionKind::Validation),
);

const RAISED: &str = "row 42 secret";
const DETAIL: &str = "hidden detail";
const HINT: &str = "hidden hint";

fn registry() -> Arc<Registry> {
    Arc::new(
        RegistryBuilder::new([&base_users::ADDON, &ADDON])
            .require_sql_objects([&MANIFEST])
            .with_sql_objects([&UNBALANCED, &LOCKED, &OTHER])
            .build()
            .unwrap(),
    )
}

fn orm_on(db: &TestDb, registry: Arc<Registry>) -> Orm {
    Orm::builder(registry, db.pool().clone())
        .policy(StaticPolicy::AllowAll)
        .build()
        .unwrap()
}

/// Raise `state` with `identifier` (when given) in a transaction of its own, since the error
/// aborts it, and return the mapped error.
async fn raise(orm: &Orm, state: &str, identifier: Option<&str>) -> Error {
    let constraint = identifier.map_or_else(String::new, |i| format!(", CONSTRAINT = '{i}'"));
    let sql = format!(
        "DO $$ BEGIN RAISE EXCEPTION '{RAISED}' USING ERRCODE = '{state}'{constraint}, \
         DETAIL = '{DETAIL}', HINT = '{HINT}'; END $$"
    );
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    tx.batch(&sql).await.expect_err("the block raises")
}

/// Nothing the server said reaches the message, the display text or the context: neither the
/// raised text, DETAIL, HINT nor the CONTEXT line PostgreSQL adds, read from the error's source.
#[track_caller]
fn assert_no_server_text(error: &Error) {
    let server = std::error::Error::source(error)
        .and_then(|s| s.downcast_ref::<tokio_postgres::Error>())
        .and_then(tokio_postgres::Error::as_db_error)
        .expect("the PostgreSQL error stays the source");
    let context = server
        .where_()
        .expect("a raise in a DO block has a CONTEXT line");
    assert_eq!(server.message(), RAISED);
    assert_eq!(server.detail(), Some(DETAIL));
    assert_eq!(server.hint(), Some(HINT));
    let shown = format!("{error} {} {:?}", error.message(), error.context());
    for secret in [RAISED, DETAIL, HINT, context] {
        assert!(!shown.contains(secret), "{secret:?} leaked into {shown}");
    }
}

/// A registered `(SQLSTATE, identifier)` pair maps to `Validation` with the declaration's message
/// verbatim; the SQLSTATE and the identifier stay on the error, the context names the qualified
/// identity, and neither the raised text, DETAIL, HINT nor CONTEXT is shown.
#[tokio::test]
async fn a_registered_condition_maps_to_its_declared_message() {
    let db = TestDb::create().await.unwrap();
    let registry = registry();
    let orm = orm_on(&db, Arc::clone(&registry));

    for (state, identifier, key) in [
        ("ERP01", "erp01_unbalanced", "unbalanced"),
        ("ERP01", "erp01_locked_period", "locked_period"),
        ("ERP02", "erp02_other", "other_state"),
    ] {
        let (id, declared) = registry.sql_objects().condition(state, identifier).unwrap();
        assert_eq!(id.to_string(), format!("test_sqcond/{key}"));
        let error = raise(&orm, state, Some(identifier)).await;
        assert_eq!(
            error.kind(),
            ErrorKind::Validation,
            "{identifier}: {error:?}"
        );
        assert_eq!(error.message(), declared.message(), "{identifier}");
        assert_eq!(error.to_string(), declared.message());
        assert_eq!(error.sqlstate(), Some(state));
        assert_eq!(error.constraint(), Some(identifier));
        let Some(Value::Map(context)) = error.context() else {
            panic!("{identifier}: no context map: {:?}", error.context());
        };
        let is = |key: &str, text: &str| context.get(key) == Some(&Value::str(text));
        assert!(is("sql_condition", &id.to_string()));
        assert!(is("sql_condition_identifier", identifier));
        assert!(is("sqlstate", state));
        assert_eq!(context.len(), 3);
        assert_no_server_text(&error);
    }
    // the two ERP01 conditions keep their own messages
    let one = raise(&orm, "ERP01", Some("erp01_unbalanced")).await;
    let two = raise(&orm, "ERP01", Some("erp01_locked_period")).await;
    assert_eq!(one.message(), "The move is not balanced.");
    assert_eq!(two.message(), "The period is locked.");
}

/// Registry half: a second declaration of one pair is refused. Mapping half: only the exact pair
/// maps; a missing, unknown or crossed identifier, a message that equals a declared text, and the
/// standard SQLSTATEs keep their mapping.
#[tokio::test]
async fn unregistered_pairs_and_standard_states_keep_their_mapping() {
    // registry half: one declaration per pair
    static CLASH: SqlObjectDecl = SqlObjectDecl::condition(
        "test_sqcond",
        "clash",
        SqlConditionDecl::new("ERP01", "erp01_unbalanced", "Other text.")
            .kind(erp_registry::SqlConditionKind::Validation),
    );
    static CLASH_MANIFEST: SqlObjectManifest = SqlObjectManifest::new(
        "test_sqcond",
        &["unbalanced", "locked_period", "other_state", "clash"],
    );
    let refused = RegistryBuilder::new([&base_users::ADDON, &ADDON])
        .require_sql_objects([&CLASH_MANIFEST])
        .with_sql_objects([&UNBALANCED, &LOCKED, &OTHER, &CLASH])
        .build()
        .expect_err("a second declaration of one pair is refused");
    assert_eq!(refused.kind(), ErrorKind::Programming);
    assert_eq!(
        refused.message(),
        "SQL object 'test_sqcond/unbalanced': condition ERP01 'erp01_unbalanced' is also declared \
         by 'test_sqcond/clash'"
    );

    // mapping half
    let db = TestDb::create().await.unwrap();
    let orm = orm_on(&db, registry());

    let error = raise(&orm, "ERP01", None).await;
    assert_eq!(error.kind(), ErrorKind::Database);
    assert_eq!(error.message(), RAISED);
    assert_eq!(error.sqlstate(), Some("ERP01"));
    assert_eq!(error.constraint(), None);
    assert!(error.context().is_none());

    let error = raise(&orm, "ERP01", Some("erp01_unknown")).await;
    assert_eq!(error.kind(), ErrorKind::Database);
    assert_eq!(error.message(), RAISED);
    assert!(error.context().is_none());

    // the identifier alone never decides: a known identifier under another SQLSTATE
    let error = raise(&orm, "ERP02", Some("erp01_unbalanced")).await;
    assert_eq!(error.kind(), ErrorKind::Database);
    assert_eq!(error.message(), RAISED);
    assert_eq!(error.sqlstate(), Some("ERP02"));
    assert!(error.context().is_none());

    // the server's text never decides: it equals a declared message, no identifier
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let error = tx
        .batch(
            "DO $$ BEGIN RAISE EXCEPTION 'The move is not balanced.' USING ERRCODE = 'ERP01'; \
             END $$",
        )
        .await
        .expect_err("raises");
    assert_eq!(error.kind(), ErrorKind::Database);
    assert_eq!(error.message(), "The move is not balanced.");
    assert!(error.context().is_none());

    // standard states keep their kinds even with a registered identifier
    for (state, kind) in [
        ("40001", ErrorKind::Concurrency),
        ("40P01", ErrorKind::Concurrency),
        ("55P03", ErrorKind::Lock),
    ] {
        let error = raise(&orm, state, Some("erp01_unbalanced")).await;
        assert_eq!(error.kind(), kind, "{state}: {error:?}");
        assert_eq!(error.message(), RAISED, "{state}");
        assert_eq!(error.sqlstate(), Some(state));
        assert!(error.context().is_none(), "{state}");
    }

    // class 23 keeps its mapping and text
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    tx.batch("CREATE TEMP TABLE sqcond_unique (v int UNIQUE)")
        .await
        .unwrap();
    tx.batch("INSERT INTO sqcond_unique VALUES (1)")
        .await
        .unwrap();
    let error = tx
        .batch("INSERT INTO sqcond_unique VALUES (1)")
        .await
        .expect_err("unique violation");
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(error.sqlstate(), Some("23505"));
    assert_eq!(
        error.message(),
        "The database refused the change: duplicate key value violates unique constraint \
         \"sqcond_unique_v_key\""
    );
    assert_eq!(error.constraint(), Some("sqcond_unique_v_key"));
}

#[tokio::test]
async fn programming_condition_is_generic_and_not_a_validation() {
    use erp_registry::SqlConditionKind;
    static PROGRAMMING: SqlObjectDecl = SqlObjectDecl::condition(
        "test_sqcond",
        "programming",
        SqlConditionDecl::new("ERP10", "sqcond_immutable", "Never expose this diagnostic.")
            .kind(SqlConditionKind::Programming),
    );
    static EXPECTED: SqlObjectManifest = SqlObjectManifest::new("test_sqcond", &["programming"]);
    let registry = Arc::new(
        RegistryBuilder::new([&base_users::ADDON, &ADDON])
            .linked(false)
            .require_sql_objects([&EXPECTED])
            .with_sql_objects([&PROGRAMMING])
            .build()
            .unwrap(),
    );
    let db = TestDb::create().await.unwrap();
    let orm = orm_on(&db, registry);
    let error = raise(&orm, "ERP10", Some("sqcond_immutable")).await;
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(error.message(), "Internal server error");
    assert_eq!(error.sqlstate(), Some("ERP10"));
    assert_eq!(error.constraint(), Some("sqcond_immutable"));
    assert!(error.context().is_none());
    assert_no_server_text(&error);
    let rpc = erp_rpc::RpcFailure::Domain(error);
    assert_eq!(rpc.status(), 500);
    assert_eq!(rpc.message(), "Internal server error");
    assert_eq!(rpc.retry_after(), None);
}
