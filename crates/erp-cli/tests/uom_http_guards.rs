//! Actual server composition and runtime login after UoM installation.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration assertions"
)]
#[path = "support/native_http.rs"]
mod native_http;
use native_http::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn installed_uom_serves_with_least_privilege_and_a_fresh_peer() {
    let fixture = NativeFixture::start().await;
    let reply = json_post(
        fixture.port,
        &fixture.cookies,
        "/erp/module/install",
        r#"{"modules":["uom"]}"#,
    );
    assert_eq!(reply.status, 200, "{}", reply.body);
    let result: serde_json::Value = serde_json::from_str(&reply.body).unwrap();
    assert!(
        result["installed"]
            .as_array()
            .unwrap()
            .iter()
            .any(|name| name == "uom")
    );
    let id:i64=fixture.client.query_one("SELECT res_id::bigint FROM ir_model_data WHERE module='uom' AND name='product_uom_dozen'",&[]).await.unwrap().get(0);
    let mut config = fixture.db.config();
    config
        .user(&fixture.role)
        .password("fixture-runtime-password");
    let (request, connection) = config
        .connect(erp_testkit::tokio_postgres::NoTls)
        .await
        .unwrap();
    let connection = tokio::spawn(connection);
    let registered = request
        .query_one(
            "SELECT uom_id,root_id,factor::text FROM erp_uom_register($1::bigint[])",
            &[&vec![id]],
        )
        .await
        .unwrap();
    assert_eq!(registered.get::<_, i64>(0), id);
    assert_eq!(registered.get::<_, String>(2), "12");
    let error = request
        .execute("DELETE FROM uom_reference_use", &[])
        .await
        .unwrap_err();
    assert_eq!(error.code().unwrap().code(), "42501");
    assert_eq!(
        error.as_db_error().unwrap().message(),
        "permission denied for table uom_reference_use"
    );
    let peer = fixture.peer();
    assert_eq!(request_http_ready(peer.port), 200);
    let reply = json_post(
        fixture.port,
        &fixture.cookies,
        "/erp/json/2/uom.uom/_unprotected_uom_xml_ids",
        "{}",
    );
    assert_eq!(reply.status, 403, "{}", reply.body);
    let body: serde_json::Value = serde_json::from_str(&reply.body).unwrap();
    assert_eq!(body["name"], "erp.exceptions.AccessError");
    assert_eq!(
        body["message"],
        "Private methods (such as 'uom.uom._unprotected_uom_xml_ids') cannot be called remotely."
    );
    drop(peer);
    drop(request);
    connection.await.unwrap().unwrap();
    fixture.cleanup().await;
}

fn request_http_ready(port: u16) -> u16 {
    request(port, "GET", "/erp/health/ready", &[], "").status
}
