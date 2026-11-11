//! `init()` and `_register_hook()` overridden with the SDK macros chain onto the no-op
//! definitions of `base`.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

mod support;

use std::sync::atomic::Ordering;

use erp_orm::model_hooks::{INIT, REGISTER_HOOK, call_init, call_register_hook};
use erp_test_models::base_users;
use erp_test_models::test_model_hooks::{self as th, REGISTERED};
use erp_testkit::Harness;
use support::one;

#[tokio::test]
async fn sdk_overrides_of_init_and_register_hook() {
    let h = Harness::builder([&base_users::ADDON, &th::ADDON])
        .build()
        .await
        .unwrap();
    let model = h.registry().model("test.model_hooks").unwrap().id;
    // the override sits on top of base's definition
    for method in [INIT, REGISTER_HOOK] {
        let chain: Vec<&str> = h
            .orm()
            .dispatch()
            .implementations(model, method)
            .iter()
            .map(|i| i.addon)
            .collect();
        assert_eq!(chain, ["test_model_hooks", "base"], "{method}");
    }
    let mut tx = h.begin().await.unwrap();
    call_init(&mut tx, "test.model_hooks").await.unwrap();
    tx.batch("INSERT INTO test_model_hooks (name) VALUES ('ada')")
        .await
        .unwrap();
    assert_eq!(
        one(&mut tx, "SELECT name FROM test_model_hooks_names")
            .await
            .as_deref(),
        Some("ADA")
    );
    assert_eq!(REGISTERED.load(Ordering::SeqCst), 0);
    call_register_hook(&mut tx, "test.model_hooks")
        .await
        .unwrap();
    assert_eq!(REGISTERED.load(Ordering::SeqCst), 1);
    // models without an override reach base's no-op
    call_init(&mut tx, "res.users").await.unwrap();
    let err = call_init(&mut tx, "no.such.model").await.unwrap_err();
    assert_eq!(
        err.message(),
        "Model 'no.such.model' does not exist in registry."
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}
