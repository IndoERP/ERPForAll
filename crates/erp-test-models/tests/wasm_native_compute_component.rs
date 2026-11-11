//! Actual unlinked package compute boundary; no native stand-in or inventory replacement.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use std::sync::Arc;

use erp_fields::{Cell, ErrorKind};
use erp_orm::{Context, WasmComputeSpec, is_wasm_abi1_native_refusal, wasm_compute_impl};
use erp_registry::{AddonDescriptor, FieldDecl, ModelDecl};
use erp_sdk::{RawVals, Value};
use erp_test_models::base_users;
use erp_testkit::{FieldPolicy, Harness};
use erp_wasm_host::{
    CallableMethods, Limits, MethodRole, OrmCapabilities, WasmAddon, WasmHost, described_methods,
};

const MODEL: &str = "test.wasm.life";
static LIFE: AddonDescriptor = AddonDescriptor {
    name: "test_wasm_life",
    depends: &["base"],
    exact_only: false,
};
static EXT: AddonDescriptor = AddonDescriptor {
    name: "test_native_compute",
    depends: &["test_wasm_life"],
    exact_only: false,
};

#[derive(Clone, Copy)]
enum NativeKind {
    Uuid,
    Xid8,
    TimestampTz,
}
impl NativeKind {
    fn field(self, name: &'static str) -> FieldDecl {
        match self {
            Self::Uuid => FieldDecl::uuid(name),
            Self::Xid8 => FieldDecl::xid8(name),
            Self::TimestampTz => FieldDecl::timestamptz(name),
        }
    }
}

async fn harness(
    native_input: Option<NativeKind>,
    native_output: Option<NativeKind>,
    deny: Option<&str>,
) -> Harness {
    let bytes = std::fs::read(
        std::env::var("ERP_WASM_LIFE_COMPONENT")
            .expect("ERP_WASM_LIFE_COMPONENT must name the actual unlinked package component"),
    )
    .unwrap();
    let host = Arc::new(WasmHost::new().unwrap());
    let component = host.component(&bytes).unwrap();
    let addon = WasmAddon::new(
        host,
        LIFE.name,
        component,
        Limits::default(),
        OrmCapabilities::NONE,
        CallableMethods::default(),
    );
    let description = addon.describe().await.unwrap();
    let described = described_methods(LIFE.name, &description).unwrap();
    assert!(
        described.iter().any(|method| method.model == MODEL
            && method.method == "_compute_copy"
            && method.role == MethodRole::Compute),
        "the actual component must describe this compute role"
    );
    let compute = wasm_compute_impl(&WasmComputeSpec {
        addon: LIFE.name.into(),
        model: MODEL.into(),
        method: "_compute_copy".into(),
        depends: vec!["qty".into()],
        reads: vec![],
        depends_context: vec![],
    })
    .unwrap();
    let mut fields = vec![FieldDecl::uuid("unrelated_native")];
    if let Some(kind) = native_input {
        fields.push(kind.field("qty"));
    }
    if let Some(kind) = native_output {
        fields.push(
            kind.field("copied_qty")
                .compute("_compute_copy")
                .compute_sudo(false)
                .store(false),
        );
    }
    let decls = [
        ModelDecl::new(LIFE.name, MODEL).fields(
            const {
                &[
                    FieldDecl::char("name").required(true),
                    FieldDecl::integer("qty"),
                    FieldDecl::integer("copied_qty")
                        .compute("_compute_copy")
                        .compute_sudo(false)
                        .store(false),
                ]
            },
        ),
        ModelDecl::new(EXT.name, MODEL)
            .inherit(&[MODEL])
            .fields(Box::leak(fields.into_boxed_slice())),
    ];
    let mut policy = FieldPolicy::new();
    if let Some(field) = deny {
        policy = policy.deny(MODEL, field, erp_orm::FieldOp::Read);
    }
    let h = Harness::builder([&base_users::ADDON, &LIFE, &EXT])
        .as_user(4)
        .with_decls(decls)
        .with_computes([compute])
        .with_wasm_methods(addon.method_impls(&description).unwrap())
        .policy(policy)
        .build()
        .await
        .unwrap();
    let mut tx = h
        .orm()
        .begin(erp_orm::PrincipalHint::system())
        .await
        .unwrap();
    tx.execute(
        "INSERT INTO res_users (id, name, login, active) VALUES (4, 'four', 'four', true)",
        &[],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    h
}

#[tokio::test]
#[ignore = "requires the actual unlinked test_wasm_life component"]
async fn native_fields_ordinary_package_compute_stays_active() {
    let h = harness(None, None, None).await;
    let ctx = Context::from_pairs([("allowed_company_ids", Value::list([Value::Int(1)]))]);
    let mut tx = h.orm().begin_with(h.principal(), ctx).await.unwrap();
    let model = tx.env().records(MODEL).unwrap();
    let made = model
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("ordinary".into()))
                    .set_cell("qty", Cell::Int(7)),
            ],
        )
        .await
        .unwrap();
    assert_eq!(
        made.cells(&mut tx, "copied_qty").await.unwrap(),
        vec![Cell::Int(7)]
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
#[ignore = "requires the actual unlinked test_wasm_life component"]
async fn native_fields_package_compute_refuses_native_metadata_after_acl() {
    for kind in [NativeKind::Uuid, NativeKind::Xid8, NativeKind::TimestampTz] {
        for (native_input, native_output, deny) in [
            (Some(kind), None, None),
            (None, Some(kind), None),
            (Some(kind), None, Some("qty")),
            (None, Some(kind), Some("copied_qty")),
        ] {
            let h = harness(native_input, native_output, deny).await;
            let ctx = Context::from_pairs([("allowed_company_ids", Value::list([Value::Int(1)]))]);
            let mut tx = h.orm().begin_with(h.principal(), ctx).await.unwrap();
            let model = tx.env().records(MODEL).unwrap();
            let made = model
                .create(
                    &mut tx,
                    vec![RawVals::new().set_cell("name", Cell::Str("native".into()))],
                )
                .await
                .unwrap();
            let error = made.cells(&mut tx, "copied_qty").await.unwrap_err();
            if deny.is_some() {
                assert_eq!(error.kind(), ErrorKind::Access, "{error}");
                assert!(!is_wasm_abi1_native_refusal(&error));
                assert!(!error.message().contains("future ABI major"), "{error}");
            } else {
                assert_eq!(error.kind(), ErrorKind::Programming, "{error}");
                assert!(is_wasm_abi1_native_refusal(&error), "{error}");
                assert!(error.message().contains("future ABI major"), "{error}");
            }
            tx.rollback().await.unwrap();
            h.cleanup().await.unwrap();
        }
    }
}
