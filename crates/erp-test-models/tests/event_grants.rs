//! Grants on the outbox event schema (A2-D5, plan S4), all verified as a real
//! login role: the runtime role may use the schema, insert the supplied columns of the table and use
//! its id sequence, and nothing else. Also the publication check (the event term of
//! `MISSING_RUNTIME_GRANTS_SQL`) and the grant SQL of the last deployed release, which must leave the
//! event ACLs as they are.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use erp_test_models::base_users;
use erp_testkit::Harness;
use erp_testkit::tokio_postgres::{Client, NoTls, error::SqlState};

const TABLE: &str = "erp_event.ir_event_outbox";
const SEQUENCE: &str = "erp_event.ir_event_outbox_id_seq";

/// The columns a writer supplies; the other four (`id`, `event_id`, `create_date`, `xid`) are defaults.
const SUPPLIED: [&str; 16] = [
    "schema_version",
    "kind",
    "model",
    "record_ids",
    "field_names",
    "cascade_models",
    "subject",
    "principal_kind",
    "actor_kind",
    "actor_id",
    "delegating_user_id",
    "company_ids",
    "correlation_id",
    "operation_id",
    "tx_ordinal",
    "causation_id",
];
const GENERATED: [&str; 4] = ["id", "event_id", "create_date", "xid"];

const INSERT: &str = "INSERT INTO erp_event.ir_event_outbox (schema_version, kind, model, \
    record_ids, field_names, cascade_models, actor_id, company_ids, principal_kind, actor_kind, operation_id, tx_ordinal, subject) \
    VALUES (1, 'create', 'res.partner', '[1]', '{}', '{}', 1, '{1}', 'user', 'user', pg_catalog.uuidv7(), 1, '1')";

const TABLE_PRIVILEGES: [&str; 8] = [
    "SELECT",
    "INSERT",
    "UPDATE",
    "DELETE",
    "TRUNCATE",
    "REFERENCES",
    "TRIGGER",
    "MAINTAIN",
];

struct Fixture {
    h: Harness,
    role: String,
    runtime: Client,
}

async fn login(h: &Harness, role: &str, password: &str) -> Client {
    let mut config = h.db().config();
    config.user(role).password(password);
    let (client, connection) = config.connect(NoTls).await.unwrap();
    tokio::spawn(connection);
    client
}

/// A database with the event schema installed and committed, a login role (`ro_<database>`, the
/// harness's throw-away login, dropped with the database) granted through `grant_event_runtime_role`,
/// and a connection made as that role.
async fn fixture() -> Fixture {
    let h = Harness::builder([&base_users::ADDON])
        .without_audit_store()
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    tx.install_event_schema().await.unwrap();
    tx.commit().await.unwrap();
    let config = h.read_only_role().await.unwrap();
    let role = config.get_user().unwrap().to_owned();
    let password = String::from_utf8(config.get_password().unwrap().to_vec()).unwrap();
    let mut tx = h.begin().await.unwrap();
    tx.grant_event_runtime_role(&role).await.unwrap();
    tx.commit().await.unwrap();
    let runtime = login(&h, &role, &password).await;
    Fixture { h, role, runtime }
}

async fn partitions(client: &Client) -> Vec<String> {
    client
        .query(
            "SELECT c.relname::text FROM pg_inherits i JOIN pg_class c ON c.oid = i.inhrelid \
             WHERE i.inhparent = 'erp_event.ir_event_outbox'::regclass ORDER BY 1",
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect()
}

/// What the runtime role can do, from the catalogs as the role itself: effective rights, so PUBLIC
/// and inherited roles count.
async fn assert_exact_allowlist(client: &Client) {
    let one =
        |sql: String| async move { client.query_one(&sql, &[]).await.unwrap().get::<_, bool>(0) };
    assert!(one("SELECT has_schema_privilege('erp_event', 'USAGE')".into()).await);
    assert!(!one("SELECT has_schema_privilege('erp_event', 'CREATE')".into()).await);
    for privilege in TABLE_PRIVILEGES {
        assert!(
            !one(format!(
                "SELECT has_table_privilege('{TABLE}', '{privilege}')"
            ))
            .await,
            "table-level {privilege}"
        );
    }
    let columns = client
        .query(
            "SELECT a.attname::text, \
             has_column_privilege('erp_event.ir_event_outbox', a.attnum, 'INSERT'), \
             has_column_privilege('erp_event.ir_event_outbox', a.attnum, 'SELECT'), \
             has_column_privilege('erp_event.ir_event_outbox', a.attnum, 'UPDATE'), \
             has_column_privilege('erp_event.ir_event_outbox', a.attnum, 'REFERENCES') \
             FROM pg_attribute a WHERE a.attrelid = 'erp_event.ir_event_outbox'::regclass \
             AND a.attnum > 0 AND NOT a.attisdropped ORDER BY a.attnum",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(columns.len(), SUPPLIED.len() + GENERATED.len());
    for row in &columns {
        let name: String = row.get(0);
        let insert: bool = row.get(1);
        assert_eq!(
            insert,
            SUPPLIED.contains(&name.as_str()),
            "INSERT on {name}"
        );
        assert!(!row.get::<_, bool>(2), "SELECT on {name}");
        assert!(!row.get::<_, bool>(3), "UPDATE on {name}");
        assert!(!row.get::<_, bool>(4), "REFERENCES on {name}");
    }
    assert!(
        one(format!(
            "SELECT has_sequence_privilege('{SEQUENCE}', 'USAGE')"
        ))
        .await
    );
    assert!(
        !one(format!(
            "SELECT has_sequence_privilege('{SEQUENCE}', 'SELECT')"
        ))
        .await
    );
    assert!(
        !one(format!(
            "SELECT has_sequence_privilege('{SEQUENCE}', 'UPDATE')"
        ))
        .await
    );
    for partition in partitions(client).await {
        let name = format!("erp_event.{partition}");
        for privilege in TABLE_PRIVILEGES {
            assert!(
                !one(format!(
                    "SELECT has_table_privilege('{name}', '{privilege}')"
                ))
                .await,
                "{privilege} on partition {partition}"
            );
        }
        for privilege in ["SELECT", "INSERT", "UPDATE", "REFERENCES"] {
            assert!(
                !one(format!(
                    "SELECT has_any_column_privilege('{name}', '{privilege}')"
                ))
                .await,
                "column {privilege} on partition {partition}"
            );
        }
    }
}

async fn denied(client: &Client, sql: &str) {
    let error = client
        .batch_execute(sql)
        .await
        .expect_err(&format!("must be refused: {sql}"));
    assert_eq!(
        error.code(),
        Some(&SqlState::INSUFFICIENT_PRIVILEGE),
        "{sql}: {error:?}"
    );
}

/// The runtime role holds exactly: USAGE on the schema, INSERT on the twelve supplied columns,
/// USAGE on the id sequence. Nothing on the four generated columns, no table-level right, no
/// column right but INSERT, no right on any partition.
#[tokio::test]
async fn runtime_role_holds_exactly_the_insert_allowlist() {
    let f = fixture().await;
    assert_exact_allowlist(&f.runtime).await;
    f.runtime.batch_execute(INSERT).await.unwrap();
    f.runtime.batch_execute(INSERT).await.unwrap();
    let owner = f.h.db().client().await.unwrap();
    let rows = owner
        .query(
            &format!("SELECT id, xid::text FROM {TABLE} ORDER BY id"),
            &[],
        )
        .await
        .unwrap();
    assert_eq!(rows.len(), 2);
    assert!(rows[0].get::<_, i64>(0) < rows[1].get::<_, i64>(0));
    drop(owner);
    f.h.cleanup().await.unwrap();
}

/// Everything beyond the allowlist is refused with 42501 for the real role: supplying a generated
/// column, reading, updating, deleting, truncating, `RETURNING`, partition DDL, creating objects in
/// the schema, `setval`, and writing a partition directly.
#[tokio::test]
async fn everything_beyond_the_allowlist_is_refused_with_42501() {
    let f = fixture().await;
    let c = &f.runtime;
    f.runtime.batch_execute(INSERT).await.unwrap();
    for generated in GENERATED {
        let value = match generated {
            "id" => "1",
            "event_id" => "'0190a8a8-0000-7000-8000-000000000000'",
            "create_date" => "now()",
            _ => "'1'::xid8",
        };
        denied(
            c,
            &format!(
                "INSERT INTO {TABLE} ({generated}, schema_version, kind, model, record_ids, \
                 field_names, cascade_models, actor_id, company_ids, principal_kind, actor_kind, operation_id, tx_ordinal, subject) \
                 VALUES ({value}, 1, 'create', 'res.partner', '[1]', '{{}}', '{{}}', 1, '{{1}}', 'user', 'user', pg_catalog.uuidv7(), 1, '1')"
            ),
        )
        .await;
    }
    let partition = partitions(c).await.remove(0);
    for sql in [
        format!("SELECT count(*) FROM {TABLE}"),
        format!("SELECT id FROM {TABLE}"),
        format!("SELECT kind FROM {TABLE}"),
        format!("UPDATE {TABLE} SET kind = 'write'"),
        format!("DELETE FROM {TABLE}"),
        format!("TRUNCATE {TABLE}"),
        format!("{INSERT} RETURNING id"),
        format!("SELECT * FROM erp_event.{partition}"),
        format!("DELETE FROM erp_event.{partition}"),
        format!("TRUNCATE erp_event.{partition}"),
        format!(
            "INSERT INTO erp_event.{partition} (schema_version, kind, model, record_ids, \
             field_names, cascade_models, actor_id, company_ids, principal_kind, actor_kind, operation_id, tx_ordinal, subject) \
             VALUES (1, 'create', 'res.partner', '[1]', '{{}}', '{{}}', 1, '{{1}}', 'user', 'user', pg_catalog.uuidv7(), 1, '1')"
        ),
        format!("SELECT setval('{SEQUENCE}', 1)"),
        format!("SELECT last_value FROM {SEQUENCE}"),
        format!("ALTER SEQUENCE {SEQUENCE} RESTART"),
        "CREATE TABLE erp_event.ir_event_outbox_p29990101 PARTITION OF erp_event.ir_event_outbox \
         FOR VALUES FROM ('2999-01-01 00:00:00+00') TO ('2999-01-02 00:00:00+00')"
            .to_owned(),
        format!("ALTER TABLE {TABLE} DETACH PARTITION erp_event.{partition}"),
        format!("DROP TABLE erp_event.{partition}"),
        format!("DROP TABLE {TABLE}"),
        format!("ALTER TABLE {TABLE} ADD COLUMN extra int"),
        "CREATE TABLE erp_event.elsewhere (id int)".to_owned(),
        "CREATE SCHEMA erp_event_other".to_owned(),
        format!(
            "CREATE TRIGGER t BEFORE INSERT ON {TABLE} FOR EACH ROW EXECUTE FUNCTION pg_catalog.suppress_redundant_updates_trigger()"
        ),
    ] {
        denied(c, &sql).await;
    }
    f.h.cleanup().await.unwrap();
}

/// The ACL state of the event schema and every default ACL of the database, as text.
async fn acl_snapshot(client: &Client) -> String {
    client
        .query_one(
            "SELECT coalesce(string_agg(x, E'\\n' ORDER BY x), '') FROM ( \
               SELECT 'nsp:' || nspname || ':' || coalesce(nspacl::text, '-') x \
                 FROM pg_namespace WHERE nspname = 'erp_event' \
               UNION ALL SELECT 'rel:' || relname || ':' || coalesce(relacl::text, '-') \
                 FROM pg_class WHERE relnamespace = 'erp_event'::regnamespace \
               UNION ALL SELECT 'att:' || c.relname || '.' || a.attname || ':' || a.attacl::text \
                 FROM pg_attribute a JOIN pg_class c ON c.oid = a.attrelid \
                WHERE c.relnamespace = 'erp_event'::regnamespace AND a.attacl IS NOT NULL \
               UNION ALL SELECT 'def:' || defaclrole::regrole::text || ':' || defaclnamespace::text \
                 || ':' || defaclobjtype::text || ':' || defaclacl::text FROM pg_default_acl \
             ) s",
            &[],
        )
        .await
        .unwrap()
        .get(0)
}

/// PUBLIC and every other role have nothing on the schema, table, sequence, columns or partitions
/// (the only grantees in any ACL are the owner and the runtime role), and no default ACL gives the
/// runtime role, PUBLIC or the schema anything.
#[tokio::test]
async fn public_and_default_acls_give_nothing() {
    let f = fixture().await;
    let owner = f.h.db().client().await.unwrap();
    let strangers: i64 = owner
        .query_one(
            "WITH acls AS ( \
               SELECT nspacl a FROM pg_namespace WHERE nspname = 'erp_event' \
               UNION ALL SELECT relacl FROM pg_class WHERE relnamespace = 'erp_event'::regnamespace \
               UNION ALL SELECT a.attacl FROM pg_attribute a JOIN pg_class c ON c.oid = a.attrelid \
                WHERE c.relnamespace = 'erp_event'::regnamespace) \
             SELECT count(*) FROM acls, aclexplode(a) x \
              WHERE x.grantee = 0 \
                 OR x.grantee NOT IN (SELECT oid FROM pg_roles WHERE rolname = $1 \
                      OR oid = (SELECT nspowner FROM pg_namespace WHERE nspname = 'erp_event'))",
            &[&f.role],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(
        strangers, 0,
        "grantees other than the owner and the runtime role"
    );
    let defaults: i64 = owner
        .query_one(
            "SELECT count(*) FROM pg_default_acl d \
             WHERE d.defaclnamespace = 'erp_event'::regnamespace \
                OR EXISTS (SELECT 1 FROM aclexplode(d.defaclacl) x \
                            WHERE x.grantee = 0 OR x.grantee = (SELECT oid FROM pg_roles WHERE rolname = $1))",
            &[&f.role],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(defaults, 0, "pg_default_acl must give nothing");
    f.h.cleanup().await.unwrap();
}

/// A partition made by the upkeep gets no privilege of its own, and the insert through the parent
/// still works for the runtime role (tuple routing needs the parent's right only).
#[tokio::test]
async fn partitions_made_by_upkeep_grant_nothing_extra() {
    let f = fixture().await;
    let mut tx = f.h.begin().await.unwrap();
    let lost: String = tx
        .query(
            "SELECT 'ir_event_outbox_p' || to_char((now() AT TIME ZONE 'UTC')::date + 3, 'YYYYMMDD')",
            &[],
        )
        .await
        .unwrap()[0]
        .get(0);
    tx.batch(&format!("DROP TABLE erp_event.{lost}"))
        .await
        .unwrap();
    assert_eq!(
        tx.ensure_event_partitions().await.unwrap(),
        vec![lost.clone()]
    );
    tx.commit().await.unwrap();
    let owner = f.h.db().client().await.unwrap();
    let acl: Option<String> = owner
        .query_one(
            &format!("SELECT relacl::text FROM pg_class WHERE oid = 'erp_event.{lost}'::regclass"),
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(acl, None, "a partition made by the upkeep carries no ACL");
    assert_exact_allowlist(&f.runtime).await;
    f.runtime.batch_execute(INSERT).await.unwrap();
    f.h.cleanup().await.unwrap();
}

/// Granting again resets: privileges a role was given by hand (table-level, other columns, the
/// sequence, a partition, the schema's `CREATE`) are removed, the allowlist is restored, and a
/// repeat changes nothing.
#[tokio::test]
async fn grant_resets_extras_and_is_idempotent() {
    let f = fixture().await;
    let role = erp_sql::quote(&f.role);
    let owner = f.h.db().client().await.unwrap();
    let partition = partitions(&f.runtime).await.remove(0);
    owner
        .batch_execute(&format!(
            "GRANT SELECT, UPDATE ON {TABLE} TO {role}; GRANT INSERT (id) ON {TABLE} TO {role}; \
             GRANT SELECT ON SEQUENCE {SEQUENCE} TO {role}; GRANT ALL ON erp_event.{partition} TO {role}; \
             GRANT CREATE ON SCHEMA erp_event TO {role}"
        ))
        .await
        .unwrap();
    let mut tx = f.h.begin().await.unwrap();
    tx.grant_event_runtime_role(&f.role).await.unwrap();
    tx.commit().await.unwrap();
    assert_exact_allowlist(&f.runtime).await;
    let once = acl_snapshot(&owner).await;
    let mut tx = f.h.begin().await.unwrap();
    tx.grant_event_runtime_role(&f.role).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(acl_snapshot(&owner).await, once);
    f.h.cleanup().await.unwrap();
}

/// The grant refuses what the other core grants refuse (the owner, PUBLIC, a missing role) and a
/// database without the event table, and changes nothing when it refuses.
#[tokio::test]
async fn grant_refuses_the_owner_public_and_a_missing_table() {
    let f = fixture().await;
    let owner = f.h.db().client().await.unwrap();
    let before = acl_snapshot(&owner).await;
    let me: String = owner
        .query_one("SELECT current_user::text", &[])
        .await
        .unwrap()
        .get(0);
    for role in [
        me.as_str(),
        "public",
        "PUBLIC",
        "pg_read_all_data",
        "no_such_role_evt",
        "",
    ] {
        let mut tx = f.h.begin().await.unwrap();
        let error = tx.grant_event_runtime_role(role).await.expect_err(role);
        assert_eq!(
            error.kind(),
            erp_fields::ErrorKind::Programming,
            "{role}: {error:?}"
        );
    }
    assert_eq!(acl_snapshot(&owner).await, before);
    f.h.cleanup().await.unwrap();

    let h = Harness::builder([&base_users::ADDON])
        .without_audit_store()
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    let error = tx
        .grant_event_runtime_role("anyone")
        .await
        .expect_err("not installed");
    assert_eq!(
        error.kind(),
        erp_fields::ErrorKind::Programming,
        "{error:?}"
    );
    assert!(
        error.message().contains("install_event_schema"),
        "{}",
        error.message()
    );
    h.cleanup().await.unwrap();
}

/// The event term of `MISSING_RUNTIME_GRANTS_SQL`, between its `/*erp_event*/` markers. The text of
/// the constant in `crates/erp-cli/src/human.rs` is read as it is written, so a change there is
/// what these tests run.
fn event_term() -> String {
    let source = include_str!("../../erp-cli/src/human.rs");
    let constant = source
        .split("const MISSING_RUNTIME_GRANTS_SQL: &str = \"")
        .nth(1)
        .expect("the publication predicate");
    let constant = constant.split("\";").next().expect("end of the predicate");
    let term = constant
        .split("/*erp_event*/")
        .nth(1)
        .expect("the erp_event term");
    term.split("/*end erp_event*/")
        .next()
        .expect("end marker")
        .to_owned()
}

async fn event_term_flags(client: &Client) -> bool {
    client
        .query_one(&format!("SELECT {}", event_term()), &[])
        .await
        .unwrap()
        .get(0)
}

/// The publication check (as the runtime role, effective rights) passes for the granted state and
/// flags every deviation: a missing right, an extra one on the table, a column, the sequence, a
/// partition, the schema, through PUBLIC, and through a default ACL on a partition made later.
#[tokio::test]
async fn publication_check_flags_every_deviation() {
    let f = fixture().await;
    let owner = f.h.db().client().await.unwrap();
    let role = erp_sql::quote(&f.role);
    assert!(
        !event_term_flags(&f.runtime).await,
        "granted state must pass"
    );
    let partition = partitions(&f.runtime).await.remove(0);
    let deviations: [(&str, String, String); 12] = [
        (
            "table SELECT",
            format!("GRANT SELECT ON {TABLE} TO {role}"),
            format!("REVOKE SELECT ON {TABLE} FROM {role}"),
        ),
        (
            "table INSERT",
            format!("GRANT INSERT ON {TABLE} TO {role}"),
            format!(
                "REVOKE INSERT ON {TABLE} FROM {role}; GRANT INSERT ({}) ON {TABLE} TO {role}",
                SUPPLIED.join(", ")
            ),
        ),
        (
            "column SELECT",
            format!("GRANT SELECT (kind) ON {TABLE} TO {role}"),
            format!("REVOKE SELECT (kind) ON {TABLE} FROM {role}"),
        ),
        (
            "column UPDATE",
            format!("GRANT UPDATE (kind) ON {TABLE} TO {role}"),
            format!("REVOKE UPDATE (kind) ON {TABLE} FROM {role}"),
        ),
        (
            "INSERT on a generated column",
            format!("GRANT INSERT (id) ON {TABLE} TO {role}"),
            format!("REVOKE INSERT (id) ON {TABLE} FROM {role}"),
        ),
        (
            "missing INSERT on a supplied column",
            format!("REVOKE INSERT (kind) ON {TABLE} FROM {role}"),
            format!("GRANT INSERT (kind) ON {TABLE} TO {role}"),
        ),
        (
            "sequence SELECT",
            format!("GRANT SELECT ON SEQUENCE {SEQUENCE} TO {role}"),
            format!("REVOKE SELECT ON SEQUENCE {SEQUENCE} FROM {role}"),
        ),
        (
            "missing sequence USAGE",
            format!("REVOKE USAGE ON SEQUENCE {SEQUENCE} FROM {role}"),
            format!("GRANT USAGE ON SEQUENCE {SEQUENCE} TO {role}"),
        ),
        (
            "partition SELECT",
            format!("GRANT SELECT ON erp_event.{partition} TO {role}"),
            format!("REVOKE SELECT ON erp_event.{partition} FROM {role}"),
        ),
        (
            "schema CREATE through PUBLIC",
            "GRANT CREATE ON SCHEMA erp_event TO PUBLIC".to_owned(),
            "REVOKE CREATE ON SCHEMA erp_event FROM PUBLIC".to_owned(),
        ),
        (
            "table SELECT through PUBLIC",
            format!("GRANT SELECT ON {TABLE} TO PUBLIC"),
            format!("REVOKE SELECT ON {TABLE} FROM PUBLIC"),
        ),
        (
            "missing schema USAGE",
            format!("REVOKE USAGE ON SCHEMA erp_event FROM {role}"),
            format!("GRANT USAGE ON SCHEMA erp_event TO {role}"),
        ),
    ];
    for (what, break_it, repair) in deviations {
        owner.batch_execute(&break_it).await.unwrap();
        assert!(event_term_flags(&f.runtime).await, "{what} must be flagged");
        owner.batch_execute(&repair).await.unwrap();
        assert!(
            !event_term_flags(&f.runtime).await,
            "{what}: repair must pass again"
        );
    }
    // upkeep removes default ACLs from new event partitions. The
    // publication predicate must still detect a direct PUBLIC grant applied afterwards.
    owner
        .batch_execute(
            "ALTER DEFAULT PRIVILEGES IN SCHEMA erp_event GRANT SELECT ON TABLES TO PUBLIC",
        )
        .await
        .unwrap();
    let mut tx = f.h.begin().await.unwrap();
    let last = partitions(&f.runtime).await.pop().unwrap();
    tx.batch(&format!("DROP TABLE erp_event.{last}"))
        .await
        .unwrap();
    assert_eq!(
        tx.ensure_event_partitions().await.unwrap(),
        vec![last.clone()]
    );
    tx.commit().await.unwrap();
    assert!(
        !event_term_flags(&f.runtime).await,
        "new partition must discard default ACLs"
    );
    owner
        .batch_execute(&format!("GRANT SELECT ON erp_event.{last} TO PUBLIC"))
        .await
        .unwrap();
    assert!(
        event_term_flags(&f.runtime).await,
        "later direct PUBLIC grant must still be flagged"
    );
    owner
        .batch_execute(&format!("REVOKE SELECT ON erp_event.{last} FROM PUBLIC"))
        .await
        .unwrap();
    assert!(
        !event_term_flags(&f.runtime).await,
        "repair must pass again"
    );
    f.h.cleanup().await.unwrap();
}

/// Without the event table the term is false (legacy databases publish as before).
#[tokio::test]
async fn publication_check_is_silent_without_the_event_table() {
    let h = Harness::builder([&base_users::ADDON])
        .without_audit_store()
        .build()
        .await
        .unwrap();
    let config = h.read_only_role().await.unwrap();
    let password = String::from_utf8(config.get_password().unwrap().to_vec()).unwrap();
    let runtime = login(&h, config.get_user().unwrap(), &password).await;
    assert!(!event_term_flags(&runtime).await);
    h.cleanup().await.unwrap();
}

/// The statements of the last deployed release's grant SQL, from the committed fixture
/// (`scripts/last-release-grants.py` extracts and checks it against the pinned commit).
fn old_release_statements(role: &str) -> Vec<String> {
    let fixture = include_str!("fixtures/last_release_runtime_grants.sql");
    let quoted = erp_sql::quote(role);
    fixture
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.replace("{role}", &quoted).replace("{node_role}", &quoted))
        .collect()
}

/// The grant SQL of the last deployed release (rollback target) leaves the ACLs of the event schema
/// exactly as they are: `nspacl`, `relacl`, `attacl` and `pg_default_acl`, and the role's effective
/// rights. The old release does not know the schema, and must not widen or narrow it.
#[tokio::test]
async fn last_release_grant_sql_leaves_event_acls_unchanged() {
    let f = fixture().await;
    let owner = f.h.db().client().await.unwrap();
    let before = acl_snapshot(&owner).await;
    assert!(before.contains("nsp:erp_event:"), "{before}");
    let mut ran = 0;
    for statement in old_release_statements(&f.role) {
        match owner.batch_execute(&statement).await {
            Ok(()) => ran += 1,
            // the fixture database lacks tables the old statement names (it is not a full install)
            Err(error) if error.code() == Some(&SqlState::UNDEFINED_TABLE) => {}
            Err(error) => panic!("{statement}: {error:?}"),
        }
    }
    assert!(ran >= 3, "the broad public grants must have run, ran {ran}");
    assert_eq!(acl_snapshot(&owner).await, before);
    assert_exact_allowlist(&f.runtime).await;
    f.runtime.batch_execute(INSERT).await.unwrap();
    // the snapshot does see a change to the event ACLs
    owner
        .batch_execute(&format!(
            "GRANT SELECT ON {TABLE} TO {}",
            erp_sql::quote(&f.role)
        ))
        .await
        .unwrap();
    assert_ne!(acl_snapshot(&owner).await, before);
    f.h.cleanup().await.unwrap();
}
