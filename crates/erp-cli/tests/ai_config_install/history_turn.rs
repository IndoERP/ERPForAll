// SPDX-License-Identifier: MIT OR Apache-2.0
use super::native_http as http;
use super::*;
use erp_security::auth::{Auth, AuthConfig, HashPool};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn provider_exchange(
    provider: &tokio::net::TcpListener,
    expect: &[&str],
    expect_ordered: &[&str],
    reply: &str,
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
                if !expect_ordered.is_empty() {
                    // The resent transcript reaches the provider oldest first, before the
                    // current question: the message texts appear in that order on the wire.
                    let positions: Vec<usize> = expect_ordered
                        .iter()
                        .map(|text| wire.find(text).unwrap_or(usize::MAX))
                        .collect();
                    assert!(
                        positions.windows(2).all(|w| w[0] <= w[1]),
                        "history order wrong: {positions:?} in {wire}"
                    );
                }
                let payload = json!({"object":"chat.completion","id":"fake-chat","model":"local-model","choices":[{"index":0,"message":{"role":"assistant","content":reply},"finish_reason":"stop"}],"usage":{"prompt_tokens":100,"completion_tokens":3,"total_tokens":103}});
                let payload = payload.to_string();
                io.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",payload.len(),payload).as_bytes()).await.unwrap();
                break;
            }
        }
    }
}

#[test]
fn resent_history_reaches_the_provider_and_oversized_history_is_refused() {
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
        setup.execute("INSERT INTO ai_budget(name,company_id,provider_id,model,period,token_limit,cost_limit,input_rate,output_rate) SELECT 'Company',$1,id,'local-model','day',10000,10000,1000,2000 FROM ai_provider WHERE name='local'",&[&retail.company]).await.unwrap();
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
        let invoke = |body: String| {
            let (cookies, csrf, origin) = (cookies.clone(), csrf.clone(), origin.clone());
            tokio::task::spawn_blocking(move || {
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
        };

        // First question: an empty transcript reaches the provider with the question only.
        let first_provider = {
            let provider = Arc::clone(&provider);
            tokio::spawn(async move {
                provider_exchange(
                    &provider,
                    &["First question.", "\"role\":\"user\""],
                    &[],
                    "First answer.",
                )
                .await;
            })
        };
        let first_body =
            json!({"company_id": retail.company, "message": "First question."}).to_string();
        let first = invoke(first_body).await.unwrap();
        assert_eq!(first.status, 200, "{}", first.body);
        assert_eq!(
            serde_json::from_str::<Value>(&first.body).unwrap(),
            json!({"text":"First answer.","model":"local-model","usage":{"input_tokens":100,"output_tokens":3,"round_trips":1,"tool_calls":0}})
        );
        first_provider.await.unwrap();

        // Follow-up: the tab resends the visible exchange; the provider sees it in order
        // before the current question.
        let second_provider = {
            let provider = Arc::clone(&provider);
            tokio::spawn(async move {
                provider_exchange(
                    &provider,
                    &["\"role\":\"user\"", "\"role\":\"assistant\""],
                    &["First question.", "First answer.", "Follow up."],
                    "Second answer.",
                )
                .await;
            })
        };
        let second_body = json!({
            "company_id": retail.company,
            "message": "Follow up.",
            "history": [
                {"role": "user", "text": "First question."},
                {"role": "assistant", "text": "First answer."}
            ]
        })
        .to_string();
        let second = invoke(second_body).await.unwrap();
        assert_eq!(second.status, 200, "{}", second.body);
        assert_eq!(
            serde_json::from_str::<Value>(&second.body).unwrap(),
            json!({"text":"Second answer.","model":"local-model","usage":{"input_tokens":100,"output_tokens":3,"round_trips":1,"tool_calls":0}})
        );
        second_provider.await.unwrap();

        // A history entry past the message limit is refused with the named error; no provider
        // call happens, so the exchange timeout is the failure signal for any regression.
        let oversized = "詳".repeat(5000);
        let invalid_body = json!({
            "company_id": retail.company,
            "message": "Follow up again.",
            "history": [{"role": "user", "text": oversized}]
        })
        .to_string();
        let invalid = invoke(invalid_body).await.unwrap();
        assert_eq!(invalid.status, 422, "{}", invalid.body);
        assert!(
            invalid.body.contains("erp.ai.message_invalid"),
            "{}",
            invalid.body
        );

        // History that does not begin with a question is refused the same way.
        let leading_body = json!({
            "company_id": retail.company,
            "message": "Follow up once more.",
            "history": [{"role": "assistant", "text": "Unsolicited."}]
        })
        .to_string();
        let leading = invoke(leading_body).await.unwrap();
        assert_eq!(leading.status, 422, "{}", leading.body);
        assert!(
            leading.body.contains("erp.ai.message_invalid"),
            "{}",
            leading.body
        );

        // Consecutive messages of one role and empty text are refused before any provider call.
        for (case, history) in [
            (
                "adjacent questions",
                json!([
                    {"role": "user", "text": "First."},
                    {"role": "user", "text": "Second."}
                ]),
            ),
            (
                "empty reply",
                json!([
                    {"role": "user", "text": "First."},
                    {"role": "assistant", "text": "   "}
                ]),
            ),
        ] {
            let body = json!({
                "company_id": retail.company,
                "message": "Follow up.",
                "history": history
            })
            .to_string();
            let response = invoke(body).await.unwrap();
            assert_eq!(response.status, 422, "{case}: {}", response.body);
            assert!(
                response.body.contains("erp.ai.message_invalid"),
                "{case}: {}",
                response.body
            );
        }

        serving.abort();
        let _cancel = serving.await;
        drop(retail);
        db.cleanup().await.unwrap();
    });
}
