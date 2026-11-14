//! Installed file identity is durable across independent module systems.
#![allow(clippy::unwrap_used, clippy::panic, reason = "test assertions")]
mod common;
use common::*;
use std::collections::BTreeMap;

use erp_fields::ErrorKind;
use erp_modules::error::Error;
use erp_modules::{InitOptions, ModuleName, ModuleSystem, OPERATION_LOCK};
use erp_orm::{Orm, SUPERUSER_ID};
use erp_testkit::TestDb;

fn validation(error: Error, message: &str) {
    match error {
        Error::Orm(error) => {
            assert_eq!(error.kind(), ErrorKind::Validation);
            assert_eq!(error.to_string(), message);
        }
        other => panic!("expected Validation, got {other:?}"),
    }
}

#[tokio::test]
async fn legacy_identity_requires_explicit_audited_enrollment() {
    let db = TestDb::create().await.unwrap();
    let addons = Addons::new();
    addons.add("note", "name='Note'\ndepends=['base']\n");
    let system = builder(&db, &addons).build().unwrap();
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    let installed = system
        .button_immediate_install(&su(&init.orm), &[n("note")])
        .await
        .unwrap();
    let receipts = rows(
        &installed.orm,
        "SELECT name, content_hash FROM installed_addon_identity ORDER BY name",
    )
    .await;
    assert_eq!(receipts.len(), 2);
    exec(
        &installed.orm,
        "DELETE FROM installed_addon_identity WHERE name='note'",
    )
    .await;
    validation(
        system
            .button_immediate_upgrade(&su(&installed.orm), &[n("note")])
            .await
            .unwrap_err(),
        "Addon 'note' identity unknown, run identity-enroll first",
    );
    system
        .identity_enroll(
            &[n("note")],
            "operator@example.test",
            "existing database provenance reviewed",
        )
        .await
        .unwrap();
    assert_eq!(
        rows(
            &installed.orm,
            "SELECT name, content_hash FROM installed_addon_identity ORDER BY name"
        )
        .await,
        receipts
    );
    assert_eq!(
        rows(
            &installed.orm,
            "SELECT operator, note, names::text FROM installed_addon_identity_audit ORDER BY id"
        )
        .await,
        vec![vec![
            Some("operator@example.test".into()),
            Some("existing database provenance reviewed".into()),
            Some("{note}".into())
        ]]
    );
    let other = ModuleSystem::builder(db.pool().clone(), addons.index())
        .linked_hooks(false)
        .build()
        .unwrap();
    let upgraded = other
        .button_immediate_upgrade(&su(&other.load().await.unwrap().orm), &[n("note")])
        .await
        .unwrap();
    assert_eq!(
        rows(
            &upgraded.orm,
            "SELECT name, content_hash FROM installed_addon_identity ORDER BY name"
        )
        .await,
        receipts
    );
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn changed_artifact_cannot_be_upgraded_or_uninstalled() {
    let db = TestDb::create().await.unwrap();
    let addons = Addons::new();
    addons.add("note", "name='Note'\ndepends=['base']\n");
    let system = builder(&db, &addons).build().unwrap();
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    let installed = system
        .button_immediate_install(&su(&init.orm), &[n("note")])
        .await
        .unwrap();
    let before = rows(
        &installed.orm,
        "SELECT row_to_json(i)::text FROM installed_addon_identity i ORDER BY name",
    )
    .await;
    addons.file("note", "changed.txt", "replacement bytes");
    for result in [
        system
            .button_immediate_upgrade(&su(&installed.orm), &[n("note")])
            .await,
        system
            .button_immediate_uninstall(&su(&installed.orm), &[n("note")])
            .await,
    ] {
        validation(
            result.unwrap_err(),
            "Addon 'note' installed artifact identity does not match available content",
        );
    }
    assert_eq!(state(&installed.orm, "note").await, "installed");
    assert_eq!(
        rows(
            &installed.orm,
            "SELECT row_to_json(i)::text FROM installed_addon_identity i ORDER BY name"
        )
        .await,
        before
    );
    db.cleanup().await.unwrap();
}

async fn receipt_hashes(orm: &Orm) -> BTreeMap<ModuleName, String> {
    rows(
        orm,
        "SELECT name, content_hash FROM installed_addon_identity ORDER BY name",
    )
    .await
    .into_iter()
    .map(|row| (n(row[0].as_deref().unwrap()), row[1].clone().unwrap()))
    .collect()
}

async fn receipt_rows(orm: &Orm) -> Vec<Vec<Option<String>>> {
    rows(
        orm,
        "SELECT row_to_json(i)::text FROM installed_addon_identity i ORDER BY name",
    )
    .await
}

async fn audit_rows(orm: &Orm) -> Vec<Vec<Option<String>>> {
    rows(
        orm,
        "SELECT row_to_json(a)::text FROM installed_addon_identity_audit a ORDER BY id",
    )
    .await
}

#[tokio::test]
async fn uninstall_removes_only_the_committed_addon_receipt() {
    let db = TestDb::create().await.unwrap();
    let addons = Addons::new();
    addons.add("note", "name='Note'\ndepends=['base']\n");
    let system = builder(&db, &addons).build().unwrap();
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    let base_receipts = receipt_rows(&init.orm).await;
    let installed = system
        .button_immediate_install(&su(&init.orm), &[n("note")])
        .await
        .unwrap();
    let hashes = receipt_hashes(&installed.orm).await;
    let admitted = erp_modules::artifact_identity::ArtifactSnapshot::capture(
        "note",
        &addons.path().join("note").canonicalize().unwrap(),
    )
    .unwrap();
    assert_eq!(hashes.get(&n("note")).unwrap(), admitted.digest());
    assert_eq!(hashes.len(), 2);
    let removed = system
        .button_immediate_uninstall(&su(&installed.orm), &[n("note")])
        .await
        .unwrap();
    assert!(removed.post_commit.failed.is_empty());
    assert!(removed.post_commit.stopped.is_none());
    assert_eq!(state(&removed.orm, "note").await, "uninstalled");
    assert_eq!(receipt_rows(&removed.orm).await, base_receipts);
    assert!(
        audit_rows(&removed.orm).await.is_empty(),
        "ordinary lifecycle changes do not impersonate operator enrollment"
    );
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn malformed_identity_refuses_upgrade_and_uninstall_without_effects() {
    let db = TestDb::create().await.unwrap();
    let addons = Addons::new();
    addons.add("note", "name='Note'\ndepends=['base']\n");
    let system = builder(&db, &addons).build().unwrap();
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    let installed = system
        .button_immediate_install(&su(&init.orm), &[n("note")])
        .await
        .unwrap();
    exec(&installed.orm, "UPDATE installed_addon_identity SET content_hash='addon-v1:not-a-digest' WHERE name='note'").await;
    let before = receipt_rows(&installed.orm).await;
    let before_catalog = states(&installed.orm).await;
    for result in [
        system
            .button_immediate_upgrade(&su(&installed.orm), &[n("note")])
            .await,
        system
            .button_immediate_uninstall(&su(&installed.orm), &[n("note")])
            .await,
    ] {
        validation(
            result.unwrap_err(),
            "Addon 'note' installed artifact identity is malformed",
        );
        assert_eq!(receipt_rows(&installed.orm).await, before);
        assert_eq!(states(&installed.orm).await, before_catalog);
        assert!(audit_rows(&installed.orm).await.is_empty());
    }
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn forward_activation_preserves_unchanged_receipts_and_updates_changed_content() {
    let db = TestDb::create().await.unwrap();
    let addons = Addons::new();
    addons.add("note", "name='Note'\ndepends=['base']\n");
    let system = builder(&db, &addons).build().unwrap();
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    let installed = system
        .button_immediate_install(&su(&init.orm), &[n("note")])
        .await
        .unwrap();
    let previous = receipt_hashes(&installed.orm).await;
    let unchanged = receipt_rows(&installed.orm).await;
    system
        .identity_activate(
            &previous,
            "operator@example.test",
            "unchanged forward release",
        )
        .await
        .unwrap();
    assert_eq!(receipt_rows(&installed.orm).await, unchanged);
    assert!(audit_rows(&installed.orm).await.is_empty());

    addons.file("note", "release.txt", "approved forward candidate");
    let candidate = builder(&db, &addons)
        .identity_previous(previous.clone())
        .build()
        .unwrap();
    let snapshot = erp_modules::artifact_identity::ArtifactSnapshot::capture(
        "note",
        &addons.path().join("note").canonicalize().unwrap(),
    )
    .unwrap();
    let loaded = candidate.load().await.unwrap();
    let migrated = candidate
        .button_immediate_upgrade(&su(&loaded.orm), &[n("note")])
        .await
        .unwrap();
    assert_eq!(migrated.upgraded, vec![n("note")]);
    committed(migrated);
    assert_eq!(
        receipt_rows(&installed.orm).await,
        unchanged,
        "candidate migration records proof without prematurely stamping receipts"
    );
    assert_eq!(
        rows(&installed.orm, "SELECT operation,content_hashes->'note'->>'previous',content_hashes->'note'->>'current',(created_at IS NOT NULL)::text FROM installed_addon_identity_audit ORDER BY id").await,
        vec![vec![Some("forward-migrate".into()), previous.get(&n("note")).cloned(), Some(snapshot.digest().into()), Some("true".into())]]
    );
    candidate
        .identity_activate(
            &previous,
            "operator@example.test",
            "verified forward release",
        )
        .await
        .unwrap();
    let current = receipt_hashes(&installed.orm).await;
    assert_eq!(current.get(&n("note")).unwrap(), snapshot.digest());
    assert_ne!(current.get(&n("note")), previous.get(&n("note")));
    assert_eq!(current.get(&n("base")), previous.get(&n("base")));
    assert_eq!(
        rows(
            &installed.orm,
            "SELECT row_to_json(i)::text FROM installed_addon_identity i WHERE name='base'"
        )
        .await,
        vec![unchanged[0].clone()],
        "unrelated addon content must not cause an identity re-stamp"
    );
    assert_eq!(
        rows(&installed.orm, "SELECT operator,note,operation,(content_hashes->>'note'),(created_at IS NOT NULL)::text FROM installed_addon_identity_audit WHERE operation='activate' ORDER BY id").await,
        vec![vec![Some("operator@example.test".into()), Some("verified forward release".into()), Some("activate".into()), Some(snapshot.digest().into()), Some("true".into())]]
    );
    let repeated_rows = receipt_rows(&installed.orm).await;
    let repeated_audit = audit_rows(&installed.orm).await;
    candidate
        .identity_activate(
            &previous,
            "operator@example.test",
            "same candidate after v1 rollback",
        )
        .await
        .unwrap();
    assert_eq!(receipt_rows(&installed.orm).await, repeated_rows);
    assert_eq!(audit_rows(&installed.orm).await, repeated_audit);
    let mut unrelated_previous = previous.clone();
    unrelated_previous.insert(n("note"), previous.get(&n("base")).unwrap().clone());
    validation(
        candidate
            .identity_activate(
                &unrelated_previous,
                "operator@example.test",
                "unrelated preimage cannot use already-current path",
            )
            .await
            .unwrap_err(),
        "Addon 'note' has no committed forward migration for the candidate artifact",
    );
    assert_eq!(receipt_rows(&installed.orm).await, repeated_rows);
    assert_eq!(audit_rows(&installed.orm).await, repeated_audit);
    exec(
        &installed.orm,
        "UPDATE installed_addon_identity SET source_kind='embedded' WHERE name='note'",
    )
    .await;
    let damaged_source = receipt_rows(&installed.orm).await;
    validation(
        candidate
            .identity_activate(
                &previous,
                "operator@example.test",
                "same hash with wrong source kind",
            )
            .await
            .unwrap_err(),
        "Addon 'note' installed artifact identity does not match available content",
    );
    assert_eq!(receipt_rows(&installed.orm).await, damaged_source);
    assert_eq!(audit_rows(&installed.orm).await, repeated_audit);
    exec(
        &installed.orm,
        "UPDATE installed_addon_identity SET source_kind='folder' WHERE name='note'",
    )
    .await;
    // An independent system reads the committed receipt rather than a process-local counter.
    let other = builder(&db, &addons).build().unwrap();
    other
        .identity_activate(
            &current,
            "operator@example.test",
            "same candidate on another node",
        )
        .await
        .unwrap();
    assert_eq!(receipt_hashes(&installed.orm).await, current);
    assert_eq!(audit_rows(&installed.orm).await.len(), 2);
    // Fixture-only loss of the original witness must not turn current hashes into authority.
    exec(&installed.orm, "ALTER TABLE installed_addon_identity_audit DISABLE TRIGGER installed_addon_identity_audit_insert_only; DELETE FROM installed_addon_identity_audit WHERE operation='forward-migrate'; ALTER TABLE installed_addon_identity_audit ENABLE TRIGGER installed_addon_identity_audit_insert_only").await;
    let without_proof = audit_rows(&installed.orm).await;
    validation(
        candidate
            .identity_activate(
                &previous,
                "operator@example.test",
                "current hashes without original proof",
            )
            .await
            .unwrap_err(),
        "Addon 'note' has no committed forward migration for the candidate artifact",
    );
    assert_eq!(receipt_hashes(&installed.orm).await, current);
    assert_eq!(audit_rows(&installed.orm).await, without_proof);

    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn forward_activation_requires_the_installed_preimage() {
    let db = TestDb::create().await.unwrap();
    let addons = Addons::new();
    addons.add("note", "name='Note'\ndepends=['base']\n");
    let system = builder(&db, &addons).build().unwrap();
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    let installed = system
        .button_immediate_install(&su(&init.orm), &[n("note")])
        .await
        .unwrap();
    let before = receipt_rows(&installed.orm).await;
    let mut wrong_previous = receipt_hashes(&installed.orm).await;
    wrong_previous.insert(n("note"), wrong_previous.get(&n("base")).unwrap().clone());
    addons.file("note", "release.txt", "new candidate bytes");
    let candidate = builder(&db, &addons).build().unwrap();
    validation(
        candidate
            .identity_activate(
                &wrong_previous,
                "operator@example.test",
                "wrong previous release",
            )
            .await
            .unwrap_err(),
        "Addon 'note' installed artifact identity does not match available content",
    );
    assert_eq!(receipt_rows(&installed.orm).await, before);
    assert!(audit_rows(&installed.orm).await.is_empty());
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn identity_audit_failure_rolls_back_enrollment_and_activation() {
    let db = TestDb::create().await.unwrap();
    let addons = Addons::new();
    addons.add("note", "name='Note'\ndepends=['base']\n");
    let system = builder(&db, &addons).build().unwrap();
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    let installed = system
        .button_immediate_install(&su(&init.orm), &[n("note")])
        .await
        .unwrap();
    let previous = receipt_hashes(&installed.orm).await;
    let before = receipt_rows(&installed.orm).await;
    exec(&installed.orm, "ALTER TABLE installed_addon_identity_audit ADD CONSTRAINT fixture_reject_identity_audit CHECK(operator <> 'rejected@example.test')").await;
    addons.file("note", "release.txt", "candidate to roll back");
    let candidate = builder(&db, &addons)
        .identity_previous(previous.clone())
        .build()
        .unwrap();
    let loaded = candidate.load().await.unwrap();
    committed(
        candidate
            .button_immediate_upgrade(&su(&loaded.orm), &[n("note")])
            .await
            .unwrap(),
    );
    assert_eq!(receipt_rows(&installed.orm).await, before);
    let migration_audit = audit_rows(&installed.orm).await;
    assert_eq!(migration_audit.len(), 1);
    for result in [
        candidate
            .identity_enroll(
                &[n("note")],
                "rejected@example.test",
                "reject enrollment audit",
            )
            .await,
        candidate
            .identity_activate(
                &previous,
                "rejected@example.test",
                "reject activation audit",
            )
            .await,
    ] {
        match result.unwrap_err() {
            Error::Orm(source) => {
                assert_eq!(source.kind(), ErrorKind::Validation);
                assert_eq!(source.sqlstate(), Some("23514"));
                assert_eq!(
                    source.message(),
                    "The database refused the change: new row for relation \"installed_addon_identity_audit\" violates check constraint \"fixture_reject_identity_audit\""
                );
            }
            other => panic!("expected identity audit constraint failure, got {other:?}"),
        }
        assert_eq!(receipt_rows(&installed.orm).await, before);
        assert_eq!(audit_rows(&installed.orm).await, migration_audit);
        assert_eq!(state(&installed.orm, "note").await, "installed");
    }
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn identity_writes_refuse_another_nodes_lifecycle_lock_and_recover_after_release() {
    let db = TestDb::create().await.unwrap();
    let addons = Addons::new();
    addons.add("note", "name='Note'\ndepends=['base']\n");
    let system = builder(&db, &addons).build().unwrap();
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    let installed = system
        .button_immediate_install(&su(&init.orm), &[n("note")])
        .await
        .unwrap();
    let before = receipt_rows(&installed.orm).await;
    let previous = receipt_hashes(&installed.orm).await;
    let other = builder(&db, &addons).build().unwrap();
    let mut owner = installed.orm.begin(SUPERUSER_ID).await.unwrap();
    owner
        .query(
            "SELECT pg_advisory_xact_lock($1,$2)",
            &[&OPERATION_LOCK.0, &OPERATION_LOCK.1],
        )
        .await
        .unwrap();
    for result in [
        other
            .identity_enroll(
                &[n("note")],
                "operator@example.test",
                "contending enrollment",
            )
            .await,
        other
            .identity_activate(&previous, "operator@example.test", "contending activation")
            .await,
    ] {
        let error = result.unwrap_err();
        assert!(matches!(error, Error::OperationInProgress));
        assert_eq!(
            error.to_string(),
            "ERPForAll is currently processing another module operation.\nPlease try again later or contact your system administrator."
        );
    }
    assert_eq!(receipt_rows(&installed.orm).await, before);
    assert!(audit_rows(&installed.orm).await.is_empty());
    owner.rollback().await.unwrap();
    drop(owner);
    other
        .identity_enroll(&[n("note")], "operator@example.test", "lock released")
        .await
        .unwrap();
    assert_eq!(receipt_rows(&installed.orm).await, before);
    assert_eq!(audit_rows(&installed.orm).await.len(), 1);
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn failed_first_receipt_write_rolls_back_the_installation() {
    let db = TestDb::create().await.unwrap();
    let addons = Addons::new();
    addons.add("note", "name='Note'\ndepends=['base']\n");
    let system = builder(&db, &addons)
        .decls([model("note", "test.note")])
        .build()
        .unwrap();
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    let before = receipt_rows(&init.orm).await;
    exec(&init.orm, "ALTER TABLE installed_addon_identity ADD CONSTRAINT fixture_reject_identity_receipt CHECK(name <> 'note')").await;
    let error = system
        .button_immediate_install(&su(&init.orm), &[n("note")])
        .await
        .unwrap_err();
    match error {
        Error::Orm(source) => {
            assert_eq!(source.kind(), ErrorKind::Validation);
            assert_eq!(source.sqlstate(), Some("23514"));
            assert_eq!(
                source.message(),
                "The database refused the change: new row for relation \"installed_addon_identity\" violates check constraint \"fixture_reject_identity_receipt\""
            );
        }
        other => panic!("expected receipt constraint failure, got {other:?}"),
    }
    assert_eq!(state(&init.orm, "note").await, "uninstalled");
    assert_eq!(receipt_rows(&init.orm).await, before);
    assert!(!table_exists(&init.orm, "test_note").await);
    assert!(audit_rows(&init.orm).await.is_empty());
    db.cleanup().await.unwrap();
}

async fn assert_dependent_operation_refuses_bad_installed_dependency(unknown: bool) {
    let db = TestDb::create().await.unwrap();
    let addons = Addons::new();
    addons
        .add("library", "name='Library'\ndepends=['base']\n")
        .add("note", "name='Note'\ndepends=['library']\n");
    let system = builder(&db, &addons).build().unwrap();
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    let installed = system
        .button_immediate_install(&su(&init.orm), &[n("note")])
        .await
        .unwrap();
    let expected = if unknown {
        exec(
            &installed.orm,
            "DELETE FROM installed_addon_identity WHERE name='library'",
        )
        .await;
        "Addon 'library' identity unknown, run identity-enroll first"
    } else {
        exec(&installed.orm, "UPDATE installed_addon_identity SET content_hash=(SELECT content_hash FROM installed_addon_identity WHERE name='base') WHERE name='library'").await;
        "Addon 'library' installed artifact identity does not match available content"
    };
    let before = receipt_rows(&installed.orm).await;
    let before_catalog = states(&installed.orm).await;
    // Library remains installed rather than being scheduled for upgrade or removal. Its
    // admitted content still participates in the registry used by either operation.
    for result in [
        system
            .button_immediate_upgrade(&su(&installed.orm), &[n("note")])
            .await,
        system
            .button_immediate_uninstall(&su(&installed.orm), &[n("note")])
            .await,
    ] {
        validation(result.unwrap_err(), expected);
        assert_eq!(receipt_rows(&installed.orm).await, before);
        assert_eq!(states(&installed.orm).await, before_catalog);
        assert!(audit_rows(&installed.orm).await.is_empty());
    }
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn dependent_operation_refuses_unknown_installed_dependency_identity() {
    assert_dependent_operation_refuses_bad_installed_dependency(true).await;
}

#[tokio::test]
async fn dependent_operation_refuses_mismatched_installed_dependency_identity() {
    assert_dependent_operation_refuses_bad_installed_dependency(false).await;
}

#[tokio::test]
async fn forward_activation_refuses_changed_content_without_committed_candidate_migration() {
    let db = TestDb::create().await.unwrap();
    let addons = Addons::new();
    addons.add("note", "name='Note'\ndepends=['base']\n");
    let system = builder(&db, &addons).build().unwrap();
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    let installed = system
        .button_immediate_install(&su(&init.orm), &[n("note")])
        .await
        .unwrap();
    let previous = receipt_hashes(&installed.orm).await;
    let receipts = receipt_rows(&installed.orm).await;
    let audit = audit_rows(&installed.orm).await;
    addons.file(
        "note",
        "release.txt",
        "candidate without a committed upgrade",
    );
    let candidate = builder(&db, &addons).build().unwrap();
    validation(
        candidate
            .identity_activate(
                &previous,
                "operator@example.test",
                "candidate migration has not run",
            )
            .await
            .unwrap_err(),
        "Addon 'note' has no committed forward migration for the candidate artifact",
    );
    assert_eq!(receipt_rows(&installed.orm).await, receipts);
    assert_eq!(audit_rows(&installed.orm).await, audit);
    assert_eq!(state(&installed.orm, "note").await, "installed");
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn failed_forward_migration_proof_write_rolls_back_upgrade_and_refuses_activation() {
    let db = TestDb::create().await.unwrap();
    let addons = Addons::new();
    addons.add("note", "name='Note'\nversion='1.0'\ndepends=['base']\n");
    let system = builder(&db, &addons).build().unwrap();
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    let installed = system
        .button_immediate_install(&su(&init.orm), &[n("note")])
        .await
        .unwrap();
    let previous = receipt_hashes(&installed.orm).await;
    let receipts = receipt_rows(&installed.orm).await;
    let catalog = rows(
        &installed.orm,
        "SELECT name,state,latest_version FROM ir_module_module ORDER BY name",
    )
    .await;
    let audit = audit_rows(&installed.orm).await;
    exec(&installed.orm, "ALTER TABLE installed_addon_identity_audit ADD CONSTRAINT fixture_reject_forward_migration CHECK(operation <> 'forward-migrate')").await;
    addons.add("note", "name='Note'\nversion='2.0'\ndepends=['base']\n");
    let candidate = builder(&db, &addons)
        .identity_previous(previous.clone())
        .build()
        .unwrap();
    let loaded = candidate.load().await.unwrap();
    let error = candidate
        .button_immediate_upgrade(&su(&loaded.orm), &[n("note")])
        .await
        .unwrap_err();
    match error {
        Error::Orm(source) => {
            assert_eq!(source.kind(), ErrorKind::Validation);
            assert_eq!(source.sqlstate(), Some("23514"));
            assert_eq!(
                source.message(),
                "The database refused the change: new row for relation \"installed_addon_identity_audit\" violates check constraint \"fixture_reject_forward_migration\""
            );
        }
        other => panic!("expected forward migration audit constraint failure, got {other:?}"),
    }
    assert_eq!(receipt_rows(&installed.orm).await, receipts);
    assert_eq!(audit_rows(&installed.orm).await, audit);
    assert_eq!(
        rows(
            &installed.orm,
            "SELECT name,state,latest_version FROM ir_module_module ORDER BY name"
        )
        .await,
        catalog
    );
    validation(
        candidate
            .identity_activate(
                &previous,
                "operator@example.test",
                "failed candidate migration is not activation authority",
            )
            .await
            .unwrap_err(),
        "Addon 'note' has no committed forward migration for the candidate artifact",
    );
    assert_eq!(receipt_rows(&installed.orm).await, receipts);
    assert_eq!(audit_rows(&installed.orm).await, audit);
    db.cleanup().await.unwrap();
}

async fn assert_bad_snapshot_model_layout_refuses_install(too_many_files: bool) {
    let db = TestDb::create().await.unwrap();
    let addons = Addons::new();
    addons.add("note", "name='Note'\ndepends=['base']\n");
    let system = builder(&db, &addons).build().unwrap();
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    let receipts = receipt_rows(&init.orm).await;
    let catalog = rows(
        &init.orm,
        "SELECT row_to_json(m)::text FROM ir_module_module m ORDER BY name",
    )
    .await;
    let tables = rows(&init.orm, "SELECT tablename::text FROM pg_catalog.pg_tables WHERE schemaname=current_schema() ORDER BY tablename").await;
    let models_path = addons
        .path()
        .join("note")
        .canonicalize()
        .unwrap()
        .join("models");
    let message = if too_many_files {
        for index in 0..=erp_modules::MAX_MODEL_FILES {
            addons.file("note", &format!("models/model_{index:04}.toml"), "");
        }
        format!("more than {} model files", erp_modules::MAX_MODEL_FILES)
    } else {
        addons.file("note", "models", "[[model]]\nname='test.note'\n");
        "not a directory".to_owned()
    };
    let error = system
        .button_immediate_install(&su(&init.orm), &[n("note")])
        .await
        .unwrap_err();
    match error {
        Error::ModelFile {
            path,
            line,
            message: actual,
            source,
            ..
        } => {
            assert_eq!(path, models_path);
            assert_eq!(line, None);
            assert_eq!(actual, message);
            assert!(source.is_none(), "layout admission is not a registry error");
        }
        other => panic!("expected model-directory admission error, got {other:?}"),
    }
    assert_eq!(state(&init.orm, "note").await, "uninstalled");
    assert_eq!(receipt_rows(&init.orm).await, receipts);
    assert_eq!(
        rows(
            &init.orm,
            "SELECT row_to_json(m)::text FROM ir_module_module m ORDER BY name"
        )
        .await,
        catalog
    );
    assert_eq!(rows(&init.orm, "SELECT tablename::text FROM pg_catalog.pg_tables WHERE schemaname=current_schema() ORDER BY tablename").await, tables);
    assert!(audit_rows(&init.orm).await.is_empty());
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn admitted_snapshot_models_regular_file_refuses_install_without_effects() {
    assert_bad_snapshot_model_layout_refuses_install(false).await;
}

#[tokio::test]
async fn admitted_snapshot_too_many_model_files_refuses_install_without_effects() {
    assert_bad_snapshot_model_layout_refuses_install(true).await;
}

#[tokio::test]
async fn admitted_snapshot_migrations_regular_file_refuses_upgrade_without_effects() {
    let db = TestDb::create().await.unwrap();
    let addons = Addons::new();
    addons.add("note", "name='Note'\nversion='1.0'\ndepends=['base']\n");
    let system = builder(&db, &addons).build().unwrap();
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    let installed = system
        .button_immediate_install(&su(&init.orm), &[n("note")])
        .await
        .unwrap();
    let previous = receipt_hashes(&installed.orm).await;
    let receipts = receipt_rows(&installed.orm).await;
    let audit = audit_rows(&installed.orm).await;
    let catalog = rows(
        &installed.orm,
        "SELECT name,state,latest_version FROM ir_module_module ORDER BY name",
    )
    .await;
    addons.add("note", "name='Note'\nversion='2.0'\ndepends=['base']\n");
    addons.file("note", "migrations", "not a directory");
    let candidate = builder(&db, &addons)
        .identity_previous(previous)
        .build()
        .unwrap();
    let loaded = candidate.load().await.unwrap();
    let error = candidate
        .button_immediate_upgrade(&su(&loaded.orm), &[n("note")])
        .await
        .unwrap_err();
    match error {
        Error::Io { path, source } => {
            assert_eq!(
                path,
                addons
                    .path()
                    .join("note")
                    .canonicalize()
                    .unwrap()
                    .join("migrations")
            );
            assert_eq!(source.kind(), std::io::ErrorKind::InvalidInput);
            assert_eq!(source.to_string(), "not a directory");
        }
        other => panic!("expected migrations directory admission error, got {other:?}"),
    }
    assert_eq!(receipt_rows(&installed.orm).await, receipts);
    assert_eq!(audit_rows(&installed.orm).await, audit);
    assert_eq!(
        rows(
            &installed.orm,
            "SELECT name,state,latest_version FROM ir_module_module ORDER BY name"
        )
        .await,
        catalog
    );
    db.cleanup().await.unwrap();
}

async fn receipt_restamp_preserves_survivor_reference(activation: bool) {
    let db = TestDb::create().await.unwrap();
    let addons = Addons::new();
    addons.add("note", "name='Note'\nversion='1.0'\ndepends=['base']\n");
    let system = builder(&db, &addons).build().unwrap();
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    let installed = system
        .button_immediate_install(&su(&init.orm), &[n("note")])
        .await
        .unwrap();
    let previous = receipt_hashes(&installed.orm).await;
    exec(&installed.orm, "CREATE TABLE fixture_identity_survivor(receipt_name text PRIMARY KEY REFERENCES installed_addon_identity(name) ON DELETE CASCADE); INSERT INTO fixture_identity_survivor VALUES ('note')").await;
    let survivor = rows(
        &installed.orm,
        "SELECT receipt_name FROM fixture_identity_survivor",
    )
    .await;
    addons.add("note", "name='Note'\nversion='2.0'\ndepends=['base']\n");
    let candidate = builder(&db, &addons)
        .identity_previous(previous.clone())
        .build()
        .unwrap();
    if activation {
        let loaded = candidate.load().await.unwrap();
        let upgraded = candidate
            .button_immediate_upgrade(&su(&loaded.orm), &[n("note")])
            .await
            .unwrap();
        assert_eq!(upgraded.upgraded, vec![n("note")]);
        assert!(upgraded.post_commit.is_clean());
        candidate
            .identity_activate(
                &previous,
                "fixture-operator",
                "Successful fixture forward activation",
            )
            .await
            .unwrap();
    } else {
        candidate
            .identity_enroll(
                &[n("note")],
                "fixture-operator",
                "Reviewed fixture source replacement",
            )
            .await
            .unwrap();
    }
    assert_eq!(
        rows(
            &installed.orm,
            "SELECT receipt_name FROM fixture_identity_survivor"
        )
        .await,
        survivor
    );
    let current = receipt_hashes(&installed.orm).await;
    assert_ne!(current.get(&n("note")), previous.get(&n("note")));
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn enrollment_restamp_does_not_delete_receipt_references() {
    receipt_restamp_preserves_survivor_reference(false).await;
}

#[tokio::test]
async fn activation_restamp_does_not_delete_receipt_references() {
    receipt_restamp_preserves_survivor_reference(true).await;
}

#[tokio::test]
async fn tooling_cache_data_is_refused_without_lifecycle_effects() {
    let db = TestDb::create().await.unwrap();
    let addons = Addons::new();
    addons.add(
        "note",
        "name='Note'\ndepends=['base']\ndata=['static/node_modules/data.xml']\n",
    );
    addons.file("note", "static/node_modules/data.xml", "<odoo/>");
    let system = builder(&db, &addons).build().unwrap();
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    let receipts = receipt_rows(&init.orm).await;
    let audit = audit_rows(&init.orm).await;
    let error = system
        .button_immediate_install(&su(&init.orm), &[n("note")])
        .await
        .unwrap_err();
    let (module, step, file, source) = step_failed(&error);
    assert_eq!(module, "note");
    assert_eq!(step, erp_modules::error::ModuleStep::DataFile);
    assert_eq!(file, Some("static/node_modules/data.xml"));
    match source {
        Error::Orm(error) => {
            assert_eq!(error.kind(), ErrorKind::Validation);
            assert_eq!(
                error.message(),
                "Addon 'note' data file 'static/node_modules/data.xml' references excluded packaging entry 'node_modules'"
            );
        }
        other => panic!("expected excluded tooling-cache refusal, got {other:?}"),
    }
    assert_eq!(state(&init.orm, "note").await, "uninstalled");
    assert_eq!(receipt_rows(&init.orm).await, receipts);
    assert_eq!(audit_rows(&init.orm).await, audit);
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn legacy_previous_uses_enrolled_database_receipts() {
    let db = TestDb::create().await.unwrap();
    let addons = Addons::new();
    addons.add("note", "name='Note'\ndepends=['base']\n");
    let old = builder(&db, &addons).build().unwrap();
    let init = old
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    let installed = old
        .button_immediate_install(&su(&init.orm), &[n("note")])
        .await
        .unwrap();
    let before = receipt_hashes(&installed.orm).await;
    addons.file("note", "changed.txt", "candidate changed bytes");
    let candidate = builder(&db, &addons)
        .identity_previous_enrolled()
        .build()
        .unwrap();
    let loaded = candidate.load().await.unwrap();
    let upgraded = candidate
        .button_immediate_upgrade(&su(&loaded.orm), &[n("note")])
        .await
        .unwrap();
    assert_eq!(receipt_hashes(&upgraded.orm).await, before);
    candidate
        .identity_activate_enrolled("operator", "forward legacy activation")
        .await
        .unwrap();
    assert_ne!(
        receipt_hashes(&upgraded.orm).await[&n("note")],
        before[&n("note")]
    );
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn legacy_bootstrap_enrolls_complete_set_and_refuses_restamping() {
    let db = TestDb::create().await.unwrap();
    let addons = Addons::new();
    addons.add("note", "name='Note'\ndepends=['base']\n");
    let old = builder(&db, &addons).build().unwrap();
    let init = old
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    let installed = old
        .button_immediate_install(&su(&init.orm), &[n("note")])
        .await
        .unwrap();
    let before = receipt_hashes(&installed.orm).await;
    exec(&installed.orm, "DELETE FROM installed_addon_identity").await;
    old.identity_enroll_legacy_installed(&before, "operator", "reviewed old files")
        .await
        .unwrap();
    assert_eq!(receipt_hashes(&installed.orm).await, before);
    old.identity_enroll_legacy_installed(&before, "operator", "retry same old files")
        .await
        .unwrap();
    addons.file("note", "changed.txt", "candidate changed bytes");
    let changed = builder(&db, &addons).build().unwrap();
    let mut hashes = before.clone();
    hashes.insert(
        n("note"),
        erp_modules::artifact_identity::ArtifactSnapshot::capture(
            "note",
            &addons.path().join("note"),
        )
        .unwrap()
        .digest()
        .to_owned(),
    );
    validation(
        changed
            .identity_enroll_legacy_installed(&hashes, "operator", "cannot restamp")
            .await
            .unwrap_err(),
        "Legacy bootstrap refuses to replace enrolled identity for addon 'note'",
    );
    assert_eq!(receipt_hashes(&installed.orm).await, before);
    assert_eq!(audit_rows(&installed.orm).await.len(), 2);
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn legacy_previous_refuses_missing_enrollment() {
    let db = TestDb::create().await.unwrap();
    let addons = Addons::new();
    let old = builder(&db, &addons).build().unwrap();
    let init = old
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    exec(
        &init.orm,
        "DELETE FROM installed_addon_identity WHERE name='base'",
    )
    .await;
    let candidate = builder(&db, &addons)
        .identity_previous_enrolled()
        .build()
        .unwrap();
    let loaded = candidate.load().await.unwrap();
    validation(
        candidate
            .button_immediate_upgrade(&su(&loaded.orm), &[n("base")])
            .await
            .unwrap_err(),
        "Addon 'base' identity unknown, run erp module identity-enroll first",
    );
    assert!(receipt_hashes(&loaded.orm).await.is_empty());
    assert!(audit_rows(&loaded.orm).await.is_empty());
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn legacy_activation_refuses_unmigrated_candidate() {
    let db = TestDb::create().await.unwrap();
    let addons = Addons::new();
    addons.add("note", "name='Note'\ndepends=['base']\n");
    let old = builder(&db, &addons).build().unwrap();
    let init = old
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    let installed = old
        .button_immediate_install(&su(&init.orm), &[n("note")])
        .await
        .unwrap();
    let before = receipt_rows(&installed.orm).await;
    addons.file("note", "changed.txt", "unmigrated candidate");
    let candidate = builder(&db, &addons).build().unwrap();
    validation(
        candidate
            .identity_activate_enrolled("operator", "must refuse")
            .await
            .unwrap_err(),
        "Addon 'note' has no committed forward migration for the candidate artifact",
    );
    assert_eq!(receipt_rows(&installed.orm).await, before);
    assert!(audit_rows(&installed.orm).await.is_empty());
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn legacy_identity_ownership_is_database_serialized() {
    let db = TestDb::create().await.unwrap();
    let addons = Addons::new();
    let old = builder(&db, &addons).build().unwrap();
    let init = old
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    let hashes = receipt_hashes(&init.orm).await;
    let other = builder(&db, &addons)
        .identity_previous_enrolled()
        .build()
        .unwrap();
    let mut owner = init.orm.begin(SUPERUSER_ID).await.unwrap();
    owner
        .query(
            "SELECT pg_advisory_xact_lock($1,$2)",
            &[&OPERATION_LOCK.0, &OPERATION_LOCK.1],
        )
        .await
        .unwrap();
    for result in [
        other
            .identity_enroll_legacy_installed(&hashes, "operator", "contending bootstrap")
            .await,
        other
            .identity_activate_enrolled("operator", "contending activation")
            .await,
    ] {
        let error = result.unwrap_err();
        assert!(matches!(error, Error::OperationInProgress));
        assert_eq!(
            error.to_string(),
            "ERPForAll is currently processing another module operation.\nPlease try again later or contact your system administrator."
        );
    }
    assert_eq!(receipt_hashes(&init.orm).await, hashes);
    assert!(audit_rows(&init.orm).await.is_empty());
    owner.rollback().await.unwrap();
    drop(owner);
    other
        .identity_enroll_legacy_installed(&hashes, "operator", "lock released")
        .await
        .unwrap();
    other
        .identity_activate_enrolled("operator", "unchanged activation")
        .await
        .unwrap();
    assert_eq!(receipt_hashes(&init.orm).await, hashes);
    assert_eq!(audit_rows(&init.orm).await.len(), 1);
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn legacy_bootstrap_refuses_existing_source_mismatch() {
    let db = TestDb::create().await.unwrap();
    let addons = Addons::new();
    let system = builder(&db, &addons).build().unwrap();
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    let hashes = receipt_hashes(&init.orm).await;
    exec(
        &init.orm,
        "UPDATE installed_addon_identity SET source_kind='package' WHERE name='base'",
    )
    .await;
    let before = receipt_rows(&init.orm).await;
    validation(
        system
            .identity_enroll_legacy_installed(&hashes, "operator", "must preserve source kind")
            .await
            .unwrap_err(),
        "Addon 'base' installed artifact identity does not match available content",
    );
    assert_eq!(receipt_rows(&init.orm).await, before);
    assert!(audit_rows(&init.orm).await.is_empty());
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn legacy_forward_upgrade_refuses_omitted_changed_addon() {
    omitted_changed_addon_refuses_forward_upgrade(true).await;
}

#[tokio::test]
async fn verified_forward_upgrade_refuses_omitted_changed_addon() {
    omitted_changed_addon_refuses_forward_upgrade(false).await;
}

async fn omitted_changed_addon_refuses_forward_upgrade(legacy: bool) {
    let db = TestDb::create().await.unwrap();
    let addons = Addons::new();
    addons.add("note", "name='Note'\ndepends=['base']\n");
    addons.add("other", "name='Other'\ndepends=['base']\n");
    let old = builder(&db, &addons).build().unwrap();
    let init = old
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    let installed = old
        .button_immediate_install(&su(&init.orm), &[n("note"), n("other")])
        .await
        .unwrap();
    let before_hashes = receipt_hashes(&installed.orm).await;
    let before_receipts = receipt_rows(&installed.orm).await;
    let before_audit = audit_rows(&installed.orm).await;
    addons.file("note", "changed.txt", "candidate changed bytes");
    let candidate_builder = builder(&db, &addons);
    let candidate = if legacy {
        candidate_builder.identity_previous_enrolled()
    } else {
        candidate_builder.identity_previous(before_hashes.clone())
    }
    .build()
    .unwrap();
    let loaded = candidate.load().await.unwrap();
    validation(
        candidate
            .button_immediate_upgrade(&su(&loaded.orm), &[n("other")])
            .await
            .unwrap_err(),
        "Addon 'note' candidate content changed but is not selected for forward upgrade",
    );
    assert_eq!(receipt_rows(&installed.orm).await, before_receipts);
    assert_eq!(audit_rows(&installed.orm).await, before_audit);
    assert_eq!(state(&installed.orm, "note").await, "installed");
    assert_eq!(state(&installed.orm, "other").await, "installed");

    let upgraded = candidate
        .button_immediate_upgrade(&su(&loaded.orm), &[n("note"), n("other")])
        .await
        .unwrap();
    assert_eq!(receipt_hashes(&upgraded.orm).await, before_hashes);
    if legacy {
        candidate
            .identity_activate_enrolled("operator", "all changed addons selected")
            .await
            .unwrap();
    } else {
        candidate
            .identity_activate(&before_hashes, "operator", "all changed addons selected")
            .await
            .unwrap();
    }
    let after = receipt_hashes(&upgraded.orm).await;
    assert_ne!(after[&n("note")], before_hashes[&n("note")]);
    assert_eq!(after[&n("other")], before_hashes[&n("other")]);
    assert_eq!(after[&n("base")], before_hashes[&n("base")]);
    db.cleanup().await.unwrap();
}
