//! Emergency revocation on the owner connection (decision L4 of
//! ): the owner revokes API keys and sessions with its own audit row in
//! the same transaction, refuses when it does not run as the owner, and refuses when the owner
//! cannot write the audit row either. "Stripped" means the INSERT right of the runtime-like login
//! on the audit tables was revoked. Every test runs against the real table the harness installs.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod auth_support;
use auth_support::*;

use std::sync::Arc;

use erp_fields::{DateTime, ErrorKind};
use erp_orm::audit::{AuditStoreState, EmergencyReason};
use erp_orm::{
    ApiKeyId, BeginOptions, Isolation, OperationIdentity, Orm, SUPERUSER_ID, Transaction, UserId,
};
use erp_security::auth::apikey::ApiKeyHasher;
use erp_security::auth::apikey_manage;
use erp_security::auth::apikey_store::{IssueRequest, KeyKind, KeyScope, issue};
use erp_security::auth::emergency::{self, MAX_TARGETS, Target};
use erp_security::auth::secret::DatabaseSecret;
use erp_security::auth::{AuthConfig, HashPoolConfig, SessionId};
use erp_testkit::Harness;
use erp_testkit::tokio_postgres::{Client, NoTls};

/// An API key made through the real issuance path.
struct Fixture {
    id: i64,
    name: String,
    prefix: String,
    secret: String,
}

async fn owner(h: &Harness) -> Client {
    let (client, connection) = h.db().config().connect(NoTls).await.unwrap();
    tokio::spawn(connection);
    client
}

/// The tenant's key hasher, derived from its stored `database.secret` as issuance does.
async fn hasher(h: &Harness) -> ApiKeyHasher {
    let mut tx = h.begin().await.unwrap();
    let secret: String = tx
        .query(
            "SELECT value FROM ir_config_parameter WHERE key='database.secret'",
            &[],
        )
        .await
        .unwrap()[0]
        .get(0);
    tx.rollback().await.unwrap();
    ApiKeyHasher::derive(&DatabaseSecret::from_stored(&secret).unwrap()).unwrap()
}

/// A key for `subject` issued by the administrator and committed.
async fn key_for(w: &World, subject: i64, name: &str) -> Fixture {
    w.h.orm().check_audit_store().await.unwrap();
    let hasher = hasher(&w.h).await;
    let mut tx = w.h.begin_as(w.admin).await.unwrap();
    let expiry = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_micros() as i64
        + 12 * 3_600_000_000;
    let issued = issue(
        &mut tx,
        &hasher,
        &IssueRequest::new(
            UserId::new(subject).unwrap(),
            name,
            KeyKind::User,
            KeyScope::Rpc,
            Some(DateTime::from_micros(expiry)),
        )
        .unwrap(),
    )
    .await
    .unwrap();
    let id = issued.id().get();
    let prefix = issued.prefix().to_owned();
    let secret = issued.into_secret().expose().to_owned();
    tx.commit().await.unwrap();
    Fixture {
        id,
        name: name.to_owned(),
        prefix,
        secret,
    }
}

fn key_id(n: i64) -> ApiKeyId {
    ApiKeyId::new(n).unwrap()
}

fn user_id(n: i64) -> UserId {
    UserId::new(n).unwrap()
}

/// The transaction `erp auth emergency-revoke` opens.
async fn owner_tx(orm: &Orm) -> Transaction {
    orm.begin_opts(
        BeginOptions::new(SUPERUSER_ID)
            .isolation(Isolation::ReadCommitted)
            .on_primary()
            .operation_identity(OperationIdentity::generate().unwrap()),
    )
    .await
    .unwrap()
}

/// Revoke on the owner ORM and commit.
async fn revoke_committed(
    orm: &Orm,
    target: &Target,
    reason: EmergencyReason,
) -> emergency::Revoked {
    let mut tx = owner_tx(orm).await;
    let revoked = emergency::revoke(&mut tx, target, reason).await.unwrap();
    tx.commit().await.unwrap();
    revoked
}

async fn key_exists(h: &Harness, id: i64) -> bool {
    owner(h)
        .await
        .query_one(
            "SELECT EXISTS (SELECT 1 FROM res_users_apikeys WHERE id = $1)",
            &[&id],
        )
        .await
        .unwrap()
        .get(0)
}

async fn session_exists(h: &Harness, id: SessionId) -> bool {
    owner(h)
        .await
        .query_one(
            "SELECT EXISTS (SELECT 1 FROM erp_session WHERE id = $1::text::uuid)",
            &[&id.to_text()],
        )
        .await
        .unwrap()
        .get(0)
}

/// One audit row that carries an emergency header.
struct EmergencyRow {
    origin: String,
    entry_kind: String,
    outcome: String,
    principal_kind: String,
    actor_kind: String,
    events: serde_json::Value,
}

/// The audit rows that carry an emergency header, oldest first.
async fn emergency_rows(h: &Harness) -> Vec<EmergencyRow> {
    owner(h)
        .await
        .query(
            "SELECT origin, entry_kind, outcome, principal_kind, actor_kind, events::text \
             FROM erp_event.ir_audit_entry \
             WHERE events @> '[{\"event\":\"emergency.revocation\"}]'::jsonb ORDER BY id",
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|row| {
            let events: String = row.get(5);
            EmergencyRow {
                origin: row.get(0),
                entry_kind: row.get(1),
                outcome: row.get(2),
                principal_kind: row.get(3),
                actor_kind: row.get(4),
                events: serde_json::from_str(&events).unwrap(),
            }
        })
        .collect()
}

/// The events of `row` named `event`.
fn events_named<'a>(events: &'a serde_json::Value, event: &str) -> Vec<&'a serde_json::Value> {
    events
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["event"] == event)
        .collect()
}

/// A login role with the DML rights of the runtime role on the public tables and the INSERT-only
/// grants on the event tables, connected as its own ORM. The harness drops the role with the
/// database. `strip` revokes its INSERT on the audit tables afterwards, and the ORM checks the
/// store as that role (`NoInsertRight`).
async fn runtime_like_orm(h: &Harness, strip: bool) -> Orm {
    let config = h.read_only_role().await.unwrap();
    let role = config.get_user().unwrap().to_owned();
    let quoted = erp_sql::quote(&role);
    let client = owner(h).await;
    client
        .batch_execute(&format!(
            "GRANT INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA public TO {quoted}; \
             GRANT USAGE, SELECT, UPDATE ON ALL SEQUENCES IN SCHEMA public TO {quoted}"
        ))
        .await
        .unwrap();
    let mut grant = h.begin().await.unwrap();
    grant.grant_event_runtime_role(&role).await.unwrap();
    grant.commit().await.unwrap();
    if strip {
        client
            .batch_execute(&format!(
                "REVOKE INSERT ON ALL TABLES IN SCHEMA erp_event FROM {quoted}"
            ))
            .await
            .unwrap();
    }
    let orm = Orm::builder(
        Arc::clone(h.registry()),
        erp_orm::pool::create_pool(config, 2).unwrap(),
    )
    .policy(erp_orm::StaticPolicy::AllowAll)
    .trust_caller_resolutions()
    .require_core_tables(erp_security::auth::core_tables())
    .build()
    .unwrap();
    let state = orm.check_audit_store().await.unwrap();
    if strip {
        assert_eq!(state, AuditStoreState::NoInsertRight);
    } else {
        assert_eq!(state, AuditStoreState::Ready);
    }
    orm
}

const NOT_OWNER: &str = "a later release: the configured owner connection does not own \
                         public.res_users_apikeys; emergency revocation runs only on the owner \
                         connection";

/// role cannot insert the audit row, and the key is kept.
#[tokio::test]
async fn runtime_revoke_fails_when_runtime_cannot_insert() {
    let w = world().await;
    let fixture = key_for(&w, w.admin, "runtime-revoke").await;
    let runtime = runtime_like_orm(&w.h, true).await;
    let mut tx = runtime
        .begin_opts(
            BeginOptions::new(user_id(w.admin))
                .operation_identity(OperationIdentity::generate().unwrap()),
        )
        .await
        .unwrap();
    let error = apikey_manage::revoke(&mut tx, &fixture.secret)
        .await
        .expect_err("the stripped runtime role cannot write the audit row");
    assert_eq!(error.kind(), ErrorKind::Internal);
    assert_eq!(
        error.message(),
        "a later release (unavailable): the audit store is not ready (the connected role may not insert \
         the audit columns); nothing was recorded, fix the store (run `erp audit maintain` or \
         the runtime grant publication as the owner role)"
    );
    tx.rollback().await.unwrap();
    assert!(key_exists(&w.h, fixture.id).await, "the key was kept");
    assert!(emergency_rows(&w.h).await.is_empty());
    w.h.cleanup().await.unwrap();
}

/// The owner's revocation commits with its own `cli` row (header plus one `api_key.revoked` event
/// with cause `emergency`) while the runtime role cannot write audit. The other clauses of the case
/// are the neighbouring tests.
#[tokio::test]
async fn owner_revokes_key_with_its_row_while_runtime_stripped() {
    let w = world().await;
    let fixture = key_for(&w, w.admin, "owner-revoke").await;
    let runtime = runtime_like_orm(&w.h, true).await;
    assert_eq!(
        runtime.audit_store_state(),
        AuditStoreState::NoInsertRight,
        "the runtime role cannot write audit"
    );
    let mut tx = owner_tx(w.h.orm()).await;
    let principal = tx.env().principal().kind().as_str();
    let revoked = emergency::revoke(
        &mut tx,
        &Target::keys(vec![key_id(fixture.id)]).unwrap(),
        EmergencyReason::Compromised,
    )
    .await
    .unwrap();
    assert_eq!(revoked.keys(), [key_id(fixture.id)]);
    assert!(revoked.sessions().is_empty());
    tx.commit().await.unwrap();
    assert!(!key_exists(&w.h, fixture.id).await);
    let rows = emergency_rows(&w.h).await;
    assert_eq!(rows.len(), 1, "one row for the run");
    let row = &rows[0];
    let events = &row.events;
    assert_eq!(
        (
            row.origin.as_str(),
            row.entry_kind.as_str(),
            row.outcome.as_str(),
            row.principal_kind.as_str(),
            row.actor_kind.as_str()
        ),
        ("cli", "change", "committed", principal, "system")
    );
    let header = events_named(events, "emergency.revocation");
    assert_eq!(header.len(), 1);
    assert_eq!(header[0]["reason"], EmergencyReason::Compromised.as_str());
    assert_eq!(
        (header[0]["keys"].as_u64(), header[0]["sessions"].as_u64()),
        (Some(1), Some(0))
    );
    let revoked_events = events_named(events, "api_key.revoked");
    assert_eq!(revoked_events.len(), 1);
    assert_eq!(revoked_events[0]["key_id"], fixture.id);
    assert_eq!(revoked_events[0]["owner"], w.admin);
    assert_eq!(revoked_events[0]["key_kind"], "user");
    assert_eq!(revoked_events[0]["route"], "rpc");
    assert_eq!(revoked_events[0]["cause"]["kind"], "emergency");
    w.h.cleanup().await.unwrap();
}

/// audit row, the call's result or the errors of the emergency path.
#[tokio::test]
async fn emergency_row_carries_no_credential() {
    let w = world().await;
    let fixture = key_for(&w, w.admin, "CANARY-key-name-7f3a").await;
    let spare = key_for(&w, w.admin, "CANARY-spare-name-91c2").await;
    let revoked = revoke_committed(
        w.h.orm(),
        &Target::keys(vec![key_id(fixture.id)]).unwrap(),
        EmergencyReason::Lost,
    )
    .await;
    let row_text: String = owner(&w.h)
        .await
        .query_one(
            "SELECT t::text FROM erp_event.ir_audit_entry t \
             WHERE events @> '[{\"event\":\"emergency.revocation\"}]'::jsonb",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    // the error of a refused run is checked too: an unknown key id next to a real one
    let mut tx = owner_tx(w.h.orm()).await;
    let error = emergency::revoke(
        &mut tx,
        &Target::keys(vec![key_id(spare.id), key_id(i64::from(i32::MAX))]).unwrap(),
        EmergencyReason::Lost,
    )
    .await
    .expect_err("the unknown id refuses the run");
    tx.rollback().await.unwrap();
    for text in [row_text, format!("{revoked:?}"), format!("{error:?}")] {
        for secret in [&fixture, &spare]
            .iter()
            .flat_map(|f| [&f.name, &f.prefix, &f.secret])
        {
            assert!(
                !text.contains(secret.as_str()),
                "a credential leaked: {text}"
            );
        }
    }
    w.h.cleanup().await.unwrap();
}

/// kept and no row is written.
#[tokio::test]
async fn unknown_key_id_rolls_back_whole_run() {
    let w = world().await;
    let fixture = key_for(&w, w.admin, "unknown-id").await;
    let absent = i64::from(i32::MAX);
    let mut tx = owner_tx(w.h.orm()).await;
    let error = emergency::revoke(
        &mut tx,
        &Target::keys(vec![key_id(fixture.id), key_id(absent)]).unwrap(),
        EmergencyReason::Incident,
    )
    .await
    .expect_err("an unknown id refuses the run");
    assert_eq!(error.kind(), ErrorKind::Missing);
    assert_eq!(
        error.message(),
        format!("a later release: no API key with the id(s) {absent}; nothing was revoked")
    );
    tx.rollback().await.unwrap();
    assert!(key_exists(&w.h, fixture.id).await, "the real key was kept");
    assert!(emergency_rows(&w.h).await.is_empty(), "no row was written");
    w.h.cleanup().await.unwrap();
}

/// each with its own audit row; a third call finds nothing and writes no row.
#[tokio::test]
async fn user_keys_chunked_each_chunk_audited() {
    let w = world().await;
    let subject = user_with_password(&w.h, "chunked-user", "pw").await;
    let first = key_for(&w, subject, "chunked").await;
    owner(&w.h)
        .await
        .execute(
            "INSERT INTO res_users_apikeys (name,user_id,created_by,kind,scope,digest,digest_kid,\
             prefix,models,methods,read_only,capabilities,expiration_date,create_date) \
             SELECT name,user_id,created_by,kind,scope,sha256(('bulk'||g)::text::bytea),\
             digest_kid,prefix,models,methods,read_only,capabilities,expiration_date,create_date \
             FROM res_users_apikeys, generate_series(1,599) g WHERE id=$1",
            &[&first.id],
        )
        .await
        .unwrap();
    let held: i64 = owner(&w.h)
        .await
        .query_one(
            "SELECT count(*) FROM res_users_apikeys WHERE user_id = $1",
            &[&subject],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(held, 600);
    let target = Target::user(user_id(subject), true, false).unwrap();
    let one = revoke_committed(w.h.orm(), &target, EmergencyReason::Offboarding).await;
    assert_eq!(one.keys().len(), MAX_TARGETS);
    assert!(one.key_chunk_was_full());
    let two = revoke_committed(w.h.orm(), &target, EmergencyReason::Offboarding).await;
    assert_eq!(two.keys().len(), 600 - MAX_TARGETS);
    assert!(!two.key_chunk_was_full());
    let three = revoke_committed(w.h.orm(), &target, EmergencyReason::Offboarding).await;
    assert!(three.keys().is_empty());
    let rows = emergency_rows(&w.h).await;
    assert_eq!(rows.len(), 2, "one row per chunk, none for the empty call");
    for (row, ended) in rows.iter().zip([one.keys().len(), two.keys().len()]) {
        let header = events_named(&row.events, "emergency.revocation");
        assert_eq!(header[0]["keys"].as_u64(), u64::try_from(ended).ok());
        assert_eq!(events_named(&row.events, "api_key.revoked").len(), ended);
    }
    let left: i64 = owner(&w.h)
        .await
        .query_one(
            "SELECT count(*) FROM res_users_apikeys WHERE user_id = $1",
            &[&subject],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(left, 0);
    w.h.cleanup().await.unwrap();
}

/// A login role that is not the owner, and a principal other than the core System principal, are
/// refused before any deletion for key, session and user targets; nothing is deleted and no row is
/// written. Not tested anywhere: the refusal of a superuser that is not the owner.
#[tokio::test]
async fn non_owner_role_refused_before_delete() {
    let w = world().await;
    let subject = user_with_password(&w.h, "non-owner-user", "pw").await;
    let fixture = key_for(&w, subject, "non-owner").await;
    let session = login(&w.auth, "non-owner-user", "pw")
        .await
        .unwrap()
        .session()
        .id();
    let non_owner = runtime_like_orm(&w.h, false).await;
    let targets = [
        Target::keys(vec![key_id(fixture.id)]).unwrap(),
        Target::sessions(vec![session]).unwrap(),
        Target::user(user_id(subject), true, true).unwrap(),
    ];
    for target in &targets {
        let mut tx = owner_tx(&non_owner).await;
        let error = emergency::revoke(&mut tx, target, EmergencyReason::Compromised)
            .await
            .expect_err("a role that does not own the tables is refused");
        assert_eq!(error.kind(), ErrorKind::Access);
        assert_eq!(error.message(), NOT_OWNER);
        tx.rollback().await.unwrap();
    }
    assert!(key_exists(&w.h, fixture.id).await, "nothing was deleted");
    assert!(session_exists(&w.h, session).await, "no session was ended");
    // the owner connection with a user principal is refused too
    let mut tx = w.h.begin_as(w.admin).await.unwrap();
    let error = emergency::revoke(&mut tx, &targets[0], EmergencyReason::Compromised)
        .await
        .expect_err("only the core System principal may revoke");
    assert_eq!(error.kind(), ErrorKind::Access);
    assert_eq!(
        error.message(),
        "a later release: emergency revocation requires the core System principal"
    );
    tx.rollback().await.unwrap();
    assert!(key_exists(&w.h, fixture.id).await);
    assert!(emergency_rows(&w.h).await.is_empty());
    w.h.cleanup().await.unwrap();
}

/// server publishes, gets the same refusal and the key is kept.
#[tokio::test]
async fn runtime_orm_cannot_call_emergency_revoke() {
    let w = world().await;
    let fixture = key_for(&w, w.admin, "runtime-orm").await;
    let runtime = runtime_like_orm(&w.h, false).await;
    let mut tx = owner_tx(&runtime).await;
    let error = emergency::revoke(
        &mut tx,
        &Target::keys(vec![key_id(fixture.id)]).unwrap(),
        EmergencyReason::Lost,
    )
    .await
    .expect_err("the runtime pool does not own the tables");
    assert_eq!(error.kind(), ErrorKind::Access);
    assert_eq!(error.message(), NOT_OWNER);
    tx.rollback().await.unwrap();
    assert!(key_exists(&w.h, fixture.id).await);
    w.h.cleanup().await.unwrap();
}

/// A store refusal text for `state`.
fn store_refusal(state: &str) -> String {
    format!(
        "a later release: the audit store cannot take the revocation's row on the owner \
         connection ({state}); nothing was revoked. Run 'erp audit maintain' first; there is no \
         unaudited revocation"
    )
}

/// that is not installed, and one with no partition day left, refuse before any deletion, naming
/// the state and `erp audit maintain`.
#[tokio::test]
async fn missing_or_short_store_refuses_before_any_delete() {
    let w = world().await;
    let fixture = key_for(&w, w.admin, "short-store").await;
    let c = owner(&w.h).await;
    let target = Target::keys(vec![key_id(fixture.id)]).unwrap();

    // not installed
    c.batch_execute("DROP TABLE erp_event.ir_audit_entry CASCADE")
        .await
        .unwrap();
    assert_eq!(
        w.h.orm().check_audit_store().await.unwrap(),
        AuditStoreState::NotInstalled
    );
    let mut tx = owner_tx(w.h.orm()).await;
    let before = tx.query_count();
    let error = emergency::revoke(&mut tx, &target, EmergencyReason::Incident)
        .await
        .expect_err("no table, no revocation");
    assert_eq!(error.kind(), ErrorKind::Internal);
    assert_eq!(
        error.message(),
        store_refusal("erp_event.ir_audit_entry is not installed")
    );
    assert_eq!(tx.query_count(), before, "refused before any statement");
    tx.rollback().await.unwrap();
    assert!(key_exists(&w.h, fixture.id).await);

    // installed again, but with no day left ahead of today
    let mut install = w.h.begin().await.unwrap();
    install.install_audit_schema().await.unwrap();
    install.commit().await.unwrap();
    let days: Vec<String> = c
        .query(
            "SELECT c.relname::text FROM pg_inherits i JOIN pg_class c ON c.oid = i.inhrelid \
             WHERE i.inhparent = 'erp_event.ir_audit_entry'::regclass ORDER BY 1",
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect();
    for name in &days[1..] {
        c.batch_execute(&format!("DROP TABLE erp_event.{name}"))
            .await
            .unwrap();
    }
    assert_eq!(
        w.h.orm().check_audit_store().await.unwrap(),
        AuditStoreState::PartitionWindowShort { days_left: 0 }
    );
    let mut tx = owner_tx(w.h.orm()).await;
    let error = emergency::revoke(&mut tx, &target, EmergencyReason::Incident)
        .await
        .expect_err("no day left, no revocation");
    assert_eq!(error.kind(), ErrorKind::Internal);
    assert_eq!(
        error.message(),
        store_refusal("only 0 day(s) of partitions ahead of today")
    );
    tx.rollback().await.unwrap();
    assert!(key_exists(&w.h, fixture.id).await);
    w.h.cleanup().await.unwrap();
}

/// partitions are gone when the row is written: the commit fails with the typed insertion error
/// and the key and the session are kept.
#[tokio::test]
async fn insertion_failure_at_commit_keeps_credentials() {
    let w = world().await;
    let subject = user_with_password(&w.h, "commit-user", "pw").await;
    let fixture = key_for(&w, subject, "commit-failure").await;
    let session = login(&w.auth, "commit-user", "pw")
        .await
        .unwrap()
        .session()
        .id();
    let c = owner(&w.h).await;
    // the check ran before the partitions went away: the state is still Ready
    assert_eq!(
        w.h.orm().check_audit_store().await.unwrap(),
        AuditStoreState::Ready
    );
    // today's and tomorrow's partitions (by the database's UTC date) are dropped, so the commit
    // fails whether or not UTC midnight passes between the drop and the commit
    c.batch_execute(
        "DO $$ DECLARE d int; BEGIN FOR d IN 0..1 LOOP EXECUTE format('DROP TABLE erp_event.%I', \
         'ir_audit_entry_p' || to_char((now() AT TIME ZONE 'UTC')::date + d, 'YYYYMMDD')); \
         END LOOP; END $$",
    )
    .await
    .unwrap();
    let mut tx = owner_tx(w.h.orm()).await;
    emergency::revoke(
        &mut tx,
        &Target::user(user_id(subject), true, true).unwrap(),
        EmergencyReason::Compromised,
    )
    .await
    .unwrap();
    let error = tx.commit().await.expect_err("no partition for today");
    assert_eq!(error.kind(), ErrorKind::Internal);
    assert_eq!(
        error.message(),
        "a later release (insertion): the audit INSERT failed (SQLSTATE 23514); no partition of \
         erp_event.ir_audit_entry covers the transaction's day; the event partition upkeep has \
         not run (it runs at server start and hourly; `erp audit maintain` refreshes both event \
         tables)"
    );
    assert!(key_exists(&w.h, fixture.id).await, "the key was kept");
    assert!(session_exists(&w.h, session).await, "the session was kept");
    w.h.cleanup().await.unwrap();
}

static HOOK_ADDON: erp_registry::AddonDescriptor = erp_registry::AddonDescriptor {
    name: "test_emergency_hook",
    depends: &["base"],
    exact_only: false,
};

erp_security::session_end_hook!(
    RECORD_ENDED,
    addon = "test_emergency_hook",
    run = record_ended
);

/// The hook of an addon that keeps a row per ended session: it writes what it was told.
fn record_ended<'a>(
    tx: &'a mut Transaction,
    ended: &'a erp_security::auth::SessionEnd,
) -> erp_orm::BoxFuture<'a, erp_fields::Result<()>> {
    Box::pin(async move {
        let ids: Vec<String> = ended.sessions().iter().map(|s| s.id().to_text()).collect();
        let reason = format!("{:?}", ended.reason());
        tx.execute(
            "INSERT INTO emergency_hook_probe (session, reason) \
             SELECT u, $2 FROM unnest($1::text[]::uuid[]) u",
            &[&ids, &reason],
        )
        .await?;
        Ok(())
    })
}

/// The `(session, reason)` pairs the hook of the probe addon wrote, ordered by session.
async fn probe(h: &Harness) -> Vec<(String, String)> {
    owner(h)
        .await
        .query(
            "SELECT session::text, reason FROM emergency_hook_probe ORDER BY 1",
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| (r.get::<_, String>(0), r.get::<_, String>(1)))
        .collect()
}

/// One named session, then all sessions of a user, end in the owner's transaction with their audit
/// events (one per id, then one with the count) and the linked session-end hook runs with reason
/// `Revoked`. The output note about unlinked addons is checked in the binary test; the unknown
/// session id is the last test of this file.
#[tokio::test]
async fn owner_ends_one_session_and_user_sessions_with_hooks() {
    let h = Harness::builder([
        &erp_addon_base::ADDON,
        &erp_data::model_data::ADDON,
        &HOOK_ADDON,
    ])
    .with_decls([erp_data::ir_model_data_decl()])
    .require_core_tables(erp_security::auth::core_tables())
    .identify_transactions()
    .before_schema("CREATE TABLE emergency_hook_probe (session uuid, reason text)")
    .build()
    .await
    .unwrap();
    let w = world_with(h, AuthConfig::DEFAULT, HashPoolConfig::DEFAULT).await;
    let subject = user_with_password(&w.h, "hook-user", "pw").await;
    let mut sessions = Vec::new();
    for _ in 0..3 {
        sessions.push(
            login(&w.auth, "hook-user", "pw")
                .await
                .unwrap()
                .session()
                .id(),
        );
    }
    // one named session
    let named = revoke_committed(
        w.h.orm(),
        &Target::sessions(vec![sessions[0]]).unwrap(),
        EmergencyReason::Lost,
    )
    .await;
    assert_eq!(named.sessions(), [sessions[0]]);
    assert!(!session_exists(&w.h, sessions[0]).await);
    assert!(session_exists(&w.h, sessions[1]).await);
    assert_eq!(
        probe(&w.h).await,
        [(sessions[0].to_text(), "Revoked".to_owned())],
        "the hook ran for the ended session in the same transaction"
    );
    let rows = emergency_rows(&w.h).await;
    assert_eq!(rows.len(), 1);
    let events = &rows[0].events;
    let ended = events_named(events, "session.revoked");
    assert_eq!(ended.len(), 1);
    assert_eq!(ended[0]["session"], sessions[0].to_text());
    assert_eq!(ended[0]["user"], subject);
    assert_eq!(ended[0]["cause"], "emergency");
    assert_eq!(
        events_named(events, "emergency.revocation")[0]["sessions"],
        1
    );

    // all sessions of the user
    let all = revoke_committed(
        w.h.orm(),
        &Target::user(user_id(subject), false, true).unwrap(),
        EmergencyReason::Offboarding,
    )
    .await;
    assert_eq!(all.sessions().len(), 2);
    assert!(all.keys().is_empty());
    for id in &sessions {
        assert!(!session_exists(&w.h, *id).await);
    }
    let mut expected: Vec<(String, String)> = sessions
        .iter()
        .map(|id| (id.to_text(), "Revoked".to_owned()))
        .collect();
    expected.sort();
    assert_eq!(probe(&w.h).await, expected);
    let rows = emergency_rows(&w.h).await;
    assert_eq!(rows.len(), 2);
    let events = &rows[1].events;
    let users = events_named(events, "user_sessions.revoked");
    assert_eq!(users.len(), 1);
    assert_eq!(users[0]["user"], subject);
    assert_eq!(users[0]["cause"], "emergency");
    assert_eq!(users[0]["count"], 2);
    assert!(events_named(events, "session.revoked").is_empty());
    w.h.cleanup().await.unwrap();
}

/// key stays and no row is written.
#[tokio::test]
async fn unknown_user_refuses_the_run() {
    let w = world().await;
    let fixture = key_for(&w, w.admin, "unknown-user").await;
    let absent = i64::from(i32::MAX);
    let mut tx = owner_tx(w.h.orm()).await;
    let error = emergency::revoke(
        &mut tx,
        &Target::user(user_id(absent), true, true).unwrap(),
        EmergencyReason::Offboarding,
    )
    .await
    .expect_err("an unknown user refuses the run");
    assert_eq!(error.kind(), ErrorKind::Missing);
    assert_eq!(
        error.message(),
        format!("a later release: no user with the id(s) {absent}; nothing was revoked")
    );
    tx.rollback().await.unwrap();
    assert!(key_exists(&w.h, fixture.id).await, "the real key was kept");
    assert!(emergency_rows(&w.h).await.is_empty(), "no row was written");
    w.h.cleanup().await.unwrap();
}

/// real session is restored by the rollback and no row is written.
#[tokio::test]
async fn unknown_session_id_rolls_back_whole_run() {
    let w = world().await;
    user_with_password(&w.h, "unknown-session-user", "pw").await;
    let session = login(&w.auth, "unknown-session-user", "pw")
        .await
        .unwrap()
        .session()
        .id();
    let absent = SessionId::generate().unwrap();
    let mut tx = owner_tx(w.h.orm()).await;
    let error = emergency::revoke(
        &mut tx,
        &Target::sessions(vec![session, absent]).unwrap(),
        EmergencyReason::Lost,
    )
    .await
    .expect_err("an unknown session id refuses the run");
    assert_eq!(error.kind(), ErrorKind::Missing);
    assert_eq!(
        error.message(),
        format!(
            "a later release: no session with the id(s) {}; nothing was revoked",
            absent.to_text()
        )
    );
    tx.rollback().await.unwrap();
    assert!(
        session_exists(&w.h, session).await,
        "the real session was kept"
    );
    assert!(emergency_rows(&w.h).await.is_empty(), "no row was written");
    w.h.cleanup().await.unwrap();
}
