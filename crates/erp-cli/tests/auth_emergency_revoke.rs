//! `erp auth emergency-revoke` through the real binary: exit
//! codes, ids-and-counts output, the audit row of each run, and the refusal when the configured
//! owner connection is not the owner. The deployment is initialized by the binary itself.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    dead_code,
    reason = "integration fixture assertions"
)]
#[path = "support/native_http.rs"]
mod native_http;

use erp_testkit::TestDb;
use std::{fs, path::Path, process::Command, process::Output};

const BIN: &str = env!("CARGO_BIN_EXE_erp");
const CANARY_NAME: &str = "CANARY-binary-key-name";
const CANARY_PREFIX: &str = "canarypx";

fn run(args: &[&str], config: &Path, tail: &[&str]) -> Output {
    Command::new(BIN)
        .args(args)
        .arg("--config")
        .arg(config)
        .args(tail)
        .output()
        .unwrap()
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[track_caller]
fn succeeded(output: &Output, what: &str) -> String {
    assert!(
        output.status.success(),
        "{what} failed: {}",
        text(&output.stderr)
    );
    text(&output.stdout)
}

/// One API key row of `uid` with a digest that is unique to `n` (the fixture does not need a key
/// that can authenticate).
async fn insert_key(client: &erp_testkit::tokio_postgres::Client, uid: i64, n: i32) -> i64 {
    client
        .query_one(
            "INSERT INTO res_users_apikeys(name,user_id,created_by,kind,scope,digest,digest_kid,\
             prefix,read_only,capabilities,create_date) \
             VALUES($2,$1,$1,'user','rpc',sha256(('binary-digest-' || $3::int4)::text::bytea),1,\
             $4,false,'subject',now() AT TIME ZONE 'utc') RETURNING id::int8",
            &[&uid, &CANARY_NAME, &n, &CANARY_PREFIX],
        )
        .await
        .unwrap()
        .get(0)
}

async fn insert_session(client: &erp_testkit::tokio_postgres::Client, uid: i64, n: i32) -> String {
    client
        .query_one(
            "INSERT INTO erp_session (id, user_id, sid_digest, token, token_kid, state, expires_at) \
             VALUES (pg_catalog.uuidv7(), $1, sha256(('sid-' || $2::int4)::text::bytea), \
             sha256(('token-' || $2::int4)::text::bytea), 1, 'active', now() + interval '1 day') \
             RETURNING id::text",
            &[&uid, &n],
        )
        .await
        .unwrap()
        .get(0)
}

async fn count(client: &erp_testkit::tokio_postgres::Client, sql: &str) -> i64 {
    client.query_one(sql, &[]).await.unwrap().get(0)
}

/// owner connection with one `cli` audit row each, prints ids and counts only, answers a user with
/// nothing to revoke with "0 revoked" and no row, refuses an unknown key id with exit 1, and
/// refuses with exit 1 and the owner text when the configured owner connection is a third login that
/// owns nothing (the configuration refuses the runtime role as its own owner).
/// The runtime role holds no INSERT right on the audit table while the owner commands run.
#[tokio::test]
async fn cli_revokes_key_and_sessions_with_runtime_stripped() {
    let db = TestDb::create().await.unwrap();
    let client = db.client().await.unwrap();
    let role = format!("{}_emrev", db.name());
    let node_role = format!("{role}_node");
    // a third login for the "not the owner" leg: ServerConfig refuses a runtime and an owner
    // connection that use the same role, so the owner file of that leg names this role
    let impostor = format!("{role}_own");
    client
        .batch_execute(&format!(
            "CREATE ROLE {} LOGIN PASSWORD 'fixture-runtime-password' NOSUPERUSER NOCREATEDB \
             NOCREATEROLE NOREPLICATION; \
             CREATE ROLE {} LOGIN PASSWORD 'fixture-runtime-password' NOSUPERUSER NOCREATEDB \
             NOCREATEROLE NOREPLICATION; \
             CREATE ROLE {} LOGIN PASSWORD 'fixture-runtime-password' NOSUPERUSER NOCREATEDB \
             NOCREATEROLE NOREPLICATION",
            erp_sql::quote(&role),
            erp_sql::quote(&node_role),
            erp_sql::quote(&impostor)
        ))
        .await
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let dir = dir.path().canonicalize().unwrap();
    let addons = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("addons");
    let config = native_http::config(&dir, &db, 18092, &role, &addons);
    native_http::private(&dir.join("password"), "fixture-administrator-password");
    let init = Command::new(BIN)
        .args(["db", "init", "--config"])
        .arg(&config)
        .args(["--admin-login", "fixture-admin", "--admin-password-file"])
        .arg(dir.join("password"))
        .output()
        .unwrap();
    succeeded(&init, "db init");
    succeeded(
        &run(&["db", "publish-grants"], &config, &[]),
        "publish-grants",
    );
    succeeded(&run(&["audit", "maintain"], &config, &[]), "audit maintain");

    // the runtime role cannot write audit rows while the owner commands run
    let quoted = erp_sql::quote(&role);
    client
        .batch_execute(&format!(
            "REVOKE INSERT ON ALL TABLES IN SCHEMA erp_event FROM {quoted}"
        ))
        .await
        .unwrap();
    let runtime_may_insert: bool = client
        .query_one(
            "SELECT has_any_column_privilege($1, 'erp_event.ir_audit_entry', 'INSERT')",
            &[&role],
        )
        .await
        .unwrap()
        .get(0);
    assert!(!runtime_may_insert, "the runtime role is stripped");

    let uid: i64 = client
        .query_one(
            "SELECT id::int8 FROM res_users WHERE login = 'fixture-admin'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    let key = insert_key(&client, uid, 1).await;
    let spare = insert_key(&client, uid, 2).await;

    // one key by id
    let out = run(
        &["auth", "emergency-revoke"],
        &config,
        &["--reason", "compromised", "--key", &key.to_string()],
    );
    let stdout = succeeded(&out, "revoke key");
    // the installed addons that the CLI registry (`base`, `model_data`) does not link: read from the
    // database, so the expectation does not depend on what init installs
    // `RegistryBuilder` always adds `web` (an extension of `base`), so its hooks are linked too
    let linked = [
        erp_addon_base::ADDON.name,
        erp_addon_web::ADDON.name,
        erp_data::model_data::ADDON.name,
    ];
    let unlinked: Vec<String> = client
        .query(
            "SELECT name FROM ir_module_module WHERE state = 'installed' ORDER BY name",
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|row| row.get::<_, String>(0))
        .filter(|name| !linked.contains(&name.as_str()))
        .collect();
    if unlinked.is_empty() {
        assert!(!stdout.contains("Note:"), "{stdout}");
    } else {
        assert!(
            stdout.contains(&format!(
                "Note: only the session-end hooks linked into this command ran. Installed addons \
                 not linked here: {}.",
                unlinked.join(", ")
            )),
            "{stdout}"
        );
    }
    assert!(
        stdout.starts_with(&format!(
            "Emergency revocation for database {} (reason: compromised).\n\
             Revoked 1 API key(s) and 0 session(s) in 1 transaction(s), each with its own audit \
             row (origin cli).\nAPI key ids: {key}",
            db.name()
        )),
        "{stdout}"
    );
    for leaked in [CANARY_NAME, CANARY_PREFIX] {
        assert!(!stdout.contains(leaked) && !text(&out.stderr).contains(leaked));
    }
    assert_eq!(
        count(
            &client,
            &format!("SELECT count(*) FROM res_users_apikeys WHERE id = {key}")
        )
        .await,
        0
    );
    assert_eq!(
        count(
            &client,
            &format!(
                "SELECT count(*) FROM erp_event.ir_audit_entry WHERE origin = 'cli' \
                 AND events @> '[{{\"event\":\"api_key.revoked\",\"key_id\":{key},\
                 \"cause\":{{\"kind\":\"emergency\"}}}}]'::jsonb"
            )
        )
        .await,
        1
    );

    // all sessions of the user
    insert_session(&client, uid, 1).await;
    insert_session(&client, uid, 2).await;
    let out = run(
        &["auth", "emergency-revoke"],
        &config,
        &[
            "--reason",
            "offboarding",
            "--user",
            &uid.to_string(),
            "--sessions",
        ],
    );
    let stdout = succeeded(&out, "revoke user sessions");
    assert!(
        stdout.contains("Revoked 0 API key(s) and 2 session(s) in 1 transaction(s)"),
        "{stdout}"
    );
    assert_eq!(
        count(&client, "SELECT count(*) FROM erp_session").await,
        0,
        "both sessions ended"
    );
    assert_eq!(
        count(
            &client,
            "SELECT count(*) FROM erp_event.ir_audit_entry WHERE origin = 'cli' \
             AND events @> '[{\"event\":\"user_sessions.revoked\",\"count\":2,\
             \"cause\":\"emergency\"}]'::jsonb"
        )
        .await,
        1
    );

    // nothing left to revoke for the user's sessions: success, "0 revoked", and no new row
    let rows_before = count(
        &client,
        "SELECT count(*) FROM erp_event.ir_audit_entry WHERE origin = 'cli'",
    )
    .await;
    let out = run(
        &["auth", "emergency-revoke"],
        &config,
        &[
            "--reason",
            "offboarding",
            "--user",
            &uid.to_string(),
            "--sessions",
        ],
    );
    let stdout = succeeded(&out, "revoke nothing");
    assert!(
        stdout
            .trim_end()
            .ends_with("0 revoked: the target had no API keys or sessions to end."),
        "{stdout}"
    );
    assert_eq!(
        count(
            &client,
            "SELECT count(*) FROM erp_event.ir_audit_entry WHERE origin = 'cli'"
        )
        .await,
        rows_before
    );

    // an unknown key id is refused, exit 1, and the real key stays
    let absent = i64::from(i32::MAX);
    let out = run(
        &["auth", "emergency-revoke"],
        &config,
        &[
            "--reason",
            "lost",
            "--key",
            &spare.to_string(),
            &absent.to_string(),
        ],
    );
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
    assert!(
        text(&out.stderr).contains(&format!(
            "a later release: no API key with the id(s) {absent}; nothing was revoked"
        )),
        "{}",
        text(&out.stderr)
    );
    assert_eq!(
        count(
            &client,
            &format!("SELECT count(*) FROM res_users_apikeys WHERE id = {spare}")
        )
        .await,
        1
    );

    // the configured owner connection is a third login that owns nothing and holds only USAGE and
    // INSERT on the audit schema (no grants on `public` beyond PostgreSQL's default schema USAGE):
    // refused, nothing deleted
    client
        .batch_execute(&format!(
            "GRANT USAGE ON SCHEMA erp_event TO {imp}; \
             GRANT INSERT ON ALL TABLES IN SCHEMA erp_event TO {imp}",
            imp = erp_sql::quote(&impostor)
        ))
        .await
        .unwrap();
    let other = dir.join("impostor-as-owner");
    fs::create_dir(&other).unwrap();
    let other_config = native_http::config(&other, &db, 18093, &role, &addons);
    native_http::private(
        &other.join("owner-url"),
        &native_http::url(&db.config(), Some(&impostor)),
    );
    let out = run(
        &["auth", "emergency-revoke"],
        &other_config,
        &["--reason", "compromised", "--key", &spare.to_string()],
    );
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
    assert!(
        text(&out.stderr).contains(
            "a later release: the configured owner connection does not own \
             public.res_users_apikeys; emergency revocation runs only on the owner connection"
        ),
        "{}",
        text(&out.stderr)
    );
    assert_eq!(
        count(
            &client,
            &format!("SELECT count(*) FROM res_users_apikeys WHERE id = {spare}")
        )
        .await,
        1,
        "the refused run deleted nothing"
    );

    drop(client);
    let mut admin = db.config();
    admin.dbname("postgres");
    db.cleanup().await.unwrap();
    let (client, connection) = admin
        .connect(erp_testkit::tokio_postgres::NoTls)
        .await
        .unwrap();
    tokio::spawn(async move { connection.await.unwrap() });
    client
        .batch_execute(&format!(
            "DROP ROLE {}; DROP ROLE {}; DROP ROLE {}",
            erp_sql::quote(&role),
            erp_sql::quote(&node_role),
            erp_sql::quote(&impostor)
        ))
        .await
        .unwrap();
}
