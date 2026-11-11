//! `store_fetch` through the JSON-2 dispatcher (
//! design). Test topics are registered with the same slice an addon
//! uses; they are in this test binary only.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod support;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock};

use erp_addon_mail::store_fetch::{
    Audience, Expected, MAX_TOPICS_PER_CALL, ParamDecl, SchemaFragment, SchemaPart, StoreTopic,
    TopicFuture, TopicInput, TopicOutput, TopicRegistry,
};
use erp_orm::{IntoPolicyProvider, ModelAllow, Orm};
use erp_rpc::{RpcConfig, RpcService};
use erp_sdk::serde_json::{Value as Json, json};
use erp_sdk::store_wire::{FieldKind, FieldSpec, FieldValue};
use erp_sdk::{Cell, Error, ErrorKind, RawVals, Transaction};
use erp_testkit::{FieldPolicy, Harness, RpcHarness};
use support::rpc::*;
use tokio::sync::Semaphore;

const PATH: &str = "/json/2/mail.store/store_fetch";

// ---------------------------------------------------------------------------------------------
// test topics

macro_rules! topic {
    ($name:ident, $topic:expr) => {
        erp_addon_mail::store_topic!($name = $topic);
    };
}

static REACHED: LazyLock<Semaphore> = LazyLock::new(|| Semaphore::new(0));
static GO: LazyLock<Semaphore> = LazyLock::new(|| Semaphore::new(0));
static DENIED_RAN: AtomicBool = AtomicBool::new(false);
static INTERNAL_RAN: AtomicBool = AtomicBool::new(false);
// set only by `test.mark`, which a test requests next to a refused request to prove it never ran
static MARK_RAN: AtomicBool = AtomicBool::new(false);

fn count<'a>(
    tx: &'a mut Transaction,
    _i: &'a TopicInput<'a>,
    out: &'a mut TopicOutput,
) -> TopicFuture<'a> {
    Box::pin(async move {
        let rows = tx
            .query("SELECT count(*)::int8 FROM res_partner", &[])
            .await?;
        let n: i64 = rows[0].get(0);
        out.data(json!({"count": n}));
        Ok(())
    })
}
topic!(
    COUNT,
    StoreTopic::new("mail", "test.count", count)
        .audience(Audience::LoggedIn)
        .statements(1)
);

fn mark<'a>(
    _tx: &'a mut Transaction,
    _i: &'a TopicInput<'a>,
    _out: &'a mut TopicOutput,
) -> TopicFuture<'a> {
    Box::pin(async move {
        MARK_RAN.store(true, Ordering::SeqCst);
        Ok(())
    })
}
topic!(
    MARK,
    StoreTopic::new("mail", "test.mark", mark).audience(Audience::LoggedIn)
);

fn pause<'a>(
    _tx: &'a mut Transaction,
    _i: &'a TopicInput<'a>,
    _out: &'a mut TopicOutput,
) -> TopicFuture<'a> {
    Box::pin(async move {
        REACHED.add_permits(1);
        GO.acquire().await.expect("semaphore open").forget();
        Ok(())
    })
}
topic!(
    PAUSE,
    StoreTopic::new("mail", "test.pause", pause).audience(Audience::LoggedIn)
);

fn write_sql<'a>(
    tx: &'a mut Transaction,
    _i: &'a TopicInput<'a>,
    _out: &'a mut TopicOutput,
) -> TopicFuture<'a> {
    Box::pin(async move {
        tx.execute("UPDATE res_users SET name = 'hacked' WHERE id = 2", &[])
            .await?;
        Ok(())
    })
}
topic!(
    WRITE_SQL,
    StoreTopic::new("mail", "test.write_sql", write_sql)
        .audience(Audience::LoggedIn)
        .statements(1)
);

fn write_orm<'a>(
    tx: &'a mut Transaction,
    input: &'a TopicInput<'a>,
    _out: &'a mut TopicOutput,
) -> TopicFuture<'a> {
    Box::pin(async move {
        let users = input.env().records("res.users")?.browse(vec![2])?;
        users
            .write(
                tx,
                RawVals::new().set_cell("name", Cell::Str("hacked".into())),
            )
            .await?;
        tx.flush().await?;
        Ok(())
    })
}
topic!(
    WRITE_ORM,
    StoreTopic::new("mail", "test.write_orm", write_orm)
        .audience(Audience::LoggedIn)
        .statements(5)
);

fn internal<'a>(
    _tx: &'a mut Transaction,
    _i: &'a TopicInput<'a>,
    out: &'a mut TopicOutput,
) -> TopicFuture<'a> {
    Box::pin(async move {
        INTERNAL_RAN.store(true, Ordering::SeqCst);
        out.data(json!({"ran": true}));
        Ok(())
    })
}
topic!(
    INTERNAL,
    StoreTopic::new("mail", "test.internal", internal).audience(Audience::Internal)
);

fn deny_check<'a>(_tx: &'a mut Transaction, _i: &'a TopicInput<'a>) -> TopicFuture<'a> {
    Box::pin(async move { Err(Error::access("denied by the test access check")) })
}
fn denied_run<'a>(
    _tx: &'a mut Transaction,
    _i: &'a TopicInput<'a>,
    _out: &'a mut TopicOutput,
) -> TopicFuture<'a> {
    Box::pin(async move {
        DENIED_RAN.store(true, Ordering::SeqCst);
        Ok(())
    })
}
topic!(
    DENY,
    StoreTopic::new("mail", "test.deny", denied_run)
        .audience(Audience::LoggedIn)
        .access(deny_check)
);

fn access_mid<'a>(
    _tx: &'a mut Transaction,
    _i: &'a TopicInput<'a>,
    out: &'a mut TopicOutput,
) -> TopicFuture<'a> {
    Box::pin(async move {
        out.set("messaging.self", 99, "share", FieldValue::Bool(true));
        Err(Error::access("late denial"))
    })
}
topic!(
    ACCESS_MID,
    StoreTopic::new("mail", "test.access_mid", access_mid).audience(Audience::LoggedIn)
);

fn budget_then_denied<'a>(
    tx: &'a mut Transaction,
    _i: &'a TopicInput<'a>,
    _out: &'a mut TopicOutput,
) -> TopicFuture<'a> {
    Box::pin(async move {
        for _ in 0..3 {
            tx.query("SELECT 1", &[]).await?;
        }
        Err(Error::access("denied after the overrun"))
    })
}
topic!(
    BUDGET_THEN_DENIED,
    StoreTopic::new("mail", "test.budget_then_denied", budget_then_denied)
        .audience(Audience::LoggedIn)
        .statements(2)
);

fn validation<'a>(
    _tx: &'a mut Transaction,
    _i: &'a TopicInput<'a>,
    _out: &'a mut TopicOutput,
) -> TopicFuture<'a> {
    Box::pin(async move { Err(Error::validation("the topic says no")) })
}
topic!(
    VALIDATION,
    StoreTopic::new("mail", "test.validation", validation).audience(Audience::LoggedIn)
);

fn bad_record<'a>(
    _tx: &'a mut Transaction,
    _i: &'a TopicInput<'a>,
    out: &'a mut TopicOutput,
) -> TopicFuture<'a> {
    Box::pin(async move {
        out.set("no.such", 1, "x", FieldValue::Bool(true));
        Ok(())
    })
}
topic!(
    BAD_RECORD,
    StoreTopic::new("mail", "test.bad_record", bad_record).audience(Audience::LoggedIn)
);

fn stage_true<'a>(
    _tx: &'a mut Transaction,
    _i: &'a TopicInput<'a>,
    out: &'a mut TopicOutput,
) -> TopicFuture<'a> {
    Box::pin(async move {
        out.set("messaging.self", 7, "share", FieldValue::Bool(true));
        Ok(())
    })
}
topic!(
    STAGE_TRUE,
    StoreTopic::new("mail", "test.stage_true", stage_true).audience(Audience::LoggedIn)
);
topic!(
    STAGE_TRUE_TOO,
    StoreTopic::new("mail", "test.stage_true_too", stage_true).audience(Audience::LoggedIn)
);

fn stage_false<'a>(
    _tx: &'a mut Transaction,
    _i: &'a TopicInput<'a>,
    out: &'a mut TopicOutput,
) -> TopicFuture<'a> {
    Box::pin(async move {
        out.set("messaging.self", 7, "share", FieldValue::Bool(false));
        Ok(())
    })
}
topic!(
    STAGE_FALSE,
    StoreTopic::new("mail", "test.stage_false", stage_false).audience(Audience::LoggedIn)
);

fn stage_delete<'a>(
    _tx: &'a mut Transaction,
    _i: &'a TopicInput<'a>,
    out: &'a mut TopicOutput,
) -> TopicFuture<'a> {
    Box::pin(async move {
        out.delete("messaging.self", 7);
        Ok(())
    })
}
topic!(
    STAGE_DELETE,
    StoreTopic::new("mail", "test.stage_delete", stage_delete).audience(Audience::LoggedIn)
);

const BIG_PARAMS: &[ParamDecl] = &[ParamDecl::text("t", 5000)];
fn big<'a>(
    _tx: &'a mut Transaction,
    input: &'a TopicInput<'a>,
    out: &'a mut TopicOutput,
) -> TopicFuture<'a> {
    Box::pin(async move {
        out.data(json!({"len": input.params().text("t")?.len()}));
        Ok(())
    })
}
topic!(
    BIG,
    StoreTopic::new("mail", "test.big", big)
        .audience(Audience::LoggedIn)
        .params(BIG_PARAMS)
);

const BUDGET_PARAMS: &[ParamDecl] = &[ParamDecl::int("n", 0, 5)];
fn budget<'a>(
    tx: &'a mut Transaction,
    input: &'a TopicInput<'a>,
    _out: &'a mut TopicOutput,
) -> TopicFuture<'a> {
    Box::pin(async move {
        for _ in 0..input.params().int("n")? {
            tx.query("SELECT 1", &[]).await?;
        }
        Ok(())
    })
}
topic!(
    BUDGET,
    StoreTopic::new("mail", "test.budget", budget)
        .audience(Audience::LoggedIn)
        .params(BUDGET_PARAMS)
        .statements(2)
);

const ECHO_PARAMS: &[ParamDecl] = &[
    ParamDecl::int("n", 1, 10),
    ParamDecl::text("label", 8).optional(),
    ParamDecl::ids("ids", 3).optional(),
];
fn echo<'a>(
    _tx: &'a mut Transaction,
    input: &'a TopicInput<'a>,
    out: &'a mut TopicOutput,
) -> TopicFuture<'a> {
    Box::pin(async move {
        let p = input.params();
        out.data(json!({
            "n": p.int("n")?,
            "label": p.opt_text("label")?,
            "ids": p.opt_ids("ids")?,
        }));
        Ok(())
    })
}
topic!(
    ECHO,
    StoreTopic::new("mail", "test.echo", echo)
        .audience(Audience::LoggedIn)
        .params(ECHO_PARAMS)
);

topic!(
    GHOST,
    StoreTopic::new("ghost_addon", "ghost.topic", echo)
        .audience(Audience::LoggedIn)
        .params(ECHO_PARAMS)
);

fn app_name<'a>(
    tx: &'a mut Transaction,
    _i: &'a TopicInput<'a>,
    out: &'a mut TopicOutput,
) -> TopicFuture<'a> {
    Box::pin(async move {
        let rows = tx
            .query("SELECT current_setting('application_name')", &[])
            .await?;
        let name: String = rows[0].get(0);
        out.data(json!({"application_name": name}));
        Ok(())
    })
}
topic!(
    APP_NAME,
    StoreTopic::new("mail", "test.app_name", app_name)
        .audience(Audience::LoggedIn)
        .statements(1)
);

// ---------------------------------------------------------------------------------------------
// harness

fn policy() -> FieldPolicy {
    FieldPolicy::new().member(ALICE, ["base.group_user"])
}

const ADDONS: [&erp_sdk::AddonDescriptor; 2] = [&erp_addon_base::ADDON, &erp_addon_mail::ADDON];

async fn give_partners(h: &RpcHarness) {
    sql_batch(
        h,
        "INSERT INTO res_partner (id, name) VALUES (102, 'alice partner'), (103, 'bob partner'); \
         UPDATE res_users SET partner_id = 102, share = false WHERE id = 2; \
         UPDATE res_users SET partner_id = 103, share = true WHERE id = 3",
    )
    .await;
}

async fn harness() -> RpcHarness {
    // the link-root check (d); the server does not call it yet (it does not link `mail`)
    TopicRegistry::require_linked(&[]).unwrap();
    let h = harness_with_policy(&ADDONS, policy(), RpcConfig::default(), false).await;
    give_partners(&h).await;
    h
}

fn topics(names: &[&str]) -> String {
    let list: Vec<Json> = names.iter().map(|n| json!({"name": n})).collect();
    json!({"wire": 1, "topics": list}).to_string()
}

async fn fetch(h: &RpcHarness, body: &str) -> erp_rpc::RpcResponse {
    call(h, PATH, body, alice()).await
}

/// The record `id` of `messaging.self` in an answer's payload, `Null` when absent.
fn rec(answer: &Json, id: u64) -> Json {
    answer["store"]["records"]["messaging.self"]
        .as_array()
        .and_then(|entries| entries.iter().find(|e| e["id"] == id))
        .cloned()
        .unwrap_or(Json::Null)
}

fn slot(answer: &Json, index: usize) -> &Json {
    &answer["topics"][index]
}

#[tokio::test]
async fn the_method_its_gates_and_the_result_shape() {
    let h = harness().await;
    let reader = alice_key(scope(7, "reader", ModelAllow::All, true));
    for caller in [alice(), reader] {
        let r = call(&h, PATH, &topics(&["messaging.self", "schema"]), caller).await;
        let answer = ok(&r);
        assert_eq!(answer["store"]["wire"], 1);
        assert_eq!(answer["store"]["asof"]["own"], Json::Null);
        assert_eq!(
            slot(&answer, 0),
            &json!({"topic": "messaging.self", "ok": true})
        );
        assert_eq!(slot(&answer, 1)["topic"], "schema");
        assert_eq!(slot(&answer, 1)["ok"], true);
        assert!(slot(&answer, 1)["data"].is_object());
        assert_eq!(rec(&answer, 2)["partner_id"], 102);
    }
    let bot = alice_key(scope(
        8,
        "effects bot",
        ModelAllow::named(["test_rpc.effects"]).unwrap(),
        false,
    ));
    let r = call(&h, PATH, &topics(&["schema"]), bot).await;
    assert_error(
        &r,
        403,
        "erp.exceptions.AccessError",
        "The API key 'effects bot' may not call 'store_fetch' on 'Messaging store' \
         (mail.store): outside its scope.",
    );
    for wire in [0, 2] {
        MARK_RAN.store(false, Ordering::SeqCst);
        let body = json!({"wire": wire, "topics": [{"name": "test.mark"}]}).to_string();
        let r = fetch(&h, &body).await;
        let text = String::from_utf8_lossy(r.body()).into_owned();
        assert_eq!(r.status(), 422, "{text}");
        let message = json(&r)["message"].as_str().unwrap().to_owned();
        assert!(message.starts_with("client_outdated: "), "{message}");
        assert_eq!(json(&r)["name"], "erp.exceptions.ValidationError");
        assert!(
            !MARK_RAN.load(Ordering::SeqCst),
            "a topic ran for wire {wire}"
        );
    }
    h.cleanup().await.unwrap();
}

/// persists a change; the message of the refused ORM write is not asserted)
#[tokio::test]
async fn read_only_is_enforced_by_the_database() {
    let h = harness().await;
    for body in [
        topics(&["test.write_sql"]),
        topics(&["messaging.self", "test.write_sql"]),
        topics(&["test.write_orm"]),
        topics(&["messaging.self", "test.write_orm"]),
    ] {
        let r = fetch(&h, &body).await;
        assert_ne!(r.status(), 200, "{}", String::from_utf8_lossy(r.body()));
        let text = String::from_utf8_lossy(r.body()).into_owned();
        assert!(!text.contains("\"store\""), "{text}");
        assert_eq!(
            sql_one(&h, "SELECT name FROM res_users WHERE id = 2").await,
            "alice"
        );
    }
    let r = fetch(&h, &topics(&["test.write_sql"])).await;
    assert_error(
        &r,
        500,
        "erp.programming",
        "'mail.store.store_fetch' is declared effect = read but tried to write through SQL; \
         declare effect = write (or wider).",
    );
    h.cleanup().await.unwrap();
}

/// later `xmax`; equality with the descriptor of a transaction opened at the same point is not asserted)
#[tokio::test]
async fn one_snapshot_and_one_asof_for_the_whole_call() {
    let h = harness().await;
    let before = count_partners(&h).await;
    let body = topics(&["test.count", "test.pause", "test.count"]);
    let (response, ()) = tokio::join!(fetch(&h, &body), async {
        REACHED.acquire().await.expect("open").forget();
        sql_batch(
            &h,
            "INSERT INTO res_partner (id, name) VALUES (500, 'late')",
        )
        .await;
        GO.add_permits(1);
    });
    let first = ok(&response);
    assert_eq!(slot(&first, 0)["data"]["count"], before);
    assert_eq!(slot(&first, 2)["data"]["count"], before);
    assert_eq!(first["store"]["asof"]["own"], Json::Null);
    // a following call sees the row and a later snapshot
    let body = topics(&["test.count"]);
    let second = ok(&fetch(&h, &body).await);
    assert_eq!(slot(&second, 0)["data"]["count"], before + 1);
    let xmax = |a: &Json| -> u64 {
        a["store"]["asof"]["xmax"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap()
    };
    assert!(xmax(&second) > xmax(&first));
    h.cleanup().await.unwrap();
}

async fn count_partners(h: &RpcHarness) -> i64 {
    sql_one(h, "SELECT count(*)::text FROM res_partner")
        .await
        .parse()
        .unwrap()
}

async fn read_pool_harness() -> RpcHarness {
    let h = RpcHarness::builder(
        Harness::builder([
            &erp_test_models::base_users::ADDON,
            &erp_test_models::test_orm::ADDON,
            &erp_test_models::test_rpc::ADDON,
            &erp_addon_base::ADDON,
            &erp_addon_mail::ADDON,
        ])
        .with_static_decls(support::currency_fixture::decls()),
        policy(),
    )
    .host(|_orm| FixtureHost)
    .config(RpcConfig::default())
    .with_signal()
    .with_read_pool("-c application_name=erp_read_pool")
    .build()
    .await
    .expect("rpc harness");
    sql_batch(
        &h,
        "INSERT INTO res_users (id, name, login, active) VALUES (2, 'alice', 'alice', true), \
         (3, 'bob', 'bob', true), (4, 'admin', 'admin', true); \
         SELECT setval(pg_get_serial_sequence('res_users', 'id'), 4)",
    )
    .await;
    give_partners(&h).await;
    h
}

/// primary-read entry naming a method that is not an exposed read is not covered)
#[tokio::test]
async fn answers_are_read_on_the_primary() {
    let h = read_pool_harness().await;
    assert!(h.orm().routing_stats().read_pool);
    let served = || h.orm().routing_stats().read_pool_served;
    let before = served();
    let r = call(
        &h,
        "/json/2/res.users/search",
        r#"{"domain": [["id", "=", 2]]}"#,
        alice(),
    )
    .await;
    ok(&r);
    assert_eq!(
        served(),
        before + 1,
        "an ordinary read goes to the read pool"
    );
    let before = served();
    let answer = ok(&fetch(&h, &topics(&["test.app_name", "messaging.self"])).await);
    assert_eq!(served(), before, "store_fetch stays on the primary");
    assert_ne!(
        slot(&answer, 0)["data"]["application_name"],
        "erp_read_pool"
    );
    assert!(answer["store"]["asof"]["xmax"].is_string());
    h.cleanup().await.unwrap();
}

/// ordinary read; the refusal of a primary-read entry naming a non-exposed method is not covered)
#[tokio::test]
async fn the_primary_read_entry_marks_the_exposed_method() {
    let h = harness().await;
    let registry = h.harness().registry();
    let id = registry.model_id("mail.store").expect("mail.store model");
    let table = h.orm().exposure();
    match table.get(id, "store_fetch") {
        Some(erp_sdk::rpc::Exposure::Exposed(e)) => {
            assert!(e.primary_read());
            assert_eq!(e.effect(), erp_sdk::rpc::Effect::Read);
        }
        other => panic!("store_fetch is not exposed: {other:?}"),
    }
    let ids = registry.model_id("res.users").expect("res.users");
    match table.get(ids, "search") {
        Some(erp_sdk::rpc::Exposure::Exposed(e)) => assert!(!e.primary_read()),
        other => panic!("search is not exposed: {other:?}"),
    }
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn an_unknown_topic_refuses_the_whole_call() {
    let h = harness().await;
    let long = "x".repeat(70);
    let cut = format!("{}…", "x".repeat(64));
    for (name, shown) in [
        ("no.such.topic", "no.such.topic".to_owned()),
        ("ghost.topic", "ghost.topic".to_owned()),
        (long.as_str(), cut),
    ] {
        MARK_RAN.store(false, Ordering::SeqCst);
        let body = json!({"wire": 1, "topics": [
            {"name": "test.mark"}, {"name": name, "params": {"n": 1}}
        ]})
        .to_string();
        let r = fetch(&h, &body).await;
        assert_error(
            &r,
            422,
            "erp.exceptions.ValidationError",
            &format!("unknown store topic '{shown}'"),
        );
        assert!(!String::from_utf8_lossy(r.body()).contains("\"store\""));
        assert!(
            !MARK_RAN.load(Ordering::SeqCst),
            "a topic ran for '{shown}'"
        );
    }
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn invalid_params_refuse_the_whole_call() {
    let h = harness().await;
    let cases: [(Json, &str); 12] = [
        (json!({}), "store topic 'test.echo': param 'n' is required"),
        (
            json!({"n": "1"}),
            "store topic 'test.echo': param 'n' must be an integer",
        ),
        (
            json!({"n": 0}),
            "store topic 'test.echo': param 'n' is 0; it must be between 1 and 10",
        ),
        (
            json!({"n": 11}),
            "store topic 'test.echo': param 'n' is 11; it must be between 1 and 10",
        ),
        (
            json!({"n": 1.5}),
            "store topic 'test.echo': param 'n' is not an integer",
        ),
        (
            json!({"n": 1, "user_id": 3}),
            "store topic 'test.echo': unknown param 'user_id'",
        ),
        (
            json!({"n": 1, "label": "123456789"}),
            "store topic 'test.echo': param 'label' is 9 bytes; at most 8 are accepted",
        ),
        (
            json!({"n": 1, "ids": [1, 2, 3, 4]}),
            "store topic 'test.echo': param 'ids' has 4 ids; at most 3 are accepted",
        ),
        (
            json!({"n": 1, "ids": [1, 0]}),
            "store topic 'test.echo': param 'ids' holds a value that is not a positive id",
        ),
        (
            json!(null),
            "store topic 'test.echo': params must be an object",
        ),
        (
            json!([1]),
            "store topic 'test.echo': params must be an object",
        ),
        (
            json!("x"),
            "store topic 'test.echo': params must be an object",
        ),
    ];
    for (params, message) in cases {
        MARK_RAN.store(false, Ordering::SeqCst);
        let body = json!({"wire": 1, "topics": [
            {"name": "test.mark"}, {"name": "test.echo", "params": params}
        ]})
        .to_string();
        let r = fetch(&h, &body).await;
        assert_error(&r, 422, "erp.exceptions.ValidationError", message);
        assert!(
            !MARK_RAN.load(Ordering::SeqCst),
            "a topic ran for {message}"
        );
    }
    let body = json!({"wire": 1, "topics": [
        {"name": "test.echo", "params": {"n": 3, "label": "ab", "ids": [4, 5]}}
    ]})
    .to_string();
    let answer = ok(&fetch(&h, &body).await);
    assert_eq!(
        slot(&answer, 0)["data"],
        json!({"n": 3, "label": "ab", "ids": [4, 5]})
    );
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn the_topic_count_is_bounded() {
    let h = harness().await;
    let name = "messaging.self";
    let many = |n: usize| topics(&vec![name; n]);
    let r = fetch(&h, &many(0)).await;
    assert_error(
        &r,
        422,
        "erp.exceptions.ValidationError",
        "store_fetch: topics must name at least one topic",
    );
    let r = fetch(&h, &many(MAX_TOPICS_PER_CALL + 1)).await;
    assert_error(
        &r,
        422,
        "erp.exceptions.ValidationError",
        "store_fetch: 17 topics requested; at most 16 are accepted",
    );
    let answer = ok(&fetch(&h, &many(MAX_TOPICS_PER_CALL)).await);
    assert_eq!(answer["topics"].as_array().unwrap().len(), 16);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn params_size_depth_and_structure_are_bounded() {
    let h = harness().await;
    let big = json!({"n": 1, "label": "x".repeat(4100)});
    let deep = json!({"n": 1, "ids": [[[[1]]]]});
    for (params, message) in [
        (
            big,
            "store topic 'test.echo': params are 4118 bytes; at most 4096 are accepted",
        ),
        (
            deep,
            "store topic 'test.echo': params are nested deeper than 4 levels",
        ),
    ] {
        let body =
            json!({"wire": 1, "topics": [{"name": "test.echo", "params": params}]}).to_string();
        let r = fetch(&h, &body).await;
        assert_error(&r, 422, "erp.exceptions.ValidationError", message);
    }
    for (body, message) in [
        (
            json!({"wire": 1, "topics": [{"name": "test.count", "id": 5}]}),
            "store_fetch: topics[0] has an unknown key 'id'",
        ),
        (
            json!({"wire": 1, "topics": [{"params": {}}]}),
            "store_fetch: topics[0] needs a name",
        ),
        (
            json!({"wire": 1, "topics": [{"name": 5}]}),
            "store_fetch: topics[0].name must be text",
        ),
        (
            json!({"wire": 1, "topics": [{"name": "test.count"}, "test.count"]}),
            "store_fetch: topics[1] must be an object with a name",
        ),
        (
            json!({"wire": 1, "topics": {"name": "test.count"}}),
            "store_fetch: topics must be a list",
        ),
    ] {
        let r = fetch(&h, &body.to_string()).await;
        assert_error(&r, 422, "erp.exceptions.ValidationError", message);
    }
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn a_denied_topic_is_a_typed_slot_and_the_others_answer() {
    let h = harness().await;
    DENIED_RAN.store(false, Ordering::SeqCst);
    INTERNAL_RAN.store(false, Ordering::SeqCst);
    let body = topics(&["test.internal", "test.deny", "messaging.self"]);
    // bob is not an internal user
    let answer = ok(&call(&h, PATH, &body, user(BOB)).await);
    assert_eq!(
        slot(&answer, 0),
        &json!({"topic": "test.internal", "ok": false, "error": {
            "kind": "access",
            "message": "You are not allowed to fetch the store topic 'test.internal'."}})
    );
    assert_eq!(
        slot(&answer, 1),
        &json!({"topic": "test.deny", "ok": false, "error": {
            "kind": "access", "message": "denied by the test access check"}})
    );
    assert_eq!(
        slot(&answer, 2),
        &json!({"topic": "messaging.self", "ok": true})
    );
    assert_eq!(rec(&answer, 3)["share"], true);
    assert!(!DENIED_RAN.load(Ordering::SeqCst));
    assert!(!INTERNAL_RAN.load(Ordering::SeqCst));
    // alice is internal: her audience check passes
    let answer = ok(&fetch(&h, &topics(&["test.internal"])).await);
    assert_eq!(slot(&answer, 0)["ok"], true);
    assert!(INTERNAL_RAN.load(Ordering::SeqCst));
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn other_failures_fail_the_whole_call_and_nothing_is_partial() {
    let h = harness().await;
    let answer = ok(&fetch(&h, &topics(&["test.access_mid", "messaging.self"])).await);
    assert_eq!(
        slot(&answer, 0),
        &json!({"topic": "test.access_mid", "ok": false, "error": {
            "kind": "access", "message": "late denial"}})
    );
    assert!(rec(&answer, 99).is_null());
    assert_eq!(rec(&answer, 2)["share"], false);
    let r = fetch(&h, &topics(&["messaging.self", "test.validation"])).await;
    assert_error(
        &r,
        422,
        "erp.exceptions.ValidationError",
        "the topic says no",
    );
    let r = fetch(&h, &topics(&["messaging.self", "test.bad_record"])).await;
    assert_error(
        &r,
        500,
        "erp.internal",
        "store topic 'test.bad_record' produced a record the store schema refuses: records.no.such: \
         unknown model no.such",
    );
    h.cleanup().await.unwrap();
}

/// `needs_groups` are in `mail_rpc.rs`)
#[tokio::test]
async fn a_topic_over_its_statement_budget_fails_the_call() {
    let h = harness().await;
    let one = |n: i64| {
        json!({"wire": 1, "topics": [{"name": "test.budget", "params": {"n": n}}]}).to_string()
    };
    ok(&fetch(&h, &one(2)).await);
    let r = fetch(&h, &one(3)).await;
    assert_error(
        &r,
        500,
        "erp.programming",
        "store topic 'test.budget' sent 3 statements; its budget is 2",
    );
    // a topic that overruns its budget and then raises Access is a defect of the topic, not a denial
    let body = topics(&["messaging.self", "test.budget_then_denied"]);
    let r = fetch(&h, &body).await;
    assert_error(
        &r,
        500,
        "erp.programming",
        "store topic 'test.budget_then_denied' sent 3 statements; its budget is 2",
    );
    let registry = TopicRegistry::linked().unwrap();
    assert_eq!(registry.topic("schema").unwrap().budget(), 0);
    assert_eq!(registry.topic("messaging.self").unwrap().budget(), 1);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn the_schema_topic() {
    let h = harness().await;
    let answer = ok(&fetch(&h, &topics(&["schema"])).await);
    let document = &slot(&answer, 0)["data"];
    assert_eq!(document["schema"], answer["store"]["schema"]);
    let generation = document["schema"].as_str().unwrap();
    assert!(generation.starts_with("s1-"), "{generation}");
    let bytes = serde_json_bytes(document);
    let decoded = erp_sdk::store_wire::Decoder::new(erp_sdk::store_wire::WireLimits::default())
        .decode_schema(&bytes)
        .expect("the schema topic's document is a valid schema");
    assert_eq!(decoded.generation().as_str(), generation);
    let model = decoded.model("messaging.self").expect("messaging.self");
    assert_eq!(
        model.field("partner_id").unwrap().kind(),
        FieldKind::Many2One
    );
    assert_eq!(model.field("share").unwrap().kind(), FieldKind::Bool);
    assert!(decoded.model("res.partner").is_some());
    let models: Vec<&str> = decoded.models().map(|(n, _)| n.as_str()).collect();
    // `thread.scheduled` adds its model to the schema `mail` contributes
    assert_eq!(
        models,
        [
            "ir.attachment",
            "mail.followers",
            "mail.message",
            "mail.message.subtype",
            "mail.scheduled.message",
            "messaging.self",
            "res.partner"
        ]
    );
    // content-derived: same parts, same token; one more field, another token
    let first = TopicRegistry::from_parts(&[], &[&PART_A], &[]).unwrap();
    let again = TopicRegistry::from_parts(&[], &[&PART_A], &[]).unwrap();
    let more = TopicRegistry::from_parts(&[], &[&PART_A, &PART_B], &[]).unwrap();
    assert_eq!(first.schema().generation(), again.schema().generation());
    assert_ne!(first.schema().generation(), more.schema().generation());
    h.cleanup().await.unwrap();
}

fn serde_json_bytes(value: &Json) -> Vec<u8> {
    value.to_string().into_bytes()
}

fn part_a(f: &mut SchemaFragment) {
    f.field("x.model", "a", FieldSpec::new(FieldKind::Bool));
}
fn part_b(f: &mut SchemaFragment) {
    f.field("x.model", "b", FieldSpec::new(FieldKind::Int));
}
fn part_a_other(f: &mut SchemaFragment) {
    f.field("x.model", "a", FieldSpec::new(FieldKind::Int));
}
static PART_A: SchemaPart = SchemaPart::new("one", part_a);
static PART_A_AGAIN: SchemaPart = SchemaPart::new("two", part_a);
static PART_B: SchemaPart = SchemaPart::new("two", part_b);
static PART_A_OTHER: SchemaPart = SchemaPart::new("three", part_a_other);

#[tokio::test]
async fn messaging_self_answers_the_callers_own_record() {
    let h = harness().await;
    let answer = ok(&fetch(&h, &topics(&["messaging.self"])).await);
    assert_eq!(rec(&answer, 2)["partner_id"], 102);
    assert_eq!(rec(&answer, 2)["share"], false);
    assert!(rec(&answer, 3).is_null());
    let answer = ok(&call(&h, PATH, &topics(&["messaging.self"]), user(BOB)).await);
    assert_eq!(rec(&answer, 3)["partner_id"], 103);
    assert_eq!(rec(&answer, 3)["share"], true);
    let body = json!({"wire": 1, "topics": [
        {"name": "messaging.self", "params": {"user_id": 3}}
    ]})
    .to_string();
    let r = fetch(&h, &body).await;
    assert_error(
        &r,
        422,
        "erp.exceptions.ValidationError",
        "store topic 'messaging.self': unknown param 'user_id'",
    );
    h.cleanup().await.unwrap();
}

fn noop<'a>(
    _tx: &'a mut Transaction,
    _i: &'a TopicInput<'a>,
    _out: &'a mut TopicOutput,
) -> TopicFuture<'a> {
    Box::pin(async { Ok(()) })
}

static T_ONE: StoreTopic = StoreTopic::new("one", "dup.topic", noop);
static T_TWO: StoreTopic = StoreTopic::new("two", "dup.topic", noop);
static T_UPPER: StoreTopic = StoreTopic::new("one", "Bad", noop);
static T_EMPTY: StoreTopic = StoreTopic::new("one", "", noop);
static T_LONG_NAME: &str = "a23456789012345678901234567890123456789012345678901234567890123456";
static T_TWICE: StoreTopic = StoreTopic::new("one", "twice", noop)
    .params(&[ParamDecl::int("n", 0, 1), ParamDecl::boolean("n")]);

static EXTRA_PARAMS: &[ParamDecl] = &[
    ParamDecl::boolean("deep").optional(),
    ParamDecl::int("n", 0, 1),
];
static T_PLUS_UNKNOWN: StoreTopic = StoreTopic::new("one", "plus_unknown", noop)
    .params(EXTRA_PARAMS)
    .statements_plus(&[("deeper", 1)]);
static T_PLUS_REQUIRED: StoreTopic = StoreTopic::new("one", "plus_required", noop)
    .params(EXTRA_PARAMS)
    .statements_plus(&[("n", 1)]);
static T_PLUS_TWICE: StoreTopic = StoreTopic::new("one", "plus_twice", noop)
    .params(EXTRA_PARAMS)
    .statements_plus(&[("deep", 1), ("deep", 2)]);
static T_PLUS_ZERO: StoreTopic = StoreTopic::new("one", "plus_zero", noop)
    .params(EXTRA_PARAMS)
    .statements_plus(&[("deep", 0)]);
static T_PLUS_OK: StoreTopic = StoreTopic::new("one", "plus_ok", noop)
    .params(EXTRA_PARAMS)
    .statements_plus(&[("deep", 2)]);

#[test]
fn registry_refuses_an_ill_formed_statements_plus() {
    let err = |topic: &'static StoreTopic| {
        let e = TopicRegistry::from_parts(&[topic], &[], &[]).unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Programming);
        e.message().to_owned()
    };
    assert_eq!(
        err(&T_PLUS_UNKNOWN),
        "store topic 'plus_unknown' of addon 'one' raises its budget for param 'deeper', which is \
         not a declared optional param"
    );
    assert_eq!(
        err(&T_PLUS_REQUIRED),
        "store topic 'plus_required' of addon 'one' raises its budget for param 'n', which is \
         not a declared optional param"
    );
    assert_eq!(
        err(&T_PLUS_TWICE),
        "store topic 'plus_twice' of addon 'one' raises its budget for param 'deep' twice"
    );
    assert_eq!(
        err(&T_PLUS_ZERO),
        "store topic 'plus_zero' of addon 'one' raises its budget for param 'deep' by 0 \
         statements: drop the entry"
    );
    TopicRegistry::from_parts(&[&T_PLUS_OK], &[], &[]).unwrap();
}

#[test]
fn registry_build_checks() {
    let err = |topics: &[&'static StoreTopic], parts: &[&'static SchemaPart], exp: &[Expected]| {
        let e = TopicRegistry::from_parts(topics, parts, exp).unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Programming);
        e.message().to_owned()
    };
    assert_eq!(
        err(&[&T_ONE, &T_TWO], &[], &[]),
        "store topic 'dup.topic' is registered twice: by addon 'one' and by addon 'two'"
    );
    assert_eq!(
        err(&[&T_UPPER], &[], &[]),
        "store topic name 'Bad' of addon 'one' is invalid: use 1..=64 bytes of a-z, 0-9, '_' and \
         '.', starting with a letter"
    );
    assert!(
        err(&[&T_EMPTY], &[], &[]).starts_with("store topic name '' of addon 'one' is invalid")
    );
    let long: &'static StoreTopic = Box::leak(Box::new(StoreTopic::new("one", T_LONG_NAME, noop)));
    assert!(err(&[long], &[], &[]).contains("is invalid"));
    assert_eq!(
        err(&[&T_TWICE], &[], &[]),
        "store topic 'twice' of addon 'one' declares param 'n' twice"
    );
    assert_eq!(
        err(&[], &[&PART_A, &PART_A_OTHER], &[]),
        "store schema field x.model.a is declared differently by addon 'one' and addon 'three'"
    );
    TopicRegistry::from_parts(&[], &[&PART_A, &PART_A_AGAIN], &[]).unwrap();
    const WANT: Expected = Expected::new("one", &["dup.topic"], true);
    assert_eq!(
        err(&[], &[&PART_A], &[WANT]),
        "store topic 'dup.topic' of addon 'one' is expected but not linked: its registration was \
         dropped from the build (crate erp-addon-one)"
    );
    assert_eq!(
        err(&[&T_ONE], &[], &[WANT]),
        "the store schema part of addon 'one' is expected but not linked: its registration was \
         dropped from the build (crate erp-addon-one)"
    );
    TopicRegistry::from_parts(&[&T_ONE], &[&PART_A], &[WANT]).unwrap();
    let linked = TopicRegistry::linked().unwrap();
    assert!(linked.topic("schema").is_some());
    assert!(linked.topic("messaging.self").is_some());
}

/// negative `wire` is only asserted to be refused with 422, and a string item is covered by 008)
#[tokio::test]
async fn request_shape_errors() {
    let h = harness().await;
    let r = fetch(&h, r#"{"topics": [{"name": "schema"}]}"#).await;
    assert_error(
        &r,
        422,
        "werkzeug.exceptions.UnprocessableEntity",
        "missing a required argument: 'wire'",
    );
    let r = fetch(&h, r#"{"wire": 1}"#).await;
    assert_error(
        &r,
        422,
        "werkzeug.exceptions.UnprocessableEntity",
        "missing a required argument: 'topics'",
    );
    for wire in ["\"1\"", "-1"] {
        let body = format!(r#"{{"wire": {wire}, "topics": [{{"name": "schema"}}]}}"#);
        let r = fetch(&h, &body).await;
        assert_eq!(r.status(), 422, "{}", String::from_utf8_lossy(r.body()));
    }
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn two_nodes_answer_the_same() {
    let h = harness_with_policy(&ADDONS, policy(), RpcConfig::default(), true).await;
    give_partners(&h).await;
    let second_orm = Orm::builder(
        Arc::clone(h.harness().registry()),
        h.harness().db().pool().clone(),
    )
    .policy(policy().into_policy_provider().unwrap())
    .build()
    .unwrap();
    let second = RpcService::new(second_orm, FixtureHost, RpcConfig::default());
    let on_second = |body: String| async {
        second
            .call(erp_rpc::RpcCall::json2(PATH, body.into_bytes(), alice()))
            .await
    };
    let body = topics(&["schema", "messaging.self", "test.count"]);
    let a = ok(&fetch(&h, &body).await);
    sql_batch(
        &h,
        "INSERT INTO res_partner (id, name) VALUES (501, 'between')",
    )
    .await;
    let b = ok(&on_second(body).await);
    assert_eq!(slot(&a, 0)["data"], slot(&b, 0)["data"]);
    assert_eq!(a["store"]["schema"], b["store"]["schema"]);
    assert_eq!(a["store"]["records"], b["store"]["records"]);
    assert_eq!(
        slot(&b, 2)["data"]["count"].as_i64().unwrap(),
        slot(&a, 2)["data"]["count"].as_i64().unwrap() + 1
    );
    let xmax = |x: &Json| -> u64 {
        x["store"]["asof"]["xmax"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap()
    };
    assert!(xmax(&b) >= xmax(&a));
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn topics_staging_the_same_record_agree_or_the_call_fails() {
    let h = harness().await;
    // equal values from two topics are applied once
    let answer = ok(&fetch(&h, &topics(&["test.stage_true", "test.stage_true_too"])).await);
    assert_eq!(rec(&answer, 7)["share"], true);
    assert_eq!(slot(&answer, 1)["ok"], true);
    // different values for one field
    let r = fetch(&h, &topics(&["test.stage_true", "test.stage_false"])).await;
    assert_error(
        &r,
        500,
        "erp.internal",
        "store topics 'test.stage_true' and 'test.stage_false' stage different values for \
         messaging.self.7.share",
    );
    // a set, then a delete of the same record
    let r = fetch(&h, &topics(&["test.stage_true", "test.stage_delete"])).await;
    assert_error(
        &r,
        500,
        "erp.internal",
        "store topic 'test.stage_delete' deletes messaging.self.7 but store topic \
         'test.stage_true' sets its field 'share'",
    );
    // a delete, then a set
    let r = fetch(&h, &topics(&["test.stage_delete", "test.stage_true"])).await;
    assert_error(
        &r,
        500,
        "erp.internal",
        "store topic 'test.stage_delete' deletes messaging.self.7 but store topic \
         'test.stage_true' sets its field 'share'",
    );
    // a repeated delete is one delete
    let answer = ok(&fetch(&h, &topics(&["test.stage_delete", "test.stage_delete"])).await);
    let entries = answer["store"]["records"]["messaging.self"]
        .as_array()
        .unwrap();
    assert_eq!(entries, &vec![json!({"delete": true, "id": 7})]);
    h.cleanup().await.unwrap();
}

/// checks; the depth and shape cases are `params_size_depth_and_structure_are_bounded`)
#[tokio::test]
async fn the_params_bound_holds_at_its_edge_and_structure_is_checked_first() {
    let h = harness().await;
    let with = |n: usize| json!({"t": "x".repeat(n)});
    let base = with(0).to_string().len();
    for (len, accepted) in [(4096 - base, true), (4097 - base, false)] {
        let body =
            json!({"wire": 1, "topics": [{"name": "test.big", "params": with(len)}]}).to_string();
        let r = fetch(&h, &body).await;
        if accepted {
            assert_eq!(ok(&r)["topics"][0]["data"]["len"], len);
        } else {
            assert_error(
                &r,
                422,
                "erp.exceptions.ValidationError",
                "store topic 'test.big': params are 4097 bytes; at most 4096 are accepted",
            );
        }
    }
    // an unknown topic first and a structural error later: the structural error is reported
    let body = json!({"wire": 1, "topics": [{"name": "no.such"}, {"name": "schema", "id": 1}]})
        .to_string();
    assert_error(
        &fetch(&h, &body).await,
        422,
        "erp.exceptions.ValidationError",
        "store_fetch: topics[1] has an unknown key 'id'",
    );
    h.cleanup().await.unwrap();
}

/// owner-crate message; `erp serve` does not call it, see the design note section 11)
#[test]
fn the_link_root_check_names_a_missing_topic() {
    TopicRegistry::require_linked(&[]).unwrap();
    let error = TopicRegistry::require_linked(&[Expected::new("mail", &["ghost.topic"], false)])
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.message(),
        "store topic 'ghost.topic' of addon 'mail' is expected but not linked: its registration \
         was dropped from the build (crate erp-addon-mail)"
    );
}
