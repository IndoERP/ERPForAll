// SPDX-License-Identifier: MIT OR Apache-2.0
//! Real login-role tests for's SELECT-only audit authority.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions"
)]
use erp_fields::ErrorKind;
use erp_orm::audit::{AuditReaderConnection, AuditReaderRoles};
use erp_test_models::base_users;
use erp_testkit::{
    Harness,
    tokio_postgres::{Client, NoTls, error::SqlState},
};

async fn fixture() -> (Harness, tokio_postgres::Config, AuditReaderRoles) {
    let h = Harness::builder([&base_users::ADDON])
        .build()
        .await
        .unwrap();
    let reader = h.read_only_role().await.unwrap();
    let name = erp_sql::quote(reader.get_user().unwrap());
    let owner = h.db().client().await.unwrap();
    owner.batch_execute(&format!("REVOKE ALL ON ALL TABLES IN SCHEMA public FROM {name}; REVOKE TEMPORARY ON DATABASE {} FROM PUBLIC", erp_sql::quote(h.db().name()))).await.unwrap();
    let runtime = format!("rt_{}", h.db().name());
    let node = format!("nd_{}", h.db().name());
    for name in [&runtime, &node] {
        owner
            .batch_execute(&format!(
                "CREATE ROLE {} LOGIN PASSWORD 'reader-fixture-password'",
                erp_sql::quote(name)
            ))
            .await
            .unwrap();
    }
    let roles =
        AuditReaderRoles::new(&runtime, h.db().config().get_user().unwrap(), Some(&node)).unwrap();
    let mut tx = h.begin().await.unwrap();
    tx.grant_event_runtime_role(&runtime).await.unwrap();
    tx.commit().await.unwrap();
    (h, reader, roles)
}
async fn cleanup(h: Harness) {
    let owner = h.db().client().await.unwrap();
    for name in [
        format!("rt_{}", h.db().name()),
        format!("nd_{}", h.db().name()),
    ] {
        owner
            .batch_execute(&format!(
                "DROP OWNED BY {0}; DROP ROLE {0}",
                erp_sql::quote(&name)
            ))
            .await
            .unwrap();
    }
    drop(owner);
    h.cleanup().await.unwrap();
}
async fn login(config: &tokio_postgres::Config) -> Client {
    let (client, connection) = config.connect(NoTls).await.unwrap();
    tokio::spawn(connection);
    client
}
async fn publish(h: &Harness, reader: &tokio_postgres::Config, roles: &AuditReaderRoles) {
    let mut tx = h.begin().await.unwrap();
    tx.grant_audit_reader_role(reader.get_user().unwrap(), roles)
        .await
        .unwrap();
    tx.commit().await.unwrap();
}

#[tokio::test]
async fn audit_reader_grants_only_parent_select_and_reconciles_direct_drift() {
    let (h, reader, roles) = fixture().await;
    publish(&h, &reader, &roles).await;
    let client = login(&reader).await;
    let mut runtime_config = h.db().config();
    runtime_config
        .user(format!("rt_{}", h.db().name()))
        .password("reader-fixture-password");
    let runtime = login(&runtime_config).await;
    runtime.batch_execute("INSERT INTO erp_event.ir_audit_entry (schema_version,entry_kind,origin,outcome,attempt,operation_id,principal_kind,subject_id,actor_kind,actor_id,company_ids,target_ids,events,encoded_bytes) VALUES (1,'change','internal','committed',1,pg_catalog.uuidv7(),'user',1,'user',1,'{}','[]','[]',0)").await.unwrap();
    let error = runtime
        .query_one("SELECT count(*) FROM erp_event.ir_audit_entry", &[])
        .await
        .unwrap_err();
    assert_eq!(error.code(), Some(&SqlState::INSUFFICIENT_PRIVILEGE));
    assert_eq!(
        error.as_db_error().unwrap().message(),
        "permission denied for table ir_audit_entry"
    );
    drop(runtime);
    client
        .query_one("SELECT count(*) FROM erp_event.ir_audit_entry", &[])
        .await
        .unwrap();
    for (sql, expected) in [
        (
            "INSERT INTO erp_event.ir_audit_entry DEFAULT VALUES",
            "permission denied for table ir_audit_entry".to_owned(),
        ),
        (
            "UPDATE erp_event.ir_audit_entry SET outcome='failed'",
            "permission denied for table ir_audit_entry".to_owned(),
        ),
        (
            "DELETE FROM erp_event.ir_audit_entry",
            "permission denied for table ir_audit_entry".to_owned(),
        ),
        (
            "TRUNCATE erp_event.ir_audit_entry",
            "permission denied for table ir_audit_entry".to_owned(),
        ),
        (
            "SELECT nextval('erp_event.ir_audit_entry_id_seq')",
            "permission denied for sequence ir_audit_entry_id_seq".to_owned(),
        ),
        (
            "SELECT last_value FROM erp_event.ir_audit_entry_id_seq",
            "permission denied for sequence ir_audit_entry_id_seq".to_owned(),
        ),
        (
            "CREATE TABLE erp_event.reader_escape (id int)",
            "permission denied for schema erp_event".to_owned(),
        ),
        (
            "CREATE TEMP TABLE reader_escape (id int)",
            format!(
                "permission denied to create temporary tables in database \"{}\"",
                h.db().name()
            ),
        ),
    ] {
        let error = client.batch_execute(sql).await.unwrap_err();
        assert_eq!(
            error.code(),
            Some(&SqlState::INSUFFICIENT_PRIVILEGE),
            "{sql}: {error}"
        );
        assert_eq!(error.as_db_error().unwrap().message(), expected);
    }
    let owner = h.db().client().await.unwrap();
    let partition: String = owner.query_one("SELECT c.relname::text FROM pg_inherits i JOIN pg_class c ON c.oid=i.inhrelid WHERE i.inhparent='erp_event.ir_audit_entry'::regclass ORDER BY c.relname LIMIT 1", &[]).await.unwrap().get(0);
    let sql = format!(
        "SELECT count(*) FROM erp_event.{}",
        erp_sql::quote(&partition)
    );
    let error = client.query_one(&sql, &[]).await.unwrap_err();
    assert_eq!(error.code(), Some(&SqlState::INSUFFICIENT_PRIVILEGE));
    assert_eq!(
        error.as_db_error().unwrap().message(),
        format!("permission denied for table {partition}")
    );
    let role = erp_sql::quote(reader.get_user().unwrap());
    owner.batch_execute(&format!("GRANT UPDATE(outcome) ON erp_event.ir_audit_entry TO {role}; GRANT SELECT ON erp_event.{} TO {role}", erp_sql::quote(&partition))).await.unwrap();
    publish(&h, &reader, &roles).await;
    publish(&h, &reader, &roles).await;
    assert!(
        !client
            .query_one(
                "SELECT has_any_column_privilege('erp_event.ir_audit_entry','UPDATE')",
                &[]
            )
            .await
            .unwrap()
            .get::<_, bool>(0)
    );
    let error = client.query_one(&sql, &[]).await.unwrap_err();
    assert_eq!(error.code(), Some(&SqlState::INSUFFICIENT_PRIVILEGE));
    assert_eq!(
        error.as_db_error().unwrap().message(),
        format!("permission denied for table {partition}")
    );
    drop(client);
    drop(owner);
    cleanup(h).await;
}

#[tokio::test]
async fn audit_reader_rejects_public_and_reachable_privileges() {
    let (h, reader, roles) = fixture().await;
    let owner = h.db().client().await.unwrap();
    let role = erp_sql::quote(reader.get_user().unwrap());
    let attacks = [
        ("CREATE FUNCTION public.reader_privilege_probe() RETURNS integer LANGUAGE sql SECURITY DEFINER AS 'SELECT 1'".to_owned(), "DROP FUNCTION public.reader_privilege_probe()".to_owned()),
        (format!("GRANT USAGE ON SCHEMA public TO {role} WITH GRANT OPTION"), format!("REVOKE GRANT OPTION FOR USAGE ON SCHEMA public FROM {role}")),
        (format!("GRANT {} TO {role} WITH ADMIN TRUE, INHERIT FALSE, SET FALSE", erp_sql::quote(h.db().config().get_user().unwrap())), format!("REVOKE {} FROM {role}", erp_sql::quote(h.db().config().get_user().unwrap()))),
        (
            "GRANT SELECT ON res_users TO PUBLIC".to_owned(),
            "REVOKE SELECT ON res_users FROM PUBLIC".to_owned(),
        ),
        (
            format!("ALTER ROLE {role} SUPERUSER"),
            format!("ALTER ROLE {role} NOSUPERUSER"),
        ),
        (
            format!("ALTER ROLE {role} CREATEROLE"),
            format!("ALTER ROLE {role} NOCREATEROLE"),
        ),
        (
            format!("ALTER ROLE {role} REPLICATION"),
            format!("ALTER ROLE {role} NOREPLICATION"),
        ),
        (
            format!(
                "GRANT {role} TO {}",
                erp_sql::quote(&format!("rt_{}", h.db().name()))
            ),
            format!(
                "REVOKE {role} FROM {}",
                erp_sql::quote(&format!("rt_{}", h.db().name()))
            ),
        ),
        (
            format!("ALTER ROLE {role} BYPASSRLS"),
            format!("ALTER ROLE {role} NOBYPASSRLS"),
        ),
        (
            format!("ALTER ROLE {role} CREATEDB"),
            format!("ALTER ROLE {role} NOCREATEDB"),
        ),
        (
            format!(
                "GRANT {} TO {role} WITH INHERIT FALSE, SET TRUE",
                erp_sql::quote(h.db().config().get_user().unwrap())
            ),
            format!(
                "REVOKE {} FROM {role}",
                erp_sql::quote(h.db().config().get_user().unwrap())
            ),
        ),
        (
            "GRANT SELECT(outcome) ON erp_event.ir_audit_entry TO PUBLIC".to_owned(),
            "REVOKE SELECT(outcome) ON erp_event.ir_audit_entry FROM PUBLIC".to_owned(),
        ),
    ];
    for (attack, repair) in attacks {
        owner.batch_execute(&attack).await.unwrap();
        let mut tx = h.begin().await.unwrap();
        let error = tx
            .grant_audit_reader_role(reader.get_user().unwrap(), &roles)
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Programming);
        assert_eq!(
            error.message(),
            "a later release: audit reader privileges or role isolation are unsafe"
        );
        drop(tx);
        owner.batch_execute(&repair).await.unwrap();
    }
    // Owner-to-reader membership grants no new authority to the owner. The forbidden
    // direction is reader-to-owner; runtime/node-to-reader remains forbidden too.
    let schema_owner = erp_sql::quote(h.db().config().get_user().unwrap());
    owner
        .batch_execute(&format!(
            "GRANT {role} TO {schema_owner} WITH INHERIT FALSE, SET TRUE"
        ))
        .await
        .unwrap();
    publish(&h, &reader, &roles).await;
    owner
        .batch_execute(&format!("REVOKE {role} FROM {schema_owner}"))
        .await
        .unwrap();
    drop(owner);
    cleanup(h).await;
}

#[tokio::test]
async fn audit_reader_connection_refuses_role_substitution_and_wrong_database() {
    let (h, reader, roles) = fixture().await;
    publish(&h, &reader, &roles).await;
    let admitted =
        AuditReaderConnection::open(reader.clone(), h.db().pool().clone(), roles.clone())
            .await
            .unwrap();
    admitted.verify().await.unwrap();
    let owner = h.db().client().await.unwrap();
    owner
        .batch_execute("GRANT SELECT ON res_users TO PUBLIC")
        .await
        .unwrap();
    let error = admitted.verify().await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.message(),
        "a later release: audit reader connection admission failed"
    );
    owner
        .batch_execute("REVOKE SELECT ON res_users FROM PUBLIC")
        .await
        .unwrap();
    admitted.verify().await.unwrap();
    drop(admitted);
    drop(owner);
    let error = AuditReaderConnection::open(h.db().config(), h.db().pool().clone(), roles.clone())
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.message(),
        "a later release: audit reader connection admission failed"
    );
    let other = Harness::builder([&base_users::ADDON])
        .build()
        .await
        .unwrap();
    let error = AuditReaderConnection::open(reader.clone(), other.db().pool().clone(), roles)
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.message(),
        "a later release: audit reader connection admission failed"
    );
    other.cleanup().await.unwrap();
    cleanup(h).await;
}

#[tokio::test]
async fn audit_reader_partition_upkeep_and_default_privileges() {
    let (h, reader, roles) = fixture().await;
    publish(&h, &reader, &roles).await;
    let owner = h.db().client().await.unwrap();
    let role = erp_sql::quote(reader.get_user().unwrap());
    let lost: String = owner.query_one("SELECT c.relname::text FROM pg_inherits i JOIN pg_class c ON c.oid=i.inhrelid WHERE i.inhparent='erp_event.ir_audit_entry'::regclass ORDER BY c.relname DESC LIMIT 1", &[]).await.unwrap().get(0);
    owner.batch_execute(&format!("DROP TABLE erp_event.{}; ALTER DEFAULT PRIVILEGES IN SCHEMA erp_event GRANT SELECT ON TABLES TO {role}", erp_sql::quote(&lost))).await.unwrap();
    let mut tx = h.begin().await.unwrap();
    assert_eq!(
        tx.ensure_audit_partitions().await.unwrap(),
        vec![lost.clone()]
    );
    tx.commit().await.unwrap();
    let client = login(&reader).await;
    client
        .query_one("SELECT count(*) FROM erp_event.ir_audit_entry", &[])
        .await
        .unwrap();
    let error = client
        .query_one(
            &format!("SELECT count(*) FROM erp_event.{}", erp_sql::quote(&lost)),
            &[],
        )
        .await
        .unwrap_err();
    assert_eq!(error.code(), Some(&SqlState::INSUFFICIENT_PRIVILEGE));
    assert_eq!(
        error.as_db_error().unwrap().message(),
        format!("permission denied for table {lost}")
    );
    publish(&h, &reader, &roles).await;
    owner
        .batch_execute(&format!(
            "ALTER DEFAULT PRIVILEGES IN SCHEMA erp_event REVOKE SELECT ON TABLES FROM {role}"
        ))
        .await
        .unwrap();
    drop(client);
    drop(owner);
    cleanup(h).await;
}

#[test]
fn audit_reader_role_set_validates_empty_duplicate_nul_and_byte_bounds() {
    let oversized = "a".repeat(64);
    for (runtime, owner, node) in [
        ("", "owner", None),
        ("runtime", "runtime", None),
        ("runtime", "owner", Some("owner")),
        ("nul\0role", "owner", None),
        (oversized.as_str(), "owner", None),
    ] {
        let error = AuditReaderRoles::new(runtime, owner, node).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Programming);
        assert_eq!(
            error.message(),
            "a later release: audit reader privileges or role isolation are unsafe"
        );
    }
    AuditReaderRoles::new(&"a".repeat(63), "owner", Some("heartbeat")).unwrap();
    AuditReaderRoles::new("utilisateur", "propriétaire", Some("nœud")).unwrap();
}
