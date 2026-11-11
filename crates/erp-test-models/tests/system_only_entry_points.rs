//! Entry points the module system calls on its own `System` transaction refuse every other
//! principal themselves (S2 of the security review): `access_entry_points.rs` lists them as
//! module-operation plumbing, and this file proves the refusal per function.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

use std::sync::Arc;

use erp_orm::{UserId, model_hooks};
use erp_registry::RegistryBuilder;
use erp_sdk::{ErrorKind, Orm, SUPERUSER_ID, StaticPolicy};
use erp_test_models::base_users;
use erp_testkit::TestDb;

fn orm_on(db: &TestDb) -> Orm {
    let registry = Arc::new(RegistryBuilder::new([&base_users::ADDON]).build().unwrap());
    Orm::builder(registry, db.pool().clone())
        .policy(StaticPolicy::AllowAll)
        .build()
        .unwrap()
}

fn refusal(call: &str, uid: i64) -> String {
    format!(
        "{call}: only a transaction of the System principal with full elevation may do this \
         (this one acts as user {uid})"
    )
}

#[tokio::test]
async fn call_init_refuses_a_user_transaction() {
    let db = TestDb::create().await.unwrap();
    let orm = orm_on(&db);
    let mut user = orm.begin(UserId::new(2).unwrap()).await.unwrap();
    let err = model_hooks::call_init(&mut user, "res.users")
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access);
    assert_eq!(err.message(), refusal("model_hooks::call_init", 2));
    user.rollback().await.unwrap();
    drop(user);
    let mut system = orm.begin(SUPERUSER_ID).await.unwrap();
    model_hooks::call_init(&mut system, "res.users")
        .await
        .unwrap();
    system.rollback().await.unwrap();
    drop(system);
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn call_register_hook_refuses_a_user_transaction() {
    let db = TestDb::create().await.unwrap();
    let orm = orm_on(&db);
    let mut user = orm.begin(UserId::new(2).unwrap()).await.unwrap();
    let err = model_hooks::call_register_hook(&mut user, "res.users")
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access);
    assert_eq!(err.message(), refusal("model_hooks::call_register_hook", 2));
    user.rollback().await.unwrap();
    drop(user);
    let mut system = orm.begin(SUPERUSER_ID).await.unwrap();
    model_hooks::call_register_hook(&mut system, "res.users")
        .await
        .unwrap();
    system.rollback().await.unwrap();
    drop(system);
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn open_installing_signal_refuses_a_user_transaction() {
    let db = TestDb::create().await.unwrap();
    let orm = orm_on(&db);
    let mut user = orm.begin(UserId::new(2).unwrap()).await.unwrap();
    let err = user.open_installing_signal().await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access);
    assert_eq!(
        err.message(),
        refusal("Transaction::open_installing_signal", 2)
    );
    user.rollback().await.unwrap();
    drop(user);
    let mut system = orm.begin(SUPERUSER_ID).await.unwrap();
    system.open_installing_signal().await.unwrap();
    system.rollback().await.unwrap();
    drop(system);
    db.cleanup().await.unwrap();
}

/// SCALE.NODES review round 1 (DB 1): the role grants change privileges, so only `System` may
/// call them; a `System` transaction gets past the principal check to the function's own checks.
#[tokio::test]
async fn the_role_grants_refuse_a_user_transaction() {
    let db = TestDb::create().await.unwrap();
    let orm = orm_on(&db);
    let mut user = orm.begin(UserId::new(2).unwrap()).await.unwrap();
    let err = user.grant_node_role("anyone").await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access);
    assert_eq!(err.message(), refusal("Transaction::grant_node_role", 2));
    let err = user.grant_signal_request_role("anyone").await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access);
    assert_eq!(
        err.message(),
        refusal("Transaction::grant_signal_request_role", 2)
    );
    let err = user.grant_vacuum_role("anyone").await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access);
    assert_eq!(err.message(), refusal("Transaction::grant_vacuum_role", 2));
    let err = user.revoke_vacuum_request_role("anyone").await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access);
    assert_eq!(
        err.message(),
        refusal("Transaction::revoke_vacuum_request_role", 2)
    );
    user.rollback().await.unwrap();
    drop(user);
    // no schema sync here: System reaches the missing-table check
    let mut system = orm.begin(SUPERUSER_ID).await.unwrap();
    let err = system.grant_node_role("anyone").await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "the node table erp_node is missing: run the schema sync (Orm::sync_schema) before \
         granting the heartbeat role"
    );
    system.rollback().await.unwrap();
    drop(system);
    db.cleanup().await.unwrap();
}

/// `Transaction::call_selection_callback` requires a system transaction.
#[tokio::test]
async fn call_selection_callback_refuses_a_user_transaction() {
    let db = TestDb::create().await.unwrap();
    let orm = orm_on(&db);
    let mut user = orm.begin(UserId::new(2).unwrap()).await.unwrap();
    let err = user
        .call_selection_callback("test_cb", "res.partner", &[])
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access);
    assert_eq!(
        err.message(),
        refusal("Transaction::call_selection_callback", 2)
    );
    let err_co = user
        .call_selection_callback_in_company("test_cb", "res.partner", &[], Some(1))
        .await
        .unwrap_err();
    assert_eq!(err_co.kind(), ErrorKind::Access);
    assert_eq!(
        err_co.message(),
        refusal("Transaction::call_selection_callback_in_company", 2)
    );
    user.rollback().await.unwrap();
    drop(user);

    let mut system = orm.begin(SUPERUSER_ID).await.unwrap();
    let err = system
        .call_selection_callback("test_cb", "res.partner", &[])
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "selection ondelete callback 'test_cb' is not registered on the ORM"
    );
    let err_co = system
        .call_selection_callback_in_company("test_cb", "res.partner", &[], Some(1))
        .await
        .unwrap_err();
    assert_eq!(err_co.kind(), ErrorKind::Programming);
    assert_eq!(
        err_co.message(),
        "selection ondelete callback 'test_cb' is not registered on the ORM"
    );
    system.rollback().await.unwrap();
    drop(system);
    db.cleanup().await.unwrap();
}

/// The hidden native lifecycle preflight has no record-access policy bypass: it admits only
/// an idle, fully elevated System transaction, and leaves transaction statements unopened.
#[tokio::test]
async fn native_catalog_preflight_requires_system_and_idle_state() {
    let db = TestDb::create().await.unwrap();
    let orm = orm_on(&db);
    let mut user = orm.begin(UserId::new(2).unwrap()).await.unwrap();
    user.capture_statements(true);
    let error = user.preflight_native_catalog_sources().await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Access);
    assert_eq!(
        error.message(),
        refusal("Transaction::preflight_native_catalog_sources", 2)
    );
    assert!(
        user.captured().is_empty(),
        "rejected principal must not start transaction statements"
    );
    user.rollback().await.unwrap();
    drop(user);

    let mut system = orm.begin(SUPERUSER_ID).await.unwrap();
    system.capture_statements(true);
    system.preflight_native_catalog_sources().await.unwrap();
    assert!(
        system.captured().is_empty(),
        "metadata preflight must not issue BEGIN or application statements"
    );
    system.query("SELECT 1", &[]).await.unwrap();
    let error = system.preflight_native_catalog_sources().await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.message(),
        "Native lifecycle source preflight requires an idle transaction"
    );
    system.rollback().await.unwrap();
    drop(system);
    db.cleanup().await.unwrap();
}

#[derive(Clone, Copy)]
enum EventDdl {
    Install,
    EnsurePartitions,
    GrantRuntimeRole,
    InstallAudit,
    EnsureAuditPartitions,
}

impl EventDdl {
    fn call(self) -> &'static str {
        match self {
            Self::Install => "Transaction::install_event_schema",
            Self::EnsurePartitions => "Transaction::ensure_event_partitions",
            Self::GrantRuntimeRole => "Transaction::grant_event_runtime_role",
            Self::InstallAudit => "Transaction::install_audit_schema",
            Self::EnsureAuditPartitions => "Transaction::ensure_audit_partitions",
        }
    }
}

async fn event_ddl_refuses_user_before_sql(operation: EventDdl) {
    let db = TestDb::create().await.unwrap();
    let orm = orm_on(&db);
    let mut user = orm.begin(UserId::new(2).unwrap()).await.unwrap();
    user.capture_statements(true);
    let result = match operation {
        EventDdl::Install => user.install_event_schema().await.map(|_| ()),
        EventDdl::EnsurePartitions => user.ensure_event_partitions().await.map(|_| ()),
        EventDdl::GrantRuntimeRole => user.grant_event_runtime_role("anyone").await,
        EventDdl::InstallAudit => user.install_audit_schema().await.map(|_| ()),
        EventDdl::EnsureAuditPartitions => user.ensure_audit_partitions().await.map(|_| ()),
    };
    let error = result.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Access, "{}", operation.call());
    assert_eq!(error.message(), refusal(operation.call(), 2));
    assert!(
        user.captured().is_empty(),
        "{} must refuse before SQL: {:?}",
        operation.call(),
        user.captured()
    );
    user.rollback().await.unwrap();
    drop(user);
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn install_event_schema_refuses_a_user_before_sql() {
    event_ddl_refuses_user_before_sql(EventDdl::Install).await;
}

#[tokio::test]
async fn ensure_event_partitions_refuses_a_user_before_sql() {
    event_ddl_refuses_user_before_sql(EventDdl::EnsurePartitions).await;
}

#[tokio::test]
async fn grant_event_runtime_role_refuses_a_user_before_sql() {
    event_ddl_refuses_user_before_sql(EventDdl::GrantRuntimeRole).await;
}

#[tokio::test]
async fn install_audit_schema_refuses_a_user_before_sql() {
    event_ddl_refuses_user_before_sql(EventDdl::InstallAudit).await;
}

#[tokio::test]
async fn ensure_audit_partitions_refuses_a_user_before_sql() {
    event_ddl_refuses_user_before_sql(EventDdl::EnsureAuditPartitions).await;
}
