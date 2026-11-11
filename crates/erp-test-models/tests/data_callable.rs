//! The data-callable level of the exposure table: which
//! methods a `<function>` may call, and that the RPC answers do not change.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use erp_sdk::rpc::{Effect, Exposure, Receiver};
use erp_test_models::{base_users, test_convert, test_convert_ui};
use erp_testkit::Harness;

async fn harness() -> Harness {
    Harness::builder([
        &base_users::ADDON,
        &test_convert::ADDON,
        &test_convert_ui::ADDON,
        &erp_data::model_data::ADDON,
    ])
    .with_decls([erp_data::ir_model_data_decl()])
    .unlink_hook(erp_data::model_data::UNLINK_HOOK)
    .build()
    .await
    .expect("harness")
}

/// A method with nothing but `#[data_callable]` is callable from data and answers `Internal`
/// over RPC; its parameter table is the binder's.
#[tokio::test]
async fn data_callable_only_is_internal_over_rpc() {
    let h = harness().await;
    let orm = h.orm();
    let model = orm.registry().model_id("test_convert.usered").unwrap();
    let table = orm.exposure();

    let records_method = table.data_callable(model, "method").expect("data-callable");
    assert_eq!(records_method.receiver(), Receiver::Records);
    let names: Vec<_> = records_method.params().iter().map(|p| p.name()).collect();
    assert_eq!(names, ["a", "b", "extra"]);
    assert!(records_method.params().iter().all(|p| !p.required()));
    assert!(matches!(
        table.get(model, "method"),
        Some(Exposure::Internal)
    ));

    let model_method = table.data_callable(model, "model_method").unwrap();
    assert_eq!(model_method.receiver(), Receiver::Model);

    let typed = table.data_callable(model, "typed_args").unwrap();
    let required: Vec<_> = typed
        .params()
        .iter()
        .map(|p| (p.name(), p.required()))
        .collect();
    assert_eq!(required, [("count", true), ("label", false)]);
    h.cleanup().await.unwrap();
}

/// A method nobody declared stays out, and is still known as a method.
#[tokio::test]
async fn an_undeclared_method_is_not_data_callable() {
    let h = harness().await;
    let orm = h.orm();
    let model = orm.registry().model_id("test_convert.usered").unwrap();
    let table = orm.exposure();
    assert!(table.data_callable(model, "internal_only").is_none());
    assert!(matches!(
        table.get(model, "internal_only"),
        Some(Exposure::Internal)
    ));
    assert!(table.data_callable(model, "no_such_method").is_none());
    assert!(table.get(model, "no_such_method").is_none());
    h.cleanup().await.unwrap();
}

/// `#[rpc(effect = …)]` is data-callable too, and `#[rpc(hide)]` does not take it away.
#[tokio::test]
async fn rpc_methods_are_data_callable_and_hide_does_not_remove_it() {
    let h = harness().await;
    let orm = h.orm();
    let model = orm.registry().model_id("test_convert.usered").unwrap();
    let table = orm.exposure();

    let rpc = table.data_callable(model, "rpc_write").unwrap();
    assert_eq!(rpc.receiver(), Receiver::Model);
    match table.get(model, "rpc_write") {
        Some(Exposure::Exposed(e)) => assert_eq!(e.effect(), Effect::Write),
        other => panic!("rpc_write: {other:?}"),
    }

    assert!(table.data_callable(model, "hidden_but_callable").is_some());
    assert!(matches!(
        table.get(model, "hidden_but_callable"),
        Some(Exposure::Hidden { .. })
    ));

    // core methods are RPC methods, so they are data-callable
    for name in ["create", "search", "write", "unlink"] {
        assert!(table.data_callable(model, name).is_some(), "{name}");
    }
    h.cleanup().await.unwrap();
}

/// `DataCallable::call` checks what the caller gave it: records of the callable's own model, and
/// no records at all for a model method.
#[tokio::test]
async fn call_refuses_records_of_another_model_and_records_for_a_model_method() {
    use erp_sdk::ErrorKind;
    use erp_sdk::rpc::{BindError, RpcArgs};
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let model = tx.registry().model_id("test_convert.usered").unwrap();
    let callable = tx.data_callable(model, "model_method").cloned().unwrap();
    let env = tx.env();

    let other = env.records("test_convert.record").unwrap();
    let Err(BindError::Call(error)) = callable
        .call(&mut tx, other, RpcArgs::new(Default::default()))
        .await
    else {
        panic!("expected a refusal");
    };
    assert_eq!(
        (error.kind(), error.message()),
        (
            ErrorKind::Programming,
            "test_convert.usered.model_method was called on records of model 'test_convert.record'"
        )
    );

    let usered = env.records("test_convert.usered").unwrap();
    let values: erp_sdk::ValueMap = [("name".to_owned(), erp_sdk::Value::str("u"))]
        .into_iter()
        .collect();
    let vals = usered.vals_from_values(values.iter()).unwrap();
    let created = usered.create(&mut tx, vec![vals]).await.unwrap();
    let Err(BindError::Call(error)) = callable
        .call(&mut tx, created, RpcArgs::new(Default::default()))
        .await
    else {
        panic!("expected a refusal");
    };
    assert_eq!(
        (error.kind(), error.message()),
        (
            ErrorKind::Programming,
            "test_convert.usered.model_method is a model method and takes no records, but was \
             called on 1 record(s)"
        )
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}
