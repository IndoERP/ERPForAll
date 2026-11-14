//! WebAssembly methods over RPC (partial): a method an addon
//! defines and declares for RPC is callable through `call_kw` / JSON-2 under the exposure and access
//! rules of a native method; one that declares nothing, an override, and a private name stay out.
//!
//! The WebAssembly layer is a stand-in `MethodInvoker` written in Rust (the chain tests do the same),
//! so the exposure and the access of the caller are proven without a component; the real component
//! goes through the browser path in `erp-cli/tests/wasm_addon_poc.rs`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod support;

use std::sync::Arc;

use erp_addon_test_wasm_dual_a::ADDON as ADDON_A;
use erp_addon_test_wasm_dual_b::ADDON as ADDON_B;
use erp_fields::Cell;
use erp_orm::rpc::{Effect, Exposure, WasmRpc, WasmRpcParam};
use erp_orm::{
    BoxFuture, DynSuper, ImplKind, MethodInvoker, Orm, RawVals, Records, Transaction,
    WasmMethodImpl,
};
use erp_rpc::{Caller, RpcCall, RpcConfig, RpcService};
use erp_sdk::Result;
use erp_test_models::base_users;
use erp_testkit::Harness;
use serde_json::json;
use support::rpc::{ALICE, BOB, FixtureHost, FixturePolicy, json, principal_with};

const MODEL: &str = "test.wasm.dual";

/// A WebAssembly layer written in Rust. `bump(by)` adds `by` to `qty` through the ORM, as the
/// caller, and returns the new quantity, as a component's body would with `get` and `set`.
#[derive(Debug)]
struct StandIn {
    method: &'static str,
    kind: ImplKind,
    rpc: Option<WasmRpc>,
}

impl MethodInvoker for StandIn {
    fn addon(&self) -> &str {
        ADDON_B.name
    }

    fn kind(&self) -> ImplKind {
        self.kind
    }

    fn rpc(&self) -> Option<WasmRpc> {
        self.rpc.clone()
    }

    fn invoke<'a>(
        &'a self,
        tx: &'a mut Transaction,
        records: &'a Records,
        args_json: &'a str,
        sup: DynSuper,
    ) -> BoxFuture<'a, Result<String>> {
        Box::pin(async move {
            match self.method {
                "bump" => {
                    let (by,): (i32,) = serde_json::from_str(args_json).unwrap();
                    let cells = records.cells(tx, "qty").await?;
                    let current = match cells.first() {
                        Some(Cell::Int(v)) => *v,
                        other => panic!("unexpected qty cell {other:?}"),
                    };
                    let next = current + by;
                    records
                        .write(tx, RawVals::new().set_cell("qty", Cell::Int(next)))
                        .await?;
                    Ok(next.to_string())
                }
                "plain" => Ok("\"plain\"".to_owned()),
                "label" => sup.call_json(tx, records, args_json).await,
                other => panic!("the stand-in has no method {other}"),
            }
        })
    }
}

fn rpc_bump() -> WasmRpc {
    WasmRpc {
        effect: Effect::Write,
        params: vec![WasmRpcParam {
            name: "by".to_owned(),
            required: true,
            expected: "an integer",
        }],
    }
}

fn layers() -> Vec<WasmMethodImpl> {
    // (method, kind, rpc declared)
    [
        ("bump", ImplKind::Define, Some(rpc_bump())),
        // defined, declares nothing for RPC
        ("plain", ImplKind::Define, None),
        // an override of a native method that declares RPC: an override never exposes
        (
            "label",
            ImplKind::Override,
            Some(WasmRpc {
                effect: Effect::Write,
                params: vec![],
            }),
        ),
    ]
    .into_iter()
    .map(|(method, kind, rpc)| WasmMethodImpl {
        model: MODEL.to_owned(),
        method: method.to_owned(),
        invoker: Arc::new(StandIn { method, kind, rpc }),
    })
    .collect()
}

async fn fixture() -> (Harness, Orm, RpcService) {
    let h = Harness::builder([&base_users::ADDON, &ADDON_A, &ADDON_B])
        .rules(FixturePolicy)
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    tx.batch(
        "INSERT INTO res_users (id, name, login, active) VALUES (2, 'alice', 'alice', true), \
         (3, 'bob', 'bob', true), (5, 'portal', 'portal', true); \
         SELECT setval(pg_get_serial_sequence('res_users', 'id'), 5)",
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let orm = h
        .orm()
        .rebuild(Arc::clone(h.registry()))
        .wasm_methods(layers())
        .build()
        .unwrap();
    let service = RpcService::new(orm.clone(), FixtureHost, RpcConfig::default());
    (h, orm, service)
}

async fn record(h: &Harness) -> i64 {
    let mut tx = h.begin().await.unwrap();
    let rows = tx
        .query(
            "INSERT INTO test_wasm_dual (name, qty) VALUES ('probe', 10) RETURNING id",
            &[],
        )
        .await
        .unwrap();
    let id: i64 = rows[0].get(0);
    tx.commit().await.unwrap();
    id
}

fn user(uid: i64) -> Caller {
    Caller::Principal(principal_with(uid, &[1], 1).build().unwrap())
}

async fn post(
    service: &RpcService,
    method: &str,
    body: &str,
    caller: Caller,
) -> erp_rpc::RpcResponse {
    service
        .call(RpcCall::json2(
            &format!("/json/2/{MODEL}/{method}"),
            body.as_bytes().to_vec(),
            caller,
        ))
        .await
}

async fn qty(h: &Harness, id: i64) -> i64 {
    let mut tx = h.begin().await.unwrap();
    let rows = tx
        .query(
            "SELECT qty::bigint FROM test_wasm_dual WHERE id = $1",
            &[&id],
        )
        .await
        .unwrap();
    tx.rollback().await.unwrap();
    rows[0].get(0)
}

#[tokio::test]
async fn only_a_defined_method_that_declares_rpc_is_exposed() {
    let (h, orm, _service) = fixture().await;
    let model = orm.registry().model_id(MODEL).unwrap();
    match orm.exposure().get(model, "bump") {
        Some(Exposure::Exposed(e)) => {
            assert_eq!(e.effect(), Effect::Write);
            let params = e.params();
            assert_eq!(params.len(), 1);
            assert_eq!(params[0].name(), "by");
            assert!(params[0].required());
        }
        other => panic!("bump: {other:?}"),
    }
    // nothing declared, an override of a native method: the chain exists, nothing exposes it
    assert!(matches!(
        orm.exposure().get(model, "plain"),
        Some(Exposure::Internal)
    ));
    assert!(matches!(
        orm.exposure().get(model, "label"),
        Some(Exposure::Internal)
    ));
    h.cleanup().await.unwrap();
}

/// caller; access, hidden and unknown names)
#[tokio::test]
async fn a_client_calls_it_as_itself_and_nothing_else() {
    let (h, _orm, service) = fixture().await;
    let id = record(&h).await;
    let body = format!(r#"{{"ids":[{id}],"by":5}}"#);

    // a user with write rights: the method runs, the write is stored, the result comes back
    let done = post(&service, "bump", &body, user(ALICE)).await;
    assert_eq!(
        done.status(),
        200,
        "{}",
        String::from_utf8_lossy(done.body())
    );
    assert_eq!(json(&done), json!(15));
    assert_eq!(qty(&h, id).await, 15);

    // a user who may only read: the ORM refuses the write inside the method, nothing changes
    let denied = post(&service, "bump", &body, user(BOB)).await;
    assert_eq!(
        denied.status(),
        403,
        "{}",
        String::from_utf8_lossy(denied.body())
    );
    assert_eq!(qty(&h, id).await, 15, "the refused call wrote nothing");

    // a required parameter is checked before the guest runs: absent, and an explicit null
    let null = post(
        &service,
        "bump",
        &format!(r#"{{"ids":[{id}],"by":null}}"#),
        user(ALICE),
    )
    .await;
    assert_eq!(
        null.status(),
        422,
        "{}",
        String::from_utf8_lossy(null.body())
    );
    assert_eq!(qty(&h, id).await, 15);
    let missing = post(
        &service,
        "bump",
        &format!(r#"{{"ids":[{id}]}}"#),
        user(ALICE),
    )
    .await;
    assert_eq!(
        missing.status(),
        422,
        "{}",
        String::from_utf8_lossy(missing.body())
    );
    // a parameter of the wrong shape too
    let wrong = post(
        &service,
        "bump",
        &format!(r#"{{"ids":[{id}],"by":"five"}}"#),
        user(ALICE),
    )
    .await;
    assert_eq!(
        wrong.status(),
        422,
        "{}",
        String::from_utf8_lossy(wrong.body())
    );
    assert_eq!(qty(&h, id).await, 15);

    // not declared / override / unknown: never callable from a client
    for method in ["plain", "label", "nonesuch"] {
        let refused = post(
            &service,
            method,
            &format!(r#"{{"ids":[{id}]}}"#),
            user(ALICE),
        )
        .await;
        assert!(
            matches!(refused.status(), 403 | 404),
            "{method}: {} {}",
            refused.status(),
            String::from_utf8_lossy(refused.body())
        );
    }
    h.cleanup().await.unwrap();
}

/// the policy gives nothing is refused with the access error, 403, and nothing is stored.
#[tokio::test]
async fn a_caller_without_rights_gets_the_access_error() {
    let (h, _orm, service) = fixture().await;
    let id = record(&h).await;
    // uid 5 is in neither the full-access nor the read-only set of the fixture policy
    let refused = post(
        &service,
        "bump",
        &format!(r#"{{"ids":[{id}],"by":1}}"#),
        user(5),
    )
    .await;
    let text = String::from_utf8_lossy(refused.body()).into_owned();
    assert_eq!(refused.status(), 403, "{text}");
    let body = json(&refused);
    assert_eq!(body["name"], "erp.exceptions.AccessError", "{text}");
    assert!(
        body["message"]
            .as_str()
            .unwrap()
            .contains("Your access rights do not let you"),
        "{text}"
    );
    assert_eq!(qty(&h, id).await, 10, "nothing was stored");
    h.cleanup().await.unwrap();
}
