//! stage A: the runtime role's privileges on the queue's tables, checked as a real login
//! role with the catalogs' effective-rights functions (the grant contract is tested as the
//! role the server runs as, never as the owner).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use erp_fields::ErrorKind;
use erp_orm::jobs::{JOB_TABLE_NAMES, JOB_UPDATABLE_COLUMNS};
use erp_test_models::base_users;
use erp_testkit::Harness;
use erp_testkit::tokio_postgres::{Client, NoTls, error::SqlState};

struct Fixture {
    h: Harness,
    role: String,
    runtime: Client,
}

async fn fixture() -> Fixture {
    let h = Harness::builder([&base_users::ADDON])
        .build()
        .await
        .unwrap();
    let config = h.read_only_role().await.unwrap();
    let role = config.get_user().unwrap().to_owned();
    let password = String::from_utf8(config.get_password().unwrap().to_vec()).unwrap();
    let mut tx = h.begin().await.unwrap();
    tx.grant_jobs_runtime_role(&role).await.unwrap();
    tx.commit().await.unwrap();
    let mut login = h.db().config();
    login.user(&role).password(&password);
    let (runtime, connection) = login.connect(NoTls).await.unwrap();
    tokio::spawn(connection);
    Fixture { h, role, runtime }
}

async fn held(client: &Client, sql: &str) -> bool {
    client.query_one(sql, &[]).await.unwrap().get::<_, bool>(0)
}

async fn denied(client: &Client, sql: &str) {
    let error = client.batch_execute(sql).await.unwrap_err();
    assert_eq!(
        error.code(),
        Some(&SqlState::INSUFFICIENT_PRIVILEGE),
        "{sql}"
    );
}

const INSERT: &str = "INSERT INTO erp_job (kind, kind_version, max_attempts, run_as_kind, run_as_user, \
                      enqueued_by_kind, enqueued_by_user) VALUES ('test.kind', 1, 3, 'user', 7, 'user', 7)";

const ACL_SQL: &str = "SELECT string_agg(relname || ':' || coalesce(relacl::text, ''), ';' ORDER BY relname) \
                       FROM pg_class WHERE relname IN ('erp_job', 'erp_job_run', 'erp_job_item_failure') \
                       AND relkind = 'r'";

#[tokio::test]
async fn the_runtime_role_updates_lifecycle_columns_and_nothing_that_names_the_job_or_its_principal()
 {
    let f = fixture().await;
    let columns = f
        .runtime
        .query(
            "SELECT a.attname::text, has_column_privilege('erp_job', a.attnum, 'UPDATE') \
             FROM pg_attribute a WHERE a.attrelid = 'erp_job'::regclass AND a.attnum > 0 \
             AND NOT a.attisdropped ORDER BY a.attnum",
            &[],
        )
        .await
        .unwrap();
    assert!(columns.len() > 30);
    for row in &columns {
        let name: String = row.get(0);
        let may_update: bool = row.get(1);
        assert_eq!(
            may_update,
            JOB_UPDATABLE_COLUMNS.contains(&name.as_str()),
            "UPDATE ({name})"
        );
    }
    for column in [
        "payload",
        "kind",
        "run_as_user",
        "run_as_kind",
        "run_as_companies",
        "job_uuid",
        "singleton_key",
        "dedup_key",
        "max_attempts",
        "enqueued_by_user",
        "correlation_id",
    ] {
        denied(
            &f.runtime,
            &format!("UPDATE erp_job SET {column} = {column}"),
        )
        .await;
    }
    f.h.cleanup().await.unwrap();
}

#[tokio::test]
async fn the_runtime_role_holds_the_documented_table_rights_and_no_others() {
    let f = fixture().await;
    let c = &f.runtime;
    for table in ["erp_job", "erp_job_run", "erp_job_item_failure"] {
        for right in [
            "SELECT",
            "INSERT",
            "UPDATE",
            "DELETE",
            "TRUNCATE",
            "REFERENCES",
            "TRIGGER",
            "MAINTAIN",
        ] {
            let expected = matches!(right, "SELECT" | "INSERT" | "DELETE");
            let actual = held(
                c,
                &format!("SELECT has_table_privilege('{table}', '{right}')"),
            )
            .await;
            assert_eq!(actual, expected, "{table} {right}");
        }
    }
    // history rows are never updated
    assert!(
        !held(
            c,
            "SELECT has_any_column_privilege('erp_job_run', 'UPDATE')"
        )
        .await
    );
    // the failure rows may refresh their error text, nothing else
    for (column, expected) in [
        ("error_code", true),
        ("error", true),
        ("failed_at", true),
        ("job_uuid", false),
        ("item_key", false),
    ] {
        let actual = held(
            c,
            &format!("SELECT has_column_privilege('erp_job_item_failure', '{column}', 'UPDATE')"),
        )
        .await;
        assert_eq!(actual, expected, "{column}");
    }
    c.batch_execute(INSERT).await.unwrap();
    c.batch_execute("UPDATE erp_job SET state = 'held'")
        .await
        .unwrap();
    c.batch_execute("DELETE FROM erp_job").await.unwrap();
    denied(c, "TRUNCATE erp_job").await;
    denied(c, "TRUNCATE erp_job_run").await;
    f.h.cleanup().await.unwrap();
}

#[tokio::test]
async fn public_holds_nothing_on_the_job_tables() {
    let f = fixture().await;
    let owner = f.h.db().client().await.unwrap();
    for table in JOB_TABLE_NAMES {
        let count: i64 = owner
            .query_one(
                "SELECT count(*) FROM information_schema.table_privileges \
                 WHERE table_name = $1 AND grantee = 'PUBLIC'",
                &[table],
            )
            .await
            .unwrap()
            .get(0);
        assert_eq!(count, 0, "{table}");
    }
    f.h.cleanup().await.unwrap();
}

#[tokio::test]
async fn granting_twice_gives_the_same_privileges() {
    let f = fixture().await;
    let owner = f.h.db().client().await.unwrap();
    let before: String = owner.query_one(ACL_SQL, &[]).await.unwrap().get(0);
    let mut tx = f.h.begin().await.unwrap();
    tx.grant_jobs_runtime_role(&f.role).await.unwrap();
    tx.commit().await.unwrap();
    let after: String = owner.query_one(ACL_SQL, &[]).await.unwrap().get(0);
    assert_eq!(before, after);
    f.h.cleanup().await.unwrap();
}

#[tokio::test]
async fn the_grant_refuses_the_table_owner_public_and_a_missing_role() {
    let h = Harness::builder([&base_users::ADDON])
        .build()
        .await
        .unwrap();
    let owner = h.db().config().get_user().unwrap().to_owned();
    for role in [owner.as_str(), "PUBLIC", "no_such_job_role"] {
        let mut tx = h.begin().await.unwrap();
        let error = tx.grant_jobs_runtime_role(role).await.unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Programming, "{role}");
        let _ = tx.rollback().await;
    }
    // only the system principal may change privileges
    let mut tx = h.begin_as(7).await.unwrap();
    let error = tx.grant_jobs_runtime_role("anyone").await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Access);
    let _ = tx.rollback().await;
    h.cleanup().await.unwrap();
}

/// The queue term of `MISSING_RUNTIME_GRANTS_SQL`, between its `/*erp_jobs*/` markers, read from
/// `crates/erp-cli/src/human.rs` as it is written (the same extraction the event grants test
/// uses, so a change there is what this test runs).
fn jobs_term() -> String {
    let source = include_str!("../../erp-cli/src/human.rs");
    let constant = source
        .split("const MISSING_RUNTIME_GRANTS_SQL: &str = \"")
        .nth(1)
        .expect("the publication predicate");
    let constant = constant.split("\";").next().expect("end of the predicate");
    let term = constant
        .split("/*erp_jobs*/")
        .nth(1)
        .expect("the erp_jobs term");
    term.split("/*end erp_jobs*/")
        .next()
        .expect("end marker")
        .to_owned()
}

async fn flags(client: &Client) -> bool {
    client
        .query_one(&format!("SELECT {}", jobs_term()), &[])
        .await
        .unwrap()
        .get(0)
}

#[tokio::test]
async fn the_publication_check_passes_the_granted_state_and_flags_every_deviation() {
    let f = fixture().await;
    let owner = f.h.db().client().await.unwrap();
    let role = erp_sql::quote(&f.role);
    assert!(!flags(&f.runtime).await, "the granted state must pass");
    let deviations = [
        ("table UPDATE", format!("GRANT UPDATE ON erp_job TO {role}")),
        (
            "table TRUNCATE",
            format!("GRANT TRUNCATE ON erp_job_run TO {role}"),
        ),
        (
            "table TRIGGER",
            format!("GRANT TRIGGER ON erp_job_item_failure TO {role}"),
        ),
        (
            "table REFERENCES",
            format!("GRANT REFERENCES ON erp_job TO {role}"),
        ),
        (
            "payload UPDATE",
            format!("GRANT UPDATE (payload) ON erp_job TO {role}"),
        ),
        (
            "principal UPDATE",
            format!("GRANT UPDATE (run_as_user) ON erp_job TO {role}"),
        ),
        (
            "history UPDATE",
            format!("GRANT UPDATE (outcome) ON erp_job_run TO {role}"),
        ),
        (
            "failure key UPDATE",
            format!("GRANT UPDATE (item_key) ON erp_job_item_failure TO {role}"),
        ),
        (
            "PUBLIC UPDATE",
            "GRANT UPDATE ON erp_job TO PUBLIC".to_owned(),
        ),
        (
            "missing DELETE",
            format!("REVOKE DELETE ON erp_job FROM {role}"),
        ),
        (
            "missing INSERT",
            format!("REVOKE INSERT ON erp_job_run FROM {role}"),
        ),
        (
            "missing SELECT",
            format!("REVOKE SELECT ON erp_job_item_failure FROM {role}"),
        ),
    ];
    for (what, change) in deviations {
        owner.batch_execute(&change).await.unwrap();
        assert!(flags(&f.runtime).await, "{what} must be flagged");
        let mut tx = f.h.begin().await.unwrap();
        tx.execute("SELECT 1", &[]).await.unwrap();
        tx.grant_jobs_runtime_role(&f.role).await.unwrap();
        tx.commit().await.unwrap();
        owner
            .batch_execute("REVOKE ALL ON erp_job, erp_job_run, erp_job_item_failure FROM PUBLIC")
            .await
            .unwrap();
        assert!(!flags(&f.runtime).await, "re-granting repairs {what}");
    }
    f.h.cleanup().await.unwrap();
}
