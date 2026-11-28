// SPDX-License-Identifier: MIT OR Apache-2.0
use super::native_http as http;
use super::*;
use erp_security::auth::{Auth, AuthConfig, HashPool};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const CORRECTION_MARKER: &str = "Your previous reply could not be rendered";

async fn provider_exchange(
    provider: &tokio::net::TcpListener,
    expect: &[&str],
    reply_content: &str,
) {
    let (mut io, _) = tokio::time::timeout(std::time::Duration::from_secs(12), provider.accept())
        .await
        .unwrap()
        .unwrap();
    let mut buffer = Vec::new();
    let mut chunk = [0; 4096];
    loop {
        let n = io.read(&mut chunk).await.unwrap();
        assert!(n > 0);
        buffer.extend_from_slice(&chunk[..n]);
        if let Some(pos) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = std::str::from_utf8(&buffer[..pos]).unwrap();
            let length = head
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length: ")
                        .and_then(|s| s.parse::<usize>().ok())
                })
                .unwrap();
            if buffer.len() >= pos + 4 + length {
                let doc: Value =
                    serde_json::from_slice(&buffer[pos + 4..pos + 4 + length]).unwrap();
                assert_eq!(doc["model"], "local-model");
                let wire = doc.to_string();
                for text in expect {
                    assert!(wire.contains(text), "missing {text:?} in {wire}");
                }
                let payload = json!({"object":"chat.completion","id":"fake-chat","model":"local-model","choices":[{"index":0,"message":{"role":"assistant","content":reply_content},"finish_reason":"stop"}],"usage":{"prompt_tokens":100,"completion_tokens":3,"total_tokens":103}});
                let payload = payload.to_string();
                io.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",payload.len(),payload).as_bytes()).await.unwrap();
                break;
            }
        }
    }
}

/// reaches the user.
#[test]
fn empty_final_reply_is_corrected_before_it_reaches_the_user() {
    big(async {
        let db = TestDb::create().await.unwrap();
        let retail = super::retail::install(&db).await;
        let auth = Arc::new(
            Auth::new(retail.orm.clone(), AuthConfig::DEFAULT, HashPool::global())
                .await
                .unwrap(),
        );
        let dir = tempfile::tempdir().unwrap();
        let listen = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listen.local_addr().unwrap().port();
        let config = http::config(dir.path(), &db, port, "test-role", dir.path());
        let cfg = Arc::new(erp_server::config::ServerConfig::load(&config).unwrap());
        let provider = Arc::new(tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap());
        let endpoint = format!("http://{}/v1", provider.local_addr().unwrap());
        http::private(&dir.path().join("fixture-ai-key"), "synthetic-only");
        let table = format!("[provider.local]\nkey_file='fixture-ai-key'\nbase_url='{endpoint}'");
        let credentials =
            erp_addon_ai_base::config::Credentials::load(Some(&table.parse().unwrap()), dir.path())
                .unwrap();
        let app = Arc::new(
            erp_server::web::WebApp::new(cfg, auth)
                .unwrap()
                .with_assistant(Arc::new(erp_cli::assistant_turn::TextAssistant::new(
                    credentials,
                ))),
        );
        app.set_login_available(true);
        let serving = tokio::spawn(app.serve(listen));
        let mut setup = retail.installed.begin(SUPERUSER_ID).await.unwrap();
        setup
            .execute(
                "UPDATE ai_provider SET active=true,base_url=$1 WHERE name='local'",
                &[&endpoint],
            )
            .await
            .unwrap();
        setup.execute("INSERT INTO ai_budget(name,company_id,provider_id,model,period,token_limit,cost_limit,input_rate,output_rate) SELECT 'Company',$1,id,'local-model','day',50000,50000,1000,2000 FROM ai_provider WHERE name='local'",&[&retail.company]).await.unwrap();
        setup.commit().await.unwrap();
        drop(setup);
        let dbname = db.name().to_owned();
        let (cookies, csrf) = tokio::task::spawn_blocking(move || {
            let login = http::request(port, "GET", "/erp/login", &[], "");
            assert_eq!(login.status, 200, "{}", login.body);
            let prefs = http::cookie(&login, "prefs");
            let csrf = http::embedded(&login, "erp-login")["csrf"].as_str().unwrap().to_string();
            let logged = http::request(port, "POST", "/erp/login", &[("content-type", "application/x-www-form-urlencoded"), ("cookie", &prefs)], &format!("db={dbname}&login=catalog-purchase&password=fixture-purchase-password&csrf_token={csrf}"));
            assert_eq!(logged.status, 303, "{}", logged.body);
            (format!("{prefs}; {}", http::cookie(&logged, "session_id")), csrf)
        })
        .await
        .unwrap();
        let turn = "/erp/ai/assistant/turn";
        let origin = format!("http://127.0.0.1:{port}");
        // First exchange: the model returns an empty reply; the server then sends the correction
        // template back and the second exchange answers with text.
        let first_provider = {
            let provider = Arc::clone(&provider);
            tokio::spawn(async move {
                provider_exchange(&provider, &["Question."], "").await;
                provider_exchange(
                    &provider,
                    &[CORRECTION_MARKER, "Question."],
                    "Corrected answer.",
                )
                .await;
            })
        };
        let body = json!({"company_id": retail.company, "message": "Question.", "request_id": "23df7958-5ec6-470e-a3e1-15ea67553a39"}).to_string();
        let (cookies, csrf, origin) = (cookies.clone(), csrf.clone(), origin.clone());
        let response = tokio::task::spawn_blocking(move || {
            http::request(
                port,
                "POST",
                turn,
                &[
                    ("cookie", &cookies),
                    ("x-csrf-token", &csrf),
                    ("origin", &origin),
                    ("content-type", "application/json"),
                    ("sec-fetch-site", "same-origin"),
                ],
                &body,
            )
        })
        .await
        .unwrap();
        assert_eq!(response.status, 200, "{}", response.body);
        let reply = serde_json::from_str::<Value>(&response.body).unwrap();
        let session = reply["session_id"].as_i64().unwrap();
        assert!(session > 0);
        assert_eq!(
            reply,
            json!({"session_id":session,"text":"Corrected answer.","model":"local-model","usage":{"input_tokens":100,"output_tokens":3,"round_trips":2,"tool_calls":0}})
        );
        first_provider.await.unwrap();
        let mut tx = retail.installed.begin(SUPERUSER_ID).await.unwrap();
        let rows=tx.query("SELECT state::text,active_session IS NULL FROM ai_usage WHERE company_id=$1 ORDER BY id",&[&retail.company]).await.unwrap();
        assert_eq!(
            rows.len(),
            2,
            "Every provider exchange consumes a separate allowance"
        );
        assert_eq!(rows[0].get::<_, String>(0), "failed");
        assert_eq!(rows[1].get::<_, String>(0), "settled");
        assert!(rows.iter().all(|r| r.get::<_, bool>(1)));
        // A new request must never observe the lease released with the old transcript.
        let same_commit = tx
            .query(
                "SELECT u.xmin = e.xmin FROM ai_usage u JOIN ai_session_event e ON e.session_id=$1
             WHERE e.kind='turn_end' AND u.company_id=$2 AND u.state='settled'",
                &[&session, &retail.company],
            )
            .await
            .unwrap();
        assert_eq!(same_commit.len(), 1);
        assert!(same_commit[0].get::<_, bool>(0));
        tx.rollback().await.unwrap();
        drop(tx);
        serving.abort();
        let _cancel = serving.await;
        drop(retail);
        db.cleanup().await.unwrap();
    });
}

/// correction rounds, naming the empty output).
#[test]
fn a_provider_that_keeps_replying_empty_is_refused_after_the_correction_rounds() {
    big(async {
        let db = TestDb::create().await.unwrap();
        let retail = super::retail::install(&db).await;
        let auth = Arc::new(
            Auth::new(retail.orm.clone(), AuthConfig::DEFAULT, HashPool::global())
                .await
                .unwrap(),
        );
        let dir = tempfile::tempdir().unwrap();
        let listen = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listen.local_addr().unwrap().port();
        let config = http::config(dir.path(), &db, port, "test-role", dir.path());
        let cfg = Arc::new(erp_server::config::ServerConfig::load(&config).unwrap());
        let provider = Arc::new(tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap());
        let endpoint = format!("http://{}/v1", provider.local_addr().unwrap());
        http::private(&dir.path().join("fixture-ai-key"), "synthetic-only");
        let table = format!("[provider.local]\nkey_file='fixture-ai-key'\nbase_url='{endpoint}'");
        let credentials =
            erp_addon_ai_base::config::Credentials::load(Some(&table.parse().unwrap()), dir.path())
                .unwrap();
        let app = Arc::new(
            erp_server::web::WebApp::new(cfg, auth)
                .unwrap()
                .with_assistant(Arc::new(erp_cli::assistant_turn::TextAssistant::new(
                    credentials,
                ))),
        );
        app.set_login_available(true);
        let serving = tokio::spawn(app.serve(listen));
        let mut setup = retail.installed.begin(SUPERUSER_ID).await.unwrap();
        setup
            .execute(
                "UPDATE ai_provider SET active=true,base_url=$1 WHERE name='local'",
                &[&endpoint],
            )
            .await
            .unwrap();
        setup.execute("INSERT INTO ai_budget(name,company_id,provider_id,model,period,token_limit,cost_limit,input_rate,output_rate) SELECT 'Company',$1,id,'local-model','day',50000,50000,1000,2000 FROM ai_provider WHERE name='local'",&[&retail.company]).await.unwrap();
        setup.commit().await.unwrap();
        drop(setup);
        let dbname = db.name().to_owned();
        let (cookies, csrf) = tokio::task::spawn_blocking(move || {
            let login = http::request(port, "GET", "/erp/login", &[], "");
            assert_eq!(login.status, 200, "{}", login.body);
            let prefs = http::cookie(&login, "prefs");
            let csrf = http::embedded(&login, "erp-login")["csrf"].as_str().unwrap().to_string();
            let logged = http::request(port, "POST", "/erp/login", &[("content-type", "application/x-www-form-urlencoded"), ("cookie", &prefs)], &format!("db={dbname}&login=catalog-purchase&password=fixture-purchase-password&csrf_token={csrf}"));
            assert_eq!(logged.status, 303, "{}", logged.body);
            (format!("{prefs}; {}", http::cookie(&logged, "session_id")), csrf)
        })
        .await
        .unwrap();
        let turn = "/erp/ai/assistant/turn";
        let origin = format!("http://127.0.0.1:{port}");
        // Default ai_base.answer_correction_rounds is 1: two provider exchanges, both empty.
        let provider_task = {
            let provider = Arc::clone(&provider);
            tokio::spawn(async move {
                provider_exchange(&provider, &["Question."], "").await;
                provider_exchange(&provider, &[CORRECTION_MARKER], "").await;
            })
        };
        let body = json!({"company_id": retail.company, "message": "Question."}).to_string();
        let (cookies, csrf, origin) = (cookies.clone(), csrf.clone(), origin.clone());
        let response = tokio::task::spawn_blocking(move || {
            http::request(
                port,
                "POST",
                turn,
                &[
                    ("cookie", &cookies),
                    ("x-csrf-token", &csrf),
                    ("origin", &origin),
                    ("content-type", "application/json"),
                    ("sec-fetch-site", "same-origin"),
                ],
                &body,
            )
        })
        .await
        .unwrap();
        assert_eq!(response.status, 422, "{}", response.body);
        assert!(
            response.body.contains("erp.ai.provider_failed"),
            "{}",
            response.body
        );
        provider_task.await.unwrap();
        serving.abort();
        let _cancel = serving.await;
        drop(retail);
        db.cleanup().await.unwrap();
    });
}
