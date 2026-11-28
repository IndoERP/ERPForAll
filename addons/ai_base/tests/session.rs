// SPDX-License-Identifier: MIT OR Apache-2.0
//! The session store (D2, D11): owner-only sessions, the append-only typed event
//! log and its grants, the caps, the one-running-turn lease, fork, the fingerprint and the
//! partition backstop (to 010, 013 to 015). All events below are
//! synthetic fixture data; no provider call happens in any of these tests.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions"
)]

use erp_addon_ai_base::session::{
    EVENT_BYTES, MESSAGE_CHARS, NewEvent, NewSession, SESSION_MAX_EVENTS, SESSION_MAX_OPEN,
    SESSION_MAX_TURNS, admit_turn, append, ensure_session_company, fingerprint, fork, open_session,
};
use erp_sdk::domain::Value as DomainValue;
use erp_sdk::{Cell, Date, DateTime, Domain, ErrorKind, Operator, RawVals, access::PrincipalParam};
use erp_testkit::{DomainPolicy, Harness, Otherwise, tokio_postgres::error::SqlState};
use serde_json::json;

const CONCEALED: &str =
    "Your access rights do not let you read 'Assistant Session' (ai.session) records.";

const SESSION_FULL: &str = "erp.ai.session_full: the session reached its event or turn limit; fork from the latest summary.";

const SESSION_LIMIT: &str =
    "erp.ai.session_limit: the open-session limit is reached; archive a session to open a new one.";

const COMPANY_CHANGED: &str = "erp.ai.session_company_changed: this session belongs to a company \
    that is no longer among your current companies; open a new session.";

const BUSY: &str = "erp.ai.busy: another assistant turn is in progress in this session.";

/// The owner-only record rule of `security/ir.access.csv` as the engine's predicate: a session is
/// readable only by the principal named in `owner_user_id` (2).
fn owned_policy() -> DomainPolicy {
    DomainPolicy::builder(Otherwise::Allow)
        .rule(
            "ai.session",
            erp_sdk::Operation::Read,
            Domain::condition(
                "owner_user_id",
                Operator::Eq,
                DomainValue::Param(PrincipalParam::Uid),
            )
            .unwrap(),
        )
        .build()
        .unwrap()
}

async fn harness() -> Harness {
    Harness::builder([&erp_addon_base::ADDON, &erp_addon_ai_base::ADDON])
        .build()
        .await
        .unwrap()
}

async fn harness_owned() -> Harness {
    Harness::builder([&erp_addon_base::ADDON, &erp_addon_ai_base::ADDON])
        .policy(owned_policy())
        .build()
        .await
        .unwrap()
}

async fn seed_company(h: &Harness, name: &str) -> i64 {
    let mut tx = h.begin().await.unwrap();
    let currencies = tx.env().records("res.currency").unwrap();
    let currency_id = match currencies
        .search(&mut tx, &Domain::constant(true))
        .await
        .unwrap()
        .ids()
        .first()
    {
        Some(id) => *id,
        None => currencies
            .create(
                &mut tx,
                vec![
                    RawVals::new()
                        .set_cell("name", Cell::Str("USD".into()))
                        .set_cell("symbol", Cell::Str("$".into())),
                ],
            )
            .await
            .unwrap()
            .ids()[0],
    };
    let company = tx
        .env()
        .records("res.company")
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str(name.into()))
                    .set_cell("currency_id", Cell::Id(currency_id)),
            ],
        )
        .await
        .unwrap()
        .ids()[0];
    tx.commit().await.unwrap();
    drop(tx);
    company
}

/// A user with a company membership, the membership table the store's admission reads.
async fn seed_user(h: &Harness, uid: i64, company: i64) {
    let mut tx = h.begin().await.unwrap();
    tx.execute(
        "INSERT INTO res_users (id, name, login, active, company_id) \
         VALUES ($1, $2, $2, true, $3)",
        &[&uid, &format!("user-{uid}"), &company],
    )
    .await
    .unwrap();
    tx.execute(
        "INSERT INTO res_company_users_rel (user_id, cid) VALUES ($1, $2)",
        &[&uid, &company],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
}

/// Set a system parameter to a small test value before the assertions run.
async fn set_parameter(h: &Harness, key: &str, value: i64) {
    let mut tx = h.begin().await.unwrap();
    tx.execute(
        "WITH updated AS (UPDATE ir_config_parameter SET value = $2 WHERE key = $1 RETURNING 1) \
         INSERT INTO ir_config_parameter (key, value) SELECT $1, $2 \
         WHERE NOT EXISTS (SELECT 1 FROM updated)",
        &[&key, &format!("{value}")],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
}

fn user_message(text: &str) -> NewEvent<'static> {
    NewEvent {
        kind: "user_message",
        version: 1,
        turn_id: None,
        payload: json!({"text": text}),
    }
}

async fn event_count(tx: &mut erp_sdk::Transaction, session: i64) -> i64 {
    tx.query(
        "SELECT count(*)::int8 FROM ai_session_event WHERE session_id = $1",
        &[&session],
    )
    .await
    .unwrap()[0]
        .get(0)
}

/// own concealed refusals; the delete surface is AI1.SESSION-RETENTION, the export surface is
/// AI1.SESSION-VIEW, and the administrator half rides on base.group_system implying
/// base.group_user, which the shipped rule's group asserts)
#[tokio::test]
async fn a_session_belongs_to_its_owner_only() {
    let h = harness_owned().await;
    let company = seed_company(&h, "Own Sessions").await;
    seed_user(&h, 7, company).await;
    seed_user(&h, 8, company).await;
    let mut tx = h.begin_as(7).await.unwrap();
    let mine = open_session(&mut tx, &NewSession::new(company))
        .await
        .unwrap();
    tx.commit().await.unwrap();

    // user 8's listings never include user 7's session, and a read by id is refused
    let mut tx = h.begin_as(8).await.unwrap();
    let sessions = tx.env().records("ai.session").unwrap();
    let listed = sessions.search(&mut tx, &Domain::TRUE).await.unwrap();
    assert!(listed.is_empty(), "user 8 listed {listed:?}");
    let error = sessions
        .browse([mine])
        .unwrap()
        .cell(&mut tx, "title")
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Access, "{error:?}");
    assert_eq!(
        error.message(),
        format!(
            "Your access rights do not let you access 'Assistant Session' (ai.session) \
                 records.\n\nRecords: ai.session({mine},), User: 8"
        )
    );
    // the store's own paths refuse a foreign session with the same concealed wording
    let error = fork(&mut tx, mine, 1).await;
    assert_eq!(
        error.as_ref().err().map(|error| error.kind()),
        Some(ErrorKind::Access),
        "fork as user 8: {error:?}"
    );
    assert_eq!(error.err().unwrap().message(), CONCEALED);
    let error = append(&mut tx, mine, &user_message("stolen"))
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Access);
    assert_eq!(error.message(), CONCEALED);
    tx.rollback().await.unwrap();
    drop(tx);

    // the owner still sees their own session
    let mut tx = h.begin_as(7).await.unwrap();
    let listed = tx
        .env()
        .records("ai.session")
        .unwrap()
        .search(&mut tx, &Domain::TRUE)
        .await
        .unwrap();
    assert_eq!(listed.ids().as_slice(), &[mine]);
    tx.rollback().await.unwrap();

    // the shipped rule: exactly one row, group_user, owner-only, without delete
    let csv = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("security/ir.access.csv"),
    )
    .unwrap();
    let row = csv
        .lines()
        .find(|line| line.starts_with("access_ai_session_user"))
        .expect("the ai.session access row");
    assert_eq!(
        row,
        "access_ai_session_user,Own assistant sessions,model_ai_session,base.group_user,cru,\"[('owner_user_id', '=', user.id)]\""
    );
    // administrators reach the rule too: base.group_system implies base.group_erp_manager,
    // which implies base.group_user, so the owner-only rule covers the administrator as well
    let xml = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("base/data/bootstrap_groups.xml"),
    )
    .unwrap();
    let record = |id: &str| {
        let start = xml
            .find(&format!("<record id=\"{id}\""))
            .unwrap_or_else(|| panic!("{id} record"));
        let end = xml[start..].find("</record>").unwrap() + start;
        &xml[start..end]
    };
    assert!(record("group_system").contains("ref('base.group_erp_manager')"));
    assert!(record("group_erp_manager").contains("ref('base.group_user')"));
    h.cleanup().await.unwrap();
}

/// archive state; the RPC list/search/resume surface is AI1.SESSION-VIEW)
#[tokio::test]
async fn session_search_data_archive_state_and_sequence_resume_are_stored() {
    let h = harness().await;
    let company = seed_company(&h, "Search Sessions").await;
    seed_user(&h, 7, company).await;
    let mut tx = h.begin_as(7).await.unwrap();
    let session = open_session(
        &mut tx,
        &NewSession {
            company,
            title: Some("Quarterly partner review".into()),
            anchor_action: None,
            anchor_model: None,
            anchor_res_id: 0,
        },
    )
    .await
    .unwrap();
    let first = append(&mut tx, session, &user_message("hello moon"))
        .await
        .unwrap();
    assert_eq!(first, 1);
    tx.commit().await.unwrap();

    let mut tx = h.begin_as(7).await.unwrap();
    // the message text is stored beside the event for search
    let text: String = tx
        .query(
            "SELECT user_text::text FROM ai_session_event WHERE session_id = $1 AND seq = 1",
            &[&session],
        )
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(text, "hello moon");
    // the title has the trigram index of the plan
    let trigram: i64 = tx
        .query(
            "SELECT count(*)::int8 FROM pg_indexes WHERE tablename = 'ai_session' \
             AND indexdef LIKE '%USING gin%' AND indexdef LIKE '%title%'",
            &[],
        )
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(trigram, 1);
    // the owner archives the session; the state persists and the events stay readable
    tx.env()
        .records("ai.session")
        .unwrap()
        .browse([session])
        .unwrap()
        .write(
            &mut tx,
            RawVals::new().set_cell("state", Cell::Str("archived".into())),
        )
        .await
        .unwrap();
    let orm_state = tx
        .env()
        .records("ai.session")
        .unwrap()
        .browse([session])
        .unwrap()
        .cell(&mut tx, "state")
        .await
        .unwrap();
    assert_eq!(orm_state, Cell::Str("archived".into()));
    assert_eq!(event_count(&mut tx, session).await, 1);
    // resuming continues the sequence
    let next = append(&mut tx, session, &user_message("welcome back"))
        .await
        .unwrap();
    assert_eq!(next, 2);
    tx.commit().await.unwrap();
    drop(tx);
    // the archived state persists
    let mut check = h.begin().await.unwrap();
    let state: String = check
        .query(
            "SELECT state::text FROM ai_session WHERE id = $1",
            &[&session],
        )
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(state, "archived");
    check.rollback().await.unwrap();
    drop(check);
    h.cleanup().await.unwrap();
}

/// record-open surface is AI1.SESSION-VIEW)
#[tokio::test]
async fn sessions_about_a_record_are_the_readers_own() {
    let h = harness_owned().await;
    let company = seed_company(&h, "Anchored Sessions").await;
    seed_user(&h, 7, company).await;
    seed_user(&h, 8, company).await;
    let anchored = NewSession {
        company,
        title: None,
        anchor_action: None,
        anchor_model: Some("res.partner".into()),
        anchor_res_id: 42,
    };
    let mut tx = h.begin_as(7).await.unwrap();
    let mine = open_session(&mut tx, &anchored).await.unwrap();
    tx.commit().await.unwrap();
    let mut tx = h.begin_as(8).await.unwrap();
    let theirs = open_session(&mut tx, &anchored).await.unwrap();
    tx.commit().await.unwrap();

    let mut tx = h.begin_as(7).await.unwrap();
    let about = tx
        .env()
        .records("ai.session")
        .unwrap()
        .search(
            &mut tx,
            &Domain::and([
                Domain::condition(
                    "anchor_model",
                    Operator::Eq,
                    DomainValue::Str("res.partner".to_owned()),
                )
                .unwrap(),
                Domain::condition("anchor_res_id", Operator::Eq, DomainValue::Int(42)).unwrap(),
            ]),
        )
        .await
        .unwrap();
    assert_eq!(about.ids().as_slice(), &[mine], "user 7 saw {about:?}");
    tx.rollback().await.unwrap();

    let mut tx = h.begin_as(8).await.unwrap();
    let about = tx
        .env()
        .records("ai.session")
        .unwrap()
        .search(
            &mut tx,
            &Domain::and([
                Domain::condition(
                    "anchor_model",
                    Operator::Eq,
                    DomainValue::Str("res.partner".to_owned()),
                )
                .unwrap(),
                Domain::condition("anchor_res_id", Operator::Eq, DomainValue::Int(42)).unwrap(),
            ]),
        )
        .await
        .unwrap();
    assert_eq!(about.ids().as_slice(), &[theirs], "user 8 saw {about:?}");
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// caller's current companies; the no-provider-request half is AI1.ASSIST-TURN)
#[tokio::test]
async fn a_changed_company_set_refuses_the_turn() {
    let h = harness().await;
    let company = seed_company(&h, "Company One").await;
    let other = seed_company(&h, "Company Two").await;
    seed_user(&h, 7, company).await;
    let mut tx = h.begin_as(7).await.unwrap();
    let session = open_session(&mut tx, &NewSession::new(company))
        .await
        .unwrap();
    let admitted = admit_turn(&mut tx, session).await;
    assert!(admitted.is_ok(), "admit failed: {admitted:?}");
    assert!(ensure_session_company(&mut tx, session).await.is_ok());
    tx.commit().await.unwrap();

    // the user's current companies no longer include the session's company
    let mut tx = h.begin().await.unwrap();
    tx.execute(
        "UPDATE res_company_users_rel SET cid = $1 WHERE user_id = 7",
        &[&other],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();

    let mut tx = h.begin_as(7).await.unwrap();
    let error = admit_turn(&mut tx, session).await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::UserError);
    assert_eq!(error.message(), COMPANY_CHANGED);
    let error = ensure_session_company(&mut tx, session).await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::UserError);
    assert_eq!(error.message(), COMPANY_CHANGED);
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// sequence continuing, and the parent is untouched; deleting the parent, the summary hiding and
/// the retired-prefix display are later slices)
#[tokio::test]
async fn a_fork_copies_events_up_to_the_fork_point() {
    let h = harness().await;
    let company = seed_company(&h, "Fork Sessions").await;
    seed_user(&h, 7, company).await;
    let mut tx = h.begin_as(7).await.unwrap();
    let session = open_session(&mut tx, &NewSession::new(company))
        .await
        .unwrap();
    for n in 1..=10 {
        let seq = append(
            &mut tx,
            session,
            &user_message(&format!("message number {n}")),
        )
        .await
        .unwrap();
        assert_eq!(seq, n);
    }
    let forked = fork(&mut tx, session, 6).await.unwrap();
    assert_ne!(forked, session);
    // the fork row names the parent and the fork point, and its sequence continues at 7
    let row = &tx
        .query(
            "SELECT parent_session_id::int8, fork_seq, last_seq, event_count, turn_count, state::text \
             FROM ai_session WHERE id = $1",
            &[&forked],
        )
        .await
        .unwrap()[0];
    assert_eq!(row.get::<_, i64>(0), session);
    assert_eq!(row.get::<_, i32>(1), 6);
    assert_eq!(row.get::<_, i32>(2), 6);
    assert_eq!(row.get::<_, i32>(3), 6);
    assert_eq!(row.get::<_, i32>(4), 0);
    assert_eq!(row.get::<_, String>(5), "open");
    // the fork holds copies of events 1 to 6 with the parent's occurred_at
    let pairs = tx
        .query(
            "SELECT seq, occurred_at FROM ai_session_event WHERE session_id = $1 ORDER BY seq",
            &[&forked],
        )
        .await
        .unwrap();
    assert_eq!(pairs.len(), 6);
    for (offset, pair) in pairs.iter().enumerate() {
        let seq: i32 = pair.get(0);
        assert_eq!(seq, offset as i32 + 1);
        let copied: DateTime = pair.get(1);
        let original: DateTime = tx
            .query(
                "SELECT occurred_at FROM ai_session_event WHERE session_id = $1 AND seq = $2",
                &[&session, &seq],
            )
            .await
            .unwrap()[0]
            .get(0);
        assert_eq!(copied, original);
    }
    // the fork's own sequence continues at 7; the parent keeps its ten events
    let next = append(&mut tx, forked, &user_message("forked onward"))
        .await
        .unwrap();
    assert_eq!(next, 7);
    assert_eq!(event_count(&mut tx, session).await, 10);
    assert_eq!(event_count(&mut tx, forked).await, 7);
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn session_caps_fail_loudly() {
    let h = harness().await;
    let company = seed_company(&h, "Capped Sessions").await;
    seed_user(&h, 7, company).await;
    set_parameter(&h, SESSION_MAX_OPEN, 1).await;
    set_parameter(&h, SESSION_MAX_EVENTS, 4).await;
    set_parameter(&h, SESSION_MAX_TURNS, 1).await;
    let mut tx = h.begin_as(7).await.unwrap();
    let session = open_session(&mut tx, &NewSession::new(company))
        .await
        .unwrap();
    let error = open_session(&mut tx, &NewSession::new(company))
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::UserError);
    assert_eq!(error.message(), SESSION_LIMIT);
    // the event cap: three messages and the turn end fit, the fifth event does not
    for n in 1..=3 {
        let seq = append(&mut tx, session, &user_message(&format!("message {n}")))
            .await
            .unwrap();
        assert_eq!(seq, n);
    }
    let ended = append(
        &mut tx,
        session,
        &NewEvent {
            kind: "turn_end",
            version: 1,
            turn_id: None,
            payload: json!({"outcome": "ok", "round_trips": 1, "tool_calls": 0}),
        },
    )
    .await
    .unwrap();
    assert_eq!(ended, 4);
    let error = append(&mut tx, session, &user_message("one too many"))
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::UserError);
    assert_eq!(error.message(), SESSION_FULL);
    assert_eq!(event_count(&mut tx, session).await, 4);
    // the turn cap: the turn count reached its limit, so no further turn is admitted
    let error = admit_turn(&mut tx, session).await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::UserError);
    assert_eq!(error.message(), SESSION_FULL);
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// grants are the sibling test)
#[tokio::test]
async fn an_unknown_kind_or_payload_version_fails_the_write_naming_it() {
    let h = harness().await;
    let company = seed_company(&h, "Typed Sessions").await;
    seed_user(&h, 7, company).await;
    let mut tx = h.begin_as(7).await.unwrap();
    let session = open_session(&mut tx, &NewSession::new(company))
        .await
        .unwrap();
    let error = append(
        &mut tx,
        session,
        &NewEvent {
            kind: "defrag",
            version: 1,
            turn_id: None,
            payload: json!({}),
        },
    )
    .await
    .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "erp.ai.event_kind_invalid: 'defrag' is not a known assistant event kind."
    );
    let error = append(
        &mut tx,
        session,
        &NewEvent {
            kind: "user_message",
            version: 2,
            turn_id: None,
            payload: json!({"text": "hi"}),
        },
    )
    .await
    .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "erp.ai.event_version_invalid: 'user_message' has no payload version 2."
    );
    let error = append(
        &mut tx,
        session,
        &NewEvent {
            kind: "user_message",
            version: 1,
            turn_id: None,
            payload: json!({"text": "hi", "extra": 1}),
        },
    )
    .await
    .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "erp.ai.event_payload_invalid: 'user_message': unknown field `extra`, expected `text`"
    );
    assert_eq!(event_count(&mut tx, session).await, 0);
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// role; the typed write refusals are the sibling test)
#[tokio::test]
async fn the_runtime_role_cannot_update_or_delete_events() {
    let h = harness().await;
    let company = seed_company(&h, "Granted Sessions").await;
    seed_user(&h, 7, company).await;
    let mut tx = h.begin_as(7).await.unwrap();
    let session = open_session(&mut tx, &NewSession::new(company))
        .await
        .unwrap();
    append(&mut tx, session, &user_message("hello"))
        .await
        .unwrap();
    tx.commit().await.unwrap();

    // a real login role narrowed to exactly the published policy (5): SELECT and
    // INSERT on the events, SELECT, INSERT and UPDATE on the sessions, nothing else anywhere
    let config = h.read_only_role().await.unwrap();
    let role = config.get_user().unwrap().to_owned();
    let mut tx = h.begin().await.unwrap();
    tx.batch(&format!(
        "REVOKE ALL ON ALL TABLES IN SCHEMA public FROM \"{role}\""
    ))
    .await
    .unwrap();
    erp_addon_ai_base::session::publish_runtime_grants(&mut tx, &role)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let (runtime, connection) = config
        .connect(erp_testkit::tokio_postgres::NoTls)
        .await
        .unwrap();
    tokio::spawn(connection);

    // INSERT works (the store's own write path)
    runtime
        .execute(
            "INSERT INTO ai_session_event (create_date, write_date, session_id, seq, \
             occurred_at, kind, payload_version, payload) \
             VALUES ((now() AT TIME ZONE 'UTC'), (now() AT TIME ZONE 'UTC'), $1, 2, \
             (now() AT TIME ZONE 'UTC'), 'assistant_message', 1, '{\"text\":\"hi\"}')",
            &[&session],
        )
        .await
        .expect("the runtime role inserts events");
    // SELECT works (the display path reads through the parent)
    let seen: i64 = runtime
        .query_one("SELECT count(*)::int8 FROM ai_session_event", &[])
        .await
        .expect("the runtime role reads events")
        .get(0);
    assert_eq!(seen, 2);
    // UPDATE and DELETE are refused by grants
    for sql in [
        "UPDATE ai_session_event SET seq = seq WHERE kind = 'user_message'",
        "DELETE FROM ai_session_event",
        "TRUNCATE ai_session_event",
    ] {
        let error = runtime.batch_execute(sql).await.expect_err(sql);
        assert_eq!(
            error.code(),
            Some(&SqlState::INSUFFICIENT_PRIVILEGE),
            "{sql}: {error:?}"
        );
    }
    // the session row: UPDATE (bookkeeping) works, DELETE is refused
    runtime
        .execute(
            "UPDATE ai_session SET last_event_at = last_event_at WHERE id = $1",
            &[&session],
        )
        .await
        .expect("the runtime role updates the session bookkeeping");
    let error = runtime
        .batch_execute("DELETE FROM ai_session")
        .await
        .expect_err("DELETE FROM ai_session");
    assert_eq!(
        error.code(),
        Some(&SqlState::INSUFFICIENT_PRIVILEGE),
        "{error:?}"
    );
    drop(runtime);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn a_tool_result_stores_references_not_values() {
    let h = harness().await;
    let company = seed_company(&h, "Reference Sessions").await;
    seed_user(&h, 7, company).await;
    let mut tx = h.begin_as(7).await.unwrap();
    let session = open_session(&mut tx, &NewSession::new(company))
        .await
        .unwrap();
    let seq = append(
        &mut tx,
        session,
        &NewEvent {
            kind: "tool_result",
            version: 1,
            turn_id: None,
            payload: json!({
                "tool": "search_records",
                "call_id": "call-1",
                "model": "res.partner",
                "ids": [101, 102, 103],
                "field_names": ["name", "email"],
                "row_count": 3,
                "truncated": false,
                "fingerprint": "a".repeat(64),
            }),
        },
    )
    .await
    .unwrap();
    assert_eq!(seq, 1);
    // a payload carrying values is refused by the schema
    let error = append(
        &mut tx,
        session,
        &NewEvent {
            kind: "tool_result",
            version: 1,
            turn_id: None,
            payload: json!({
                "tool": "search_records",
                "call_id": "call-2",
                "model": "res.partner",
                "ids": [101],
                "field_names": ["name"],
                "row_count": 1,
                "truncated": false,
                "fingerprint": "b".repeat(64),
                "values": [{"name": "Acme Traders", "email": "acme@example.com"}],
            }),
        },
    )
    .await
    .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "erp.ai.event_payload_invalid: 'tool_result': unknown field `values`, expected one of \
         `tool`, `call_id`, `model`, `ids`, `field_names`, `row_count`, `truncated`, \
         `fingerprint`, `grouped`"
    );
    // the stored row holds the references and none of the record values
    let row = &tx
        .query(
            "SELECT payload::text, source_refs::text FROM ai_session_event \
             WHERE session_id = $1 AND seq = 1",
            &[&session],
        )
        .await
        .unwrap()[0];
    let payload: String = row.get(0);
    assert!(payload.contains("res.partner"), "{payload}");
    assert!(!payload.contains("Acme"), "{payload}");
    assert!(!payload.contains("acme@example.com"), "{payload}");
    let refs: serde_json::Value = serde_json::from_str(&row.get::<_, String>(1)).unwrap();
    assert_eq!(
        refs,
        json!({"field_names": ["name", "email"], "ids": [101, 102, 103], "model": "res.partner"})
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[test]
fn the_fingerprint_is_over_ids_and_write_dates_not_values() {
    let t1 = DateTime::from_parts(Date::from_ymd(2026, 11, 20).unwrap(), 10, 0, 0, 0).unwrap();
    let t2 = DateTime::from_parts(Date::from_ymd(2026, 11, 21).unwrap(), 10, 0, 0, 0).unwrap();
    // the same ids, write dates and field names digest the same whatever their order or the
    // record values (which never enter the input)
    let first = fingerprint("res.partner", &[103, 101], &[t1, t1], &["email", "name"]).unwrap();
    let second = fingerprint("res.partner", &[101, 103], &[t1, t1], &["name", "email"]).unwrap();
    assert_eq!(first, second);
    assert_eq!(first.len(), 64);
    // a changed write_date changes the fingerprint
    let changed = fingerprint("res.partner", &[101, 103], &[t1, t2], &["name", "email"]).unwrap();
    assert_ne!(first, changed);
    // a different model or field set changes it too
    let other_fields = fingerprint("res.partner", &[101, 103], &[t1, t1], &["name"]).unwrap();
    assert_ne!(first, other_fields);
}

#[tokio::test]
async fn event_size_caps_refuse_never_truncate() {
    let h = harness().await;
    let company = seed_company(&h, "Sized Sessions").await;
    seed_user(&h, 7, company).await;
    set_parameter(&h, MESSAGE_CHARS, 10).await;
    set_parameter(&h, EVENT_BYTES, 64).await;
    let mut tx = h.begin_as(7).await.unwrap();
    let session = open_session(&mut tx, &NewSession::new(company))
        .await
        .unwrap();
    let error = append(&mut tx, session, &user_message("hello world"))
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::UserError);
    assert_eq!(
        error.message(),
        "erp.ai.event_too_large: 'user_message' exceeds 10 characters; nothing was appended."
    );
    let error = append(
        &mut tx,
        session,
        &NewEvent {
            kind: "tool_call",
            version: 1,
            turn_id: None,
            payload: json!({
                "tool": "search_records",
                "call_id": "call-big",
                "arguments": {"query": "x".repeat(200)},
            }),
        },
    )
    .await
    .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::UserError);
    assert_eq!(
        error.message(),
        "erp.ai.event_too_large: 'tool_call' exceeds 64 bytes; nothing was appended."
    );
    assert_eq!(event_count(&mut tx, session).await, 0);
    // an exactly-fitting message is stored whole
    let seq = append(&mut tx, session, &user_message("0123456789"))
        .await
        .unwrap();
    assert_eq!(seq, 1);
    let stored: String = tx
        .query(
            "SELECT user_text::text FROM ai_session_event WHERE session_id = $1 AND seq = 1",
            &[&session],
        )
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(stored, "0123456789");
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// Append in its own transaction, retrying a serialization failure like the turn's unit of work
/// does: a concurrent append can make the other's sequence update fail once.
async fn append_in_a_turn(h: &Harness, uid: i64, session: i64, text: &'static str) -> i32 {
    for _ in 0..3 {
        let mut tx = h.begin_as(uid).await.unwrap();
        match append(&mut tx, session, &user_message(text)).await {
            Ok(seq) => {
                tx.commit().await.unwrap();
                drop(tx);
                return seq;
            }
            Err(error) if error.kind() == ErrorKind::Concurrency => {
                tx.rollback().await.unwrap();
                drop(tx);
                continue;
            }
            Err(error) => panic!("append failed: {error:?}"),
        }
    }
    panic!("append kept serializing");
}

/// gapless order on one database; the two-process SCALE fixture and the HTTP 429 mapping are
/// AI1.ASSIST-TURN)
#[tokio::test]
async fn concurrent_appends_keep_one_order_and_the_lease_admits_one_turn() {
    let h = harness().await;
    let company = seed_company(&h, "Concurrent Sessions").await;
    seed_user(&h, 7, company).await;
    let mut tx = h.begin_as(7).await.unwrap();
    let session = open_session(&mut tx, &NewSession::new(company))
        .await
        .unwrap();
    tx.commit().await.unwrap();

    // one running turn: the second admission answers busy while the first turn's transaction
    // holds the lease; when it ends, the lease goes with it and the session is free again
    let mut holder = h.begin_as(7).await.unwrap();
    admit_turn(&mut holder, session).await.unwrap();
    let mut second = h.begin_as(7).await.unwrap();
    let error = admit_turn(&mut second, session).await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::UserError);
    assert_eq!(error.message(), BUSY);
    holder.commit().await.unwrap();
    drop(holder);
    second.rollback().await.unwrap();
    drop(second);
    let mut again = h.begin_as(7).await.unwrap();
    admit_turn(&mut again, session).await.unwrap();
    again.rollback().await.unwrap();
    drop(again);

    // two concurrent appends serialize on the session row and take consecutive sequences
    let (ra, rb) = tokio::join!(
        append_in_a_turn(&h, 7, session, "from a"),
        append_in_a_turn(&h, 7, session, "from b"),
    );
    let (ra, rb) = (ra, rb);
    assert_eq!(ra.min(rb), 1);
    assert_eq!(ra.max(rb), 2);
    assert_ne!(ra, rb);
    let mut check = h.begin().await.unwrap();
    let seqs = check
        .query(
            "SELECT seq FROM ai_session_event WHERE session_id = $1 ORDER BY seq",
            &[&session],
        )
        .await
        .unwrap();
    let seqs: Vec<i32> = seqs.iter().map(|row| row.get(0)).collect();
    assert_eq!(seqs, vec![1, 2]);
    let counters = &check
        .query(
            "SELECT last_seq, event_count FROM ai_session WHERE id = $1",
            &[&session],
        )
        .await
        .unwrap()[0];
    assert_eq!(counters.get::<_, i32>(0), 2);
    assert_eq!(counters.get::<_, i32>(1), 2);
    check.rollback().await.unwrap();
    drop(check);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn a_missing_partition_fails_the_insert_and_upkeep_recovers() {
    let h = harness().await;
    let company = seed_company(&h, "Partitioned Sessions").await;
    seed_user(&h, 7, company).await;
    let mut tx = h.begin_as(7).await.unwrap();
    let session = open_session(&mut tx, &NewSession::new(company))
        .await
        .unwrap();
    append(&mut tx, session, &user_message("before"))
        .await
        .unwrap();
    tx.commit().await.unwrap();

    // drop the current week's partition
    let mut tx = h.begin().await.unwrap();
    let current: String = tx
        .query(
            "SELECT 'ai_session_event_p' || to_char(date_trunc('week', now() AT TIME ZONE 'UTC'), 'YYYYMMDD')",
            &[],
        )
        .await
        .unwrap()[0]
        .get(0);
    tx.batch(&format!("DROP TABLE \"{current}\""))
        .await
        .unwrap();
    tx.commit().await.unwrap();

    // the append fails naming the table, and nothing is appended
    let mut tx = h.begin_as(7).await.unwrap();
    let error = append(&mut tx, session, &user_message("during"))
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Database);
    assert!(
        error
            .message()
            .starts_with("no partition of 'ai.session.event' accepts create_date"),
        "the refusal must name the table: {error:?}"
    );
    tx.rollback().await.unwrap();
    drop(tx);
    let mut count = h.begin().await.unwrap();
    // the dropped partition took the earlier event with it; the refused append added nothing
    assert_eq!(event_count(&mut count, session).await, 0);
    count.rollback().await.unwrap();
    drop(count);

    // the core partition maintenance recreates the window; the append then succeeds
    let now: DateTime = h
        .begin()
        .await
        .unwrap()
        .query("SELECT (now() AT TIME ZONE 'UTC')::timestamp", &[])
        .await
        .unwrap()[0]
        .get(0);
    let report = h
        .orm()
        .maintain_partitions(h.db().pool(), now)
        .await
        .unwrap();
    assert!(report.is_complete(), "{report:?}");
    let mut tx = h.begin_as(7).await.unwrap();
    let seq = append(&mut tx, session, &user_message("after"))
        .await
        .unwrap();
    assert_eq!(seq, 2);
    tx.commit().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn private_operation_payload_is_immutable_and_consumed_with_the_business_transaction() {
    use erp_addon_ai_base::operation;
    let h = harness().await;
    let company = seed_company(&h, "Action receipts").await;
    seed_user(&h, 7, company).await;
    seed_user(&h, 8, company).await;
    let begin = |uid| {
        h.orm().begin_with(
            erp_sdk::UserId::new(uid).unwrap(),
            erp_sdk::Context::new().with_company_id(company),
        )
    };
    let mut tx = begin(7).await.unwrap();
    let session = open_session(&mut tx, &NewSession::new(company))
        .await
        .unwrap();
    let turn = erp_sdk::Uuid::from_bytes([5; 16]);
    let key = erp_sdk::Uuid::from_bytes([6; 16]).to_string();
    let action = "res.partner.example";
    let payload = r#"{"note":"private input"}"#;
    operation::prepare(&mut tx, session, turn, &key, action, payload)
        .await
        .unwrap();
    operation::prepare(&mut tx, session, turn, &key, action, payload)
        .await
        .unwrap();
    assert!(
        operation::prepare(&mut tx, session, turn, &key, action, "{}")
            .await
            .is_err()
    );
    tx.commit().await.unwrap();
    let mut other = begin(8).await.unwrap();
    assert!(
        operation::receipts(&mut other, session, turn)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(operation::claim(&mut other, &key).await.is_err());
    other.rollback().await.unwrap();
    let mut abandoned = begin(7).await.unwrap();
    operation::claim(&mut abandoned, &key).await.unwrap();
    abandoned.rollback().await.unwrap();
    let mut done = begin(7).await.unwrap();
    operation::claim(&mut done, &key).await.unwrap();
    done.commit().await.unwrap();
    let mut replay = begin(7).await.unwrap();
    assert!(operation::claim(&mut replay, &key).await.is_err());
    let visible = operation::receipts(&mut replay, session, turn)
        .await
        .unwrap();
    assert_eq!(
        visible,
        vec![json!({"name":action,"model":"res.partner","ids":[],"status":"completed"})]
    );
    // A deleted target (such as a vacuumed transient wizard) leaves an owner-visible
    // disposition, without retaining an actionable ID or disclosing the obsolete input.
    let stale_turn = erp_sdk::Uuid::from_bytes([7; 16]);
    let stale_key = erp_sdk::Uuid::from_bytes([8; 16]).to_string();
    operation::prepare(
        &mut replay,
        session,
        stale_turn,
        &stale_key,
        action,
        r#"{"ids":[999999999],"note":"private obsolete input"}"#,
    )
    .await
    .unwrap();
    let stale = operation::receipts(&mut replay, session, stale_turn)
        .await
        .unwrap();
    assert_eq!(
        stale,
        vec![json!({"name":action,"model":"res.partner","ids":[],"status":"unavailable"})]
    );
    replay.rollback().await.unwrap();
}
