// SPDX-License-Identifier: MIT OR Apache-2.0
//! STORE runs against the same genuinely installed base as the policy acceptance fixture.
use super::*;

// Exercise owner publication after the same real module installation as the served CLI.
// RPC owns its existing key DDL; referencing it also makes the test binary link that owner.
async fn store_fixture() -> Fixture {
    let f = fixture().await;
    let mut tx = f.setup.begin(SUPERUSER_ID).await.unwrap();
    tx.batch(erp_rpc::IDEMPOTENCY_DDL).await.unwrap();
    tx.install_approval_schema().await.unwrap();
    tx.commit().await.unwrap();
    f.user(
        "store administrator",
        &[f.main],
        &[f.group_user, f.group_system],
    )
    .await;
    f
}

const TABLES: [&str; 4] = [
    "erp_approval_request",
    "erp_approval_decision",
    "erp_approval_receipt",
    "erp_approval_result",
];

#[test]
fn installed_store_has_empty_logged_evidence_tables() {
    run(|| async {
        let f = store_fixture().await;
        let db = f.db.client().await.unwrap();
        for table in TABLES {
            let row = db
                .query_one(
                    "SELECT c.relkind::text, c.relpersistence::text FROM pg_class c \
                 WHERE c.oid = to_regclass($1)",
                    &[&format!("public.{table}")],
                )
                .await;
            assert!(row.is_ok(), "installed base is missing {table}: {row:?}");
            let row = row.unwrap();
            assert_eq!(row.get::<_, String>(0), "r", "{table}");
            assert_eq!(row.get::<_, String>(1), "p", "{table}");
            let count: i64 = db
                .query_one(&format!("SELECT count(*) FROM {table}"), &[])
                .await
                .unwrap()
                .get(0);
            assert_eq!(count, 0, "installation invents no {table} history");
        }
        let count: i64 = db
            .query_one(
                "SELECT count(*) FROM ir_approval_policy WHERE principal_kind='agent' \
             AND outcome='approve' AND effect IN ('destructive','external')",
                &[],
            )
            .await
            .unwrap()
            .get(0);
        assert_eq!(count, 2, "preserve POLICY defaults");
        f.db.cleanup().await.unwrap();
    });
}

async fn request(f: &Fixture) -> i64 {
    let uid = f.user("store requester", &[f.main], &[f.group_user]).await;
    let revision = f
        .record(
            MODEL,
            policy("store revision", "write", "approve").set_cell("active", Cell::Bool(false)),
        )
        .await;
    let db = f.db.client().await.unwrap();
    let key: i64 = db.query_one(
        "INSERT INTO ir_idempotency_key (key,principal_kind,subject_uid,model,method,request_digest, \
         binding_digest,policy_generation,resolution_epoch,expires_at) \
         VALUES (uuidv7(),'user',$1,'res.partner','write',decode(repeat('01',32),'hex'), \
         decode(repeat('02',32),'hex'),1,1,clock_timestamp()+interval '1 day') RETURNING id", &[&uid],
    ).await.unwrap().get(0);
    db.query_one(
        "INSERT INTO erp_approval_request (idempotency_id,principal_kind,subject_uid,key, \
         policy_revision_id,credential_kind,session_id,model,method,method_revision,effect, \
         envelope_version,request_digest,envelope_digest,binding_digest,policy_generation, \
         resolution_epoch,operation_id,envelope,expires_at) \
         SELECT id,principal_kind,subject_uid,key,$2,'session',uuidv7(),model,method,'v1','write', \
         1,request_digest,decode(repeat('03',32),'hex'),binding_digest,policy_generation, \
         resolution_epoch,uuidv7(),decode('7b7d','hex'),clock_timestamp()+interval '1 day' \
         FROM ir_idempotency_key WHERE id=$1 RETURNING id",
        &[&key, &revision],
    )
    .await
    .unwrap()
    .get(0)
}

#[test]
fn reservation_is_atomic_and_cannot_be_expired_or_deleted() {
    run(|| async {
        let f = store_fixture().await;
        let id = request(&f).await;
        let db = f.db.client().await.unwrap();
        let infinity: bool = db
            .query_one(
                "SELECT k.expires_at='infinity'::timestamptz FROM ir_idempotency_key k \
             JOIN erp_approval_request r ON r.idempotency_id=k.id WHERE r.id=$1",
                &[&id],
            )
            .await
            .unwrap()
            .get(0);
        assert!(infinity);
        for sql in [
            "UPDATE ir_idempotency_key SET expires_at=clock_timestamp()-interval '1 second' \
             WHERE id IN (SELECT idempotency_id FROM erp_approval_request)",
            "DELETE FROM ir_idempotency_key WHERE id IN (SELECT idempotency_id FROM erp_approval_request)",
        ] {
            let error = db.batch_execute(sql).await.unwrap_err();
            let pg = error.as_db_error().unwrap();
            assert_eq!(pg.code(), &tokio_postgres::error::SqlState::CHECK_VIOLATION);
            assert_eq!(
                pg.message(),
                "Approval idempotency reservation is immutable."
            );
        }
        db.batch_execute("UPDATE erp_approval_request SET state='rejected'")
            .await
            .unwrap();
        assert!(
            db.query_one(
                "SELECT expires_at='infinity'::timestamptz FROM ir_idempotency_key",
                &[]
            )
            .await
            .unwrap()
            .get::<_, bool>(0)
        );
        drop(db);
        f.db.cleanup().await.unwrap();
    });
}

#[test]
fn runtime_login_cannot_rewrite_evidence_or_delete_history() {
    run(|| async {
        let f = store_fixture().await;
        request(&f).await;
        let role = format!(
            "trust_{}",
            erp_orm::CorrelationId::generate()
                .unwrap()
                .as_str()
                .replace('-', "")
        );
        let password = erp_orm::CorrelationId::generate().unwrap().to_string();
        let owner = f.db.client().await.unwrap();
        owner
            .batch_execute(&format!(
                "CREATE ROLE {} LOGIN PASSWORD {}; GRANT USAGE ON SCHEMA public TO {}",
                erp_sql::quote(&role),
                erp_sql::literal(&password),
                erp_sql::quote(&role)
            ))
            .await
            .unwrap();
        let mut tx = f.setup.begin(SUPERUSER_ID).await.unwrap();
        tx.grant_approval_runtime_role(&role).await.unwrap();
        tx.commit().await.unwrap();
        let mut config = f.db.config();
        config.user(&role).password(&password);
        let (runtime, connection) = config.connect(tokio_postgres::NoTls).await.unwrap();
        let connection = tokio::spawn(connection);
        for table in TABLES {
            for permission in [
                "UPDATE",
                "DELETE",
                "TRUNCATE",
                "REFERENCES",
                "TRIGGER",
                "MAINTAIN",
            ] {
                let held: bool = runtime
                    .query_one("SELECT has_table_privilege($1,$2)", &[&table, &permission])
                    .await
                    .unwrap()
                    .get(0);
                assert_eq!(
                    held,
                    table == "erp_approval_result" && permission == "DELETE",
                    "{table} {permission}"
                );
            }
            let columns = runtime
                .query(
                    "SELECT attname::text,has_column_privilege(attrelid,attnum,'UPDATE') \
                FROM pg_attribute WHERE attrelid=to_regclass($1) AND attnum>0 AND NOT attisdropped",
                    &[&table],
                )
                .await
                .unwrap();
            for column in columns {
                let name: String = column.get(0);
                let expected = table == "erp_approval_request"
                    && ["state", "state_changed_at"].contains(&name.as_str());
                assert_eq!(column.get::<_, bool>(1), expected, "{table}.{name}");
                if !expected {
                    let error = runtime
                        .batch_execute(&format!(
                            "UPDATE {table} SET {}={}",
                            erp_sql::quote(&name),
                            // Identity assignment is validated before ACLs; use its legal form.
                            if name == "id" {
                                "DEFAULT".to_owned()
                            } else {
                                erp_sql::quote(&name)
                            }
                        ))
                        .await
                        .unwrap_err();
                    assert_eq!(
                        error.code(),
                        Some(&tokio_postgres::error::SqlState::INSUFFICIENT_PRIVILEGE)
                    );
                    assert_eq!(
                        error.as_db_error().unwrap().message(),
                        format!("permission denied for table {table}")
                    );
                }
            }
            if table != "erp_approval_result" {
                for operation in ["DELETE FROM", "TRUNCATE"] {
                    let error = runtime
                        .batch_execute(&format!("{operation} {table}"))
                        .await
                        .unwrap_err();
                    assert_eq!(
                        error.code(),
                        Some(&tokio_postgres::error::SqlState::INSUFFICIENT_PRIVILEGE)
                    );
                    assert_eq!(
                        error.as_db_error().unwrap().message(),
                        format!("permission denied for table {table}")
                    );
                }
            }
        }
        runtime.batch_execute("UPDATE erp_approval_request SET state='rejected',state_changed_at=clock_timestamp()").await.unwrap();
        let denied_manifest = runtime
            .batch_execute("SELECT * FROM erp_approval_schema")
            .await
            .unwrap_err();
        assert_eq!(
            denied_manifest.code(),
            Some(&tokio_postgres::error::SqlState::INSUFFICIENT_PRIVILEGE)
        );
        assert_eq!(
            denied_manifest.as_db_error().unwrap().message(),
            "permission denied for table erp_approval_schema"
        );
        // A non-inheriting SET ROLE path is part of the privilege boundary too.
        let leaked_role = format!("{role}_leak");
        owner.batch_execute(&format!(
            "CREATE ROLE {} NOLOGIN; GRANT UPDATE(envelope) ON erp_approval_request TO {}; GRANT {} TO {} WITH INHERIT FALSE, SET TRUE",
            erp_sql::quote(&leaked_role), erp_sql::quote(&leaked_role), erp_sql::quote(&leaked_role), erp_sql::quote(&role)
        )).await.unwrap();
        let mut publication = f.setup.begin(SUPERUSER_ID).await.unwrap();
        let error = publication
            .grant_approval_runtime_role(&role)
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Programming);
        assert_eq!(
            error.message(),
            "Runtime role cannot preserve approval evidence privileges."
        );
        assert!(publication.is_poisoned());
        publication.rollback().await.unwrap();
        owner
            .batch_execute(&format!(
                "REVOKE {} FROM {}; DROP OWNED BY {}; DROP ROLE {}",
                erp_sql::quote(&leaked_role),
                erp_sql::quote(&role),
                erp_sql::quote(&leaked_role),
                erp_sql::quote(&leaked_role)
            ))
            .await
            .unwrap();
        drop(runtime);
        connection.await.unwrap().unwrap();
        owner
            .batch_execute(&format!(
                "DROP OWNED BY {}; DROP ROLE {}",
                erp_sql::quote(&role),
                erp_sql::quote(&role)
            ))
            .await
            .unwrap();
        drop(owner);
        f.db.cleanup().await.unwrap();
    });
}

#[test]
fn schema_drift_is_refused_and_rollback_restores_the_contract() {
    run(|| async {
        let f = store_fixture().await;
        for ddl in [
            "ALTER TABLE erp_approval_request ADD COLUMN extra text",
            "ALTER TABLE erp_approval_request ALTER COLUMN model DROP NOT NULL",
            "ALTER TABLE erp_approval_request ALTER COLUMN state SET DEFAULT 'approved'",
            "ALTER TABLE erp_approval_request DROP CONSTRAINT erp_approval_request_policy_revision_id_fkey",
            "DROP INDEX erp_approval_request_policy",
            "ALTER SEQUENCE erp_approval_request_id_seq INCREMENT BY 2",
            "ALTER TABLE erp_approval_request DISABLE TRIGGER erp_approval_reserve_key",
            "ALTER TABLE ir_idempotency_key DISABLE TRIGGER erp_approval_reserved_key_guard",
            "ALTER TABLE erp_approval_result SET UNLOGGED",
        ] {
            let mut tx = f.setup.begin(SUPERUSER_ID).await.unwrap();
            tx.batch(ddl).await.unwrap();
            let error = tx.check_approval_schema().await.unwrap_err();
            assert_eq!(error.kind(), ErrorKind::Programming, "{ddl}");
            assert_eq!(
                error.message(),
                "Approval store differs from its installed schema contract.",
                "{ddl}"
            );
            tx.rollback().await.unwrap();
            let mut check = f.setup.begin(SUPERUSER_ID).await.unwrap();
            check.install_approval_schema().await.unwrap();
            check.rollback().await.unwrap();
        }
        f.db.cleanup().await.unwrap();
    });
}

#[test]
fn installer_never_adopts_an_existing_unverified_table() {
    run(|| async {
        let f = fixture().await;
        let mut tx = f.setup.begin(SUPERUSER_ID).await.unwrap();
        tx.batch(erp_rpc::IDEMPOTENCY_DDL).await.unwrap();
        tx.batch("CREATE TABLE erp_approval_request (unverified text)")
            .await
            .unwrap();
        tx.commit().await.unwrap();
        let mut tx = f.setup.begin(SUPERUSER_ID).await.unwrap();
        let error = tx.install_approval_schema().await.unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Programming);
        assert_eq!(
            error.message(),
            "Approval store differs from its installed schema contract."
        );
        assert!(tx.is_poisoned());
        tx.rollback().await.unwrap();
        let db = f.db.client().await.unwrap();
        assert!(
            db.query_one("SELECT to_regclass('erp_approval_schema') IS NULL", &[])
                .await
                .unwrap()
                .get::<_, bool>(0)
        );
        assert_eq!(db.query_one("SELECT attname::text FROM pg_attribute WHERE attrelid='erp_approval_request'::regclass AND attnum>0", &[]).await.unwrap().get::<_,String>(0), "unverified");
        drop(db);
        f.db.cleanup().await.unwrap();
    });
}

#[test]
fn user_approve_activation_is_fenced_before_enforcement() {
    run(|| async {
        let f = store_fixture().await;
        let id = f
            .record(
                MODEL,
                policy("inactive draft", "write", "approve").set_cell("active", Cell::Bool(false)),
            )
            .await;
        let db = f.db.client().await.unwrap();
        let error = db
            .execute(
                "UPDATE ir_approval_policy SET active=true WHERE id=$1",
                &[&id],
            )
            .await
            .unwrap_err();
        assert_eq!(
            error.code(),
            Some(&tokio_postgres::error::SqlState::CHECK_VIOLATION)
        );
        assert_eq!(
            error.as_db_error().unwrap().message(),
            "User approval activation requires a later release and a later release."
        );
        assert!(
            !db.query_one("SELECT active FROM ir_approval_policy WHERE id=$1", &[&id])
                .await
                .unwrap()
                .get::<_, bool>(0)
        );
        drop(db);
        f.db.cleanup().await.unwrap();
    });
}

#[test]
fn installation_failure_leaves_the_previous_schema_and_receipts_intact() {
    run(|| async {
        let f = fixture().await;
        let mut tx = f.setup.begin(SUPERUSER_ID).await.unwrap();
        tx.batch(erp_rpc::IDEMPOTENCY_DDL).await.unwrap();
        tx.batch("INSERT INTO ir_idempotency_key (key,principal_kind,subject_uid,model,method,request_digest,binding_digest,policy_generation,resolution_epoch,status,response,expires_at) VALUES (uuidv7(),'user',1,'res.partner','write',decode(repeat('01',32),'hex'),decode(repeat('02',32),'hex'),1,1,200,decode('7b7d','hex'),clock_timestamp()+interval '1 day')").await.unwrap();
        tx.commit().await.unwrap();
        let mut tx = f.setup.begin(SUPERUSER_ID).await.unwrap();
        tx.install_approval_schema().await.unwrap();
        let error = tx.query("SELECT 1/0", &[]).await.unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Database);
        assert_eq!(error.sqlstate(), Some("22012"));
        assert_eq!(error.message(), "division by zero");
        tx.rollback().await.unwrap();
        let db = f.db.client().await.unwrap();
        let row = db.query_one("SELECT to_regclass('erp_approval_schema') IS NULL, to_regprocedure('erp_approval_reserve_key()') IS NULL, (SELECT count(*) FROM ir_idempotency_key WHERE status=200 AND response=decode('7b7d','hex'))", &[]).await.unwrap();
        assert!(row.get::<_, bool>(0));
        assert!(row.get::<_, bool>(1));
        assert_eq!(row.get::<_, i64>(2), 1);
        assert!(f.setup.registry().model("erp.approval.request").is_none());
        drop(db);
        f.db.cleanup().await.unwrap();
    });
}
