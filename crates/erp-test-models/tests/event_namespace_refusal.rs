//! Every generic ORM path refuses a model of the closed event namespace (A2-D5,
//! plan S2): create, write, unlink, upsert, copy, search, read, grouping,
//! streaming (export), `exists`/locks and the RPC bindings return one typed error before any
//! statement is sent, and an ordinary model next to it is untouched.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use std::collections::BTreeMap;
use std::sync::Arc;

use erp_fields::{ErrorKind, Value, ValueMap};
use erp_orm::rpc::{BindError, Exposure, RpcArgs};
use erp_orm::{ReadGroup, SearchOptions, StreamOptions, UpsertCall};
use erp_registry::{FieldDecl, ModelDecl, PartitionDecl, PartitionInterval, PartitionRetain};
use erp_sdk::Domain;
use erp_test_models::base_users;
use erp_testkit::Harness;

static TEST_EVENT: erp_sdk::AddonDescriptor = erp_sdk::AddonDescriptor {
    name: "test_event_orm",
    depends: &["base"],
    exact_only: false,
};

const EVENT: &str = "test_event_orm.event";
const PLAIN: &str = "test_event_orm.plain";

/// The message of the one guard, for the model above.
const REFUSED: &str = "model 'test_event_orm.event' is in the closed event namespace: the generic \
    ORM refuses every record operation; the a later release capture path is the only writer and nothing \
    reads it through the ORM";

fn event_decl() -> ModelDecl {
    ModelDecl::new("test_event_orm", EVENT)
        .log_access(false)
        .fields(
            const {
                &[
                    FieldDecl::timestamptz("recorded_at")
                        .required(true)
                        .server_set(true),
                    FieldDecl::char("name"),
                ]
            },
        )
        .partition(
            PartitionDecl::new(PartitionInterval::Day, 7, PartitionRetain::Intervals(30))
                .by("recorded_at"),
        )
}

async fn harness() -> Harness {
    Harness::builder([&base_users::ADDON, &TEST_EVENT])
        .with_core_event_decls([event_decl()])
        .with_decls([
            ModelDecl::new("test_event_orm", PLAIN).fields(const { &[FieldDecl::char("name")] })
        ])
        .build()
        .await
        .unwrap()
}

fn vmap(name: &str) -> BTreeMap<String, Value> {
    BTreeMap::from([("name".to_owned(), Value::str(name))])
}

#[track_caller]
fn assert_refused<T: std::fmt::Debug>(result: erp_fields::Result<T>) {
    let error = result.expect_err("the generic ORM must refuse an event-namespace model");
    assert_eq!(error.kind(), ErrorKind::Programming, "{error:?}");
    assert_eq!(error.message(), REFUSED);
}

/// create, write, unlink and upsert on an event-namespace model are refused as the superuser and
/// as an ordinary user, with no statement sent, and the ordinary model keeps working.
#[tokio::test]
async fn generic_mutations_refuse_event_namespace_models() {
    let h = harness().await;
    for uid in [erp_orm::SUPERUSER_ID.get(), 2] {
        let mut tx = h.begin_as(uid).await.unwrap();
        let events = tx.env().records(EVENT).unwrap();
        let ghost = events.browse([1]).unwrap();
        let before = tx.query_count();

        assert_refused(events.create_values(&mut tx, &[vmap("x")]).await);
        assert_refused(events.create(&mut tx, Vec::new()).await);
        let change = ValueMap::from_iter([("name".to_owned(), Value::str("x"))]);
        assert_refused(ghost.write_values(&mut tx, change.iter()).await);
        assert_refused(ghost.set_value(&mut tx, "name", Value::str("x")).await);
        assert_refused(ghost.unlink(&mut tx).await);
        assert_refused(
            events
                .upsert(&mut tx, UpsertCall::new("name").with_rows([]))
                .await,
        );
        assert_refused(
            events
                .upsert_values(&mut tx, "name", &[vmap("x")], &[], &[])
                .await,
        );
        assert_eq!(
            tx.query_count(),
            before,
            "a refused call sends no statement"
        );

        // the refusal leaves the transaction usable: an ordinary model is unaffected
        let plain = tx.env().records(PLAIN).unwrap();
        if uid == erp_orm::SUPERUSER_ID.get() {
            let made = plain.create_values(&mut tx, &[vmap("ok")]).await.unwrap();
            assert_eq!(made.len(), 1);
        }
    }
    h.cleanup().await.unwrap();
}

/// copy, `exists` and the row locks are refused with no statement sent.
#[tokio::test]
async fn copy_exists_and_locks_refuse_event_namespace_models() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let ghost = tx.env().records(EVENT).unwrap().browse([1]).unwrap();
    let before = tx.query_count();
    assert_refused(ghost.copy(&mut tx, None).await);
    assert_refused(ghost.copy_data(&mut tx, None).await);
    assert_refused(ghost.exists(&mut tx).await);
    assert_refused(ghost.lock_for_update(&mut tx, false).await);
    assert_refused(ghost.try_lock_for_update(&mut tx, false, None).await);
    assert_eq!(tx.query_count(), before);
    h.cleanup().await.unwrap();
}

/// search, count, name search, read, fetch and a value read are refused as the superuser too (the
/// superuser skips the model-level access check on the read side), with no statement sent.
#[tokio::test]
async fn generic_reads_refuse_event_namespace_models() {
    let h = harness().await;
    for uid in [erp_orm::SUPERUSER_ID.get(), 2] {
        let mut tx = h.begin_as(uid).await.unwrap();
        let events = tx.env().records(EVENT).unwrap();
        let ghost = events.browse([1]).unwrap();
        let before = tx.query_count();

        assert_refused(events.search(&mut tx, &Domain::TRUE).await);
        assert_refused(
            events
                .search_with(&mut tx, &Domain::TRUE, &SearchOptions::new().limit(1))
                .await,
        );
        assert_refused(events.search_count(&mut tx, &Domain::TRUE, None).await);
        assert_refused(events.name_search(&mut tx, "x").await);
        assert_refused(
            events
                .search_read(&mut tx, &Domain::TRUE, &["name"], &SearchOptions::new())
                .await,
        );
        assert_refused(ghost.read(&mut tx, &["name"]).await);
        assert_refused(ghost.fetch(&mut tx, Some(&["name"])).await);
        assert_refused(ghost.value(&mut tx, "name").await);
        assert_refused(
            events
                .read_group(
                    &mut tx,
                    ReadGroup::new(Domain::TRUE)
                        .groupby(["name"])
                        .aggregates(["__count"]),
                )
                .await,
        );
        assert_eq!(
            tx.query_count(),
            before,
            "a refused call sends no statement"
        );
    }
    h.cleanup().await.unwrap();
}

/// the export stream over an event-namespace model refuses its first window.
#[tokio::test]
async fn export_stream_refuses_event_namespace_models() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let ghost = tx.env().records(EVENT).unwrap().browse([1]).unwrap();
    let before = tx.query_count();
    let mut stream = ghost
        .stream_records(&["name"], &StreamOptions::default())
        .unwrap();
    assert_refused(stream.next(&mut tx).await.map(|_| ()));
    assert_eq!(tx.query_count(), before);
    h.cleanup().await.unwrap();
}

/// the core RPC bindings (read, search, create, write, unlink) refuse an event-namespace model with
/// the same error, so no RPC route reaches its table through the ORM.
#[tokio::test]
async fn rpc_bindings_refuse_event_namespace_models() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let events = tx.env().records(EVENT).unwrap();
    let model = events.model_id();
    let before = tx.query_count();
    for method in [
        "search_read",
        "search_count",
        "read",
        "create",
        "write",
        "unlink",
    ] {
        let records = if matches!(method, "read" | "write" | "unlink") {
            events.browse([1]).unwrap()
        } else {
            events.clone()
        };
        let exposure = tx
            .exposure()
            .get(model, method)
            .unwrap_or_else(|| panic!("{method} is a base method"));
        let Exposure::Exposed(exposed) = exposure else {
            panic!("{method} must be exposed by the base addon");
        };
        let exposed = exposed.clone();
        let args = match method {
            "create" => ValueMap::from_iter([("vals_list".to_owned(), Value::List(Arc::from([])))]),
            "write" => {
                ValueMap::from_iter([("vals".to_owned(), Value::Map(Arc::new(ValueMap::new())))])
            }
            "search_count" => {
                ValueMap::from_iter([("domain".to_owned(), Value::List(Arc::from([])))])
            }
            _ => ValueMap::new(),
        };
        let result = exposed.call(&mut tx, records, RpcArgs::new(args)).await;
        match result {
            Err(BindError::Call(error)) => {
                assert_eq!(error.kind(), ErrorKind::Programming, "{method}: {error:?}");
                assert_eq!(error.message(), REFUSED, "{method}");
            }
            other => panic!("{method}: expected the refusal, got {other:?}"),
        }
    }
    assert_eq!(tx.query_count(), before);
    h.cleanup().await.unwrap();
}
