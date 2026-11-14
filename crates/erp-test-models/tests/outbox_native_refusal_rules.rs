// SPDX-License-Identifier: MIT OR Apache-2.0
//! a record rule is answered before the ABI1 native-field refusal.
//! The guest adapter calls `wasm_abi1_check_fields` and `wasm_abi1_check_call` before it touches
//! a record (`erp-wasm-host/src/adapter.rs`); these tests drive those two hooks directly, as an
//! ordinary user (not the superuser), without needing the guest component.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

use std::collections::BTreeMap;

use erp_fields::ErrorKind;
use erp_orm::{
    Operation, PrincipalHint, Records, Transaction, is_wasm_abi1_native_refusal,
    wasm_abi1_check_call, wasm_abi1_check_fields,
};
use erp_registry::{AddonDescriptor, FieldDecl, ModelDecl};
use erp_sdk::{Domain, Operator, Value};
use erp_test_models::base_users;
use erp_testkit::{DomainPolicy, Harness, Otherwise};

static ADDON: AddonDescriptor = AddonDescriptor {
    name: "test_native_rules",
    depends: &["base"],
    exact_only: false,
};
const ROOT: &str = "test.native.rules.root";
const CHILD: &str = "test.native.rules.child";

struct World {
    h: Harness,
    hidden: i64,
    visible: i64,
    hidden_child: i64,
}

async fn world() -> World {
    let only = |name: &'static str| Domain::condition(name, Operator::Eq, "shown").unwrap();
    let rules = DomainPolicy::builder(Otherwise::Allow)
        .rule(ROOT, Operation::Read, only("name"))
        .rule(ROOT, Operation::Write, only("name"))
        .rule(CHILD, Operation::Read, only("name"))
        .rule(CHILD, Operation::Write, only("name"))
        .build()
        .unwrap();
    let h = Harness::builder([&base_users::ADDON, &ADDON])
        .as_user(4)
        .policy(rules)
        .with_decls([
            ModelDecl::new(ADDON.name, ROOT).fields(
                const {
                    &[
                        FieldDecl::char("name"),
                        FieldDecl::uuid("native_uuid"),
                        FieldDecl::many2many("native_child_ids", CHILD),
                    ]
                },
            ),
            ModelDecl::new(ADDON.name, CHILD)
                .fields(const { &[FieldDecl::char("name"), FieldDecl::uuid("native_uuid")] }),
        ])
        .build()
        .await
        .unwrap();
    // fixtures are made by the system principal, which no record rule applies to
    let mut setup = h.orm().begin(PrincipalHint::system()).await.unwrap();
    setup
        .batch("INSERT INTO res_users (id, name, login, active) VALUES (4, 'four', 'four', true)")
        .await
        .unwrap();
    let mut make = async |model: &str, name: &str| {
        setup
            .env()
            .records(model)
            .unwrap()
            .create_values(
                &mut setup,
                &[BTreeMap::from([("name".into(), Value::str(name))])],
            )
            .await
            .unwrap()
            .ids()[0]
    };
    let hidden = make(ROOT, "secret").await;
    let visible = make(ROOT, "shown").await;
    let hidden_child = make(CHILD, "secret").await;
    setup.commit().await.unwrap();
    World {
        h,
        hidden,
        visible,
        hidden_child,
    }
}

fn root(tx: &Transaction, id: i64) -> Records {
    tx.env().records(ROOT).unwrap().browse([id]).unwrap()
}

/// The text of the record-rule refusal for user 4.
fn denied(verb: &str, model: &str, id: i64) -> String {
    format!(
        "Your access rights do not let you {verb} '{model}' ({model}) records.\n\nRecords: {model}({id},), User: 4"
    )
}

#[tokio::test]
async fn record_rule_answers_before_the_native_field_refusal() {
    let w = world().await;
    let mut tx = w.h.begin().await.unwrap();
    let names = ["native_uuid".to_owned()];
    for operation in [Operation::Write, Operation::Read] {
        let hidden = root(&tx, w.hidden);
        let error = wasm_abi1_check_fields(&mut tx, &hidden, operation, &names)
            .await
            .unwrap_err();
        assert!(
            !is_wasm_abi1_native_refusal(&error),
            "{operation:?}: {error:?}"
        );
        assert_eq!(error.kind(), ErrorKind::Access, "{operation:?}");
        assert_eq!(
            error.message(),
            denied(
                if operation == Operation::Write {
                    "modify"
                } else {
                    "access"
                },
                ROOT,
                w.hidden
            ),
            "{operation:?}"
        );
        // control: the same request on the visible record gets the native-field refusal
        let visible = root(&tx, w.visible);
        let error = wasm_abi1_check_fields(&mut tx, &visible, operation, &names)
            .await
            .unwrap_err();
        assert!(
            is_wasm_abi1_native_refusal(&error),
            "{operation:?}: {error:?}"
        );
        assert_eq!(error.kind(), ErrorKind::Programming, "{operation:?}");
        assert_eq!(
            error.message(),
            "field 'test.native.rules.root.native_uuid' (uuid) is not supported across frozen WASM ABI1; requires a future ABI major with typed native fields",
            "{operation:?}"
        );
    }
    // a hidden child reached through a visible root: the access error, not the native refusal
    let args = serde_json::json!([{
        "native_child_ids": [[1, w.hidden_child, {"native_uuid": null}]]
    }])
    .to_string();
    let visible = root(&tx, w.visible);
    let error = wasm_abi1_check_call(&mut tx, &visible, "write", &args)
        .await
        .unwrap_err();
    assert!(!is_wasm_abi1_native_refusal(&error), "{error:?}");
    assert_eq!(error.kind(), ErrorKind::Access);
    assert_eq!(error.message(), denied("modify", CHILD, w.hidden_child));
    tx.rollback().await.unwrap();
    w.h.cleanup().await.unwrap();
}
