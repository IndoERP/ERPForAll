//! The audit trail's real store (stage A): the table, its fixed shape, the
//! partition window, the insert-only runtime grants verified as a real login role, the single
//! INSERT of a transaction attempt, and the refusals that fail closed. Every test runs against the
//! real table the harness installs; there is no recording sink.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use std::sync::Arc;

use erp_fields::ErrorKind;
use erp_orm::audit::{
    ApiKeyRevoked, ApiKeysCollected, AuditStoreState, AuthEvent, KeyRouteScope, PasswordChanged,
    RevokeCause,
};
use erp_orm::{
    ApiKeyId, BeginOptions, KeyKind, MethodAllow, ModelAllow, OperationIdentity, PrincipalBuilder,
    SUPERUSER_ID, Scope, Transaction, UserId,
};
use erp_test_models::base_users;
use erp_testkit::Harness;
use erp_testkit::tokio_postgres::{Client, NoTls, error::SqlState};

const TABLE: &str = "erp_event.ir_audit_entry";
const AUDIT_INSERT_PREFIX: &str = "INSERT INTO \"erp_event\".\"ir_audit_entry\"";

/// The columns a writer supplies; the other four are database defaults.
const SUPPLIED: [&str; 26] = [
    "schema_version",
    "entry_kind",
    "origin",
    "outcome",
    "error_class",
    "attempt",
    "operation_id",
    "correlation_id",
    "causation_id",
    "original_operation_id",
    "principal_kind",
    "subject_id",
    "actor_kind",
    "actor_id",
    "delegating_user_id",
    "company_ids",
    "registry_generation",
    "policy_generation",
    "model",
    "method",
    "effect",
    "target_ids",
    "args_digest",
    "redaction_version",
    "events",
    "encoded_bytes",
];
const GENERATED: [&str; 4] = ["id", "entry_id", "create_date", "xid"];

/// A minimal valid row, naming only supplied columns.
const RUNTIME_INSERT: &str = "INSERT INTO erp_event.ir_audit_entry (schema_version, entry_kind, \
    origin, outcome, attempt, operation_id, principal_kind, subject_id, actor_kind, actor_id, \
    company_ids, target_ids, events, encoded_bytes) \
    VALUES (1, 'change', 'internal', 'committed', 1, pg_catalog.uuidv7(), 'user', 1, 'user', 1, \
    '{}', '[]', '[]', 0)";

async fn harness() -> Harness {
    Harness::builder([&base_users::ADDON])
        .before_schema("CREATE TABLE audit_probe (n integer)")
        .build()
        .await
        .unwrap()
}

fn options() -> BeginOptions {
    BeginOptions::new(SUPERUSER_ID).operation_identity(OperationIdentity::generate().unwrap())
}

async fn begin(h: &Harness) -> Transaction {
    h.orm().begin_opts(options()).await.unwrap()
}

/// An owner connection (the owner role holds every right on the audit table).
async fn owner(h: &Harness) -> Client {
    let (client, connection) = h.db().config().connect(NoTls).await.unwrap();
    tokio::spawn(connection);
    client
}

async fn rows(h: &Harness) -> i64 {
    owner(h)
        .await
        .query_one(&format!("SELECT count(*) FROM {TABLE}"), &[])
        .await
        .unwrap()
        .get(0)
}

async fn probe(h: &Harness) -> i64 {
    owner(h)
        .await
        .query_one("SELECT count(*) FROM audit_probe", &[])
        .await
        .unwrap()
        .get(0)
}

fn audit_inserts(tx: &Transaction) -> usize {
    tx.captured()
        .iter()
        .filter(|s| s.starts_with(AUDIT_INSERT_PREFIX))
        .count()
}

fn user(n: i64) -> UserId {
    UserId::new(n).unwrap()
}

fn key(n: i64) -> ApiKeyId {
    ApiKeyId::new(n).unwrap()
}

fn revoked(n: i64) -> AuthEvent {
    AuthEvent::ApiKeyRevoked(ApiKeyRevoked::new(
        key(n),
        user(7),
        KeyKind::User,
        KeyRouteScope::Rpc,
        RevokeCause::Owner,
    ))
}

#[track_caller]
fn refused(error: &erp_fields::Error, code: &str) {
    assert_eq!(error.kind(), ErrorKind::Programming, "{error:?}");
    assert!(
        error.message().contains(&format!("({code})")),
        "expected refusal ({code}), got: {}",
        error.message()
    );
}

async fn login(h: &Harness, role: &str, password: &str) -> Client {
    let mut config = h.db().config();
    config.user(role).password(password);
    let (client, connection) = config.connect(NoTls).await.unwrap();
    tokio::spawn(connection);
    client
}

/// A login role granted through `grant_event_runtime_role`, connected.
async fn runtime_role(h: &Harness) -> Client {
    let config = h.read_only_role().await.unwrap();
    let role = config.get_user().unwrap().to_owned();
    let password = String::from_utf8(config.get_password().unwrap().to_vec()).unwrap();
    let mut tx = h.begin().await.unwrap();
    tx.grant_event_runtime_role(&role).await.unwrap();
    tx.commit().await.unwrap();
    login(h, &role, &password).await
}

async fn partitions(client: &Client) -> Vec<String> {
    client
        .query(
            "SELECT c.relname::text FROM pg_inherits i JOIN pg_class c ON c.oid = i.inhrelid \
             WHERE i.inhparent = 'erp_event.ir_audit_entry'::regclass ORDER BY 1",
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect()
}

/// partitions and no default partition; a second install changes nothing; the upkeep adds only the
/// missing day.
#[tokio::test]
async fn install_makes_thirty_two_partitions_and_is_idempotent() {
    let h = harness().await;
    let c = owner(&h).await;
    let expected: Vec<String> = c
        .query(
            "SELECT 'ir_audit_entry_p' || to_char((now() AT TIME ZONE 'UTC')::date + g, 'YYYYMMDD') \
             FROM generate_series(0, 31) g ORDER BY 1",
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect();
    assert_eq!(expected.len(), 32);
    assert_eq!(partitions(&c).await, expected);
    let defaults: i64 = c
        .query_one(
            "SELECT count(*) FROM pg_inherits i JOIN pg_class c ON c.oid = i.inhrelid \
             WHERE i.inhparent = 'erp_event.ir_audit_entry'::regclass \
             AND pg_get_expr(c.relpartbound, c.oid) = 'DEFAULT'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(defaults, 0);
    let mut tx = h.begin().await.unwrap();
    let again = tx.install_audit_schema().await.unwrap();
    assert!(!again.created_table);
    assert!(again.created_partitions.is_empty());
    assert_eq!(again.existing_partitions, expected);
    let lost = expected[9].clone();
    tx.batch(&format!("DROP TABLE \"erp_event\".\"{lost}\""))
        .await
        .unwrap();
    assert_eq!(tx.ensure_audit_partitions().await.unwrap(), vec![lost]);
    let attached: i64 = tx
        .query(
            "SELECT count(*) FROM pg_inherits WHERE inhparent = 'erp_event.ir_audit_entry'::regclass",
            &[],
        )
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(attached, 32);
    h.cleanup().await.unwrap();
}

/// Run `forge` on the installed table, then install again: it must be refused with `code`.
async fn forged(forge: &str, code: &str) {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    tx.install_audit_schema().await.unwrap();
    tx.batch(forge)
        .await
        .unwrap_or_else(|e| panic!("{forge}: {e:?}"));
    let error = tx
        .install_audit_schema()
        .await
        .expect_err("a forged object must be refused");
    refused(&error, code);
    h.cleanup().await.unwrap();
}

/// default, type, nullability, columns, constraints (including the three CHECKs), indexes,
/// sequence and partitions.
#[tokio::test]
async fn forged_audit_shapes_are_refused() {
    let t = "erp_event.ir_audit_entry";
    forged(
        &format!("ALTER TABLE {t} ALTER COLUMN create_date DROP DEFAULT"),
        "forged-default",
    )
    .await;
    forged(
        &format!("ALTER TABLE {t} ALTER COLUMN id SET DEFAULT 7"),
        "forged-default",
    )
    .await;
    forged(
        &format!(
            "ALTER TABLE {t} ALTER COLUMN xid DROP DEFAULT, ALTER COLUMN xid TYPE bigint USING 0"
        ),
        "forged-type",
    )
    .await;
    forged(
        &format!("ALTER TABLE {t} ALTER COLUMN origin DROP NOT NULL"),
        "forged-nullability",
    )
    .await;
    forged(
        &format!("ALTER TABLE {t} ADD COLUMN extra text"),
        "forged-column",
    )
    .await;
    forged(
        &format!("ALTER TABLE {t} DROP COLUMN method"),
        "forged-column",
    )
    .await;
    forged(
        &format!("ALTER TABLE {t} ADD CONSTRAINT forged CHECK (attempt > 0)"),
        "forged-constraint",
    )
    .await;
    forged(
        &format!("ALTER TABLE {t} DROP CONSTRAINT ir_audit_entry_kind_check"),
        "forged-constraint",
    )
    .await;
    forged(
        &format!(
            "ALTER TABLE {t} DROP CONSTRAINT ir_audit_entry_subject_check, \
             ADD CONSTRAINT ir_audit_entry_subject_check CHECK (true)"
        ),
        "forged-constraint",
    )
    .await;
    forged(
        &format!("CREATE TABLE {t}_pdefault PARTITION OF {t} DEFAULT"),
        "default-partition",
    )
    .await;
    forged(
        &format!(
            "CREATE TABLE {t}_p20200101 PARTITION OF {t} FOR VALUES FROM ('2020-01-01 00:00:00+00') \
             TO ('2020-01-03 00:00:00+00')"
        ),
        "forged-partition",
    )
    .await;
    forged(
        "DROP INDEX erp_event.ir_audit_entry_correlation_idx",
        "forged-index",
    )
    .await;
    forged(
        "DROP INDEX erp_event.ir_audit_entry_cursor_idx",
        "forged-index",
    )
    .await;
    forged(
        "ALTER SEQUENCE erp_event.ir_audit_entry_id_seq CYCLE",
        "forged-sequence",
    )
    .await;
}

/// namespace without touching each other.
#[tokio::test]
async fn audit_and_outbox_installs_are_independent() {
    let h = harness().await;
    let c = owner(&h).await;
    let outbox: bool = c
        .query_one(
            "SELECT to_regclass('erp_event.ir_event_outbox') IS NOT NULL",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert!(!outbox, "the harness installs only the audit table");
    let mut tx = h.begin().await.unwrap();
    let report = tx.install_event_schema().await.unwrap();
    assert!(report.created_table);
    tx.commit().await.unwrap();
    let both: i64 = c
        .query_one(
            "SELECT count(*) FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = 'erp_event' AND c.relname IN ('ir_event_outbox', 'ir_audit_entry')",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(both, 2);
    h.cleanup().await.unwrap();
}

/// nothing else: no generated column, no SELECT, UPDATE, DELETE or TRUNCATE on the parent or a
/// partition, no `setval`, and the effective rights say the same.
#[tokio::test]
async fn runtime_role_is_insert_only_on_the_supplied_columns() {
    let h = harness().await;
    let runtime = runtime_role(&h).await;
    runtime.batch_execute(RUNTIME_INSERT).await.unwrap();
    assert_eq!(rows(&h).await, 1);
    // generated columns are refused with 42501
    for column in GENERATED {
        let value = match column {
            "id" => "99",
            "entry_id" => "pg_catalog.uuidv7()",
            "create_date" => "now() - interval '40 days'",
            _ => "pg_catalog.pg_current_xact_id()",
        };
        let sql = RUNTIME_INSERT
            .replace("(schema_version,", &format!("({column}, schema_version,"))
            .replace("VALUES (1,", &format!("VALUES ({value}, 1,"));
        let error = runtime.batch_execute(&sql).await.expect_err(column);
        assert_eq!(
            error.code(),
            Some(&SqlState::INSUFFICIENT_PRIVILEGE),
            "{column}: {error:?}"
        );
    }
    let partition = partitions(&owner(&h).await).await.remove(0);
    for target in [TABLE.to_owned(), format!("erp_event.{partition}")] {
        for sql in [
            format!("SELECT count(*) FROM {target}"),
            format!("SELECT id FROM {target}"),
            format!("UPDATE {target} SET outcome = 'failed'"),
            format!("DELETE FROM {target}"),
            format!("TRUNCATE {target}"),
        ] {
            let error = runtime.batch_execute(&sql).await.expect_err(&sql);
            assert_eq!(
                error.code(),
                Some(&SqlState::INSUFFICIENT_PRIVILEGE),
                "{sql}: {error:?}"
            );
        }
    }
    let error = runtime
        .batch_execute("SELECT setval('erp_event.ir_audit_entry_id_seq', 1000000)")
        .await
        .expect_err("setval");
    assert_eq!(error.code(), Some(&SqlState::INSUFFICIENT_PRIVILEGE));
    // effective rights as the role: exactly the supplied columns may be inserted
    let columns = runtime
        .query(
            "SELECT a.attname::text, has_column_privilege('erp_event.ir_audit_entry', a.attnum, 'INSERT'), \
             has_column_privilege('erp_event.ir_audit_entry', a.attnum, 'SELECT'), \
             has_column_privilege('erp_event.ir_audit_entry', a.attnum, 'UPDATE') \
             FROM pg_attribute a WHERE a.attrelid = 'erp_event.ir_audit_entry'::regclass \
             AND a.attnum > 0 AND NOT a.attisdropped ORDER BY a.attnum",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(columns.len(), SUPPLIED.len() + GENERATED.len());
    for row in &columns {
        let name: String = row.get(0);
        assert_eq!(
            row.get::<_, bool>(1),
            SUPPLIED.contains(&name.as_str()),
            "INSERT on {name}"
        );
        assert!(!row.get::<_, bool>(2), "SELECT on {name}");
        assert!(!row.get::<_, bool>(3), "UPDATE on {name}");
    }
    h.cleanup().await.unwrap();
}

/// insert-only with its sequence.
#[tokio::test]
async fn grant_covers_both_installed_event_tables() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    tx.install_event_schema().await.unwrap();
    tx.commit().await.unwrap();
    let runtime = runtime_role(&h).await;
    for (table, sequence) in [
        (
            "erp_event.ir_audit_entry",
            "erp_event.ir_audit_entry_id_seq",
        ),
        (
            "erp_event.ir_event_outbox",
            "erp_event.ir_event_outbox_id_seq",
        ),
    ] {
        let exact: bool = runtime
            .query_one(
                &format!(
                    "SELECT has_any_column_privilege('{table}', 'INSERT') \
                     AND NOT has_table_privilege('{table}', 'SELECT') \
                     AND NOT has_any_column_privilege('{table}', 'SELECT') \
                     AND NOT has_table_privilege('{table}', 'UPDATE') \
                     AND has_sequence_privilege('{sequence}', 'USAGE') \
                     AND NOT has_sequence_privilege('{sequence}', 'UPDATE')"
                ),
                &[],
            )
            .await
            .unwrap()
            .get(0);
        assert!(exact, "{table}");
    }
    h.cleanup().await.unwrap();
}

/// attribution of the transaction's environment; three events cost one INSERT; a rolled back
/// savepoint drops its event; a rollback leaves nothing.
#[tokio::test]
async fn auth_events_commit_with_the_change_in_one_insert() {
    let h = harness().await;
    let mut tx = begin(&h).await;
    tx.capture_statements(true);
    tx.execute("INSERT INTO audit_probe (n) VALUES (1)", &[])
        .await
        .unwrap();
    tx.audit_auth(revoked(91)).unwrap();
    tx.audit_auth(AuthEvent::PasswordChanged(PasswordChanged::new(
        user(7),
        user(7),
    )))
    .unwrap();
    tx.audit_auth(AuthEvent::ApiKeysCollected(
        ApiKeysCollected::new(vec![key(5), key(6)]).unwrap(),
    ))
    .unwrap();
    // an event under a savepoint that rolls back describes a change that did not happen
    let outcome = tx
        .savepoint(async |tx| {
            tx.audit_auth(revoked(92))?;
            Err::<(), _>(erp_fields::Error::validation("undo"))
        })
        .await;
    assert!(outcome.is_err());
    let env = tx.env();
    let identity = env.operation_identity().unwrap().clone();
    let kind = env.principal().kind().as_str();
    let subject = env.principal().subject().get();
    tx.commit().await.unwrap();
    assert_eq!(audit_inserts(&tx), 1, "three events, one INSERT");
    assert_eq!(rows(&h).await, 1);
    assert_eq!(probe(&h).await, 1);
    let row = owner(&h)
        .await
        .query_one(
            "SELECT entry_kind, origin, outcome, attempt, operation_id::text, correlation_id::text, \
             principal_kind, subject_id, actor_kind, actor_id, jsonb_array_length(events), \
             events->0->>'event', events->1->>'event', events->2->>'event', create_date IS NOT NULL, \
             xid IS NOT NULL, entry_id IS NOT NULL, encoded_bytes \
             FROM erp_event.ir_audit_entry",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(row.get::<_, String>(0), "change");
    assert_eq!(row.get::<_, String>(1), "internal");
    assert_eq!(row.get::<_, String>(2), "committed");
    assert_eq!(row.get::<_, i16>(3), 1);
    assert_eq!(row.get::<_, String>(4), identity.operation().as_str());
    assert_eq!(row.get::<_, String>(5), identity.correlation().as_str());
    assert_eq!(row.get::<_, String>(6), kind);
    assert_eq!(row.get::<_, i64>(7), subject);
    assert_eq!(row.get::<_, String>(8), "system");
    assert_eq!(row.get::<_, i64>(9), subject);
    assert_eq!(row.get::<_, i32>(10), 3, "the rolled back event is gone");
    assert_eq!(row.get::<_, String>(11), "api_key.revoked");
    assert_eq!(row.get::<_, String>(12), "password.changed");
    assert_eq!(row.get::<_, String>(13), "api_keys.collected");
    assert!(row.get::<_, bool>(14) && row.get::<_, bool>(15) && row.get::<_, bool>(16));
    assert!(row.get::<_, i32>(17) > 0);

    // a rollback writes no row and no change
    let mut tx = begin(&h).await;
    tx.execute("INSERT INTO audit_probe (n) VALUES (2)", &[])
        .await
        .unwrap();
    tx.audit_auth(revoked(93)).unwrap();
    tx.rollback().await.unwrap();
    assert_eq!(rows(&h).await, 1);
    assert_eq!(probe(&h).await, 1);

    // a transaction with nothing to say writes no audit statement
    let mut tx = begin(&h).await;
    tx.capture_statements(true);
    tx.execute("INSERT INTO audit_probe (n) VALUES (3)", &[])
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(audit_inserts(&tx), 0);
    assert_eq!(rows(&h).await, 1);
    h.cleanup().await.unwrap();
}

/// the audit row: the principal carries it, the attribution columns do not.
#[tokio::test]
async fn the_api_key_name_of_the_principal_is_not_recorded() {
    let h = harness().await;
    let scope = Scope::builder(key(41), "CANARY-key-name-7f3a")
        .models(ModelAllow::All)
        .methods(MethodAllow::All)
        .build()
        .unwrap();
    let principal = PrincipalBuilder::user(user(5))
        .scope(scope)
        .build()
        .unwrap();
    let mut tx = h
        .orm()
        .begin_opts(
            BeginOptions::for_principal(principal)
                .operation_identity(OperationIdentity::generate().unwrap()),
        )
        .await
        .unwrap();
    tx.audit_auth(revoked(94)).unwrap();
    tx.commit().await.unwrap();
    let row = owner(&h)
        .await
        .query_one(
            "SELECT actor_kind, actor_id, subject_id, t::text FROM erp_event.ir_audit_entry t",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(row.get::<_, String>(0), "api_key_user");
    assert_eq!(row.get::<_, i64>(1), 41);
    assert_eq!(row.get::<_, i64>(2), 5);
    let text: String = row.get(3);
    assert!(!text.contains("CANARY"), "{text}");
    h.cleanup().await.unwrap();
}

/// the typed error naming the SQLSTATE, and the business change of the same transaction is absent.
#[tokio::test]
async fn insertion_failure_rolls_the_business_change_back() {
    let h = harness().await;
    let today: String = owner(&h)
        .await
        .query_one(
            "SELECT 'ir_audit_entry_p' || to_char((now() AT TIME ZONE 'UTC')::date, 'YYYYMMDD')",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    owner(&h)
        .await
        .batch_execute(&format!("DROP TABLE erp_event.{today}"))
        .await
        .unwrap();
    let mut tx = begin(&h).await;
    tx.execute("INSERT INTO audit_probe (n) VALUES (1)", &[])
        .await
        .unwrap();
    tx.audit_auth(revoked(95)).unwrap();
    let error = tx.commit().await.expect_err("no partition for today");
    assert_eq!(error.kind(), ErrorKind::Internal);
    assert_eq!(
        error.message(),
        "a later release (insertion): the audit INSERT failed (SQLSTATE 23514); no partition covers \
         the row's date, run `erp audit maintain` as the owner role"
    );
    assert_eq!(probe(&h).await, 0, "the business change rolled back");
    assert_eq!(rows(&h).await, 0);
    h.cleanup().await.unwrap();
}

/// call before any statement: the statement counter does not move.
#[tokio::test]
async fn a_store_that_is_not_ready_refuses_before_any_statement() {
    let h = Harness::builder([&base_users::ADDON])
        .without_audit_store()
        .build()
        .await
        .unwrap();
    assert_eq!(h.orm().audit_store_state(), AuditStoreState::Unchecked);
    let mut tx = begin(&h).await;
    let before = tx.query_count();
    let error = tx.audit_auth(revoked(1)).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Internal);
    assert_eq!(
        error.message(),
        "a later release (unavailable): the audit store is not ready (the store was not checked yet); \
         nothing was recorded, fix the store (run `erp audit maintain` or the runtime grant \
         publication as the owner role)"
    );
    // after a check the state names what is missing
    assert_eq!(
        h.orm().check_audit_store().await.unwrap(),
        AuditStoreState::NotInstalled
    );
    let before_second = tx.query_count();
    let error = tx.audit_auth(revoked(1)).unwrap_err();
    assert_eq!(
        error.message(),
        "a later release (unavailable): the audit store is not ready (erp_event.ir_audit_entry is not \
         installed); nothing was recorded, fix the store (run `erp audit maintain` or the \
         runtime grant publication as the owner role)"
    );
    assert_eq!(tx.query_count(), before);
    assert_eq!(tx.query_count(), before_second);
    // installing it and checking again makes it ready
    let mut owner_tx = h.begin().await.unwrap();
    owner_tx.install_audit_schema().await.unwrap();
    owner_tx.commit().await.unwrap();
    assert_eq!(
        h.orm().check_audit_store().await.unwrap(),
        AuditStoreState::Ready
    );
    h.cleanup().await.unwrap();
}

/// statement.
#[tokio::test]
async fn a_read_only_transaction_refuses_lifecycle_events() {
    let h = harness().await;
    let mut tx = h
        .orm()
        .begin_opts(options().read_only("audit.probe.read"))
        .await
        .unwrap();
    let before = tx.query_count();
    let error = tx.audit_auth(revoked(1)).unwrap_err();
    refused(&error, "read-only-transaction");
    assert_eq!(
        error.message(),
        "a later release (read-only-transaction): lifecycle changes, audited superuser runs and call \
         summaries need a write transaction"
    );
    assert_eq!(tx.query_count(), before);
    h.cleanup().await.unwrap();
}

/// partition window.
#[tokio::test]
async fn the_store_check_names_what_is_wrong() {
    let h = harness().await;
    assert_eq!(
        h.orm().check_audit_store().await.unwrap(),
        AuditStoreState::Ready
    );
    // drop all but today's and tomorrow's partitions: one day left is writable but short
    let days: Vec<String> = partitions(&owner(&h).await).await;
    for name in &days[2..] {
        owner(&h)
            .await
            .batch_execute(&format!("DROP TABLE erp_event.{name}"))
            .await
            .unwrap();
    }
    let state = h.orm().check_audit_store().await.unwrap();
    assert_eq!(
        state,
        AuditStoreState::PartitionWindowShort { days_left: 1 }
    );
    assert!(state.allows_writes());
    owner(&h)
        .await
        .batch_execute(&format!("DROP TABLE erp_event.{}", days[1]))
        .await
        .unwrap();
    let state = h.orm().check_audit_store().await.unwrap();
    assert_eq!(
        state,
        AuditStoreState::PartitionWindowShort { days_left: 0 }
    );
    assert!(!state.allows_writes());
    h.cleanup().await.unwrap();
}

/// stops at the first gap: a missing day inside the window is not hidden by the partitions that
/// lie beyond it.
#[tokio::test]
async fn a_gap_in_the_partition_window_ends_it() {
    let h = harness().await;
    let days: Vec<String> = partitions(&owner(&h).await).await;
    // today, tomorrow, a gap at day +2, then the rest: the window is one day ahead, not thirty
    owner(&h)
        .await
        .batch_execute(&format!("DROP TABLE erp_event.{}", days[2]))
        .await
        .unwrap();
    let state = h.orm().check_audit_store().await.unwrap();
    assert_eq!(
        state,
        AuditStoreState::PartitionWindowShort { days_left: 1 },
        "the newest partition is day +31 but only today and tomorrow are consecutive"
    );
    // tomorrow missing too: only today is left, and the store refuses recording
    owner(&h)
        .await
        .batch_execute(&format!("DROP TABLE erp_event.{}", days[1]))
        .await
        .unwrap();
    let state = h.orm().check_audit_store().await.unwrap();
    assert_eq!(
        state,
        AuditStoreState::PartitionWindowShort { days_left: 0 }
    );
    h.cleanup().await.unwrap();
}

/// is audited per call: three calls are one entry with count 3, the cause names the addon, and the
/// returned environment is superuser mode; a superuser run without a group is refused before
/// anything is recorded.
#[tokio::test]
async fn unscoped_sudo_is_audited_per_call() {
    let h = harness().await;
    let principal = PrincipalBuilder::user(user(5)).build().unwrap();
    let mut tx = h
        .orm()
        .begin_opts(
            BeginOptions::for_principal(principal)
                .operation_identity(OperationIdentity::generate().unwrap()),
        )
        .await
        .unwrap();
    let env = tx.env();
    assert!(!env.principal().elevation().is_full());
    for _ in 0..3 {
        let elevated = tx.audited_unscoped_sudo(&env, "erp_audit_test").unwrap();
        assert!(elevated.principal().elevation().is_full());
        assert_eq!(elevated.principal().subject(), env.principal().subject());
    }
    let error = tx
        .audited_superuser_env(
            &env,
            &erp_orm::audit::SuperuserAction {
                action_id: 1,
                root_action_id: 1,
                group_ids: Box::new([]),
                flag_write_uid: None,
                flag_write_date_utc_micros: None,
            },
        )
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.message(),
        "a later release (elevation): a superuser run needs at least one group"
    );
    tx.commit().await.unwrap();
    let events: String = owner(&h)
        .await
        .query_one("SELECT events::text FROM erp_event.ir_audit_entry", &[])
        .await
        .unwrap()
        .get(0);
    let events: serde_json::Value = serde_json::from_str(&events).unwrap();
    let events = events.as_array().unwrap();
    assert_eq!(events.len(), 1, "one entry for three calls");
    assert_eq!(events[0]["reason"], "core.sudo.unscoped");
    assert_eq!(events[0]["count"], 3);
    assert_eq!(events[0]["grant"]["cause"]["kind"], "unscoped_sudo");
    assert_eq!(events[0]["grant"]["cause"]["addon"], "erp_audit_test");
    assert_eq!(events[0]["original"]["subject"], 5);
    h.cleanup().await.unwrap();
}

/// process-level `SCALE.TEST` harness is not used) concurrent audited commits from both nodes all
/// land with distinct ids and operation ids; a node that goes away before COMMIT leaves neither a
/// change nor a row; a committed row is visible from the other node at once.
/// Tags: scale
#[tokio::test]
async fn two_nodes_write_audit_rows_and_a_lost_node_leaves_nothing() {
    let h = harness().await;
    let node_a = h.orm().clone();
    let node_b = erp_orm::Orm::builder(Arc::clone(h.registry()), h.db().pool().clone())
        .build()
        .unwrap();
    assert!(node_b.check_audit_store().await.unwrap().is_ready());
    let mut tasks = Vec::new();
    for n in 0..8 {
        let orm = if n % 2 == 0 {
            node_a.clone()
        } else {
            node_b.clone()
        };
        tasks.push(tokio::spawn(async move {
            let mut tx = orm.begin_opts(options()).await.unwrap();
            tx.execute("INSERT INTO audit_probe (n) VALUES ($1)", &[&n])
                .await
                .unwrap();
            tx.audit_auth(revoked(100 + i64::from(n))).unwrap();
            tx.commit().await.unwrap();
        }));
    }
    for task in tasks {
        task.await.unwrap();
    }
    let distinct: (i64, i64, i64) = {
        let row = owner(&h)
            .await
            .query_one(
                "SELECT count(*), count(DISTINCT id), count(DISTINCT operation_id) \
                 FROM erp_event.ir_audit_entry",
                &[],
            )
            .await
            .unwrap();
        (row.get(0), row.get(1), row.get(2))
    };
    assert_eq!(distinct, (8, 8, 8));
    assert_eq!(probe(&h).await, 8);

    // a node lost before COMMIT: its change and its row are gone together
    let mut tx = node_b.begin_opts(options()).await.unwrap();
    tx.execute("INSERT INTO audit_probe (n) VALUES (99)", &[])
        .await
        .unwrap();
    tx.audit_auth(revoked(200)).unwrap();
    drop(tx);
    assert_eq!(rows(&h).await, 8);
    assert_eq!(probe(&h).await, 8);

    // a node lost after COMMIT: the other node sees the row
    let mut tx = node_a.begin_opts(options()).await.unwrap();
    tx.audit_auth(revoked(201)).unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let mut seen = node_b
        .begin(erp_orm::PrincipalHint::system())
        .await
        .unwrap();
    let count: i64 = seen
        .query("SELECT count(*) FROM erp_event.ir_audit_entry", &[])
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(count, 9);
    seen.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// failed login is an `auth` attempt row without subject, principal, login text or address, and
/// with a bounded error class.
#[tokio::test]
async fn an_unauthenticated_attempt_row_has_no_subject() {
    let h = harness().await;
    let identity = OperationIdentity::generate().unwrap();
    h.orm()
        .record_audit_attempt(erp_orm::audit::AuditAttempt::unauthenticated(
            identity.clone(),
            erp_orm::audit::ErrorClass::Access,
        ))
        .await
        .unwrap();
    let row = owner(&h)
        .await
        .query_one(
            "SELECT entry_kind, origin, outcome, error_class, subject_id, principal_kind, \
             actor_kind, actor_id, company_ids, operation_id::text, events::text, model, method \
             FROM erp_event.ir_audit_entry",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(row.get::<_, String>(0), "attempt");
    assert_eq!(row.get::<_, String>(1), "auth");
    assert_eq!(row.get::<_, String>(2), "failed");
    assert_eq!(row.get::<_, Option<String>>(3).as_deref(), Some("access"));
    assert_eq!(row.get::<_, Option<i64>>(4), None);
    assert_eq!(row.get::<_, Option<String>>(5), None);
    assert_eq!(row.get::<_, Option<String>>(6), None);
    assert_eq!(row.get::<_, Option<i64>>(7), None);
    assert!(row.get::<_, Vec<i64>>(8).is_empty());
    assert_eq!(row.get::<_, String>(9), identity.operation().as_str());
    assert_eq!(row.get::<_, String>(10), "[]");
    assert_eq!(row.get::<_, Option<String>>(11), None);
    assert_eq!(row.get::<_, Option<String>>(12), None);
    h.cleanup().await.unwrap();
}
