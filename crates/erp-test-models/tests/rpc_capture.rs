//! The statement capture of one JSON-2 call (PERF.SERVE-FLOOR,
//! §2): off by default; on, the answer lists exactly the statements the answering attempt counted.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod support;

use erp_rpc::RpcCall;
use support::rpc::*;

#[tokio::test]
async fn a_call_captures_its_statements_only_when_asked() {
    let h = harness().await;
    let id = effect(&h, "captured", 1).await;
    let path = "/json/2/test_rpc.effects/read";
    let body = format!(r#"{{"ids": [{id}], "fields": ["name"]}}"#);

    let plain = call(&h, path, &body, alice()).await;
    ok(&plain);
    assert_eq!(plain.captured(), None, "no capture unless asked");

    let response = h
        .service()
        .call(RpcCall::json2(path, body.as_bytes().to_vec(), alice()).capture_statements())
        .await;
    assert_eq!(
        ok(&response),
        ok(&plain),
        "the capture changes nothing in the answer"
    );
    let captured = response.captured().expect("the capture was asked for");
    assert_eq!(
        captured.len() as u64,
        response.statements(),
        "the capture lists the statements the call counted: {captured:?}"
    );
    assert!(
        captured.iter().any(|s| s.contains("\"test_rpc_effects\"")),
        "the read's statement is captured: {captured:?}"
    );
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn a_failed_call_carries_no_capture() {
    let h = harness().await;
    let response = h
        .service()
        .call(
            RpcCall::json2(
                "/json/2/test_rpc.effects/read",
                br#"{"ids": [1], "fields": ["no_such_field"]}"#.to_vec(),
                alice(),
            )
            .capture_statements(),
        )
        .await;
    assert_ne!(response.status(), 200);
    assert_eq!(
        response.captured(),
        None,
        "a failure answers without a capture"
    );
    h.cleanup().await.unwrap();
}
