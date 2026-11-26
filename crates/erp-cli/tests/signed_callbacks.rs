//! The native server verifies provider deliveries before ORM dispatch.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration assertions"
)]

#[path = "support/multiuser_fixture.rs"]
mod multiuser_fixture;
#[path = "support/native_http.rs"]
mod native_http;

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use hmac::{Hmac, KeyInit, Mac};
use multiuser_fixture::{Subject, run, success};
use serde_json::{Value, json};
use sha2::Sha256;
use std::time::{SystemTime, UNIX_EPOCH};

// Synthetic test material. NativeFixture removes both private key files when it exits.
const SIGNING_KEY: [u8; 32] = [0x5b; 32];
const EVENT: &str = "019a0000-0000-7000-8000-000000000023";
const CALLBACK: &str = "/erp/callback/preferences";

fn sign(origin: &str, database: &str, timestamp: &str, event: &str, body: &str) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(&SIGNING_KEY).unwrap();
    mac.update(format!(
        "erp-callback-v1\n{origin}\n{database}\npreferences\nres.users\nwrite\n{timestamp}\n{event}\n{body}"
    ).as_bytes());
    format!("v1={}", URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes()))
}

fn delivery(
    port: u16,
    path: &str,
    timestamp: &str,
    event: &str,
    signature: &str,
    body: &str,
    extra: &[(&str, &str)],
) -> native_http::Reply {
    let mut headers = vec![
        ("content-type", "application/json"),
        ("x-erp-callback-timestamp", timestamp),
        ("x-erp-callback-signature", signature),
        ("idempotency-key", event),
    ];
    headers.extend_from_slice(extra);
    native_http::request(port, "POST", path, &headers, body)
}

#[test]
fn signed_callbacks_enforce_live_access_and_deduplicate_across_native_nodes() {
    run(|fixture| {
        let native = fixture.native;
        let integration = fixture.user(Subject::A);
        let other = fixture.user(Subject::B);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(native.client.batch_execute(
            "INSERT INTO ir_config_parameter (key,value) VALUES ('base.enable_programmatic_api_keys','True')
             ON CONFLICT (key) DO UPDATE SET value=EXCLUDED.value;
             UPDATE res_groups SET api_key_duration=7 WHERE id IN
             (SELECT res_id FROM ir_model_data WHERE module='base' AND name='group_user')",
        )).unwrap();
        let key_request = json!({
            "db": native.db.name(),
            "login": integration.login,
            "password": format!("{} & = fixture £ password", native.db.name()),
            "device_name": "provider callback fixture",
        });
        let issued = native_http::request(
            native.port,
            "POST",
            "/erp/session/api-key",
            &[("content-type", "application/json")],
            &key_request.to_string(),
        );
        assert_eq!(
            issued.status, 200,
            "production RPC key issuance (body redacted)"
        );
        let issued: Value = serde_json::from_str(&issued.body).unwrap();
        let signing_file = native.dir.path().join("callback-signing.key");
        let api_file = native.dir.path().join("callback-api.key");
        native_http::private(&signing_file, &URL_SAFE_NO_PAD.encode(SIGNING_KEY));
        native_http::private(&api_file, issued["api_key"].as_str().unwrap());
        let signing_file = toml::Value::String(signing_file.to_str().unwrap().to_owned());
        let api_file = toml::Value::String(api_file.to_str().unwrap().to_owned());
        let peer_config = format!(
            "[callbacks.preferences]\nmodel='res.users'\nmethod='write'\nsigning_key_file={signing_file}\napi_key_file={api_file}\n\
             [callbacks.other]\nmodel='res.users'\nmethod='write'\nsigning_key_file={signing_file}\napi_key_file={api_file}"
        );
        let mut peer = native.peer_with_config(&peer_config);
        let origin = format!("http://127.0.0.1:{}", native.port);
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let timestamp = now.to_string();
        let body = json!({"ids":[integration.id],"vals":{"tz":"Europe/London"}}).to_string();
        let signed = |time: &str, event: &str, body: &str| {
            sign(&origin, native.db.name(), time, event, body)
        };
        let signature = signed(&timestamp, EVENT, &body);
        let send = |port, path: &str, body: &str, signature: &str| {
            delivery(port, path, &timestamp, EVENT, signature, body, &[])
        };
        assert_eq!(send(native.port, CALLBACK, &body, &signature).status, 404);
        assert_eq!(
            send(peer.port, "/erp/callback/other", &body, &signature).status,
            401
        );
        let stale = (now - 600).to_string();
        assert_eq!(
            delivery(
                peer.port,
                CALLBACK,
                &stale,
                EVENT,
                &signed(&stale, EVENT, &body),
                &body,
                &[]
            )
            .status,
            401
        );
        assert_eq!(
            send(peer.port, CALLBACK, &format!("{body} "), &signature).status,
            401
        );
        assert_eq!(
            delivery(
                peer.port,
                CALLBACK,
                &timestamp,
                EVENT,
                &signature,
                &body,
                &[("idempotency-key", EVENT)]
            )
            .status,
            401
        );
        let oversized = " ".repeat(65_537);
        assert_eq!(
            send(
                peer.port,
                CALLBACK,
                &oversized,
                &signed(&timestamp, EVENT, &oversized)
            )
            .status,
            413
        );
        assert_eq!(
            delivery(
                peer.port,
                CALLBACK,
                &timestamp,
                EVENT,
                &signature,
                &body,
                &[("cookie", "session_id=fixture")]
            )
            .status,
            400
        );

        let read_tz = |uid: i64| {
            success(&fixture.rpc(
                Subject::Admin,
                "res.users",
                "read",
                json!({"ids":[uid],"fields":["tz"]}),
            ))[0]["tz"]
                .clone()
        };
        let tampered = json!({"ids":[other.id],"vals":{"tz":"Europe/London"}}).to_string();
        assert_eq!(
            send(
                peer.port,
                CALLBACK,
                &tampered,
                &signed(&timestamp, EVENT, &tampered)
            )
            .status,
            403
        );
        assert_eq!(read_tz(other.id), "UTC");
        let foreign_context = json!({"ids":[integration.id],"vals":{"tz":"Europe/London"},"context":{"allowed_company_ids":[other.company]}}).to_string();
        assert_eq!(
            send(
                peer.port,
                CALLBACK,
                &foreign_context,
                &signed(&timestamp, EVENT, &foreign_context)
            )
            .status,
            422
        );
        const PAYLOAD_MARKER: &str = "callback-redaction-fixture";
        let invalid = json!({"ids":[integration.id],"vals":{"tz":PAYLOAD_MARKER}}).to_string();
        let refused = send(
            peer.port,
            CALLBACK,
            &invalid,
            &signed(&timestamp, EVENT, &invalid),
        );
        assert_eq!(refused.status, 422);
        assert!(!refused.body.contains(PAYLOAD_MARKER));
        assert!(
            !std::fs::read_to_string(peer.dir.path().join("server.stderr"))
                .unwrap()
                .contains(PAYLOAD_MARKER)
        );
        let committed = send(peer.port, CALLBACK, &body, &signature);
        assert_eq!(committed.status, 204);
        assert!(committed.body.is_empty());
        assert_eq!(read_tz(integration.id), "Europe/London");
        let changed = json!({"ids":[integration.id],"vals":{"tz":"Europe/Paris"}});
        assert_eq!(
            send(
                peer.port,
                CALLBACK,
                &changed.to_string(),
                &signed(&timestamp, EVENT, &changed.to_string())
            )
            .status,
            422
        );
        assert_eq!(
            success(&fixture.rpc(Subject::A, "res.users", "write", changed)),
            json!(true)
        );
        assert_eq!(read_tz(integration.id), "Europe/Paris");
        peer.server.terminate();
        drop(peer);
        let restarted = native.peer_with_config(&peer_config);
        assert_eq!(
            send(restarted.port, CALLBACK, &body, &signature).status,
            204
        );
        assert_eq!(read_tz(integration.id), "Europe/Paris");

        let entries = runtime
            .block_on(native.client.query(
                "SELECT entry_kind, outcome, subject_id, actor_kind FROM erp_event.ir_audit_entry
              WHERE origin='inbound_callback' AND model='res.users' AND method='write'
              ORDER BY id",
                &[],
            ))
            .unwrap();
        let entries: Vec<(String, String, i64)> = entries
            .into_iter()
            .map(|row| {
                assert_eq!(row.get::<_, &str>(3), "api_key_user");
                (row.get(0), row.get(1), row.get(2))
            })
            .collect();
        assert!(entries.contains(&("attempt".into(), "denied".into(), integration.id)));
        assert!(entries.contains(&("attempt".into(), "replayed".into(), integration.id)));
        assert_eq!(
            entries
                .iter()
                .filter(|(entry, outcome, _)| entry == "change" && outcome == "committed")
                .count(),
            1
        );
        let stored_result = runtime.block_on(native.client.query_one(
            "SELECT response FROM ir_idempotency_key WHERE model='res.users' AND method='write' AND subject_uid=$1",
            &[&integration.id],
        )).unwrap().get::<_, Vec<u8>>(0);
        assert_eq!(stored_result, b"null");
        assert_eq!(
            runtime
                .block_on(native.client.execute(
                    "DELETE FROM res_users_apikeys WHERE user_id=$1",
                    &[&integration.id],
                ))
                .unwrap(),
            1
        );
        assert_eq!(
            send(restarted.port, CALLBACK, &body, &signature).status,
            503,
            "a revoked credential cannot replay a stored success"
        );
    });
}
