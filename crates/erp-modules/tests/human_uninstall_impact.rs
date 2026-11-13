//! Lifecycle seam tests use a trusted test authorizer, not fake authentication evidence.
#![allow(clippy::unwrap_used, clippy::panic, reason = "test assertions")]
mod common;
use common::*;
use erp_fields::ErrorKind;
use erp_modules::error::Error;
use erp_modules::install::{InstallAuthorization, InstallAuthorizer};
use erp_modules::system::{ModuleSystem, OperationOutcome};
use erp_modules::{InitOptions, OPERATION_LOCK};
use erp_orm::{BoxFuture, GroupSet, PrincipalBuilder, Resolution, Transaction, UserId};
use erp_registry::AddonDescriptor;
use erp_testkit::TestDb;
use std::num::NonZeroU64;
use std::sync::atomic::{AtomicUsize, Ordering};

static NOTE: AddonDescriptor = AddonDescriptor {
    name: "note",
    depends: &["base"],
    exact_only: false,
};
struct Authorizer {
    deny: bool,
    uid: i64,
    calls: AtomicUsize,
}
impl InstallAuthorizer for Authorizer {
    fn authorize<'a>(
        &'a self,
        tx: &'a mut Transaction,
    ) -> BoxFuture<'a, erp_fields::Result<InstallAuthorization>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let locked: bool = tx.query("SELECT EXISTS (SELECT 1 FROM pg_locks WHERE locktype='advisory' AND granted AND pid=pg_backend_pid() AND classid=$1::int8::oid AND objid=$2::int8::oid AND objsubid=2)", &[&i64::from(OPERATION_LOCK.0), &i64::from(OPERATION_LOCK.1)]).await?[0].get(0);
            assert!(locked, "authority must be checked under the lifecycle lock");
            assert!(tx.commit().await.is_err(), "authorizer must not own commit");
            if self.deny {
                return Err(erp_fields::Error::access("revoked system authority"));
            }
            let principal = PrincipalBuilder::user(UserId::new(self.uid).unwrap())
                .resolution(
                    Resolution::in_id_order(GroupSet::default(), [], NonZeroU64::MIN).unwrap(),
                )
                .build()
                .unwrap();
            InstallAuthorization::new(principal, "019a0000-0000-7000-8000-000000000001".into())
        })
    }
}
/// Preview and confirmed execution agree on the exact dropped model and preserve data until confirmation.
#[tokio::test]
async fn preview_is_read_only_and_confirmed_uninstall_drops_exact_model() {
    let db = TestDb::create().await.unwrap();
    let addons = Addons::new();
    addons.add("note", "name='Note'\ndepends=['base']\n");
    let system = builder(&db, &addons)
        .code([&NOTE])
        .decls([model("note", "test.note")])
        .build()
        .unwrap();
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    ensure_signal_schema(&init.orm).await;
    exec(
        &init.orm,
        "INSERT INTO res_users(id, name) VALUES (2, 'Administrator')",
    )
    .await;
    let authorizer = Authorizer {
        deny: false,
        uid: 2,
        calls: AtomicUsize::new(0),
    };
    let installed = system
        .install_authorized(&[n("note")], &authorizer)
        .await
        .unwrap();
    exec(
        &installed.orm,
        "INSERT INTO test_note(id, name) VALUES (11, 'secret first'), (22, 'secret second')",
    )
    .await;
    let before_states = states(&installed.orm).await;
    let before_audit = rows(&installed.orm, "SELECT actor_uid::text, session_id::text, requested::text FROM erp_module_install_audit ORDER BY id").await;
    let before_lifecycle_audit = rows(
        &installed.orm,
        "SELECT row_to_json(a)::text FROM erp_module_lifecycle_audit a ORDER BY id",
    )
    .await;
    let before_data = rows(
        &installed.orm,
        "SELECT id::text, name FROM test_note ORDER BY id",
    )
    .await;

    let impact = system
        .preview_uninstall_authorized(&[n("note")], &authorizer)
        .await
        .unwrap();
    assert_eq!(impact.requested(), &[n("note")]);
    assert_eq!(impact.resolved(), &[n("note")]);
    assert_eq!(impact.dropped_models().len(), 1);
    let dropped = &impact.dropped_models()[0];
    assert_eq!(dropped.model(), "test.note");
    assert_eq!(dropped.table(), "test_note");
    assert_eq!(dropped.row_count(), "2");
    assert!(!impact.fingerprint().as_str().is_empty());
    let serialized = serde_json::to_string(&impact).unwrap();
    assert!(!serialized.contains("secret first"));
    assert!(!serialized.contains("secret second"));
    assert!(!serialized.contains("record_ids"));
    assert_eq!(states(&installed.orm).await, before_states);
    assert_eq!(rows(&installed.orm, "SELECT actor_uid::text, session_id::text, requested::text FROM erp_module_install_audit ORDER BY id").await, before_audit);
    assert_eq!(
        rows(
            &installed.orm,
            "SELECT id::text, name FROM test_note ORDER BY id"
        )
        .await,
        before_data
    );

    assert_eq!(
        rows(
            &installed.orm,
            "SELECT row_to_json(a)::text FROM erp_module_lifecycle_audit a ORDER BY id"
        )
        .await,
        before_lifecycle_audit
    );
    let removed = system
        .uninstall_authorized(&[n("note")], impact.fingerprint().as_str(), &authorizer)
        .await
        .unwrap();
    assert_eq!(authorizer.calls.load(Ordering::SeqCst), 5);
    assert_eq!(state(&removed.orm, "note").await, "uninstalled");
    assert!(!table_exists(&removed.orm, "test_note").await);
    assert_eq!(state(&removed.orm, "base").await, "installed");
    assert_eq!(rows(&removed.orm, "SELECT actor_uid::text, session_id::text, operation, requested::text, resolved::text, fingerprint, outcome FROM erp_module_lifecycle_audit WHERE operation='uninstall'").await,
        vec![vec![Some("2".into()), Some("019a0000-0000-7000-8000-000000000001".into()), Some("uninstall".into()), Some("{note}".into()), Some("{note}".into()), Some(impact.fingerprint().as_str().into()), Some("committed".into())]]);
    committed(removed);
    db.cleanup().await.unwrap();
}

fn allowed() -> Authorizer {
    Authorizer {
        deny: false,
        uid: 2,
        calls: AtomicUsize::new(0),
    }
}

async fn installed_note() -> (TestDb, Addons, ModuleSystem, OperationOutcome) {
    let db = TestDb::create().await.unwrap();
    let addons = Addons::new();
    addons.add("note", "name='Note'\ndepends=['base']\n");
    let system = builder(&db, &addons)
        .code([&NOTE])
        .decls([model("note", "test.note")])
        .build()
        .unwrap();
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    ensure_signal_schema(&init.orm).await;
    exec(
        &init.orm,
        "INSERT INTO res_users(id, name) VALUES (2, 'Administrator'), (3, 'Other administrator')",
    )
    .await;
    let installed = system
        .install_authorized(&[n("note")], &allowed())
        .await
        .unwrap();
    exec(
        &installed.orm,
        "INSERT INTO test_note(id, name) VALUES (11, 'secret record')",
    )
    .await;
    (db, addons, system, installed)
}

fn assert_error(error: Error, kind: ErrorKind, message: &str) {
    match error {
        Error::Orm(source) => {
            assert_eq!(source.kind(), kind);
            assert_eq!(source.message(), message);
        }
        other => panic!("expected typed ORM {kind:?}, got {other:?}"),
    }
}

async fn assert_note_intact(orm: &erp_orm::Orm) {
    assert_eq!(state(orm, "note").await, "installed");
    assert!(table_exists(orm, "test_note").await);
    assert_eq!(
        rows(
            orm,
            "SELECT count(*)::text FROM erp_module_lifecycle_audit WHERE operation='uninstall'"
        )
        .await,
        vec![vec![Some("0".into())]]
    );
}

#[tokio::test]
async fn same_count_membership_change_invalidates_confirmation() {
    let (db, _addons, system, installed) = installed_note().await;
    let before = system
        .preview_uninstall_authorized(&[n("note")], &allowed())
        .await
        .unwrap();
    exec(&installed.orm, "DELETE FROM test_note WHERE id=11; INSERT INTO test_note(id, name) VALUES (99, 'replacement secret')").await;
    let after = system
        .preview_uninstall_authorized(&[n("note")], &allowed())
        .await
        .unwrap();
    assert_eq!(
        before.dropped_models()[0].row_count(),
        after.dropped_models()[0].row_count()
    );
    assert_ne!(before.fingerprint().as_str(), after.fingerprint().as_str());
    assert_error(
        system
            .uninstall_authorized(&[n("note")], before.fingerprint().as_str(), &allowed())
            .await
            .unwrap_err(),
        ErrorKind::Conflict,
        "Uninstall impact changed; preview again before confirming",
    );
    assert_note_intact(&installed.orm).await;
    assert_eq!(
        rows(&installed.orm, "SELECT id::text FROM test_note").await,
        vec![vec![Some("99".into())]]
    );
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn changed_row_count_invalidates_confirmation() {
    let (db, _addons, system, installed) = installed_note().await;
    let before = system
        .preview_uninstall_authorized(&[n("note")], &allowed())
        .await
        .unwrap();
    exec(
        &installed.orm,
        "INSERT INTO test_note(id, name) VALUES (99, 'additional secret')",
    )
    .await;
    let after = system
        .preview_uninstall_authorized(&[n("note")], &allowed())
        .await
        .unwrap();
    assert_eq!(before.dropped_models()[0].row_count(), "1");
    assert_eq!(after.dropped_models()[0].row_count(), "2");
    assert_error(
        system
            .uninstall_authorized(&[n("note")], before.fingerprint().as_str(), &allowed())
            .await
            .unwrap_err(),
        ErrorKind::Conflict,
        "Uninstall impact changed; preview again before confirming",
    );
    assert_note_intact(&installed.orm).await;
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn different_actor_cannot_reuse_confirmation() {
    let (db, _addons, system, installed) = installed_note().await;
    let before = system
        .preview_uninstall_authorized(&[n("note")], &allowed())
        .await
        .unwrap();
    let other = Authorizer {
        uid: 3,
        ..allowed()
    };
    let after = system
        .preview_uninstall_authorized(&[n("note")], &other)
        .await
        .unwrap();
    assert_ne!(before.fingerprint().as_str(), after.fingerprint().as_str());
    assert_error(
        system
            .uninstall_authorized(&[n("note")], before.fingerprint().as_str(), &other)
            .await
            .unwrap_err(),
        ErrorKind::Conflict,
        "Uninstall impact changed; preview again before confirming",
    );
    assert_note_intact(&installed.orm).await;
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn database_identity_binds_confirmation() {
    let (db_a, _addons_a, system_a, _installed_a) = installed_note().await;
    let (db_b, _addons_b, system_b, installed_b) = installed_note().await;
    let a = system_a
        .preview_uninstall_authorized(&[n("note")], &allowed())
        .await
        .unwrap();
    let b = system_b
        .preview_uninstall_authorized(&[n("note")], &allowed())
        .await
        .unwrap();
    assert_ne!(a.fingerprint().as_str(), b.fingerprint().as_str());
    assert_error(
        system_b
            .uninstall_authorized(&[n("note")], a.fingerprint().as_str(), &allowed())
            .await
            .unwrap_err(),
        ErrorKind::Conflict,
        "Uninstall impact changed; preview again before confirming",
    );
    assert_note_intact(&installed_b.orm).await;
    db_a.cleanup().await.unwrap();
    db_b.cleanup().await.unwrap();
}

#[tokio::test]
async fn durable_generation_invalidates_confirmation_across_systems() {
    let (db, addons, system, installed) = installed_note().await;
    let before = system
        .preview_uninstall_authorized(&[n("note")], &allowed())
        .await
        .unwrap();
    exec(
        &installed.orm,
        "INSERT INTO erp_signal_generation(key) VALUES ('registry')",
    )
    .await;
    let other = builder(&db, &addons)
        .code([&NOTE])
        .decls([model("note", "test.note")])
        .build()
        .unwrap();
    let after = other
        .preview_uninstall_authorized(&[n("note")], &allowed())
        .await
        .unwrap();
    assert_ne!(before.fingerprint().as_str(), after.fingerprint().as_str());
    assert_error(
        other
            .uninstall_authorized(&[n("note")], before.fingerprint().as_str(), &allowed())
            .await
            .unwrap_err(),
        ErrorKind::Conflict,
        "Uninstall impact changed; preview again before confirming",
    );
    assert_note_intact(&installed.orm).await;
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn revoked_authority_refuses_preview_and_execution_without_effects() {
    let (db, _addons, system, installed) = installed_note().await;
    let before = system
        .preview_uninstall_authorized(&[n("note")], &allowed())
        .await
        .unwrap();
    let revoked = Authorizer {
        deny: true,
        ..allowed()
    };
    assert_error(
        system
            .preview_uninstall_authorized(&[n("note")], &revoked)
            .await
            .unwrap_err(),
        ErrorKind::Access,
        "revoked system authority",
    );
    assert_error(
        system
            .uninstall_authorized(&[n("note")], before.fingerprint().as_str(), &revoked)
            .await
            .unwrap_err(),
        ErrorKind::Access,
        "revoked system authority",
    );
    assert_eq!(revoked.calls.load(Ordering::SeqCst), 2);
    assert_note_intact(&installed.orm).await;
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn protected_base_refuses_without_effects() {
    let (db, _addons, system, installed) = installed_note().await;
    let before = states(&installed.orm).await;
    assert_error(
        system
            .preview_uninstall_authorized(&[n("base")], &allowed())
            .await
            .unwrap_err(),
        ErrorKind::Validation,
        "Uninstall cannot remove addon 'base' required by protected addon 'base'",
    );
    assert_error(
        system
            .uninstall_authorized(
                &[n("base")],
                &format!("uninstall-v1:{}", "0".repeat(64)),
                &allowed(),
            )
            .await
            .unwrap_err(),
        ErrorKind::Validation,
        "Uninstall cannot remove addon 'base' required by protected addon 'base'",
    );
    assert_eq!(states(&installed.orm).await, before);
    assert_note_intact(&installed.orm).await;
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn invalid_requests_refuse_without_effects() {
    let (db, addons, system, installed) = installed_note().await;
    addons.add("extra", "name='Extra'\ndepends=['base']\n");
    system.update_list(&su(&installed.orm)).await.unwrap();
    let before = states(&installed.orm).await;
    let excessive: Vec<_> = (0..65).map(|index| n(&format!("addon_{index}"))).collect();
    for (names, diagnostic) in [
        (vec![], "Uninstall requires between 1 and 64 addon names"),
        (
            vec![n("note"), n("note")],
            "Uninstall requires unique addon names",
        ),
        (
            vec![n("missing")],
            "Uninstall requires installed addon 'missing'",
        ),
        (
            vec![n("extra")],
            "Uninstall requires installed addon 'extra'",
        ),
        (excessive, "Uninstall requires between 1 and 64 addon names"),
    ] {
        assert_error(
            system
                .preview_uninstall_authorized(&names, &allowed())
                .await
                .unwrap_err(),
            ErrorKind::Validation,
            diagnostic,
        );
        assert_error(
            system
                .uninstall_authorized(
                    &names,
                    &format!("uninstall-v1:{}", "0".repeat(64)),
                    &allowed(),
                )
                .await
                .unwrap_err(),
            ErrorKind::Validation,
            diagnostic,
        );
        assert_eq!(states(&installed.orm).await, before);
        assert_note_intact(&installed.orm).await;
    }
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn pending_operation_refuses_without_effects() {
    let (db, addons, system, installed) = installed_note().await;
    let impact = system
        .preview_uninstall_authorized(&[n("note")], &allowed())
        .await
        .unwrap();
    addons.add("extra", "name='Extra'\ndepends=['base']\n");
    system.update_list(&su(&installed.orm)).await.unwrap();
    system
        .button_install(
            &su(&installed.orm),
            &[n("extra")],
            &erp_modules::InstallOptions::new(),
        )
        .await
        .unwrap();
    let before = states(&installed.orm).await;
    assert_error(
        system
            .preview_uninstall_authorized(&[n("note")], &allowed())
            .await
            .unwrap_err(),
        ErrorKind::Validation,
        "Pending module operations must be resolved before human lifecycle operation",
    );
    assert_error(
        system
            .uninstall_authorized(&[n("note")], impact.fingerprint().as_str(), &allowed())
            .await
            .unwrap_err(),
        ErrorKind::Validation,
        "Pending module operations must be resolved before human lifecycle operation",
    );
    assert_eq!(states(&installed.orm).await, before);
    assert_note_intact(&installed.orm).await;
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn audit_failure_rolls_back_uninstall() {
    let (db, _addons, system, installed) = installed_note().await;
    exec(&installed.orm, "CREATE TABLE fixture_empty_actor(id bigint PRIMARY KEY); ALTER TABLE erp_module_lifecycle_audit ADD CONSTRAINT fixture_reject_uninstall FOREIGN KEY(actor_uid) REFERENCES fixture_empty_actor(id) NOT VALID").await;
    let impact = system
        .preview_uninstall_authorized(&[n("note")], &allowed())
        .await
        .unwrap();
    assert_error(
        system
            .uninstall_authorized(&[n("note")], impact.fingerprint().as_str(), &allowed())
            .await
            .unwrap_err(),
        ErrorKind::Validation,
        "The operation cannot be completed: insert or update on table \"erp_module_lifecycle_audit\" violates foreign key constraint \"fixture_reject_uninstall\"",
    );
    assert_note_intact(&installed.orm).await;
    assert_eq!(
        rows(&installed.orm, "SELECT id::text, name FROM test_note").await,
        vec![vec![Some("11".into()), Some("secret record".into())]]
    );
    db.cleanup().await.unwrap();
}

static CHILD: AddonDescriptor = AddonDescriptor {
    name: "child",
    depends: &["note"],
    exact_only: false,
};

#[tokio::test]
async fn preview_resolves_reverse_dependency_closure_deterministically() {
    let db = TestDb::create().await.unwrap();
    let addons = Addons::new();
    addons
        .add("note", "name='Note'\ndepends=['base']\n")
        .add("child", "name='Child'\ndepends=['note']\n");
    let system = builder(&db, &addons)
        .code([&NOTE, &CHILD])
        .decls([model("note", "test.note"), model("child", "test.child")])
        .build()
        .unwrap();
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    ensure_signal_schema(&init.orm).await;
    exec(
        &init.orm,
        "INSERT INTO res_users(id, name) VALUES (2, 'Administrator')",
    )
    .await;
    let installed = system
        .install_authorized(&[n("child")], &allowed())
        .await
        .unwrap();
    let preview = system
        .preview_uninstall_authorized(&[n("note")], &allowed())
        .await
        .unwrap();
    assert_eq!(preview.requested(), &[n("note")]);
    assert_eq!(preview.resolved(), &[n("child"), n("note")]);
    let models: Vec<_> = preview
        .dropped_models()
        .iter()
        .map(|drop| (drop.model(), drop.row_count()))
        .collect();
    assert_eq!(models, vec![("test.child", "0"), ("test.note", "0")]);
    let a = system
        .preview_uninstall_authorized(&[n("note"), n("child")], &allowed())
        .await
        .unwrap();
    let b = system
        .preview_uninstall_authorized(&[n("child"), n("note")], &allowed())
        .await
        .unwrap();
    assert_eq!(a.requested(), b.requested());
    assert_eq!(a.fingerprint().as_str(), b.fingerprint().as_str());
    let removed = system
        .uninstall_authorized(&[n("note")], preview.fingerprint().as_str(), &allowed())
        .await
        .unwrap();
    assert_eq!(state(&removed.orm, "child").await, "uninstalled");
    assert_eq!(state(&removed.orm, "note").await, "uninstalled");
    assert!(!table_exists(&removed.orm, "test_child").await);
    assert!(!table_exists(&removed.orm, "test_note").await);
    assert_eq!(state(&installed.orm, "base").await, "installed");
    committed(removed);
    db.cleanup().await.unwrap();
}

fn forbidden_uninstall_hook(_tx: &mut Transaction) -> BoxFuture<'_, erp_fields::Result<()>> {
    Box::pin(async { panic!("unplanned uninstall hook must never run") })
}
static FORBIDDEN_HOOK: erp_modules::HookImpl = erp_modules::HookImpl::new(
    "note",
    erp_modules::HookKind::Uninstall,
    "forbidden_uninstall_hook",
    forbidden_uninstall_hook,
);

#[tokio::test]
async fn unsupported_hook_refuses_before_any_effect() {
    let (db, addons, _system, installed) = installed_note().await;
    addons.add(
        "note",
        "name='Note'\ndepends=['base']\nuninstall_hook='forbidden_uninstall_hook'\n",
    );
    let system = builder(&db, &addons)
        .code([&NOTE])
        .decls([model("note", "test.note")])
        .hooks([&FORBIDDEN_HOOK])
        .build()
        .unwrap();
    assert_error(
        system
            .preview_uninstall_authorized(&[n("note")], &allowed())
            .await
            .unwrap_err(),
        ErrorKind::Validation,
        "Uninstall impact cannot describe uninstall hook of addon 'note'; unsupported until a later release",
    );
    assert_error(
        system
            .uninstall_authorized(
                &[n("note")],
                &format!("uninstall-v1:{}", "0".repeat(64)),
                &allowed(),
            )
            .await
            .unwrap_err(),
        ErrorKind::Validation,
        "Uninstall impact cannot describe uninstall hook of addon 'note'; unsupported until a later release",
    );
    assert_note_intact(&installed.orm).await;
    db.cleanup().await.unwrap();
}

async fn assert_survivor_refused(
    system: &ModuleSystem,
    installed: &OperationOutcome,
    effect: &str,
) {
    let before_states = states(&installed.orm).await;
    let before_audit = rows(
        &installed.orm,
        "SELECT row_to_json(a)::text FROM erp_module_lifecycle_audit a ORDER BY id",
    )
    .await;
    let survivor_rows = if table_exists(&installed.orm, "fixture_survivor").await {
        Some(
            rows(
                &installed.orm,
                "SELECT row_to_json(s)::text FROM fixture_survivor s ORDER BY id",
            )
            .await,
        )
    } else {
        None
    };
    let before_xids = rows(
        &installed.orm,
        "SELECT module,name,model,res_id::text FROM ir_model_data ORDER BY module,name",
    )
    .await;
    let before_users = rows(
        &installed.orm,
        "SELECT id::text,name FROM res_users ORDER BY id",
    )
    .await;
    let diagnostic =
        format!("Uninstall impact cannot describe {effect}; unsupported until a later release");
    assert_error(
        system
            .preview_uninstall_authorized(&[n("note")], &allowed())
            .await
            .unwrap_err(),
        ErrorKind::Validation,
        &diagnostic,
    );
    assert_error(
        system
            .uninstall_authorized(
                &[n("note")],
                &format!("uninstall-v1:{}", "0".repeat(64)),
                &allowed(),
            )
            .await
            .unwrap_err(),
        ErrorKind::Validation,
        &diagnostic,
    );
    assert_eq!(states(&installed.orm).await, before_states);
    assert_eq!(
        rows(
            &installed.orm,
            "SELECT row_to_json(a)::text FROM erp_module_lifecycle_audit a ORDER BY id"
        )
        .await,
        before_audit
    );
    if let Some(before) = survivor_rows {
        assert_eq!(
            rows(
                &installed.orm,
                "SELECT row_to_json(s)::text FROM fixture_survivor s ORDER BY id"
            )
            .await,
            before
        );
    }
    assert_eq!(
        rows(
            &installed.orm,
            "SELECT module,name,model,res_id::text FROM ir_model_data ORDER BY module,name"
        )
        .await,
        before_xids
    );
    assert_eq!(
        rows(
            &installed.orm,
            "SELECT id::text,name FROM res_users ORDER BY id"
        )
        .await,
        before_users
    );
    assert_note_intact(&installed.orm).await;
    assert_eq!(
        rows(
            &installed.orm,
            "SELECT id::text,name FROM test_note ORDER BY id"
        )
        .await,
        vec![vec![Some("11".into()), Some("secret record".into())]]
    );
}

#[tokio::test]
async fn survivor_builtin_unlink_refuses_before_deletion() {
    let (db, _addons, system, installed) = installed_note().await;
    exec(&installed.orm, "INSERT INTO ir_model_data(module,name,model,res_id,noupdate) VALUES ('note','surviving_user','res.users',3,false)").await;
    assert_survivor_refused(
        &system,
        &installed,
        "unlink hook of survivor model 'res.users'",
    )
    .await;
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn shared_survivor_unlink_refusal_preserves_all_ownership() {
    let (db, _addons, system, installed) = installed_note().await;
    exec(&installed.orm, "INSERT INTO ir_model_data(module,name,model,res_id,noupdate) VALUES ('note','surviving_user','res.users',3,false),('base','shared_user','res.users',3,false)").await;
    assert_survivor_refused(
        &system,
        &installed,
        "unlink hook of survivor model 'res.users'",
    )
    .await;
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn unavailable_survivor_model_refuses_before_deletion() {
    let (db, _addons, system, installed) = installed_note().await;
    exec(&installed.orm, "INSERT INTO ir_model_data(module,name,model,res_id,noupdate) VALUES ('note','missing_survivor','fixture.missing',7,false)").await;
    assert_survivor_refused(
        &system,
        &installed,
        "survivor record deletion for unavailable model 'fixture.missing'",
    )
    .await;
    db.cleanup().await.unwrap();
}

async fn installed_note_with_survivor(
    decl: erp_registry::ModelDecl,
    impls: &[&'static erp_orm::MethodImpl],
) -> (TestDb, Addons, ModuleSystem, OperationOutcome) {
    static SURVIVOR: AddonDescriptor = AddonDescriptor {
        name: "survivor",
        depends: &["base"],
        exact_only: false,
    };
    let db = TestDb::create().await.unwrap();
    let addons = Addons::new();
    addons.add("note", "name='Note'\ndepends=['base']\n");
    addons.add("survivor", "name='Survivor'\ndepends=['base']\n");
    let decl: &'static erp_registry::ModelDecl = Box::leak(Box::new(decl));
    let system = builder(&db, &addons)
        .code([&NOTE, &SURVIVOR])
        .decls([model("note", "test.note"), decl])
        .impls(impls.iter().copied())
        .build()
        .unwrap();
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    ensure_signal_schema(&init.orm).await;
    exec(
        &init.orm,
        "INSERT INTO res_users(id,name) VALUES(2,'Administrator'),(3,'Other administrator')",
    )
    .await;
    let installed = system
        .install_authorized(&[n("survivor"), n("note")], &allowed())
        .await
        .unwrap();
    exec(&installed.orm, "INSERT INTO test_note(id,name) VALUES(11,'secret record'); INSERT INTO ir_model_data(module,name,model,res_id,noupdate) VALUES ('note','survivor','fixture.survivor',7,false)").await;
    (db, addons, system, installed)
}

#[tokio::test]
async fn tableless_survivor_model_refuses_before_deletion() {
    let decl = erp_registry::ModelDecl::new("survivor", "fixture.survivor")
        .description("Survivor")
        .kind(erp_registry::ModelKind::Abstract);
    let (db, _addons, system, installed) = installed_note_with_survivor(decl, &[]).await;
    assert_survivor_refused(
        &system,
        &installed,
        "survivor record deletion for table-less model 'fixture.survivor'",
    )
    .await;
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn partitioned_survivor_model_refuses_before_deletion() {
    let decl = erp_registry::ModelDecl::new("survivor", "fixture.survivor")
        .description("Survivor")
        .partition(erp_registry::PartitionDecl::new(
            erp_registry::PartitionInterval::Day,
            2,
            erp_registry::PartitionRetain::Intervals(3),
        ));
    let (db, _addons, system, installed) = installed_note_with_survivor(decl, &[]).await;
    // An actual partitioned relation is already refused by the schema inventory. Replace only
    // fixture storage to exercise the independent registry guard, without claiming schema parity.
    exec(&installed.orm, "DROP TABLE fixture_survivor CASCADE; CREATE TABLE fixture_survivor(id bigint PRIMARY KEY); INSERT INTO fixture_survivor VALUES(7)").await;
    assert_survivor_refused(
        &system,
        &installed,
        "survivor record deletion for partitioned model 'fixture.survivor'",
    )
    .await;
    db.cleanup().await.unwrap();
}

fn survivor_unlink_entry() -> &'static (dyn std::any::Any + Send + Sync) {
    fn unlink<'a>(
        _: &'a mut Transaction,
        _: &'a erp_orm::Records,
        _: (),
        _: erp_orm::Super<(), ()>,
    ) -> BoxFuture<'a, erp_fields::Result<()>> {
        Box::pin(async { panic!("unsupported survivor unlink callback ran") })
    }
    static F: erp_orm::MethodFn<(), ()> = unlink;
    &F
}
static SURVIVOR_UNLINK: erp_orm::MethodImpl = erp_orm::MethodImpl {
    addon: "survivor",
    model: "fixture.survivor",
    method: "unlink",
    kind: erp_orm::ImplKind::Override,
    sig_name: std::any::type_name::<erp_orm::MethodFn<(), ()>>,
    entry: survivor_unlink_entry,
    file: file!(),
    line: line!(),
};

#[tokio::test]
async fn custom_survivor_unlink_refuses_before_deletion() {
    let decl = erp_registry::ModelDecl::new("survivor", "fixture.survivor").description("Survivor");
    let (db, _addons, system, installed) =
        installed_note_with_survivor(decl, &[&SURVIVOR_UNLINK]).await;
    exec(&installed.orm, "INSERT INTO fixture_survivor(id) VALUES(7)").await;
    assert_survivor_refused(
        &system,
        &installed,
        "unlink hook of survivor model 'fixture.survivor'",
    )
    .await;
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn preview_target_cardinality_boundary_is_enforced() {
    let (db, _addons, system, installed) = installed_note().await;
    let limit = erp_modules::MAX_UNINSTALL_TARGETS;
    exec(&installed.orm, "DELETE FROM test_note").await;
    let baseline = system
        .preview_uninstall_authorized(&[n("note")], &allowed())
        .await
        .unwrap();
    let row_limit = limit.checked_sub(baseline.target_count()).unwrap();
    assert!(
        row_limit > 0,
        "fixture metadata already exhausts target admission"
    );
    exec(&installed.orm, &format!("INSERT INTO test_note(id, name) SELECT n, 'bounded' FROM generate_series(1, {row_limit}) n")).await;
    let at_limit = system
        .preview_uninstall_authorized(&[n("note")], &allowed())
        .await
        .unwrap();
    assert_eq!(at_limit.target_count(), limit);
    assert_eq!(
        at_limit.dropped_models()[0].row_count(),
        row_limit.to_string()
    );
    exec(
        &installed.orm,
        &format!(
            "INSERT INTO test_note(id, name) VALUES ({}, 'over bound')",
            row_limit + 1
        ),
    )
    .await;
    assert_error(
        system
            .preview_uninstall_authorized(&[n("note")], &allowed())
            .await
            .unwrap_err(),
        ErrorKind::Validation,
        &format!(
            "Uninstall impact exceeds {} target identities",
            erp_modules::MAX_UNINSTALL_TARGETS
        ),
    );
    assert_error(
        system
            .uninstall_authorized(&[n("note")], at_limit.fingerprint().as_str(), &allowed())
            .await
            .unwrap_err(),
        ErrorKind::Validation,
        &format!(
            "Uninstall impact exceeds {} target identities",
            erp_modules::MAX_UNINSTALL_TARGETS
        ),
    );
    assert_note_intact(&installed.orm).await;
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn preview_waits_for_target_lock_under_lifecycle_and_schema_locks() {
    use std::sync::Arc;
    use std::time::Duration;
    let (db, _addons, system, installed) = installed_note().await;
    let system = Arc::new(system);
    let mut blocker = installed.orm.begin(erp_orm::SUPERUSER_ID).await.unwrap();
    blocker
        .batch("LOCK TABLE test_note IN ACCESS EXCLUSIVE MODE")
        .await
        .unwrap();
    let task_system = Arc::clone(&system);
    let preview = tokio::spawn(async move {
        task_system
            .preview_uninstall_authorized(&[n("note")], &allowed())
            .await
    });
    assert!(eventually(Duration::from_secs(3), || async {
        rows(&installed.orm, "SELECT EXISTS(SELECT 1 FROM pg_locks WHERE relation='test_note'::regclass AND NOT granted)::text").await[0][0].as_deref() == Some("true")
    }).await, "planner never requested the target table lock");
    assert!(advisory_held(&installed.orm, OPERATION_LOCK).await);
    let schema_key: i64 = erp_sql::ddl::SCHEMA_LOCK
        .trim_start_matches("SELECT pg_advisory_xact_lock(")
        .trim_end_matches(')')
        .parse()
        .unwrap();
    assert_eq!(rows(&installed.orm, &format!("SELECT EXISTS (SELECT 1 FROM pg_locks WHERE locktype='advisory' AND granted AND objsubid=1 AND database=(SELECT oid FROM pg_database WHERE datname=current_database()) AND ((classid::int8 << 32) | objid::int8)={schema_key})::text")).await,
        vec![vec![Some("true".into())]]);
    blocker.rollback().await.unwrap();
    let impact = preview.await.unwrap().unwrap();
    assert_eq!(impact.dropped_models()[0].row_count(), "1");
    assert_note_intact(&installed.orm).await;
    db.cleanup().await.unwrap();
}

struct PausedPreviewAuthorizer {
    inner: Authorizer,
    continue_preview: tokio::sync::Notify,
}
impl InstallAuthorizer for PausedPreviewAuthorizer {
    fn authorize<'a>(
        &'a self,
        tx: &'a mut Transaction,
    ) -> BoxFuture<'a, erp_fields::Result<InstallAuthorization>> {
        Box::pin(async move {
            let authorization = self.inner.authorize(tx).await?;
            if self.inner.calls.load(Ordering::SeqCst) == 2 {
                self.continue_preview.notified().await;
            }
            Ok(authorization)
        })
    }
}

#[tokio::test]
async fn preview_membership_fence_allows_concurrent_read_and_write() {
    use std::sync::Arc;
    use std::time::Duration;
    let (db, _addons, system, installed) = installed_note().await;
    let system = Arc::new(system);
    let authorizer = Arc::new(PausedPreviewAuthorizer {
        inner: allowed(),
        continue_preview: tokio::sync::Notify::new(),
    });
    // An existing writer makes the old ACCESS EXCLUSIVE preview fence visibly queue.
    let mut writer = installed.orm.begin(erp_orm::SUPERUSER_ID).await.unwrap();
    writer
        .batch("LOCK TABLE test_note IN ROW EXCLUSIVE MODE")
        .await
        .unwrap();
    let task_system = Arc::clone(&system);
    let task_authorizer = Arc::clone(&authorizer);
    let preview = tokio::spawn(async move {
        task_system
            .preview_uninstall_authorized(&[n("note")], task_authorizer.as_ref())
            .await
    });
    assert!(eventually(Duration::from_secs(3), || async {
        authorizer.inner.calls.load(Ordering::SeqCst) == 2 ||
        rows(&installed.orm, "SELECT EXISTS(SELECT 1 FROM pg_locks WHERE relation='test_note'::regclass AND mode='AccessExclusiveLock' AND NOT granted)::text").await[0][0].as_deref() == Some("true")
    }).await, "preview reached neither its reauthorization barrier nor its old exclusive lock wait");
    let pending_exclusive = rows(&installed.orm, "SELECT EXISTS(SELECT 1 FROM pg_locks WHERE relation='test_note'::regclass AND mode='AccessExclusiveLock')::text").await;
    if pending_exclusive[0][0].as_deref() == Some("true") {
        preview.abort();
        writer.rollback().await.unwrap();
        assert_eq!(
            pending_exclusive,
            vec![vec![Some("false".into())]],
            "read-only preview requested ACCESS EXCLUSIVE while a writer held the target"
        );
        return;
    }
    assert_eq!(authorizer.inner.calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        rows(&installed.orm, "SELECT id::text FROM test_note").await,
        vec![vec![Some("11".into())]]
    );
    // A second writer must also proceed while preview is paused, not wait behind its fence.
    exec(
        &installed.orm,
        "INSERT INTO test_note(id,name) VALUES(22,'concurrent writer')",
    )
    .await;
    writer.rollback().await.unwrap();
    authorizer.continue_preview.notify_one();
    let impact = preview.await.unwrap().unwrap();
    assert_eq!(
        impact.dropped_models()[0].row_count(),
        "1",
        "preview keeps its repeatable-read snapshot"
    );
    assert_error(
        system
            .uninstall_authorized(&[n("note")], impact.fingerprint().as_str(), &allowed())
            .await
            .unwrap_err(),
        ErrorKind::Conflict,
        "Uninstall impact changed; preview again before confirming",
    );
    assert_note_intact(&installed.orm).await;
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn idless_metadata_fk_references_refuse_with_typed_effect() {
    let (db, _addons, system, installed) = installed_note().await;
    exec(&installed.orm, "CREATE TABLE ir_model_fields(id bigint PRIMARY KEY,model text); INSERT INTO ir_model_fields VALUES(1,'test.note'); CREATE TABLE fixture_field_rel(field_id bigint CONSTRAINT fixture_idless_metadata_fk REFERENCES ir_model_fields(id) ON DELETE CASCADE); INSERT INTO fixture_field_rel VALUES(1)").await;
    let diagnostic = "Uninstall impact cannot describe survivor foreign key effect 'fixture_idless_metadata_fk'; unsupported until a later release";
    assert_error(
        system
            .preview_uninstall_authorized(&[n("note")], &allowed())
            .await
            .unwrap_err(),
        ErrorKind::Validation,
        diagnostic,
    );
    assert_error(
        system
            .uninstall_authorized(
                &[n("note")],
                &format!("uninstall-v1:{}", "0".repeat(64)),
                &allowed(),
            )
            .await
            .unwrap_err(),
        ErrorKind::Validation,
        diagnostic,
    );
    assert_eq!(
        rows(
            &installed.orm,
            "SELECT field_id::text FROM fixture_field_rel"
        )
        .await,
        vec![vec![Some("1".into())]]
    );
    assert_note_intact(&installed.orm).await;
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn empty_idless_metadata_fk_allows_exact_uninstall() {
    let (db, _addons, system, installed) = installed_note().await;
    exec(&installed.orm, "CREATE TABLE ir_model_fields(id bigint PRIMARY KEY,model text); INSERT INTO ir_model_fields VALUES(1,'test.note'),(2,'base.retained'); CREATE TABLE fixture_field_rel(field_id bigint CONSTRAINT fixture_idless_metadata_fk REFERENCES ir_model_fields(id) ON DELETE CASCADE); INSERT INTO fixture_field_rel VALUES(2)").await;
    let impact = system
        .preview_uninstall_authorized(&[n("note")], &allowed())
        .await
        .unwrap();
    let removed = system
        .uninstall_authorized(&[n("note")], impact.fingerprint().as_str(), &allowed())
        .await
        .unwrap();
    assert_eq!(
        rows(&removed.orm, "SELECT id::text FROM ir_model_fields").await,
        vec![vec![Some("2".into())]]
    );
    assert_eq!(
        rows(&removed.orm, "SELECT field_id::text FROM fixture_field_rel").await,
        vec![vec![Some("2".into())]]
    );
    assert_eq!(state(&removed.orm, "note").await, "uninstalled");
    committed(removed);
    db.cleanup().await.unwrap();
}

async fn ensure_signal_schema(orm: &erp_orm::Orm) {
    let mut tx = orm.begin(erp_orm::SUPERUSER_ID).await.unwrap();
    tx.install_signal().await.unwrap();
    tx.commit().await.unwrap();
}

static WEB: AddonDescriptor = AddonDescriptor {
    name: "web",
    depends: &["base"],
    exact_only: false,
};

/// `web` is always present from the base install and depends on `base` only, so the protected
/// dependency whose removal is refused is `base`, named with its protecting root `web`.
#[tokio::test]
async fn protected_web_dependency_cannot_be_removed_indirectly() {
    let db = TestDb::create().await.unwrap();
    let addons = Addons::new();
    addons
        .add("note", "name='Note'\ndepends=['base']\n")
        .add("web", "name='Web'\ndepends=['base']\n");
    let system = builder(&db, &addons)
        .code([&NOTE, &WEB])
        .decls([model("note", "test.note")])
        .build()
        .unwrap();
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    ensure_signal_schema(&init.orm).await;
    exec(
        &init.orm,
        "INSERT INTO res_users(id, name) VALUES (2, 'Administrator')",
    )
    .await;
    let installed = system
        .install_authorized(&[n("web")], &allowed())
        .await
        .unwrap();
    let before = states(&installed.orm).await;
    assert_error(
        system
            .preview_uninstall_authorized(&[n("base")], &allowed())
            .await
            .unwrap_err(),
        ErrorKind::Validation,
        "Uninstall cannot remove addon 'base' required by protected addon 'web'",
    );
    assert_error(
        system
            .uninstall_authorized(
                &[n("base")],
                &format!("uninstall-v1:{}", "0".repeat(64)),
                &allowed(),
            )
            .await
            .unwrap_err(),
        ErrorKind::Validation,
        "Uninstall cannot remove addon 'base' required by protected addon 'web'",
    );
    assert_eq!(states(&installed.orm).await, before);
    assert_eq!(state(&installed.orm, "base").await, "installed");
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn custom_loader_refuses_without_effects() {
    let (db, addons, _system, installed) = installed_note().await;
    let system = builder(&db, &addons)
        .code([&NOTE])
        .decls([model("note", "test.note")])
        .data_loader(std::sync::Arc::new(RecordingLoader))
        .build()
        .unwrap();
    assert_error(
        system
            .preview_uninstall_authorized(&[n("note")], &allowed())
            .await
            .unwrap_err(),
        ErrorKind::Validation,
        "Uninstall impact cannot describe custom data loader purge callbacks; unsupported until a later release",
    );
    assert_error(
        system
            .uninstall_authorized(
                &[n("note")],
                &format!("uninstall-v1:{}", "0".repeat(64)),
                &allowed(),
            )
            .await
            .unwrap_err(),
        ErrorKind::Validation,
        "Uninstall impact cannot describe custom data loader purge callbacks; unsupported until a later release",
    );
    assert!(
        take_log().is_empty(),
        "custom purge must not run while computing impact"
    );
    assert_note_intact(&installed.orm).await;
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn missing_durable_generation_table_refuses_without_effects() {
    let (db, _addons, system, installed) = installed_note().await;
    exec(&installed.orm, "DROP TABLE erp_signal_generation").await;
    assert_error(
        system
            .preview_uninstall_authorized(&[n("note")], &allowed())
            .await
            .unwrap_err(),
        ErrorKind::Validation,
        "Uninstall impact requires durable registry generation; run Transaction::install_signal before preview",
    );
    assert_error(
        system
            .uninstall_authorized(
                &[n("note")],
                &format!("uninstall-v1:{}", "0".repeat(64)),
                &allowed(),
            )
            .await
            .unwrap_err(),
        ErrorKind::Validation,
        "Uninstall impact requires durable registry generation; run Transaction::install_signal before preview",
    );
    assert_note_intact(&installed.orm).await;
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn malformed_fingerprint_refuses_without_effects() {
    let (db, _addons, system, installed) = installed_note().await;
    for fingerprint in [
        String::new(),
        "not a digest".into(),
        format!("uninstall-v1:{}", "0".repeat(63)),
        format!("uninstall-v1:{}", "0".repeat(65)),
        format!("uninstall-v1:{}", "g".repeat(64)),
    ] {
        assert_error(
            system
                .uninstall_authorized(&[n("note")], &fingerprint, &allowed())
                .await
                .unwrap_err(),
            ErrorKind::Validation,
            "Invalid uninstall impact fingerprint",
        );
        assert_note_intact(&installed.orm).await;
    }
    db.cleanup().await.unwrap();
}

struct RevocableAuthorizer {
    inner: Authorizer,
    revoked: std::sync::atomic::AtomicBool,
}
impl InstallAuthorizer for RevocableAuthorizer {
    fn authorize<'a>(
        &'a self,
        tx: &'a mut Transaction,
    ) -> BoxFuture<'a, erp_fields::Result<InstallAuthorization>> {
        Box::pin(async move {
            let authorization = self.inner.authorize(tx).await?;
            if self.revoked.load(Ordering::SeqCst) {
                return Err(erp_fields::Error::access(
                    "system authority revoked during membership lock wait",
                ));
            }
            Ok(authorization)
        })
    }
}

#[tokio::test]
async fn authority_revoked_while_waiting_for_membership_locks_refuses_without_effects() {
    use std::sync::Arc;
    use std::time::Duration;
    let (db, _addons, system, installed) = installed_note().await;
    let system = Arc::new(system);
    let impact = system
        .preview_uninstall_authorized(&[n("note")], &allowed())
        .await
        .unwrap();
    for execute in [false, true] {
        let authorizer = Arc::new(RevocableAuthorizer {
            inner: allowed(),
            revoked: std::sync::atomic::AtomicBool::new(false),
        });
        let mut blocker = installed.orm.begin(erp_orm::SUPERUSER_ID).await.unwrap();
        blocker
            .batch("LOCK TABLE test_note IN ACCESS EXCLUSIVE MODE")
            .await
            .unwrap();
        let task_system = Arc::clone(&system);
        let task_authorizer = Arc::clone(&authorizer);
        let fingerprint = impact.fingerprint().as_str().to_owned();
        let operation = tokio::spawn(async move {
            if execute {
                task_system
                    .uninstall_authorized(&[n("note")], &fingerprint, task_authorizer.as_ref())
                    .await
                    .map(|_| ())
            } else {
                task_system
                    .preview_uninstall_authorized(&[n("note")], task_authorizer.as_ref())
                    .await
                    .map(|_| ())
            }
        });
        assert!(eventually(Duration::from_secs(3), || async {
            rows(&installed.orm, "SELECT EXISTS(SELECT 1 FROM pg_locks WHERE relation='test_note'::regclass AND NOT granted)::text").await[0][0].as_deref() == Some("true")
        }).await, "operation never reached the membership lock wait");
        assert_eq!(authorizer.inner.calls.load(Ordering::SeqCst), 1);
        authorizer.revoked.store(true, Ordering::SeqCst);
        blocker.rollback().await.unwrap();
        assert_error(
            operation.await.unwrap().unwrap_err(),
            ErrorKind::Access,
            "system authority revoked during membership lock wait",
        );
        assert_eq!(authorizer.inner.calls.load(Ordering::SeqCst), 2);
        assert_note_intact(&installed.orm).await;
        assert_eq!(
            rows(&installed.orm, "SELECT id::text, name FROM test_note").await,
            vec![vec![Some("11".into()), Some("secret record".into())]]
        );
    }
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn legacy_lifecycle_audit_migration_preserves_existing_history() {
    let (db, _addons, system, installed) = installed_note().await;
    let history_sql = "SELECT id::text, actor_uid::text, session_id::text, operation, requested::text, resolved::text, installed::text, upgraded::text, outcome, created_at::text FROM erp_module_lifecycle_audit ORDER BY id";
    let install_history_sql =
        "SELECT row_to_json(a)::text FROM erp_module_install_audit a ORDER BY id";
    let before = rows(&installed.orm, history_sql).await;
    let install_before = rows(&installed.orm, install_history_sql).await;
    assert!(
        !before.is_empty(),
        "legacy fixture must contain historical audit entries"
    );
    exec(&installed.orm, "DROP TRIGGER erp_module_lifecycle_audit_insert_only ON erp_module_lifecycle_audit; ALTER TABLE erp_module_lifecycle_audit DROP CONSTRAINT erp_module_lifecycle_audit_fingerprint_check; ALTER TABLE erp_module_lifecycle_audit DROP COLUMN fingerprint; ALTER TABLE erp_module_lifecycle_audit DROP COLUMN uninstalled; ALTER TABLE erp_module_lifecycle_audit DROP CONSTRAINT erp_module_lifecycle_audit_operation_check; ALTER TABLE erp_module_lifecycle_audit ADD CONSTRAINT erp_module_lifecycle_audit_operation_check CHECK (operation IN ('install','upgrade'))").await;
    let migrated = system
        .upgrade_authorized(&[n("note")], &allowed())
        .await
        .unwrap();
    let after = rows(&migrated.orm, history_sql).await;
    assert_eq!(&after[..before.len()], before.as_slice());
    assert_eq!(
        rows(&migrated.orm, install_history_sql).await,
        install_before
    );
    assert_eq!(rows(&migrated.orm, "SELECT uninstalled::text, fingerprint FROM erp_module_lifecycle_audit WHERE operation='install' ORDER BY id").await,
        vec![vec![Some("{}".into()), None]]);
    let impact = system
        .preview_uninstall_authorized(&[n("note")], &allowed())
        .await
        .unwrap();
    let removed = system
        .uninstall_authorized(&[n("note")], impact.fingerprint().as_str(), &allowed())
        .await
        .unwrap();
    assert_eq!(rows(&removed.orm, "SELECT uninstalled::text, fingerprint FROM erp_module_lifecycle_audit WHERE operation='uninstall'").await,
        vec![vec![Some("{note}".into()), Some(impact.fingerprint().as_str().into())]]);
    committed(migrated);
    committed(removed);
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn lifecycle_audit_is_insert_only() {
    let (db, _addons, system, _installed) = installed_note().await;
    let impact = system
        .preview_uninstall_authorized(&[n("note")], &allowed())
        .await
        .unwrap();
    let removed = system
        .uninstall_authorized(&[n("note")], impact.fingerprint().as_str(), &allowed())
        .await
        .unwrap();
    let before = rows(
        &removed.orm,
        "SELECT row_to_json(a)::text FROM erp_module_lifecycle_audit a ORDER BY id",
    )
    .await;
    let client = db.client().await.unwrap();
    for sql in [
        "UPDATE erp_module_lifecycle_audit SET actor_uid=3",
        "DELETE FROM erp_module_lifecycle_audit",
        "TRUNCATE erp_module_lifecycle_audit",
    ] {
        let error = client.batch_execute(sql).await.unwrap_err();
        let source = error.as_db_error().unwrap();
        assert_eq!(source.code().code(), "P0001");
        assert_eq!(source.message(), "Lifecycle audit is insert-only");
        assert_eq!(
            rows(
                &removed.orm,
                "SELECT row_to_json(a)::text FROM erp_module_lifecycle_audit a ORDER BY id"
            )
            .await,
            before
        );
    }
    committed(removed);
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn unplanned_database_effects_refuse_without_effects() {
    for (ddl, effect, survivor_probe) in [
        (
            "CREATE TABLE fixture_survivor(id bigint PRIMARY KEY, note_id bigint CONSTRAINT fixture_survivor_note_fkey REFERENCES test_note(id) ON DELETE CASCADE); INSERT INTO fixture_survivor VALUES (7,11)",
            "survivor foreign key 'fixture_survivor_note_fkey' referencing dropped table 'test_note'",
            "SELECT note_id::text FROM fixture_survivor WHERE id=7",
        ),
        (
            "CREATE VIEW fixture_note_view AS SELECT id, name FROM test_note",
            "database view dependency 'fixture_note_view'",
            "SELECT id::text FROM fixture_note_view",
        ),
        (
            "CREATE FUNCTION fixture_event() RETURNS event_trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'unplanned event callback ran'; END $$; CREATE EVENT TRIGGER fixture_uninstall_event ON ddl_command_start WHEN TAG IN ('DROP TABLE') EXECUTE FUNCTION fixture_event()",
            "database event trigger 'fixture_uninstall_event'",
            "SELECT id::text FROM test_note",
        ),
    ] {
        let (db, _addons, system, installed) = installed_note().await;
        exec(&installed.orm, ddl).await;
        let before = rows(&installed.orm, survivor_probe).await;
        let diagnostic =
            format!("Uninstall impact cannot describe {effect}; unsupported until a later release");
        assert_error(
            system
                .preview_uninstall_authorized(&[n("note")], &allowed())
                .await
                .unwrap_err(),
            ErrorKind::Validation,
            &diagnostic,
        );
        assert_error(
            system
                .uninstall_authorized(
                    &[n("note")],
                    &format!("uninstall-v1:{}", "0".repeat(64)),
                    &allowed(),
                )
                .await
                .unwrap_err(),
            ErrorKind::Validation,
            &diagnostic,
        );
        assert_note_intact(&installed.orm).await;
        assert_eq!(rows(&installed.orm, survivor_probe).await, before);
        db.cleanup().await.unwrap();
    }
}

struct PausedLockedAuthorizer {
    inner: Authorizer,
    entered: tokio::sync::Notify,
    release: tokio::sync::Semaphore,
    backend_pid: std::sync::atomic::AtomicI32,
}
impl InstallAuthorizer for PausedLockedAuthorizer {
    fn authorize<'a>(
        &'a self,
        tx: &'a mut Transaction,
    ) -> BoxFuture<'a, erp_fields::Result<InstallAuthorization>> {
        Box::pin(async move {
            let authorization = self.inner.authorize(tx).await?;
            if self.inner.calls.load(Ordering::SeqCst) == 2 {
                let pid: i32 = tx.query("SELECT pg_backend_pid()", &[]).await?[0].get(0);
                self.backend_pid.store(pid, Ordering::SeqCst);
                self.entered.notify_one();
                self.release.acquire().await.unwrap().forget();
            }
            Ok(authorization)
        })
    }
}

#[tokio::test]
async fn confirmed_uninstall_fences_a_concurrent_target_insert_until_commit() {
    use std::sync::Arc;
    use std::time::Duration;
    let (db, _addons, system, installed) = installed_note().await;
    let system = Arc::new(system);
    let impact = system
        .preview_uninstall_authorized(&[n("note")], &allowed())
        .await
        .unwrap();
    let authorizer = Arc::new(PausedLockedAuthorizer {
        inner: allowed(),
        entered: tokio::sync::Notify::new(),
        release: tokio::sync::Semaphore::new(0),
        backend_pid: std::sync::atomic::AtomicI32::new(0),
    });
    let task_system = Arc::clone(&system);
    let task_authorizer = Arc::clone(&authorizer);
    let fingerprint = impact.fingerprint().as_str().to_owned();
    let removal = tokio::spawn(async move {
        task_system
            .uninstall_authorized(&[n("note")], &fingerprint, task_authorizer.as_ref())
            .await
    });
    tokio::time::timeout(Duration::from_secs(3), authorizer.entered.notified())
        .await
        .unwrap();
    assert_eq!(authorizer.inner.calls.load(Ordering::SeqCst), 2);
    let client = db.client().await.unwrap();
    let writer_pid: i32 = client
        .query_one("SELECT pg_backend_pid()", &[])
        .await
        .unwrap()
        .get(0);
    let writer = tokio::spawn(async move {
        client
            .execute(
                "INSERT INTO test_note(id, name) VALUES (99, 'concurrent unacknowledged row')",
                &[],
            )
            .await
    });
    let removal_pid = authorizer.backend_pid.load(Ordering::SeqCst);
    assert!(
        eventually(Duration::from_secs(3), || async {
            rows(
                &installed.orm,
                &format!("SELECT ({removal_pid}=ANY(pg_blocking_pids({writer_pid})))::text"),
            )
            .await[0][0]
                .as_deref()
                == Some("true")
        })
        .await,
        "target writer was not blocked by the confirmed uninstall transaction"
    );
    assert_eq!(rows(&installed.orm, &format!("SELECT EXISTS(SELECT 1 FROM pg_locks WHERE pid={writer_pid} AND relation='test_note'::regclass AND NOT granted)::text")).await,
        vec![vec![Some("true".into())]]);
    authorizer.release.add_permits(1);
    let removed = removal.await.unwrap().unwrap();
    let failure = writer.await.unwrap().unwrap_err();
    let source = failure.as_db_error().unwrap();
    assert_eq!(source.code().code(), "42P01");
    assert_eq!(source.message(), "relation \"test_note\" does not exist");
    assert!(!table_exists(&removed.orm, "test_note").await);
    assert_eq!(state(&removed.orm, "note").await, "uninstalled");
    assert_eq!(
        rows(
            &removed.orm,
            "SELECT count(*)::text FROM erp_module_lifecycle_audit WHERE operation='uninstall'"
        )
        .await,
        vec![vec![Some("1".into())]]
    );
    committed(removed);
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn changed_schema_identity_invalidates_confirmation_without_effects() {
    for ddl in [
        "ALTER TABLE test_note ADD COLUMN unacknowledged text",
        "CREATE INDEX fixture_unacknowledged_index ON test_note(name)",
    ] {
        let (db, _addons, system, installed) = installed_note().await;
        let before = system
            .preview_uninstall_authorized(&[n("note")], &allowed())
            .await
            .unwrap();
        exec(&installed.orm, ddl).await;
        let after = system
            .preview_uninstall_authorized(&[n("note")], &allowed())
            .await
            .unwrap();
        assert_eq!(
            before.dropped_models()[0].row_count(),
            after.dropped_models()[0].row_count()
        );
        assert_ne!(before.fingerprint().as_str(), after.fingerprint().as_str());
        assert_error(
            system
                .uninstall_authorized(&[n("note")], before.fingerprint().as_str(), &allowed())
                .await
                .unwrap_err(),
            ErrorKind::Conflict,
            "Uninstall impact changed; preview again before confirming",
        );
        assert_note_intact(&installed.orm).await;
        assert_eq!(
            rows(&installed.orm, "SELECT id::text, name FROM test_note").await,
            vec![vec![Some("11".into()), Some("secret record".into())]]
        );
        db.cleanup().await.unwrap();
    }
}

#[tokio::test]
async fn post_commit_failure_does_not_erase_committed_uninstall() {
    let (db, _addons, system, installed) = installed_note().await;
    schedule_managed(
        &installed.orm,
        &n("base"),
        &pk("fixture_missing_stats"),
        &analyze("fixture_missing_stats_table"),
    )
    .await
    .unwrap();
    let impact = system
        .preview_uninstall_authorized(&[n("note")], &allowed())
        .await
        .unwrap();
    let removed = system
        .uninstall_authorized(&[n("note")], impact.fingerprint().as_str(), &allowed())
        .await
        .unwrap();
    assert_eq!(removed.post_commit.failed.len(), 1);
    let failure = &removed.post_commit.failed[0];
    assert_eq!(failure.step.module.as_str(), "base");
    assert_eq!(failure.step.key.as_str(), "fixture_missing_stats");
    assert_eq!(failure.sqlstate.as_deref(), Some("42P01"));
    assert_eq!(
        failure.error,
        "relation \"public.fixture_missing_stats_table\" does not exist"
    );
    assert!(!failure.parked);
    assert!(removed.post_commit.stopped.is_none());
    assert!(!table_exists(&removed.orm, "test_note").await);
    assert_eq!(state(&removed.orm, "note").await, "uninstalled");
    assert_eq!(rows(&removed.orm, "SELECT operation, fingerprint, outcome FROM erp_module_lifecycle_audit WHERE operation='uninstall'").await,
        vec![vec![Some("uninstall".into()), Some(impact.fingerprint().as_str().into()), Some("committed".into())]]);
    assert_eq!(rows(&removed.orm, "SELECT state FROM ir_module_post_commit WHERE module='base' AND key='fixture_missing_stats'").await,
        vec![vec![Some("failed".into())]]);
    db.cleanup().await.unwrap();
}

// A bounded native declaration on the existing metadata model. This deliberately does not
// represent full production base composition, whose register hooks are separately refused.
static METADATA_POLICY_TRIGGER: erp_registry::ModelDecl =
    erp_registry::ModelDecl::extension("base_data", "ir.model.data", &["ir.model.data"])
        .table_objects(&[erp_registry::TableObjectDecl {
            name: "_policy_bump",
            kind: erp_registry::TableObjectKind::SignalTrigger(
                erp_registry::SignalTriggerDecl::new(erp_registry::TriggerSignal::Policy)
                    .without_updates(),
            ),
        }])
        .at(file!(), line!());

async fn installed_note_with_metadata_policy_trigger()
-> (TestDb, Addons, ModuleSystem, OperationOutcome) {
    let db = TestDb::create().await.unwrap();
    let addons = Addons::new();
    addons.add("note", "name='Note'\ndepends=['base']\n");
    let system = builder(&db, &addons)
        .code([&NOTE])
        .decls([model("note", "test.note"), &METADATA_POLICY_TRIGGER])
        .build()
        .unwrap();
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    ensure_signal_schema(&init.orm).await;
    exec(
        &init.orm,
        "INSERT INTO res_users(id,name) VALUES (2,'Administrator')",
    )
    .await;
    let installed = system
        .install_authorized(&[n("note")], &allowed())
        .await
        .unwrap();
    exec(&installed.orm, "INSERT INTO test_note(id,name) VALUES (11,'private addon row'); INSERT INTO ir_model_data(module,name,model,res_id,noupdate) VALUES ('note','record','test.note',11,false)").await;
    assert_eq!(rows(&installed.orm, "SELECT t.tgname FROM pg_trigger t WHERE t.tgrelid='ir_model_data'::regclass AND t.tgname='ir_model_data_policy_bump' AND NOT t.tgisinternal").await,
        vec![vec![Some("ir_model_data_policy_bump".into())]]);
    (db, addons, system, installed)
}

#[tokio::test]
async fn bounded_native_metadata_policy_trigger_allows_confirmed_removal() {
    let (db, _addons, system, installed) = installed_note_with_metadata_policy_trigger().await;
    let generation_sql =
        "SELECT coalesce(sum(weight),0)::text FROM erp_signal_generation WHERE key='policy'";
    let before = rows(&installed.orm, generation_sql).await;
    let impact = system
        .preview_uninstall_authorized(&[n("note")], &allowed())
        .await
        .unwrap();
    assert_eq!(rows(&installed.orm, generation_sql).await, before);
    assert_eq!(
        rows(
            &installed.orm,
            "SELECT res_id::text FROM ir_model_data WHERE module='note' AND name='record'"
        )
        .await,
        vec![vec![Some("11".into())]]
    );
    let removed = system
        .uninstall_authorized(&[n("note")], impact.fingerprint().as_str(), &allowed())
        .await
        .unwrap();
    assert!(!table_exists(&removed.orm, "test_note").await);
    assert_eq!(state(&removed.orm, "note").await, "uninstalled");
    assert_eq!(
        rows(
            &removed.orm,
            "SELECT count(*)::text FROM ir_model_data WHERE module='note'"
        )
        .await,
        vec![vec![Some("0".into())]]
    );
    let before: i64 = before[0][0].as_ref().unwrap().parse().unwrap();
    assert_eq!(
        rows(&removed.orm, generation_sql).await,
        vec![vec![Some((before + 1).to_string())]]
    );
    committed(removed);
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn replaced_native_metadata_policy_function_is_refused() {
    let (db, _addons, system, installed) = installed_note_with_metadata_policy_trigger().await;
    exec(&installed.orm, "CREATE OR REPLACE FUNCTION erp_signal_bump_policy() RETURNS trigger LANGUAGE plpgsql SET search_path=pg_catalog, public, pg_temp AS $$ BEGIN RAISE EXCEPTION 'untrusted policy function ran'; END $$").await;
    let diagnostic = "Uninstall impact cannot describe database trigger 'ir_model_data.ir_model_data_policy_bump'; unsupported until a later release";
    assert_error(
        system
            .preview_uninstall_authorized(&[n("note")], &allowed())
            .await
            .unwrap_err(),
        ErrorKind::Validation,
        diagnostic,
    );
    assert_error(
        system
            .uninstall_authorized(
                &[n("note")],
                &format!("uninstall-v1:{}", "0".repeat(64)),
                &allowed(),
            )
            .await
            .unwrap_err(),
        ErrorKind::Validation,
        diagnostic,
    );
    assert_note_intact(&installed.orm).await;
    assert_eq!(
        rows(
            &installed.orm,
            "SELECT res_id::text FROM ir_model_data WHERE module='note' AND name='record'"
        )
        .await,
        vec![vec![Some("11".into())]]
    );
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn altered_native_metadata_policy_search_path_is_refused() {
    for configuration in [
        "public, pg_catalog, pg_temp",
        "pg_catalog, public, pg_temp, other",
    ] {
        let (db, _addons, system, installed) = installed_note_with_metadata_policy_trigger().await;
        let before = rows(
            &installed.orm,
            "SELECT res_id::text FROM ir_model_data WHERE module='note' AND name='record'",
        )
        .await;
        let policy_sql =
            "SELECT coalesce(sum(weight),0)::text FROM erp_signal_generation WHERE key='policy'";
        let policy_before = rows(&installed.orm, policy_sql).await;
        exec(
            &installed.orm,
            &format!("ALTER FUNCTION erp_signal_bump_policy() SET search_path = {configuration}"),
        )
        .await;
        let diagnostic = "Uninstall impact cannot describe database trigger 'ir_model_data.ir_model_data_policy_bump'; unsupported until a later release";
        assert_error(
            system
                .preview_uninstall_authorized(&[n("note")], &allowed())
                .await
                .unwrap_err(),
            ErrorKind::Validation,
            diagnostic,
        );
        assert_error(
            system
                .uninstall_authorized(
                    &[n("note")],
                    &format!("uninstall-v1:{}", "0".repeat(64)),
                    &allowed(),
                )
                .await
                .unwrap_err(),
            ErrorKind::Validation,
            diagnostic,
        );
        assert_note_intact(&installed.orm).await;
        assert_eq!(
            rows(
                &installed.orm,
                "SELECT res_id::text FROM ir_model_data WHERE module='note' AND name='record'"
            )
            .await,
            before
        );
        assert_eq!(rows(&installed.orm, policy_sql).await, policy_before);
        db.cleanup().await.unwrap();
    }
}

async fn assert_executable_schema_expression_refused(ddl: &str, effect: &str) {
    let (db, _addons, system, installed) = installed_note().await;
    exec(&installed.orm, "CREATE TABLE fixture_survivor_effect(id bigint PRIMARY KEY, hits bigint NOT NULL); INSERT INTO fixture_survivor_effect VALUES(1,0)").await;
    exec(&installed.orm, ddl).await;
    // DDL validation/default materialization may invoke the fixture while creating it.
    // Only callbacks after this reset belong to the lifecycle operation under test.
    exec(&installed.orm, "UPDATE fixture_survivor_effect SET hits=0").await;
    let diagnostic =
        format!("Uninstall impact cannot describe {effect}; unsupported until a later release");
    assert_error(
        system
            .preview_uninstall_authorized(&[n("note")], &allowed())
            .await
            .unwrap_err(),
        ErrorKind::Validation,
        &diagnostic,
    );
    assert_eq!(
        rows(
            &installed.orm,
            "SELECT hits::text FROM fixture_survivor_effect WHERE id=1"
        )
        .await,
        vec![vec![Some("0".into())]]
    );
    assert_error(
        system
            .uninstall_authorized(
                &[n("note")],
                &format!("uninstall-v1:{}", "0".repeat(64)),
                &allowed(),
            )
            .await
            .unwrap_err(),
        ErrorKind::Validation,
        &diagnostic,
    );
    assert_eq!(
        rows(
            &installed.orm,
            "SELECT hits::text FROM fixture_survivor_effect WHERE id=1"
        )
        .await,
        vec![vec![Some("0".into())]]
    );
    assert_note_intact(&installed.orm).await;
    assert_eq!(
        rows(&installed.orm, "SELECT id::text, name FROM test_note").await,
        vec![vec![Some("11".into()), Some("secret record".into())]]
    );
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn executable_catalog_check_expression_refuses_without_effects() {
    assert_executable_schema_expression_refused(
        "CREATE FUNCTION fixture_mutating_check(candidate text) RETURNS boolean LANGUAGE plpgsql VOLATILE AS $$ BEGIN IF candidate='uninstalled' THEN UPDATE fixture_survivor_effect SET hits=hits+1 WHERE id=1; END IF; RETURN true; END $$; ALTER TABLE ir_module_module ADD CONSTRAINT fixture_mutating_check CHECK (fixture_mutating_check(state))",
        "executable CHECK expression on table 'ir_module_module'",
    ).await;
}

#[tokio::test]
async fn executable_audit_default_expression_refuses_without_effects() {
    assert_executable_schema_expression_refused(
        "CREATE FUNCTION fixture_mutating_default() RETURNS text LANGUAGE plpgsql VOLATILE AS $$ BEGIN UPDATE fixture_survivor_effect SET hits=hits+1 WHERE id=1; RETURN 'callback'; END $$; ALTER TABLE erp_module_lifecycle_audit ADD COLUMN fixture_effect text DEFAULT fixture_mutating_default()",
        "executable default expression on table 'erp_module_lifecycle_audit'",
    ).await;
}

#[tokio::test]
async fn metadata_fk_rewrite_actions_refuse_before_survivor_update() {
    for (action, default_clause) in [("SET NULL", ""), ("SET DEFAULT", " DEFAULT 2")] {
        let (db, _addons, system, installed) = installed_note().await;
        exec(&installed.orm, "CREATE TABLE fixture_survivor_effect(id bigint PRIMARY KEY,hits bigint NOT NULL); INSERT INTO fixture_survivor_effect VALUES(1,0); CREATE TABLE ir_model_fields(id bigint PRIMARY KEY,model text); INSERT INTO ir_model_fields VALUES(1,'test.note'),(2,'base.retained'); CREATE TABLE ir_model(id bigint PRIMARY KEY,model text,company_restriction text)").await;
        exec(&installed.orm, &format!("ALTER TABLE ir_model ADD COLUMN fixture_field_id bigint{default_clause} CONSTRAINT fixture_metadata_action REFERENCES ir_model_fields(id) ON DELETE {action}; INSERT INTO ir_model(id,model,fixture_field_id) VALUES(1,'test.note',1); CREATE FUNCTION fixture_metadata_update() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN UPDATE fixture_survivor_effect SET hits=hits+1 WHERE id=1; RETURN NEW; END $$; CREATE TRIGGER fixture_metadata_update AFTER UPDATE ON ir_model FOR EACH ROW EXECUTE FUNCTION fixture_metadata_update()")).await;
        let diagnostic = format!(
            "Uninstall impact cannot describe metadata foreign key action {action} for constraint 'fixture_metadata_action'; unsupported until a later release"
        );
        assert_error(
            system
                .preview_uninstall_authorized(&[n("note")], &allowed())
                .await
                .unwrap_err(),
            ErrorKind::Validation,
            &diagnostic,
        );
        assert_error(
            system
                .uninstall_authorized(
                    &[n("note")],
                    &format!("uninstall-v1:{}", "0".repeat(64)),
                    &allowed(),
                )
                .await
                .unwrap_err(),
            ErrorKind::Validation,
            &diagnostic,
        );
        assert_eq!(
            rows(
                &installed.orm,
                "SELECT hits::text FROM fixture_survivor_effect WHERE id=1"
            )
            .await,
            vec![vec![Some("0".into())]]
        );
        assert_eq!(
            rows(
                &installed.orm,
                "SELECT fixture_field_id::text FROM ir_model WHERE id=1"
            )
            .await,
            vec![vec![Some("1".into())]]
        );
        assert_eq!(
            rows(
                &installed.orm,
                "SELECT id::text FROM ir_model_fields ORDER BY id"
            )
            .await,
            vec![vec![Some("1".into())], vec![Some("2".into())]]
        );
        assert_note_intact(&installed.orm).await;
        db.cleanup().await.unwrap();
    }
}

#[tokio::test]
async fn executable_audit_domain_type_refuses_without_effects() {
    assert_executable_schema_expression_refused(
        "CREATE FUNCTION fixture_mutating_domain(candidate text) RETURNS boolean LANGUAGE plpgsql VOLATILE AS $$ BEGIN UPDATE fixture_survivor_effect SET hits=hits+1 WHERE id=1; RETURN true; END $$; CREATE DOMAIN fixture_audit_domain AS text CHECK(fixture_mutating_domain(VALUE)); ALTER TABLE erp_module_lifecycle_audit ADD COLUMN fixture_effect fixture_audit_domain DEFAULT 'committed'",
        "custom column type on table 'erp_module_lifecycle_audit'",
    ).await;
}

#[tokio::test]
async fn executable_generated_and_index_expressions_refuse_without_effects() {
    for (ddl, effect) in [
        (
            "CREATE FUNCTION fixture_generated_callback(candidate text) RETURNS text LANGUAGE plpgsql IMMUTABLE AS $$ BEGIN IF candidate='uninstalled' THEN RAISE EXCEPTION 'unplanned generated callback ran'; END IF; RETURN candidate; END $$; ALTER TABLE ir_module_module ADD COLUMN fixture_effect text GENERATED ALWAYS AS (fixture_generated_callback(state)) STORED",
            "executable generated expression on table 'ir_module_module'",
        ),
        (
            "CREATE FUNCTION fixture_index_callback(candidate text) RETURNS text LANGUAGE plpgsql IMMUTABLE AS $$ BEGIN RETURN candidate; END $$; CREATE INDEX fixture_callback_index ON ir_module_module(fixture_index_callback(state))",
            "executable index expression on table 'ir_module_module'",
        ),
        (
            "CREATE FUNCTION fixture_predicate_callback(candidate text) RETURNS boolean LANGUAGE plpgsql IMMUTABLE AS $$ BEGIN RETURN candidate IS NOT NULL; END $$; CREATE INDEX fixture_callback_predicate ON ir_module_module(name) WHERE fixture_predicate_callback(state)",
            "executable index expression on table 'ir_module_module'",
        ),
    ] {
        assert_executable_schema_expression_refused(ddl, effect).await;
    }
}

/// Evidence utility: capture the database's own deparse of the native write-table expressions.
/// Exact expression admission is checked by the engine tests after this inventory is captured.
#[tokio::test]
async fn native_write_expression_inventory() {
    let (db, _addons, _system, installed) = installed_note().await;
    let inventory = rows(
        &installed.orm,
        r#"
        WITH write_tables AS (
            SELECT c.oid, c.relname::text AS table_name
            FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace
            WHERE n.nspname=current_schema() AND c.relname IN
                ('ir_module_module','erp_signal_generation','erp_module_lifecycle_audit')
        )
        SELECT table_name, 'check'::text AS kind, c.conname::text AS object_name,
               pg_get_expr(c.conbin,c.conrelid)::text AS definition
        FROM write_tables w JOIN pg_constraint c ON c.conrelid=w.oid WHERE c.contype='c'
        UNION ALL
        SELECT table_name, CASE WHEN a.attgenerated<>'' THEN 'generated' ELSE 'default' END,
               a.attname::text, pg_get_expr(d.adbin,d.adrelid)::text
        FROM write_tables w JOIN pg_attrdef d ON d.adrelid=w.oid
        JOIN pg_attribute a ON a.attrelid=d.adrelid AND a.attnum=d.adnum
        UNION ALL
        SELECT table_name, 'type', a.attname::text,
               format('%I.%I typtype=%s generated=%s identity=%s',
                      tn.nspname,t.typname,t.typtype,a.attgenerated,a.attidentity)::text
        FROM write_tables w JOIN pg_attribute a ON a.attrelid=w.oid
        JOIN pg_type t ON t.oid=a.atttypid JOIN pg_namespace tn ON tn.oid=t.typnamespace
        WHERE a.attnum>0 AND NOT a.attisdropped
        ORDER BY 1,2,3
    "#,
    )
    .await;
    for table in [
        "ir_module_module",
        "erp_signal_generation",
        "erp_module_lifecycle_audit",
    ] {
        assert!(
            inventory.iter().any(|row| row[0].as_deref() == Some(table)),
            "missing native write-table inventory for {table}"
        );
    }
    for row in inventory {
        assert!(
            row.iter().all(Option::is_some),
            "unexpected NULL schema inventory value: {row:?}"
        );
        eprintln!("NATIVE_WRITE_EXPRESSION {row:?}");
    }
    db.cleanup().await.unwrap();
}

struct SearchPathAuthorizer(Authorizer);
impl InstallAuthorizer for SearchPathAuthorizer {
    fn authorize<'a>(
        &'a self,
        tx: &'a mut Transaction,
    ) -> BoxFuture<'a, erp_fields::Result<InstallAuthorization>> {
        Box::pin(async move {
            let authorization = self.0.authorize(tx).await?;
            tx.batch("SET LOCAL search_path=public,pg_catalog").await?;
            Ok(authorization)
        })
    }
}

#[tokio::test]
async fn application_first_search_path_refuses_before_planning() {
    let (db, _addons, system, installed) = installed_note().await;
    let authorizer = SearchPathAuthorizer(allowed());
    let diagnostic = "Uninstall impact cannot describe search_path without pg_catalog precedence; unsupported until a later release";
    assert_error(
        system
            .preview_uninstall_authorized(&[n("note")], &authorizer)
            .await
            .unwrap_err(),
        ErrorKind::Validation,
        diagnostic,
    );
    assert_error(
        system
            .uninstall_authorized(
                &[n("note")],
                &format!("uninstall-v1:{}", "0".repeat(64)),
                &authorizer,
            )
            .await
            .unwrap_err(),
        ErrorKind::Validation,
        diagnostic,
    );
    assert_eq!(authorizer.0.calls.load(Ordering::SeqCst), 2);
    assert_note_intact(&installed.orm).await;
    assert_eq!(
        rows(&installed.orm, "SELECT id::text,name FROM test_note").await,
        vec![vec![Some("11".into()), Some("secret record".into())]]
    );
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn removed_table_executable_check_expression_refuses_without_effects() {
    assert_executable_schema_expression_refused(
        "CREATE FUNCTION fixture_removed_check(candidate text) RETURNS boolean LANGUAGE plpgsql VOLATILE AS $$ BEGIN UPDATE fixture_survivor_effect SET hits=hits+1 WHERE id=1; RETURN true; END $$; ALTER TABLE test_note ADD CONSTRAINT fixture_removed_check CHECK(fixture_removed_check(name))",
        "executable CHECK expression on table 'test_note'",
    ).await;
}

#[tokio::test]
async fn native_metadata_cascade_with_source_first_order_removes_only_planned_rows() {
    let (db, _addons, system, installed) = installed_note().await;
    exec(&installed.orm, "CREATE TABLE ir_model(id bigint PRIMARY KEY,model text,company_restriction text); INSERT INTO ir_model VALUES(1,'test.note',NULL),(2,'base.retained',NULL); CREATE TABLE ir_model_fields(id bigint PRIMARY KEY,model text,model_id bigint CONSTRAINT fixture_native_metadata_cascade REFERENCES ir_model(id) ON DELETE CASCADE); INSERT INTO ir_model_fields VALUES(1,'test.note',1),(2,'base.retained',2)").await;
    let models_before = rows(
        &installed.orm,
        "SELECT id::text,model FROM ir_model ORDER BY id",
    )
    .await;
    let fields_before = rows(
        &installed.orm,
        "SELECT id::text,model,model_id::text FROM ir_model_fields ORDER BY id",
    )
    .await;
    let impact = system
        .preview_uninstall_authorized(&[n("note")], &allowed())
        .await
        .unwrap();
    assert_eq!(
        rows(
            &installed.orm,
            "SELECT id::text,model FROM ir_model ORDER BY id"
        )
        .await,
        models_before
    );
    assert_eq!(
        rows(
            &installed.orm,
            "SELECT id::text,model,model_id::text FROM ir_model_fields ORDER BY id"
        )
        .await,
        fields_before
    );
    let removed = system
        .uninstall_authorized(&[n("note")], impact.fingerprint().as_str(), &allowed())
        .await
        .unwrap();
    assert_eq!(
        rows(
            &removed.orm,
            "SELECT id::text,model FROM ir_model ORDER BY id"
        )
        .await,
        vec![vec![Some("2".into()), Some("base.retained".into())]]
    );
    assert_eq!(
        rows(
            &removed.orm,
            "SELECT id::text,model,model_id::text FROM ir_model_fields ORDER BY id"
        )
        .await,
        vec![vec![
            Some("2".into()),
            Some("base.retained".into()),
            Some("2".into())
        ]]
    );
    assert!(!table_exists(&removed.orm, "test_note").await);
    assert_eq!(state(&removed.orm, "note").await, "uninstalled");
    assert_eq!(state(&removed.orm, "base").await, "installed");
    assert_eq!(
        rows(
            &removed.orm,
            "SELECT count(*)::text FROM erp_module_lifecycle_audit WHERE operation='uninstall'"
        )
        .await,
        vec![vec![Some("1".into())]]
    );
    committed(removed);
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn audit_fk_target_executable_index_refuses_without_effects() {
    assert_executable_schema_expression_refused(
        "CREATE FUNCTION fixture_actor_index(candidate text) RETURNS text LANGUAGE plpgsql IMMUTABLE AS $$ BEGIN RETURN candidate; END $$; CREATE INDEX fixture_actor_expression_index ON res_users(fixture_actor_index(name))",
        "executable index expression on table 'res_users'",
    ).await;
}

struct TempCatalogAuthorizer(Authorizer);
impl InstallAuthorizer for TempCatalogAuthorizer {
    fn authorize<'a>(
        &'a self,
        tx: &'a mut Transaction,
    ) -> BoxFuture<'a, erp_fields::Result<InstallAuthorization>> {
        Box::pin(async move {
            let authorization = self.0.authorize(tx).await?;
            tx.batch("CREATE TEMP TABLE IF NOT EXISTS pg_event_trigger(evtname name, evtenabled \"char\")").await?;
            Ok(authorization)
        })
    }
}

#[tokio::test]
async fn temporary_catalog_shadowing_refuses_before_planning() {
    let (db, _addons, system, installed) = installed_note().await;
    let diagnostic = "Uninstall impact cannot describe search_path without pg_catalog precedence; unsupported until a later release";
    assert_error(
        system
            .preview_uninstall_authorized(&[n("note")], &TempCatalogAuthorizer(allowed()))
            .await
            .unwrap_err(),
        ErrorKind::Validation,
        diagnostic,
    );
    assert_error(
        system
            .uninstall_authorized(
                &[n("note")],
                &format!("uninstall-v1:{}", "0".repeat(64)),
                &TempCatalogAuthorizer(allowed()),
            )
            .await
            .unwrap_err(),
        ErrorKind::Validation,
        diagnostic,
    );
    assert_note_intact(&installed.orm).await;
    assert_eq!(
        rows(&installed.orm, "SELECT id::text,name FROM test_note").await,
        vec![vec![Some("11".into()), Some("secret record".into())]]
    );
    db.cleanup().await.unwrap();
}

fn native_fixture_identities(
    addons: &Addons,
) -> std::collections::BTreeMap<erp_modules::manifest::ModuleName, String> {
    std::fs::read_dir(addons.path())
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            let name = entry.file_name().into_string().unwrap();
            let snapshot = erp_modules::artifact_identity::ArtifactSnapshot::capture(
                &name,
                &entry.path().canonicalize().unwrap(),
            )
            .unwrap();
            (n(&name), snapshot.digest().to_owned())
        })
        .collect()
}

#[tokio::test]
async fn configured_signal_preflight_rejects_preexisting_temporary_store_on_same_connection() {
    let (db, addons, _original_system, installed) = installed_note().await;
    // One Fast-recycling pooled backend makes the planted temporary state deterministic.
    let pool = erp_orm::pool::create_pool(db.config(), 1).unwrap();
    let client = pool.get().await.unwrap();
    let baseline = erp_orm::Signal::read(&client).await.unwrap();
    let backend_pid: i32 = client
        .query_one("SELECT pg_backend_pid()", &[])
        .await
        .unwrap()
        .get(0);
    client.batch_execute("CREATE TEMP TABLE erp_signal_generation(fixture_marker bigint); INSERT INTO pg_temp.erp_signal_generation VALUES(17)").await.unwrap();
    drop(client);
    let identities = native_fixture_identities(&addons);
    let system = ModuleSystem::builder(pool.clone(), addons.index())
        .linked_hooks(false)
        .native_artifact_identities(identities)
        .code([&NOTE])
        .decls([model("note", "test.note")])
        .signal(erp_orm::Signal::new(baseline.clone()))
        .build()
        .unwrap();
    let authorizer = allowed();
    for (configuration, diagnostic) in [
        (
            "RESET search_path",
            "Native lifecycle opening requires pg_catalog as the first effective schema",
        ),
        (
            "SET search_path=pg_catalog,pg_temp,public",
            "Native lifecycle opening requires a persistent ordinary signal store table without row-level security, partitioning or inheritance",
        ),
    ] {
        let client = pool.get().await.unwrap();
        client.batch_execute(configuration).await.unwrap();
        assert_eq!(
            client
                .query_one("SELECT pg_backend_pid()", &[])
                .await
                .unwrap()
                .get::<_, i32>(0),
            backend_pid
        );
        drop(client);
        assert_error(
            system
                .preview_uninstall_authorized(&[n("note")], &authorizer)
                .await
                .unwrap_err(),
            ErrorKind::Validation,
            diagnostic,
        );
        assert_error(
            system
                .uninstall_authorized(
                    &[n("note")],
                    &format!("uninstall-v1:{}", "0".repeat(64)),
                    &authorizer,
                )
                .await
                .unwrap_err(),
            ErrorKind::Validation,
            diagnostic,
        );
        assert_eq!(
            authorizer.calls.load(Ordering::SeqCst),
            0,
            "opening must refuse before authority or signal reads"
        );
        // The wrong-shaped fake store would make the lazy BEGIN signal aggregate fail with
        // missing-column errors. A usable idle backend and unchanged fake row prove early refusal.
        let client = pool.get().await.unwrap();
        assert_eq!(
            client
                .query_one("SELECT pg_backend_pid()", &[])
                .await
                .unwrap()
                .get::<_, i32>(0),
            backend_pid
        );
        assert_eq!(
            client
                .query_one(
                    "SELECT fixture_marker FROM pg_temp.erp_signal_generation",
                    &[]
                )
                .await
                .unwrap()
                .get::<_, i64>(0),
            17
        );
        drop(client);
        assert_note_intact(&installed.orm).await;
    }
    drop(system);
    pool.close();
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn configured_signal_preflight_rejects_callback_aggregate_on_text_weight() {
    let (db, addons, _original_system, installed) = installed_note().await;
    exec(&installed.orm, "INSERT INTO erp_signal_generation(key) VALUES('registry'); CREATE TABLE fixture_signal_effect(id bigint PRIMARY KEY,hits bigint NOT NULL); INSERT INTO fixture_signal_effect VALUES(1,0)").await;
    let pool = erp_orm::pool::create_pool(db.config(), 1).unwrap();
    let client = pool.get().await.unwrap();
    let baseline = erp_orm::Signal::read(&client).await.unwrap();
    let backend_pid: i32 = client
        .query_one("SELECT pg_backend_pid()", &[])
        .await
        .unwrap()
        .get(0);
    client.batch_execute("ALTER TABLE erp_signal_generation DROP CONSTRAINT erp_signal_generation_weight_check; ALTER TABLE erp_signal_generation ALTER COLUMN weight DROP DEFAULT; ALTER TABLE erp_signal_generation ALTER COLUMN weight TYPE text USING weight::text; ALTER TABLE erp_signal_generation ALTER COLUMN weight SET DEFAULT '1'; CREATE FUNCTION fixture_signal_sum(acc bigint,value text) RETURNS bigint LANGUAGE plpgsql VOLATILE AS $$ BEGIN UPDATE fixture_signal_effect SET hits=hits+1 WHERE id=1; RAISE EXCEPTION 'unplanned signal aggregate callback ran'; END $$; CREATE AGGREGATE public.sum(text)(SFUNC=fixture_signal_sum,STYPE=bigint,INITCOND='0')").await.unwrap();
    drop(client);
    let identities = native_fixture_identities(&addons);
    let system = ModuleSystem::builder(pool.clone(), addons.index())
        .linked_hooks(false)
        .native_artifact_identities(identities)
        .code([&NOTE])
        .decls([model("note", "test.note")])
        .signal(erp_orm::Signal::new(baseline.clone()))
        .build()
        .unwrap();
    let authorizer = allowed();
    let diagnostic = "Native lifecycle opening requires native signal storage types and built-in aggregate resolution";
    assert_error(
        system
            .preview_uninstall_authorized(&[n("note")], &authorizer)
            .await
            .unwrap_err(),
        ErrorKind::Validation,
        diagnostic,
    );
    assert_error(
        system
            .uninstall_authorized(
                &[n("note")],
                &format!("uninstall-v1:{}", "0".repeat(64)),
                &authorizer,
            )
            .await
            .unwrap_err(),
        ErrorKind::Validation,
        diagnostic,
    );
    assert_eq!(authorizer.calls.load(Ordering::SeqCst), 0);
    let client = pool.get().await.unwrap();
    assert_eq!(
        client
            .query_one("SELECT pg_backend_pid()", &[])
            .await
            .unwrap()
            .get::<_, i32>(0),
        backend_pid
    );
    assert_eq!(
        client
            .query_one("SELECT hits FROM fixture_signal_effect WHERE id=1", &[])
            .await
            .unwrap()
            .get::<_, i64>(0),
        0
    );
    drop(client);
    assert_note_intact(&installed.orm).await;
    assert_eq!(
        rows(&installed.orm, "SELECT id::text,name FROM test_note").await,
        vec![vec![Some("11".into()), Some("secret record".into())]]
    );
    drop(system);
    pool.close();
    db.cleanup().await.unwrap();
}

struct KeepServing;

impl erp_orm::Refresh for KeepServing {
    fn refresh<'a>(
        &'a self,
        _changes: &'a erp_orm::Changes,
    ) -> erp_orm::BoxFuture<'a, erp_fields::Result<erp_orm::Refreshed>> {
        Box::pin(async { Ok(erp_orm::Refreshed::InPlace) })
    }
}

/// The committed uninstall bumped the configured Signal: other workers see a newer generation.
async fn assert_generation_advanced(
    pool: &deadpool_postgres::Pool,
    baseline: &erp_orm::Generations,
) {
    let total = |g: &erp_orm::Generations| g.iter().map(|(_, generation)| generation).sum::<u64>();
    let client = pool.get().await.unwrap();
    let after = erp_orm::Signal::read(&client).await.unwrap();
    assert!(
        total(&after) > total(baseline),
        "uninstall did not advance the Signal generation: {baseline:?} -> {after:?}"
    );
}

#[tokio::test]
async fn configured_signal_with_native_storage_admits_preview_and_uninstall() {
    let (db, addons, _original_system, _installed) = installed_note().await;
    let pool = erp_orm::pool::create_pool(db.config(), 1).unwrap();
    let client = pool.get().await.unwrap();
    let baseline = erp_orm::Signal::read(&client).await.unwrap();
    drop(client);
    let identities = native_fixture_identities(&addons);
    let system = ModuleSystem::builder(pool.clone(), addons.index())
        .linked_hooks(false)
        .native_artifact_identities(identities)
        .code([&NOTE])
        .decls([model("note", "test.note")])
        .signal(erp_orm::Signal::new(baseline.clone()).with_refresh(KeepServing))
        .build()
        .unwrap();
    let authorizer = allowed();
    let impact = system
        .preview_uninstall_authorized(&[n("note")], &authorizer)
        .await
        .unwrap();
    assert_eq!(impact.resolved(), &[n("note")]);
    assert_eq!(impact.dropped_models()[0].model(), "test.note");
    assert_eq!(impact.dropped_models()[0].table(), "test_note");
    let removed = system
        .uninstall_authorized(&[n("note")], impact.fingerprint().as_str(), &authorizer)
        .await
        .unwrap();
    assert_eq!(state(&removed.orm, "note").await, "uninstalled");
    assert!(!table_exists(&removed.orm, "test_note").await);
    committed(removed);
    assert_generation_advanced(&pool, &baseline).await;
    drop(system);
    pool.close();
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn configured_signal_preflight_rejects_varchar_key() {
    let (db, addons, _original_system, installed) = installed_note().await;
    let pool = erp_orm::pool::create_pool(db.config(), 1).unwrap();
    let client = pool.get().await.unwrap();
    client
        .batch_execute(
            "ALTER TABLE erp_signal_generation ALTER COLUMN key TYPE varchar(64) USING key::varchar(64)",
        )
        .await
        .unwrap();
    let baseline = erp_orm::Signal::read(&client).await.unwrap();
    drop(client);
    let identities = native_fixture_identities(&addons);
    let system = ModuleSystem::builder(pool.clone(), addons.index())
        .linked_hooks(false)
        .native_artifact_identities(identities)
        .code([&NOTE])
        .decls([model("note", "test.note")])
        .signal(erp_orm::Signal::new(baseline.clone()).with_refresh(KeepServing))
        .build()
        .unwrap();
    let authorizer = allowed();
    let diagnostic = "Native lifecycle opening requires native signal storage types and built-in aggregate resolution";
    assert_error(
        system
            .preview_uninstall_authorized(&[n("note")], &authorizer)
            .await
            .unwrap_err(),
        ErrorKind::Validation,
        diagnostic,
    );
    assert_error(
        system
            .uninstall_authorized(
                &[n("note")],
                &format!("uninstall-v1:{}", "0".repeat(64)),
                &authorizer,
            )
            .await
            .unwrap_err(),
        ErrorKind::Validation,
        diagnostic,
    );
    assert_eq!(authorizer.calls.load(Ordering::SeqCst), 0);
    assert_note_intact(&installed.orm).await;
    drop(system);
    pool.close();
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn configured_signal_preflight_rejects_user_trigger() {
    let (db, addons, _original_system, installed) = installed_note().await;
    let pool = erp_orm::pool::create_pool(db.config(), 1).unwrap();
    let client = pool.get().await.unwrap();
    let baseline = erp_orm::Signal::read(&client).await.unwrap();
    client
        .batch_execute(
            "CREATE FUNCTION fixture_sig_trig() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NEW; END $$; \
             CREATE TRIGGER trg_test BEFORE INSERT ON erp_signal_generation FOR EACH ROW EXECUTE FUNCTION fixture_sig_trig()",
        )
        .await
        .unwrap();
    drop(client);
    let identities = native_fixture_identities(&addons);
    let system = ModuleSystem::builder(pool.clone(), addons.index())
        .linked_hooks(false)
        .native_artifact_identities(identities)
        .code([&NOTE])
        .decls([model("note", "test.note")])
        .signal(erp_orm::Signal::new(baseline.clone()))
        .build()
        .unwrap();
    let authorizer = allowed();
    let diagnostic = "Native lifecycle opening requires native signal storage types and built-in aggregate resolution";
    assert_error(
        system
            .preview_uninstall_authorized(&[n("note")], &authorizer)
            .await
            .unwrap_err(),
        ErrorKind::Validation,
        diagnostic,
    );
    assert_error(
        system
            .uninstall_authorized(
                &[n("note")],
                &format!("uninstall-v1:{}", "0".repeat(64)),
                &authorizer,
            )
            .await
            .unwrap_err(),
        ErrorKind::Validation,
        diagnostic,
    );
    assert_eq!(authorizer.calls.load(Ordering::SeqCst), 0);
    assert_note_intact(&installed.orm).await;
    drop(system);
    pool.close();
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn configured_signal_preflight_rejects_rewrite_rule() {
    let (db, addons, _original_system, installed) = installed_note().await;
    let pool = erp_orm::pool::create_pool(db.config(), 1).unwrap();
    let client = pool.get().await.unwrap();
    let baseline = erp_orm::Signal::read(&client).await.unwrap();
    client
        .batch_execute(
            "CREATE RULE notify_rule AS ON UPDATE TO erp_signal_generation DO ALSO NOTHING",
        )
        .await
        .unwrap();
    drop(client);
    let identities = native_fixture_identities(&addons);
    let system = ModuleSystem::builder(pool.clone(), addons.index())
        .linked_hooks(false)
        .native_artifact_identities(identities)
        .code([&NOTE])
        .decls([model("note", "test.note")])
        .signal(erp_orm::Signal::new(baseline.clone()))
        .build()
        .unwrap();
    let authorizer = allowed();
    let diagnostic = "Native lifecycle opening requires native signal storage types and built-in aggregate resolution";
    assert_error(
        system
            .preview_uninstall_authorized(&[n("note")], &authorizer)
            .await
            .unwrap_err(),
        ErrorKind::Validation,
        diagnostic,
    );
    assert_error(
        system
            .uninstall_authorized(
                &[n("note")],
                &format!("uninstall-v1:{}", "0".repeat(64)),
                &authorizer,
            )
            .await
            .unwrap_err(),
        ErrorKind::Validation,
        diagnostic,
    );
    assert_eq!(authorizer.calls.load(Ordering::SeqCst), 0);
    assert_note_intact(&installed.orm).await;
    drop(system);
    pool.close();
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn configured_signal_preflight_rejects_generated_column() {
    let (db, addons, _original_system, installed) = installed_note().await;
    let pool = erp_orm::pool::create_pool(db.config(), 1).unwrap();
    let client = pool.get().await.unwrap();
    let baseline = erp_orm::Signal::read(&client).await.unwrap();
    client
        .batch_execute(
            "ALTER TABLE erp_signal_generation DROP COLUMN weight; \
             ALTER TABLE erp_signal_generation ADD COLUMN weight bigint GENERATED ALWAYS AS (1) STORED",
        )
        .await
        .unwrap();
    drop(client);
    let identities = native_fixture_identities(&addons);
    let system = ModuleSystem::builder(pool.clone(), addons.index())
        .linked_hooks(false)
        .native_artifact_identities(identities)
        .code([&NOTE])
        .decls([model("note", "test.note")])
        .signal(erp_orm::Signal::new(baseline.clone()))
        .build()
        .unwrap();
    let authorizer = allowed();
    let diagnostic = "Native lifecycle opening requires native signal storage types and built-in aggregate resolution";
    assert_error(
        system
            .preview_uninstall_authorized(&[n("note")], &authorizer)
            .await
            .unwrap_err(),
        ErrorKind::Validation,
        diagnostic,
    );
    assert_error(
        system
            .uninstall_authorized(
                &[n("note")],
                &format!("uninstall-v1:{}", "0".repeat(64)),
                &authorizer,
            )
            .await
            .unwrap_err(),
        ErrorKind::Validation,
        diagnostic,
    );
    assert_eq!(authorizer.calls.load(Ordering::SeqCst), 0);
    assert_note_intact(&installed.orm).await;
    drop(system);
    pool.close();
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn configured_signal_preflight_rejects_non_catalog_sum_int8() {
    let (db, addons, _original_system, installed) = installed_note().await;
    let pool = erp_orm::pool::create_pool(db.config(), 1).unwrap();
    let client = pool.get().await.unwrap();
    let baseline = erp_orm::Signal::read(&client).await.unwrap();
    client
        .batch_execute(
            "CREATE FUNCTION fixture_sum_int8(acc bigint, val bigint) RETURNS bigint LANGUAGE plpgsql AS $$ \
             BEGIN RAISE EXCEPTION 'custom sum ran'; END $$; \
             CREATE AGGREGATE public.sum(bigint)(SFUNC=fixture_sum_int8, STYPE=bigint, INITCOND='0')",
        )
        .await
        .unwrap();
    drop(client);
    let identities = native_fixture_identities(&addons);
    let system = ModuleSystem::builder(pool.clone(), addons.index())
        .linked_hooks(false)
        .native_artifact_identities(identities)
        .code([&NOTE])
        .decls([model("note", "test.note")])
        .signal(erp_orm::Signal::new(baseline.clone()))
        .build()
        .unwrap();
    let authorizer = allowed();
    let diagnostic = "Native lifecycle opening requires native signal storage types and built-in aggregate resolution";
    assert_error(
        system
            .preview_uninstall_authorized(&[n("note")], &authorizer)
            .await
            .unwrap_err(),
        ErrorKind::Validation,
        diagnostic,
    );
    assert_error(
        system
            .uninstall_authorized(
                &[n("note")],
                &format!("uninstall-v1:{}", "0".repeat(64)),
                &authorizer,
            )
            .await
            .unwrap_err(),
        ErrorKind::Validation,
        diagnostic,
    );
    assert_eq!(authorizer.calls.load(Ordering::SeqCst), 0);
    assert_note_intact(&installed.orm).await;
    drop(system);
    pool.close();
    db.cleanup().await.unwrap();
}

/// Plant `setup` on the configured signal store and require the idle opening to refuse it
/// before authority, BEGIN or any generation read.
async fn assert_signal_store_shape_refused(setup: &str) {
    let (db, addons, _original_system, installed) = installed_note().await;
    let pool = erp_orm::pool::create_pool(db.config(), 1).unwrap();
    let client = pool.get().await.unwrap();
    let baseline = erp_orm::Signal::read(&client).await.unwrap();
    client.batch_execute(setup).await.unwrap();
    drop(client);
    let system = ModuleSystem::builder(pool.clone(), addons.index())
        .linked_hooks(false)
        .native_artifact_identities(native_fixture_identities(&addons))
        .code([&NOTE])
        .decls([model("note", "test.note")])
        .signal(erp_orm::Signal::new(baseline).with_refresh(KeepServing))
        .build()
        .unwrap();
    let authorizer = allowed();
    let diagnostic = "Native lifecycle opening requires a persistent ordinary signal store table without row-level security, partitioning or inheritance";
    assert_error(
        system
            .preview_uninstall_authorized(&[n("note")], &authorizer)
            .await
            .unwrap_err(),
        ErrorKind::Validation,
        diagnostic,
    );
    assert_error(
        system
            .uninstall_authorized(
                &[n("note")],
                &format!("uninstall-v1:{}", "0".repeat(64)),
                &authorizer,
            )
            .await
            .unwrap_err(),
        ErrorKind::Validation,
        diagnostic,
    );
    assert_eq!(authorizer.calls.load(Ordering::SeqCst), 0);
    assert_note_intact(&installed.orm).await;
    drop(system);
    pool.close();
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn configured_signal_preflight_rejects_row_level_security() {
    assert_signal_store_shape_refused(
        "CREATE FUNCTION fixture_signal_policy() RETURNS boolean LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'signal policy callback ran'; END $$; \
         ALTER TABLE erp_signal_generation ENABLE ROW LEVEL SECURITY; \
         ALTER TABLE erp_signal_generation FORCE ROW LEVEL SECURITY; \
         CREATE POLICY fixture_signal_policy ON erp_signal_generation USING (fixture_signal_policy())",
    )
    .await;
}

#[tokio::test]
async fn configured_signal_preflight_rejects_inheritance_child() {
    assert_signal_store_shape_refused(
        "CREATE TABLE fixture_signal_child() INHERITS (erp_signal_generation)",
    )
    .await;
}

#[tokio::test]
async fn configured_signal_preflight_rejects_inheritance_parent() {
    assert_signal_store_shape_refused(
        "CREATE TABLE fixture_signal_parent(key text, weight int8); \
         ALTER TABLE erp_signal_generation INHERIT fixture_signal_parent",
    )
    .await;
}

#[tokio::test]
async fn configured_signal_preflight_rejects_partition() {
    assert_signal_store_shape_refused(
        "CREATE TABLE fixture_signal_parts(id uuid NOT NULL, key text NOT NULL, weight int8 NOT NULL, created_at timestamptz NOT NULL) PARTITION BY HASH (id); \
         ALTER TABLE fixture_signal_parts ATTACH PARTITION erp_signal_generation FOR VALUES WITH (MODULUS 1, REMAINDER 0)",
    )
    .await;
}

#[tokio::test]
async fn installed_identity_cascade_into_survivor_refuses_before_effects() {
    let (db, _addons, system, installed) = installed_note().await;
    exec(&installed.orm, "CREATE TABLE fixture_receipt_survivor(id bigint PRIMARY KEY, receipt_name text CONSTRAINT fixture_survivor_receipt_fkey REFERENCES installed_addon_identity(name) ON DELETE CASCADE); INSERT INTO fixture_receipt_survivor VALUES(7,'note')").await;
    let receipts = rows(
        &installed.orm,
        "SELECT row_to_json(i)::text FROM installed_addon_identity i ORDER BY name",
    )
    .await;
    let survivor = rows(
        &installed.orm,
        "SELECT id::text,receipt_name FROM fixture_receipt_survivor ORDER BY id",
    )
    .await;
    let audit = rows(
        &installed.orm,
        "SELECT row_to_json(a)::text FROM erp_module_lifecycle_audit a ORDER BY id",
    )
    .await;
    let diagnostic = "Uninstall impact cannot describe survivor foreign key 'fixture_survivor_receipt_fkey' referencing installed addon identity; unsupported until a later release";
    assert_error(
        system
            .preview_uninstall_authorized(&[n("note")], &allowed())
            .await
            .unwrap_err(),
        ErrorKind::Validation,
        diagnostic,
    );
    assert_error(
        system
            .uninstall_authorized(
                &[n("note")],
                &format!("uninstall-v1:{}", "0".repeat(64)),
                &allowed(),
            )
            .await
            .unwrap_err(),
        ErrorKind::Validation,
        diagnostic,
    );
    assert_note_intact(&installed.orm).await;
    assert_eq!(
        rows(
            &installed.orm,
            "SELECT id::text,receipt_name FROM fixture_receipt_survivor ORDER BY id"
        )
        .await,
        survivor
    );
    assert_eq!(
        rows(
            &installed.orm,
            "SELECT row_to_json(i)::text FROM installed_addon_identity i ORDER BY name"
        )
        .await,
        receipts
    );
    assert_eq!(
        rows(
            &installed.orm,
            "SELECT row_to_json(a)::text FROM erp_module_lifecycle_audit a ORDER BY id"
        )
        .await,
        audit
    );
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn cross_schema_installed_identity_cascade_refuses_before_effects() {
    let (db, _addons, system, installed) = installed_note().await;
    exec(&installed.orm, "CREATE SCHEMA fixture_receipt_other; CREATE TABLE fixture_receipt_other.survivor(id bigint PRIMARY KEY, receipt_name text CONSTRAINT fixture_cross_schema_receipt_fkey REFERENCES installed_addon_identity(name) ON DELETE CASCADE); INSERT INTO fixture_receipt_other.survivor VALUES(7,'note')").await;
    let receipts = rows(
        &installed.orm,
        "SELECT row_to_json(i)::text FROM installed_addon_identity i ORDER BY name",
    )
    .await;
    let survivor = rows(
        &installed.orm,
        "SELECT id::text,receipt_name FROM fixture_receipt_other.survivor ORDER BY id",
    )
    .await;
    let diagnostic = "Uninstall impact cannot describe cross-schema foreign key lookup for constraint 'fixture_cross_schema_receipt_fkey'; unsupported until a later release";
    assert_error(
        system
            .preview_uninstall_authorized(&[n("note")], &allowed())
            .await
            .unwrap_err(),
        ErrorKind::Validation,
        diagnostic,
    );
    assert_error(
        system
            .uninstall_authorized(
                &[n("note")],
                &format!("uninstall-v1:{}", "0".repeat(64)),
                &allowed(),
            )
            .await
            .unwrap_err(),
        ErrorKind::Validation,
        diagnostic,
    );
    assert_note_intact(&installed.orm).await;
    assert_eq!(
        rows(
            &installed.orm,
            "SELECT id::text,receipt_name FROM fixture_receipt_other.survivor ORDER BY id"
        )
        .await,
        survivor
    );
    assert_eq!(
        rows(
            &installed.orm,
            "SELECT row_to_json(i)::text FROM installed_addon_identity i ORDER BY name"
        )
        .await,
        receipts
    );
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn metadata_fk_source_identity_shapes_refuse_survivor_effects() {
    for identity_type in [
        "text",
        "smallint NOT NULL",
        "integer NOT NULL",
        "bigint NOT NULL",
    ] {
        let (db, _addons, system, installed) = installed_note().await;
        exec(&installed.orm, &format!("CREATE TABLE ir_model_fields(id bigint PRIMARY KEY,model text); INSERT INTO ir_model_fields VALUES(1,'test.note'); CREATE TABLE fixture_field_rel(id {identity_type},field_id bigint CONSTRAINT fixture_identity_metadata_fk REFERENCES ir_model_fields(id)); INSERT INTO fixture_field_rel VALUES('9',1)")).await;
        let diagnostic = "Uninstall impact cannot describe survivor foreign key effect 'fixture_identity_metadata_fk'; unsupported until a later release";
        assert_error(
            system
                .preview_uninstall_authorized(&[n("note")], &allowed())
                .await
                .unwrap_err(),
            ErrorKind::Validation,
            diagnostic,
        );
        assert_error(
            system
                .uninstall_authorized(
                    &[n("note")],
                    &format!("uninstall-v1:{}", "0".repeat(64)),
                    &allowed(),
                )
                .await
                .unwrap_err(),
            ErrorKind::Validation,
            diagnostic,
        );
        assert_eq!(
            rows(
                &installed.orm,
                "SELECT id::text, field_id::text FROM fixture_field_rel"
            )
            .await,
            vec![vec![Some("9".into()), Some("1".into())]]
        );
        assert_note_intact(&installed.orm).await;
        db.cleanup().await.unwrap();
    }
}

#[tokio::test]
async fn noninteger_metadata_fk_without_matching_reference_allows_exact_uninstall() {
    let (db, _addons, system, installed) = installed_note().await;
    exec(&installed.orm, "CREATE TABLE ir_model_fields(id bigint PRIMARY KEY,model text); INSERT INTO ir_model_fields VALUES(1,'test.note'),(2,'base.retained'); CREATE TABLE fixture_field_rel(id text,field_id bigint CONSTRAINT fixture_identity_metadata_fk REFERENCES ir_model_fields(id) ON DELETE CASCADE); INSERT INTO fixture_field_rel VALUES('survivor',2)").await;
    let impact = system
        .preview_uninstall_authorized(&[n("note")], &allowed())
        .await
        .unwrap();
    let removed = system
        .uninstall_authorized(&[n("note")], impact.fingerprint().as_str(), &allowed())
        .await
        .unwrap();
    assert_eq!(
        rows(&removed.orm, "SELECT id::text FROM ir_model_fields").await,
        vec![vec![Some("2".into())]]
    );
    assert_eq!(
        rows(
            &removed.orm,
            "SELECT id,field_id::text FROM fixture_field_rel"
        )
        .await,
        vec![vec![Some("survivor".into()), Some("2".into())]]
    );
    assert_eq!(state(&removed.orm, "note").await, "uninstalled");
    committed(removed);
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn nullable_metadata_fk_identity_refuses_with_typed_effect() {
    let (db, _addons, system, installed) = installed_note().await;
    exec(&installed.orm, "CREATE TABLE ir_model_fields(id bigint PRIMARY KEY,model text); INSERT INTO ir_model_fields VALUES(1,'test.note'); CREATE TABLE fixture_field_rel(id bigint,field_id bigint CONSTRAINT fixture_nullable_metadata_fk REFERENCES ir_model_fields(id)); INSERT INTO fixture_field_rel VALUES(NULL,1)").await;
    let diagnostic = "Uninstall impact cannot describe survivor foreign key effect 'fixture_nullable_metadata_fk'; unsupported until a later release";
    assert_error(
        system
            .preview_uninstall_authorized(&[n("note")], &allowed())
            .await
            .unwrap_err(),
        ErrorKind::Validation,
        diagnostic,
    );
    assert_note_intact(&installed.orm).await;
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn confirmation_rechecks_writer_committed_before_membership_lock_grant() {
    use std::sync::Arc;
    use std::time::Duration;
    let (db, _addons, system, installed) = installed_note().await;
    let system = Arc::new(system);
    let impact = system
        .preview_uninstall_authorized(&[n("note")], &allowed())
        .await
        .unwrap();
    let mut writer = installed.orm.begin(erp_orm::SUPERUSER_ID).await.unwrap();
    writer
        .batch("INSERT INTO test_note(id,name) VALUES(12,'committed while confirmation waits')")
        .await
        .unwrap();
    let task_system = Arc::clone(&system);
    let fingerprint = impact.fingerprint().as_str().to_owned();
    let confirmation = tokio::spawn(async move {
        task_system
            .uninstall_authorized(&[n("note")], &fingerprint, &allowed())
            .await
    });
    assert!(eventually(Duration::from_secs(3), || async {
        rows(&installed.orm, "SELECT EXISTS(SELECT 1 FROM pg_locks WHERE relation='test_note'::regclass AND mode='AccessExclusiveLock' AND NOT granted)::text").await[0][0].as_deref() == Some("true")
    }).await, "confirmation never waited for its exclusive membership lock");
    writer.commit().await.unwrap();
    drop(writer);
    assert_error(
        confirmation.await.unwrap().unwrap_err(),
        ErrorKind::Conflict,
        "Uninstall impact changed; preview again before confirming",
    );
    assert_eq!(
        rows(&installed.orm, "SELECT id::text FROM test_note ORDER BY id").await,
        vec![vec![Some("11".into())], vec![Some("12".into())]]
    );
    assert_note_intact(&installed.orm).await;
    db.cleanup().await.unwrap();
}

struct UnexpectedLifecycleService(std::sync::Arc<AtomicUsize>);
impl erp_modules::lifecycle_hook::LifecycleHook for UnexpectedLifecycleService {
    fn before_commit<'a>(
        &'a self,
        _tx: &'a mut Transaction,
        _change: erp_modules::lifecycle_hook::LifecycleChange<'a>,
    ) -> BoxFuture<'a, erp_modules::Result<()>> {
        Box::pin(async move {
            self.0.fetch_add(1, Ordering::SeqCst);
            Err(erp_fields::Error::programming("unplanned lifecycle service executed").into())
        })
    }
}

async fn uninstall_durable_snapshot(orm: &erp_orm::Orm) -> Vec<Vec<Option<String>>> {
    rows(orm, "SELECT 'state', row_to_json(m)::text FROM ir_module_module m WHERE name='note' UNION ALL SELECT 'identity', row_to_json(i)::text FROM installed_addon_identity i UNION ALL SELECT 'audit', row_to_json(a)::text FROM erp_module_lifecycle_audit a UNION ALL SELECT 'signal', row_to_json(s)::text FROM erp_signal_generation s UNION ALL SELECT 'data', row_to_json(d)::text FROM test_note d ORDER BY 1,2").await
}

#[tokio::test]
async fn registered_lifecycle_service_refuses_preview_without_effects() {
    let (db, addons, _plain, installed) = installed_note().await;
    let calls = std::sync::Arc::new(AtomicUsize::new(0));
    let system = builder(&db, &addons)
        .code([&NOTE])
        .decls([model("note", "test.note")])
        .lifecycle_hook(std::sync::Arc::new(UnexpectedLifecycleService(
            calls.clone(),
        )))
        .build()
        .unwrap();
    let before = uninstall_durable_snapshot(&installed.orm).await;
    assert_error(
        system
            .preview_uninstall_authorized(&[n("note")], &allowed())
            .await
            .unwrap_err(),
        ErrorKind::Validation,
        "Uninstall impact cannot describe registered lifecycle services; unsupported until a later release",
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(uninstall_durable_snapshot(&installed.orm).await, before);
    assert_note_intact(&installed.orm).await;
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn registered_lifecycle_service_refuses_confirmation_without_effects() {
    let (db, addons, plain, installed) = installed_note().await;
    let preview = plain
        .preview_uninstall_authorized(&[n("note")], &allowed())
        .await
        .unwrap();
    let calls = std::sync::Arc::new(AtomicUsize::new(0));
    let system = builder(&db, &addons)
        .code([&NOTE])
        .decls([model("note", "test.note")])
        .lifecycle_hook(std::sync::Arc::new(UnexpectedLifecycleService(
            calls.clone(),
        )))
        .build()
        .unwrap();
    let before = uninstall_durable_snapshot(&installed.orm).await;
    assert_error(
        system
            .uninstall_authorized(&[n("note")], preview.fingerprint().as_str(), &allowed())
            .await
            .unwrap_err(),
        ErrorKind::Validation,
        "Uninstall impact cannot describe registered lifecycle services; unsupported until a later release",
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(uninstall_durable_snapshot(&installed.orm).await, before);
    assert_note_intact(&installed.orm).await;
    db.cleanup().await.unwrap();
}

struct EmptyPackageRuntime(std::sync::Arc<AtomicUsize>);
impl erp_modules::PackageRuntime for EmptyPackageRuntime {
    fn model_defs(
        &self,
        module: &erp_modules::ModuleName,
    ) -> erp_modules::Result<Vec<erp_registry::ModelDef>> {
        Err(erp_fields::Error::programming(format!(
            "empty fixture runtime has no package '{module}'"
        ))
        .into())
    }
    fn wasm_methods(
        &self,
        _plain: &erp_orm::Orm,
        _modules: &[erp_modules::ModuleName],
    ) -> erp_modules::Result<Vec<erp_orm::WasmMethodImpl>> {
        // This fixture explicitly has no package methods; native declarations remain native.
        Ok(Vec::new())
    }
    fn before_commit<'a>(
        &'a self,
        _tx: &'a mut Transaction,
        _change: erp_modules::PackageChange<'a>,
    ) -> BoxFuture<'a, erp_modules::Result<()>> {
        Box::pin(async move {
            self.0.fetch_add(1, Ordering::SeqCst);
            Err(erp_fields::Error::programming("unplanned package callback executed").into())
        })
    }
}

#[tokio::test]
async fn empty_package_runtime_refuses_without_effects() {
    let (db, addons, _plain, installed) = installed_note().await;
    let calls = std::sync::Arc::new(AtomicUsize::new(0));
    let system = builder(&db, &addons)
        .code([&NOTE])
        .decls([model("note", "test.note")])
        .package_runtime(std::sync::Arc::new(EmptyPackageRuntime(calls.clone())))
        .build()
        .unwrap();
    let before = uninstall_durable_snapshot(&installed.orm).await;
    for error in [
        system
            .preview_uninstall_authorized(&[n("note")], &allowed())
            .await
            .unwrap_err(),
        system
            .uninstall_authorized(
                &[n("note")],
                &format!("uninstall-v1:{}", "0".repeat(64)),
                &allowed(),
            )
            .await
            .unwrap_err(),
    ] {
        assert_error(
            error,
            ErrorKind::Validation,
            "Uninstall impact cannot describe package runtime lifecycle callbacks; unsupported until a later release",
        );
    }
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(uninstall_durable_snapshot(&installed.orm).await, before);
    assert_note_intact(&installed.orm).await;
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn composition_blockers_are_negative_only_and_deterministic() {
    use erp_modules::UninstallCompositionBlocker as B;
    let db = TestDb::create().await.unwrap();
    let addons = Addons::new();
    let calls = std::sync::Arc::new(AtomicUsize::new(0));
    let make = |signal, packages, loader, services| {
        let mut build = builder(&db, &addons);
        if signal {
            build = build.signal(erp_orm::Signal::new(erp_orm::Generations::default()));
        }
        if packages {
            build = build.package_runtime(std::sync::Arc::new(EmptyPackageRuntime(calls.clone())));
        }
        if loader {
            build = build.data_loader(std::sync::Arc::new(RecordingLoader));
        }
        if services {
            build = build.lifecycle_hook(std::sync::Arc::new(UnexpectedLifecycleService(
                calls.clone(),
            )));
        }
        build.build().unwrap()
    };
    for (flags, expected, message) in [
        (
            (true, true, true, true),
            Some(B::PackageRuntime),
            Some(
                "Uninstall impact cannot describe package runtime lifecycle callbacks; unsupported until a later release",
            ),
        ),
        ((true, false, false, false), None, None),
        (
            (false, true, true, true),
            Some(B::PackageRuntime),
            Some(
                "Uninstall impact cannot describe package runtime lifecycle callbacks; unsupported until a later release",
            ),
        ),
        (
            (false, false, true, true),
            Some(B::CustomDataLoader),
            Some(
                "Uninstall impact cannot describe custom data loader purge callbacks; unsupported until a later release",
            ),
        ),
        (
            (false, false, false, true),
            Some(B::LifecycleServices),
            Some(
                "Uninstall impact cannot describe registered lifecycle services; unsupported until a later release",
            ),
        ),
    ] {
        let system = make(flags.0, flags.1, flags.2, flags.3);
        assert_eq!(system.uninstall_composition_blocker(), expected);
        if let Some(msg) = message {
            assert_eq!(
                system.uninstall_composition_blocker().unwrap().message(),
                msg
            );
        }
        assert_eq!(system.uninstall_composition_blocker(), expected);
    }
    let plain = make(false, false, false, false);
    assert_eq!(plain.uninstall_composition_blocker(), None);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(take_log().is_empty());
    db.cleanup().await.unwrap();
}
