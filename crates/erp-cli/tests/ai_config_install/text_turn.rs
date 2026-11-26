// SPDX-License-Identifier: MIT OR Apache-2.0
use super::native_http as http;
use super::*;
use erp_security::auth::{Auth, AuthConfig, HashPool};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[test]
fn browser_text_turn_requires_csrf_and_budget_then_returns_metered_provider_text() {
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
        let provider = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
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
        setup.commit().await.unwrap();
        drop(setup);
        let company = retail.company;
        let dbname = db.name().to_owned();
        let (cookies,csrf)=tokio::task::spawn_blocking(move||{
            let login=http::request(port,"GET","/erp/login",&[],"");
            assert_eq!(login.status,200,"{}",login.body);
            let prefs=http::cookie(&login,"prefs");
            let csrf=http::embedded(&login,"erp-login")["csrf"].as_str().unwrap().to_string();
            let logged=http::request(port,"POST","/erp/login",&[("content-type","application/x-www-form-urlencoded"),("cookie",&prefs)],&format!("db={dbname}&login=catalog-purchase&password=fixture-purchase-password&csrf_token={csrf}"));
            assert_eq!(logged.status,303,"{}",logged.body);
            (format!("{prefs}; {}",http::cookie(&logged,"session_id")),csrf)
        }).await.unwrap();
        let status = "/erp/ai/assistant/status";
        let turn = "/erp/ai/assistant/turn";
        let body = json!({"company_id":company,"message":"Synthetic ping."}).to_string();
        let origin = format!("http://127.0.0.1:{port}");
        let invoke = |method: &str, path: &str, token: Option<&str>, auth: Option<&str>| {
            let mut headers = vec![
                ("cookie", cookies.as_str()),
                ("origin", origin.as_str()),
                ("sec-fetch-site", "same-origin"),
                ("content-type", "application/json"),
            ];
            if let Some(token) = token {
                headers.push(("x-csrf-token", token));
            }
            if let Some(auth) = auth {
                headers.push(("authorization", auth));
            }
            http::request(
                port,
                method,
                path,
                &headers,
                if method == "POST" { &body } else { "" },
            )
        };
        let yes = invoke("GET", status, None, None);
        assert_eq!(yes.status, 200, "{}", yes.body);
        assert_eq!(
            serde_json::from_str::<Value>(&yes.body).unwrap(),
            json!({"available":true, "history":{"turns":32,"bytes":262144,"message_bytes":4096}})
        );
        assert_eq!(invoke("POST", turn, None, None).status, 403);
        assert_eq!(
            invoke("POST", turn, Some(&csrf), Some("Bearer denied")).status,
            403
        );
        let missing = invoke("POST", turn, Some(&csrf), None);
        assert_eq!(missing.status, 422, "{}", missing.body);
        assert!(
            missing.body.contains("erp.ai.budget_missing"),
            "{}",
            missing.body
        );
        let mut setup = retail.installed.begin(SUPERUSER_ID).await.unwrap();
        let uid = retail.users[0].get();
        setup.execute("INSERT INTO ai_budget(name,company_id,provider_id,model,period,token_limit,cost_limit,input_rate,output_rate) SELECT 'Company',$1,id,'local-model','day',10000,10000,1000,2000 FROM ai_provider WHERE name='local'",&[&company]).await.unwrap();
        let provider_id = setup
            .query("SELECT id FROM ai_provider WHERE name='local'", &[])
            .await
            .unwrap()[0]
            .get::<_, i64>(0);
        let vals = RawVals::new()
            .set_cell("name", Cell::Str("User".into()))
            .set_cell("company_id", Cell::Id(company))
            .set_cell("user_id", Cell::Id(uid))
            .set_cell("provider_id", Cell::Id(provider_id))
            .set_cell("model", Cell::Str("local-model".into()))
            .set_cell("period", Cell::Str("day".into()))
            .set_cell("token_limit", Cell::Int(1))
            .set_cell("cost_limit", Cell::BigInt(erp_sdk::BigInt::from_i64(10000)));
        setup
            .env()
            .records("ai.budget")
            .unwrap()
            .create(&mut setup, vec![vals])
            .await
            .unwrap();
        setup.commit().await.unwrap();
        drop(setup);
        let mut wrong = retail.installed.begin(SUPERUSER_ID).await.unwrap();
        assert!(
            wrong
                .execute(
                    "UPDATE ai_budget SET input_rate=1 WHERE user_id=$1",
                    &[&uid]
                )
                .await
                .is_err()
        );
        wrong.rollback().await.unwrap();
        drop(wrong);
        let exhausted = invoke("POST", turn, Some(&csrf), None);
        assert!(
            exhausted.body.contains("erp.ai.budget_exhausted"),
            "{}",
            exhausted.body
        );
        let mut setup = retail.installed.begin(SUPERUSER_ID).await.unwrap();
        setup
            .execute(
                "UPDATE ai_budget SET token_limit=10000 WHERE user_id=$1",
                &[&uid],
            )
            .await
            .unwrap();
        setup.commit().await.unwrap();
        drop(setup);
        let fixture_orm = retail.installed.clone();
        let other_orm = retail.orm.clone();
        let other_user = retail.users[1];
        let other_credentials =
            erp_addon_ai_base::config::Credentials::load(Some(&table.parse().unwrap()), dir.path())
                .unwrap();
        let concurrent = (cookies.clone(), csrf.clone(), origin.clone(), body.clone());
        let upstream = tokio::spawn(async move {
            let (mut io, _) =
                tokio::time::timeout(std::time::Duration::from_secs(12), provider.accept())
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
                    assert!(head.lines().any(|line| {
                        line.eq_ignore_ascii_case("authorization: bearer synthetic-only")
                    }));
                    let length = head
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length: ")
                                .and_then(|s| s.parse::<usize>().ok())
                        })
                        .unwrap();
                    if buffer.len() >= pos + 4 + length {
                        let doc: Value = serde_json::from_slice(&buffer[pos + 4..]).unwrap();
                        assert_eq!(doc["model"], "local-model");
                        assert!(doc.to_string().contains("Synthetic ping."));
                        break;
                    }
                }
            }
            let mut check = fixture_orm.begin(SUPERUSER_ID).await.unwrap();
            let rows = check
                .query("SELECT state FROM ai_usage WHERE user_id=$1", &[&uid])
                .await
                .unwrap();
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].get::<_, String>(0), "reserved");
            check.rollback().await.unwrap();
            let busy = tokio::task::spawn_blocking(move || {
                http::request(
                    port,
                    "POST",
                    turn,
                    &[
                        ("cookie", &concurrent.0),
                        ("x-csrf-token", &concurrent.1),
                        ("origin", &concurrent.2),
                        ("content-type", "application/json"),
                        ("sec-fetch-site", "same-origin"),
                    ],
                    &concurrent.3,
                )
            })
            .await
            .unwrap();
            assert_eq!(busy.status, 429, "{}", busy.body);
            assert!(busy.body.contains("erp.ai.busy"));
            // Independently pin another user's snapshot after reservation but before settlement.
            // The usage reservation still fits. Its eventual metered charge exhausts the company.
            let mut stale = other_orm
                .begin_with(other_user, Context::new().with_company_id(company))
                .await
                .unwrap();
            let env = stale.env();
            let route = erp_addon_ai_base::routing::resolve(&env, &mut stale, &other_credentials)
                .await
                .unwrap();
            let state: String = stale
                .query("SELECT state FROM ai_usage WHERE user_id=$1", &[&uid])
                .await
                .unwrap()[0]
                .get(0);
            assert_eq!(state, "reserved");
            let payload=json!({"object":"chat.completion","id":"fake-chat","model":"local-model","choices":[{"index":0,"message":{"role":"assistant","content":"Synthetic pong."},"finish_reason":"stop"}],"usage":{"prompt_tokens":11000,"completion_tokens":3,"total_tokens":11003}}).to_string();
            io.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",payload.len(),payload).as_bytes()).await.unwrap();
            (stale, route)
        });
        // Blocking browser wire lives on a third thread so the provider and server can both make progress.
        let ok = tokio::task::block_in_place(|| invoke("POST", turn, Some(&csrf), None));
        assert_eq!(ok.status, 200, "{}", ok.body);
        assert_eq!(
            serde_json::from_str::<Value>(&ok.body).unwrap(),
            json!({"text":"Synthetic pong.","model":"local-model","usage":{"input_tokens":11000,"output_tokens":3}})
        );
        let (mut stale, route) = upstream.await.unwrap();
        let refused = erp_addon_ai_base::ledger::reserve(
            &mut stale,
            "second-user-fixture-session",
            &route,
            1,
            1,
            30,
        )
        .await
        .err()
        .expect("a pre-settlement snapshot cannot authorize more spending");
        assert_eq!(refused.sqlstate(), Some("40001"), "{refused:?}");
        stale.rollback().await.unwrap();
        drop(stale);
        let exhausted = invoke("POST", turn, Some(&csrf), None);
        assert_eq!(exhausted.status, 422);
        assert!(exhausted.body.contains("erp.ai.budget_exhausted"));
        let mut check = retail.installed.begin(SUPERUSER_ID).await.unwrap();
        let rows = check
            .query(
                "SELECT state,tokens,cost,active_session, NOT (to_jsonb(u)::text LIKE ANY(ARRAY['%Synthetic ping.%','%Synthetic pong.%','%synthetic-only%'])) FROM ai_usage u WHERE user_id=$1",
                &[&uid],
            )
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get::<_, String>(0), "settled");
        assert_eq!(rows[0].get::<_, i64>(1), 11003);
        assert_eq!(rows[0].get::<_, i64>(2), 11006);
        assert!(rows[0].get::<_, Option<String>>(3).is_none());
        assert!(
            rows[0].get::<_, bool>(4),
            "usage ledger retained exchange data"
        );
        check.rollback().await.unwrap();
        drop(check);
        serving.abort();
        let _cancel = serving.await;
        drop(retail);
        db.cleanup().await.unwrap();
    });
}
