//! Actual HTML RPC policy/report delivery, rollback and idempotent response storage.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]
mod support;
use serde_json::json;
use support::rpc::*;
const CREATE: &str = "/json/2/test_orm.mixed/create";

#[tokio::test]
async fn html_rpc_default_refuses_removals_without_storing() {
    let h = harness().await;
    let response = call(
        &h,
        CREATE,
        r#"{"vals_list":[{"html":"<p onclick='secret'>Body</p>"}]}"#,
        alice(),
    )
    .await;
    assert_eq!(
        response.status(),
        422,
        "{}",
        String::from_utf8_lossy(response.body())
    );
    let error: serde_json::Value = serde_json::from_slice(response.body()).unwrap();
    assert_eq!(error["name"], "erp.exceptions.ValidationError");
    assert_eq!(error["html_reports"][0]["field"], "html");
    assert_eq!(error["html_reports"][0]["entries"][0]["name"], "onclick");
    assert!(!String::from_utf8_lossy(response.body()).contains("secret"));
    assert_eq!(count(&h, "test_orm_mixed").await, 0);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn html_rpc_clean_report_and_replay_share_the_stored_response() {
    let h = harness().await;
    let k = key("018f3a1c-7b2d-7c4e-9a10-00000000abcd");
    let body =
        json!({"html_validation":"clean", "vals_list":[{"html":"<p onclick='secret'>Body</p>"}]})
            .to_string();
    let first = call_keyed(&h, CREATE, &body, alice(), &k).await;
    assert_eq!(
        first.status(),
        200,
        "{}",
        String::from_utf8_lossy(first.body())
    );
    let result: serde_json::Value = serde_json::from_slice(first.body()).unwrap();
    assert!(result["result"].is_array());
    assert_eq!(result["html_reports"][0]["model"], "test_orm.mixed");
    assert_eq!(result["html_reports"][0]["field"], "html");
    assert_eq!(result["html_reports"][0]["entries"][0]["count"], 1);
    assert_eq!(
        sql_one(&h, "SELECT html FROM test_orm_mixed").await,
        "<p>Body</p>"
    );
    let again = call_keyed(&h, CREATE, &body, alice(), &k).await;
    assert!(again.replayed());
    assert_eq!(again.body(), first.body());
    let changed = body.replace("\"clean\"", "\"strict\"");
    let mismatch = call_keyed(&h, CREATE, &changed, alice(), &k).await;
    assert_eq!(mismatch.status(), 422);
    assert_eq!(count(&h, "test_orm_mixed").await, 1);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn html_rpc_explicit_strict_preserves_errors_and_legacy_success_shape() {
    let h = harness().await;
    let response = call(
        &h,
        CREATE,
        r#"{"html_validation":"strict","vals_list":[{"html":"<p onclick='private'>Body</p>"}]}"#,
        alice(),
    )
    .await;
    assert_eq!(response.status(), 422);
    let error: serde_json::Value = serde_json::from_slice(response.body()).unwrap();
    assert_eq!(error["html_reports"][0]["model"], "test_orm.mixed");
    assert_eq!(
        error["html_reports"][0]["entries"][0]["reason"],
        "event_handler"
    );
    assert_eq!(count(&h, "test_orm_mixed").await, 0);
    let clean = call(
        &h,
        CREATE,
        r#"{"vals_list":[{"html":"<p>Safe</p>"}]}"#,
        alice(),
    )
    .await;
    assert_eq!(clean.status(), 200);
    assert!(
        serde_json::from_slice::<serde_json::Value>(clean.body())
            .unwrap()
            .is_array()
    );
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn html_rpc_clean_mode_does_not_authorize_a_denied_write() {
    let h = harness().await;
    let response = call(
        &h,
        CREATE,
        r#"{"html_validation":"clean","vals_list":[{"html":"<p onclick='private'>Body</p>"}]}"#,
        user(BOB),
    )
    .await;
    assert_eq!(response.status(), 403);
    assert_eq!(count(&h, "test_orm_mixed").await, 0);
    assert!(!String::from_utf8_lossy(response.body()).contains("private"));
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn html_rpc_report_envelope_counts_toward_result_limit_and_rolls_back() {
    let config = erp_rpc::RpcConfig::builder()
        .result_bytes(1 << 20)
        .build()
        .unwrap();
    let h = harness_with(&[], config, false).await;
    let text = format!("<p onclick='private'>{}</p>", "x".repeat((1 << 20) - 80));
    let body =
        json!({"html_validation":"clean", "vals":{"html":text}, "specification":{"html":{}}})
            .to_string();
    let response = call(&h, "/json/2/test_orm.mixed/web_save", &body, alice()).await;
    assert_eq!(
        response.status(),
        422,
        "{}",
        String::from_utf8_lossy(response.body())
    );
    let error: serde_json::Value = serde_json::from_slice(response.body()).unwrap();
    assert_eq!(error["name"], "erp.result_too_large");
    assert_eq!(count(&h, "test_orm_mixed").await, 0);
    h.cleanup().await.unwrap();
}
