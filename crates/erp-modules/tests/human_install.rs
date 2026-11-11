//! Lifecycle seam tests use a trusted test authorizer, not fake authentication evidence.
#![allow(clippy::unwrap_used, clippy::panic, reason = "test assertions")]
mod common;
use common::*;
use erp_fields::ErrorKind;
use erp_modules::error::{Error, ModuleStep};
use erp_modules::install::{InstallAuthorization, InstallAuthorizer};
use erp_modules::{HookImpl, HookKind, InitOptions, OPERATION_LOCK};
use erp_orm::{Actor, BoxFuture, GroupSet, PrincipalBuilder, Resolution, Transaction, UserId};
use erp_registry::AddonDescriptor;
use erp_testkit::TestDb;
use std::num::NonZeroU64;
use std::sync::atomic::{AtomicUsize, Ordering};

static NOTE: AddonDescriptor = AddonDescriptor {
    name: "note",
    depends: &["base"],
    exact_only: false,
};
static NOTE_WITH_EXTRA: AddonDescriptor = AddonDescriptor {
    name: "note",
    depends: &["base", "extra"],
    exact_only: false,
};
struct Authorizer {
    deny: bool,
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
            let principal = PrincipalBuilder::user(UserId::new(2).unwrap())
                .resolution(
                    Resolution::in_id_order(GroupSet::default(), [], NonZeroU64::MIN).unwrap(),
                )
                .build()
                .unwrap();
            InstallAuthorization::new(principal, "019a0000-0000-7000-8000-000000000001".into())
        })
    }
}
fn inspect_actor(tx: &mut Transaction) -> BoxFuture<'_, erp_fields::Result<()>> {
    Box::pin(async move {
        let env = tx.env();
        assert_eq!(env.uid(), 1);
        let Some(Actor::Impersonator(actor)) = env.principal().actor() else {
            panic!("human actor lost")
        };
        assert_eq!(actor.subject().get(), 2);
        assert!(matches!(actor.elevation(), erp_orm::Elevation::None));
        Ok(())
    })
}
static INSPECT: HookImpl =
    HookImpl::new("note", HookKind::PostInit, "inspect_actor", inspect_actor);

#[tokio::test]
async fn authorized_install_preserves_actor_and_commits_audit() {
    let db = TestDb::create().await.unwrap();
    let addons = Addons::new();
    addons.add(
        "note",
        "name='Note'\ndepends=['base']\npost_init_hook='inspect_actor'\n",
    );
    let system = builder(&db, &addons)
        .code([&NOTE])
        .hooks([&INSPECT])
        .decls([model("note", "test.note")])
        .build()
        .unwrap();
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    exec(
        &init.orm,
        "INSERT INTO res_users(id, name) VALUES (2, 'Administrator')",
    )
    .await;
    let authorizer = Authorizer {
        deny: false,
        calls: AtomicUsize::new(0),
    };
    let result = system
        .install_authorized(&[n("note")], &authorizer)
        .await
        .unwrap();
    assert_eq!(authorizer.calls.load(Ordering::SeqCst), 1);
    assert_eq!(state(&result.orm, "note").await, "installed");
    assert_eq!(rows(&result.orm, "SELECT actor_uid::text, session_id::text, requested::text FROM erp_module_install_audit").await,
        vec![vec![Some("2".into()), Some("019a0000-0000-7000-8000-000000000001".into()), Some("{note}".into())]]);
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn authorized_install_creates_audit_table_on_legacy_database() {
    let db = TestDb::create().await.unwrap();
    let addons = Addons::new();
    addons.add("note", "name='Note'\ndepends=['base']\n");
    let system = builder(&db, &addons).code([&NOTE]).build().unwrap();
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    exec(&init.orm, "DROP TABLE erp_module_install_audit").await;
    assert!(!table_exists(&init.orm, "erp_module_install_audit").await);

    exec(
        &init.orm,
        "INSERT INTO res_users(id, name) VALUES (2, 'Administrator')",
    )
    .await;
    let authorizer = Authorizer {
        deny: false,
        calls: AtomicUsize::new(0),
    };
    let result = system
        .install_authorized(&[n("note")], &authorizer)
        .await
        .unwrap();

    assert!(table_exists(&result.orm, "erp_module_install_audit").await);
    assert_eq!(authorizer.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        rows(
            &result.orm,
            "SELECT actor_uid::text, requested::text FROM erp_module_install_audit"
        )
        .await,
        vec![vec![Some("2".into()), Some("{note}".into())]]
    );
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn revoked_authority_changes_nothing() {
    let db = TestDb::create().await.unwrap();
    let addons = Addons::new();
    addons.add("note", "name='Note'\ndepends=['base']\n");
    let system = builder(&db, &addons).code([&NOTE]).build().unwrap();
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    exec(&init.orm, "DROP TABLE erp_module_install_audit").await;
    assert!(!table_exists(&init.orm, "erp_module_install_audit").await);
    let before = states(&init.orm).await;
    let authorizer = Authorizer {
        deny: true,
        calls: AtomicUsize::new(0),
    };
    let error = system
        .install_authorized(&[n("note")], &authorizer)
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "revoked system authority");
    match error {
        Error::Orm(source) => {
            assert_eq!(source.kind(), ErrorKind::Access);
            assert_eq!(source.message(), "revoked system authority");
        }
        other => panic!("expected the typed authority error, got {other:?}"),
    }
    assert_eq!(authorizer.calls.load(Ordering::SeqCst), 1);
    assert_eq!(states(&init.orm).await, before);
    assert!(!table_exists(&init.orm, "erp_module_install_audit").await);
    db.cleanup().await.unwrap();
}

fn fail_install(_tx: &mut Transaction) -> BoxFuture<'_, erp_fields::Result<()>> {
    Box::pin(async {
        Err(erp_fields::Error::validation(
            "fixture hook rejects installation",
        ))
    })
}
static FAIL_INSTALL: HookImpl =
    HookImpl::new("note", HookKind::PostInit, "fail_install", fail_install);

#[tokio::test]
async fn failed_install_does_not_leave_schema_or_audit() {
    let db = TestDb::create().await.unwrap();
    let addons = Addons::new();
    addons.add(
        "note",
        "name='Note'\ndepends=['base']\npost_init_hook='fail_install'\n",
    );
    let system = builder(&db, &addons)
        .code([&NOTE])
        .hooks([&FAIL_INSTALL])
        .decls([model("note", "test.note")])
        .build()
        .unwrap();
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    exec(
        &init.orm,
        "INSERT INTO res_users(id, name) VALUES (2, 'Administrator')",
    )
    .await;
    let authorizer = Authorizer {
        deny: false,
        calls: AtomicUsize::new(0),
    };
    let error = system
        .install_authorized(&[n("note")], &authorizer)
        .await
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "Module \"note\": post_init_hook 'fail_install' failed"
    );
    match error {
        Error::StepFailed {
            module,
            step:
                ModuleStep::Hook {
                    kind: HookKind::PostInit,
                    name: "fail_install",
                },
            file: None,
            source,
        } => {
            assert_eq!(module.as_str(), "note");
            match *source {
                Error::Orm(source) => {
                    assert_eq!(source.kind(), ErrorKind::Validation);
                    assert_eq!(source.message(), "fixture hook rejects installation");
                }
                other => panic!("expected the original hook error as an ORM source, got {other:?}"),
            }
        }
        other => panic!("expected a post-init hook step failure, got {other:?}"),
    }
    assert!(!table_exists(&init.orm, "test_note").await);
    assert_eq!(state(&init.orm, "note").await, "uninstalled");
    assert_eq!(
        rows(
            &init.orm,
            "SELECT count(*)::text FROM erp_module_install_audit"
        )
        .await,
        vec![vec![Some("0".into())]]
    );
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn busy_lifecycle_never_checks_or_uses_authority() {
    let db = TestDb::create().await.unwrap();
    let addons = Addons::new();
    addons.add("note", "name='Note'\ndepends=['base']\n");
    let system = builder(&db, &addons).code([&NOTE]).build().unwrap();
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    let mut held = init.orm.begin(erp_orm::SUPERUSER_ID).await.unwrap();
    held.query(
        "SELECT pg_advisory_xact_lock($1, $2)",
        &[&OPERATION_LOCK.0, &OPERATION_LOCK.1],
    )
    .await
    .unwrap();
    let authorizer = Authorizer {
        deny: false,
        calls: AtomicUsize::new(0),
    };
    assert!(matches!(
        system.install_authorized(&[n("note")], &authorizer).await,
        Err(erp_modules::Error::OperationInProgress)
    ));
    assert_eq!(authorizer.calls.load(Ordering::SeqCst), 0);
    held.rollback().await.unwrap();
    assert_eq!(state(&init.orm, "note").await, "uninstalled");
    db.cleanup().await.unwrap();
}

async fn installed_note() -> (
    TestDb,
    Addons,
    erp_modules::ModuleSystem,
    erp_modules::OperationOutcome,
) {
    let db = TestDb::create().await.unwrap();
    let addons = Addons::new();
    addons.add("note", "name='Note'\nversion='1.0'\ndepends=['base']\n");
    let system = builder(&db, &addons).code([&NOTE]).build().unwrap();
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    exec(
        &init.orm,
        "INSERT INTO res_users(id,name) VALUES (2,'Administrator')",
    )
    .await;
    let installed = system
        .button_immediate_install(&su(&init.orm), &[n("note")])
        .await
        .unwrap();
    (db, addons, system, installed)
}

fn allowed() -> Authorizer {
    Authorizer {
        deny: false,
        calls: AtomicUsize::new(0),
    }
}

fn assert_validation(error: Error, message: &str) {
    match error {
        Error::Orm(source) => {
            assert_eq!(source.kind(), ErrorKind::Validation);
            assert_eq!(source.message(), message);
        }
        other => panic!("expected Validation, got {other:?}"),
    }
}

#[tokio::test]
async fn authorized_upgrade_audits_resolved_closure_and_preserves_install_history() {
    let (db, addons, old_system, _before_audit) = installed_note().await;
    let installed = old_system
        .install_authorized(&[n("note")], &allowed())
        .await
        .unwrap();
    assert!(installed.post_commit.is_clean());
    let historical = rows(&installed.orm, "SELECT actor_uid::text,session_id::text,requested::text,installed::text,created_at::text FROM erp_module_install_audit ORDER BY id").await;
    assert_eq!(historical.len(), 1);
    addons.add("extra", "name='Extra'\ndepends=['base']\n");
    addons.add(
        "note",
        "name='Note'\nversion='2.0'\ndepends=['base','extra']\n",
    );
    // A new native binary must agree with the changed manifest dependency set.
    let system = builder(&db, &addons)
        .code([&NOTE_WITH_EXTRA])
        .build()
        .unwrap();
    enroll_fixture_transition(&system, &["note"]).await;
    let authorizer = allowed();
    let upgraded = system
        .upgrade_authorized(&[n("note")], &authorizer)
        .await
        .unwrap();
    assert_eq!(authorizer.calls.load(Ordering::SeqCst), 1);
    assert!(upgraded.post_commit.is_clean());
    assert_eq!(upgraded.upgraded, vec![n("note")]);
    assert_eq!(upgraded.installed, vec![n("extra")]);
    assert_eq!(
        rows(
            &upgraded.orm,
            "SELECT latest_version FROM ir_module_module WHERE name='note'"
        )
        .await,
        vec![vec![Some("2.0.0".into())]]
    );
    assert_eq!(rows(&upgraded.orm, "SELECT actor_uid::text,session_id::text,operation,requested::text,resolved::text,installed::text,upgraded::text,outcome FROM erp_module_lifecycle_audit WHERE operation='upgrade'").await,
        vec![vec![Some("2".into()),Some("019a0000-0000-7000-8000-000000000001".into()),Some("upgrade".into()),Some("{note}".into()),Some("{extra,note}".into()),Some("{extra}".into()),Some("{note}".into()),Some("committed".into())]]);
    assert_eq!(rows(&installed.orm, "SELECT actor_uid::text,session_id::text,requested::text,installed::text,created_at::text FROM erp_module_install_audit ORDER BY id").await, historical);
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn authorized_upgrade_rejects_invalid_requests() {
    let (db, addons, system, installed) = installed_note().await;
    let authorizer = allowed();
    assert_validation(
        system
            .upgrade_authorized(&[n("unknown")], &authorizer)
            .await
            .unwrap_err(),
        "Upgrade requires installed addon 'unknown'",
    );
    let at_bound: Vec<_> = (0..64)
        .map(|index| n(&format!("missing_{index}")))
        .collect();
    assert_validation(
        system
            .upgrade_authorized(&at_bound, &authorizer)
            .await
            .unwrap_err(),
        "Upgrade requires installed addon 'missing_0'",
    );
    for names in [vec![], vec![n("note"); 65]] {
        assert_validation(
            system
                .upgrade_authorized(&names, &authorizer)
                .await
                .unwrap_err(),
            "Upgrade requires between 1 and 64 addon names",
        );
    }
    assert_validation(
        system
            .upgrade_authorized(&[n("note"), n("note")], &authorizer)
            .await
            .unwrap_err(),
        "Upgrade requires unique addon names",
    );
    addons.add("extra", "name='Extra'\ndepends=['base']\n");
    assert_validation(
        system
            .upgrade_authorized(&[n("extra")], &authorizer)
            .await
            .unwrap_err(),
        "Upgrade requires installed addon 'extra'",
    );
    assert_eq!(state(&installed.orm, "note").await, "installed");
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn authorized_upgrade_rechecks_authority_before_legacy_audit_creation() {
    let (db, _addons, system, installed) = installed_note().await;
    exec(&installed.orm, "DROP TABLE erp_module_lifecycle_audit").await;
    let authorizer = Authorizer {
        deny: true,
        calls: AtomicUsize::new(0),
    };
    let error = system
        .upgrade_authorized(&[n("note")], &authorizer)
        .await
        .unwrap_err();
    match error {
        Error::Orm(source) => {
            assert_eq!(source.kind(), ErrorKind::Access);
            assert_eq!(source.message(), "revoked system authority");
        }
        other => panic!("expected Access, got {other:?}"),
    }
    assert!(!table_exists(&installed.orm, "erp_module_lifecycle_audit").await);
    assert_eq!(
        rows(
            &installed.orm,
            "SELECT latest_version FROM ir_module_module WHERE name='note'"
        )
        .await,
        vec![vec![Some("1.0.0".into())]]
    );
    let result = system
        .upgrade_authorized(&[n("note")], &allowed())
        .await
        .unwrap();
    assert!(table_exists(&result.orm, "erp_module_lifecycle_audit").await);
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn authorized_upgrade_refuses_pending_work() {
    let (db, addons, system, installed) = installed_note().await;
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
    assert_validation(
        system
            .upgrade_authorized(&[n("note")], &allowed())
            .await
            .unwrap_err(),
        "Pending module operations must be resolved before human lifecycle operation",
    );
    assert_eq!(states(&installed.orm).await, before);
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn authorized_upgrade_audit_failure_rolls_back_all_effects() {
    let (db, addons, _old_system, installed) = installed_note().await;
    exec(&installed.orm,"ALTER TABLE erp_module_lifecycle_audit ADD CONSTRAINT fixture_reject_upgrade CHECK(operation <> 'upgrade')").await;
    addons.add("extra", "name='Extra'\ndepends=['base']\n");
    addons.add(
        "note",
        "name='Note'\nversion='2.0'\ndepends=['base','extra']\n",
    );
    // A new native binary must agree with the changed manifest dependency set.
    let system = builder(&db, &addons)
        .code([&NOTE_WITH_EXTRA])
        .build()
        .unwrap();
    enroll_fixture_transition(&system, &["note"]).await;
    let error = system
        .upgrade_authorized(&[n("note")], &allowed())
        .await
        .unwrap_err();
    match error {
        Error::Orm(source) => {
            assert_eq!(source.kind(), ErrorKind::Validation);
            assert_eq!(source.sqlstate(), Some("23514"));
            assert_eq!(
                source.message(),
                "The operation cannot be completed: new row for relation \"erp_module_lifecycle_audit\" violates check constraint \"fixture_reject_upgrade\""
            );
        }
        other => panic!("expected audit constraint failure, got {other:?}"),
    }
    assert_eq!(state(&installed.orm, "note").await, "installed");
    assert_eq!(
        rows(
            &installed.orm,
            "SELECT latest_version FROM ir_module_module WHERE name='note'"
        )
        .await,
        vec![vec![Some("1.0.0".into())]]
    );
    assert_eq!(
        rows(
            &installed.orm,
            "SELECT count(*)::text FROM ir_module_module WHERE name='extra' AND state='installed'"
        )
        .await,
        vec![vec![Some("0".into())]]
    );
    assert_eq!(
        rows(
            &installed.orm,
            "SELECT count(*)::text FROM erp_module_lifecycle_audit"
        )
        .await,
        vec![vec![Some("0".into())]]
    );
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn authorized_upgrade_admits_every_affected_addon_before_effects() {
    let (db, addons, system, installed) = installed_note().await;
    addons.add("dependent", "name='Dependent'\ndepends=['note']\n");
    let result = system
        .button_immediate_install(&su(&installed.orm), &[n("dependent")])
        .await
        .unwrap();
    let upgraded = system
        .upgrade_authorized(&[n("note")], &allowed())
        .await
        .unwrap();
    let mut upgraded_names = upgraded.upgraded.clone();
    upgraded_names.sort();
    assert_eq!(upgraded_names, vec![n("dependent"), n("note")]);
    assert_eq!(state(&result.orm, "dependent").await, "installed");
    // Remove the dependent through the trusted CLI seam to isolate a new install dependency.
    let result = system
        .button_immediate_uninstall(&su(&result.orm), &[n("dependent")])
        .await
        .unwrap();
    addons.add("extra", "name='Extra'\ndepends=['base']\n");
    addons.file(
        "extra",
        "migrations/2.0/pre.py",
        "# prohibited executable content",
    );
    addons.add(
        "note",
        "name='Note'\nversion='2.0'\ndepends=['base','extra']\n",
    );
    // A new native binary must agree with the changed manifest dependency set.
    let system = builder(&db, &addons)
        .code([&NOTE_WITH_EXTRA])
        .build()
        .unwrap();
    enroll_fixture_transition(&system, &["note"]).await;
    let error = system
        .upgrade_authorized(&[n("note")], &allowed())
        .await
        .unwrap_err();
    match error {
        Error::AddonCodeMismatch { module, message } => {
            assert_eq!(module, n("extra"));
            assert_eq!(
                message,
                "Human installation of an addon that is not linked requires a data-only addon: migrations is not allowed (rule: no migrations/ directory: its SQL runs at upgrade)"
            );
        }
        other => panic!("expected code admission refusal, got {other:?}"),
    }
    assert_eq!(
        rows(
            &result.orm,
            "SELECT latest_version FROM ir_module_module WHERE name='note'"
        )
        .await,
        vec![vec![Some("1.0.0".into())]]
    );
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn authorized_upgrade_busy_lock_never_calls_authorizer() {
    let (db, _addons, system, installed) = installed_note().await;
    let mut held = installed.orm.begin(erp_orm::SUPERUSER_ID).await.unwrap();
    held.query(
        "SELECT pg_advisory_xact_lock($1,$2)",
        &[&OPERATION_LOCK.0, &OPERATION_LOCK.1],
    )
    .await
    .unwrap();
    let authorizer = allowed();
    let error = system
        .upgrade_authorized(&[n("note")], &authorizer)
        .await
        .unwrap_err();
    assert!(matches!(error, Error::OperationInProgress));
    assert_eq!(
        error.to_string(),
        "ERPForAll is currently processing another module operation.\nPlease try again later or contact your system administrator."
    );
    assert_eq!(authorizer.calls.load(Ordering::SeqCst), 0);
    held.rollback().await.unwrap();
    db.cleanup().await.unwrap();
}

fn lose_commit_answer(tx: &mut Transaction) -> BoxFuture<'_, erp_fields::Result<()>> {
    Box::pin(async move {
        // Pending state owned by a different request must survive ambiguous recovery.
        tx.execute(
            "UPDATE ir_module_module SET state='to upgrade' WHERE name='base'",
            &[],
        )
        .await?;
        erp_modules::schedule_post_commit(
            tx,
            &n("note"),
            &pk("unknown_probe"),
            &analyze("ir_module_module"),
        )
        .await?;
        tx.inject_commit_fault(erp_orm::CommitFault::AnswerLostAfterCommit);
        Ok(())
    })
}
static LOSE_COMMIT_ANSWER: HookImpl = HookImpl::new(
    "note",
    HookKind::PostInit,
    "lose_commit_answer",
    lose_commit_answer,
);

#[tokio::test]
async fn unknown_install_commit_does_not_reset_unrelated_pending_state() {
    let db = TestDb::create().await.unwrap();
    let addons = Addons::new();
    addons.add(
        "note",
        "name='Note'\ndepends=['base']\npost_init_hook='lose_commit_answer'\n",
    );
    let system = builder(&db, &addons)
        .code([&NOTE])
        .hooks([&LOSE_COMMIT_ANSWER])
        .build()
        .unwrap();
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    exec(
        &init.orm,
        "INSERT INTO res_users(id, name) VALUES (2, 'Administrator')",
    )
    .await;
    let authorizer = Authorizer {
        deny: false,
        calls: AtomicUsize::new(0),
    };
    let error = system
        .install_authorized(&[n("note")], &authorizer)
        .await
        .unwrap_err();
    match error {
        Error::CommitOutcomeUnknown { source } => {
            assert_eq!(source.kind(), ErrorKind::Database);
            assert_eq!(
                source.message(),
                "database connection closed: injected fault after COMMIT"
            );
        }
        other => panic!("expected typed unknown commit, got {other:?}"),
    }
    assert_eq!(state(&init.orm, "note").await, "installed");
    assert_eq!(state(&init.orm, "base").await, "to upgrade");
    assert_eq!(
        rows(
            &init.orm,
            "SELECT count(*)::text FROM erp_module_install_audit"
        )
        .await,
        vec![vec![Some("1".into())]]
    );
    assert_eq!(rows(&init.orm, "SELECT state::text FROM ir_module_post_commit WHERE module='note' AND key='unknown_probe'").await, vec![vec![Some("pending".into())]], "unknown commit must not run post-commit work");
    assert_eq!(
        authorizer.calls.load(Ordering::SeqCst),
        1,
        "unknown writes are not replayed"
    );
    db.cleanup().await.unwrap();
}
