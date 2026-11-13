// SPDX-License-Identifier: MIT OR Apache-2.0
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]
mod attachment_support;
use attachment_support::*;
use erp_addon_base::attachments::{
    ownership::{self, Purpose, Settlement},
    recovery::{self, Reason},
};
use erp_blob::{AttachmentStore, Namespace, Op, OwnerMarker, When};
use erp_orm::{BeginOptions, Isolation, PrincipalHint};
async fn contested(f: &Fixture) -> Namespace {
    f.h.orm().check_audit_store().await.unwrap();
    let mut tx = f
        .h
        .orm()
        .begin_opts(BeginOptions::new(PrincipalHint::system()).isolation(Isolation::ReadCommitted))
        .await
        .unwrap();
    let p = ownership::prepare(&mut tx, "main", Purpose::Observe)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let a = p.execute().await;
    let mut tx = f
        .h
        .orm()
        .begin_opts(BeginOptions::new(PrincipalHint::system()).isolation(Isolation::ReadCommitted))
        .await
        .unwrap();
    assert!(matches!(
        ownership::settle(&mut tx, &p, a).await.unwrap(),
        Settlement::Confirmed { .. }
    ));
    tx.commit().await.unwrap();
    f.store
        .create_contested(p.namespace(), p.marker())
        .await
        .unwrap();
    p.namespace().clone()
}
#[tokio::test]
async fn forced_recovery_is_audited_and_does_not_reuse_observation() {
    let f = fixture(external_limits()).await;
    let ns = contested(&f).await;
    let db = f.h.db().client().await.unwrap();
    db.execute("UPDATE ir_blob_owner_state SET observation_started_at=clock_timestamp()-interval '700 seconds',observation_not_before=clock_timestamp()-interval '1 second'",&[]).await.unwrap();
    assert_eq!(scalar(&f.h,"SELECT count(*) FROM ir_blob_owner_state WHERE observation_id IS NOT NULL AND observation_not_before<clock_timestamp()").await,1);
    let inspected = recovery::inspect(f.h.orm(), "main").await.unwrap();
    let claim = recovery::begin(f.h.orm(), &inspected, Reason::RestorePrimary)
        .await
        .unwrap();
    assert_eq!(
        ownership::health(f.h.orm(), "main").await.unwrap(),
        ownership::Health::Unavailable
    );
    let mut cleanup = f
        .h
        .orm()
        .begin_opts(BeginOptions::new(PrincipalHint::system()).isolation(Isolation::ReadCommitted))
        .await
        .unwrap();
    let e = erp_addon_base::attachments::gc::check_cleanup(&mut cleanup, "main")
        .await
        .unwrap_err();
    assert_eq!(e.kind(), erp_sdk::ErrorKind::Internal);
    assert_eq!(
        e.message(),
        "The attachment store failed: Attachment cleanup refused: observation is incomplete."
    );
    cleanup.rollback().await.unwrap();
    drop(cleanup);
    claim.run(f.h.orm()).await.unwrap();
    assert_eq!(
        scalar(
            &f.h,
            "SELECT count(*) FROM ir_blob_owner_claim WHERE previous_coordinator->>'mode'='healthy'"
        )
        .await,
        1
    );
    assert_eq!(f.store.contested(&ns).await.unwrap(), None);
    assert!(
        f.store
            .contest_state(&ns)
            .await
            .unwrap()
            .unwrap()
            .resolved()
    );
    assert_eq!(
        ownership::health(f.h.orm(), "main").await.unwrap(),
        ownership::Health::Healthy
    );
    assert_eq!(
        ownership::observation(f.h.orm(), "main").await.unwrap(),
        ownership::Observation::Unavailable
    );
    assert_eq!(
        scalar(&f.h, "SELECT count(*) FROM ir_blob_owner_claim").await,
        1
    );
    assert_eq!(
        scalar(
            &f.h,
            "SELECT count(*) FROM ir_blob_owner_recovery WHERE phase='completed'"
        )
        .await,
        1
    );
    assert_eq!(scalar(&f.h,"SELECT count(*) FROM erp_event.ir_audit_entry WHERE origin='cli' AND method LIKE 'attachments.claim.%'").await,4);
    claim.run(f.h.orm()).await.unwrap();
    assert_eq!(scalar(&f.h,"SELECT count(*) FROM erp_event.ir_audit_entry WHERE origin='cli' AND method LIKE 'attachments.claim.%'").await,4);
    f.h.cleanup().await.unwrap();
}
#[tokio::test]
async fn force_claim_resumes_lost_external_replies() {
    let f = fixture(external_limits()).await;
    let ns = contested(&f).await;
    let claim = recovery::begin(
        f.h.orm(),
        &recovery::inspect(f.h.orm(), "main").await.unwrap(),
        Reason::RetireClone,
    )
    .await
    .unwrap();
    f.store.faults().inject(Op::PutOwner, When::After, 2, 1); // Skip the two successful private-probe publications.
    let e = claim.run(f.h.orm()).await.unwrap_err();
    assert_eq!(e.kind(), erp_sdk::ErrorKind::Internal);
    assert_eq!(
        e.message(),
        format!(
            "The attachment store failed: the attachment store is unavailable (retryable): injected fault in PutOwner (After its effect); attachment recovery {} is incomplete; inspect/resume this operation and preserve markers.",
            claim.id()
        )
    );
    let chosen = f.store.owner(&ns).await.unwrap().unwrap();
    assert!(f.store.contested(&ns).await.unwrap().is_some());
    f.store
        .faults()
        .inject(Op::ResolveContested, When::After, 1, 1); // Skip the private probe resolution on exact-owner retry.
    let resumed = recovery::resume(f.h.orm(), "main", claim.id())
        .await
        .unwrap();
    let e = resumed.run(f.h.orm()).await.unwrap_err();
    assert_eq!(e.kind(), erp_sdk::ErrorKind::Internal);
    assert_eq!(
        e.message(),
        format!(
            "The attachment store failed: the attachment store is unavailable (retryable): injected fault in ResolveContested (After its effect); attachment recovery {} is incomplete; inspect/resume this operation and preserve markers.",
            claim.id()
        )
    );
    assert_eq!(
        ownership::health(f.h.orm(), "main").await.unwrap(),
        ownership::Health::Unavailable
    );
    recovery::resume(f.h.orm(), "main", claim.id())
        .await
        .unwrap()
        .run(f.h.orm())
        .await
        .unwrap();
    assert_eq!(f.store.owner(&ns).await.unwrap().unwrap(), chosen);
    f.h.cleanup().await.unwrap();
}
#[tokio::test]
async fn force_claim_refuses_a_changed_owner_snapshot() {
    let f = fixture(external_limits()).await;
    let ns = contested(&f).await;
    let inspected = recovery::inspect(f.h.orm(), "main").await.unwrap();
    let old = f.store.owner(&ns).await.unwrap().unwrap();
    let newer = OwnerMarker::new(
        old.marker().identity().clone(),
        3,
        Namespace::random().unwrap(),
        100,
    )
    .unwrap();
    f.store
        .put_owner(&ns, &newer, Some(old.version()))
        .await
        .unwrap();
    let e = recovery::begin(f.h.orm(), &inspected, Reason::RestorePrimary)
        .await
        .unwrap_err();
    assert_eq!(e.kind(), erp_sdk::ErrorKind::Internal);
    assert_eq!(
        e.message(),
        "Attachment ownership snapshot changed; inspect and confirm again."
    );
    assert_eq!(
        scalar(&f.h, "SELECT count(*) FROM ir_blob_owner_claim").await,
        0
    );
    f.h.cleanup().await.unwrap();
}

#[tokio::test]
async fn recovery_runtime_grants_are_narrow_and_effective_escalation_is_detected() {
    use erp_testkit::tokio_postgres::{NoTls, error::SqlState};
    let f = fixture(external_limits()).await;
    let config = f.h.read_only_role().await.unwrap();
    let role = config.get_user().unwrap();
    let owner = f.h.db().client().await.unwrap();
    let source = include_str!("../../erp-cli/src/human.rs");
    let term = source
        .split("/*attachment_recovery*/")
        .nth(1)
        .unwrap()
        .split("/*end attachment_recovery*/")
        .next()
        .unwrap();
    let grants = source
        .split("/*attachment_recovery_grants*/")
        .nth(1)
        .unwrap()
        .split('"')
        .nth(1)
        .unwrap()
        .replace("{role_sql}", &erp_sql::quote(role));
    owner
        .batch_execute(&format!(
            "GRANT SELECT,INSERT,UPDATE,DELETE ON ir_blob_owner_claim,ir_blob_owner_recovery TO {}",
            erp_sql::quote(role)
        ))
        .await
        .unwrap();
    let (client, connection) = config.connect(NoTls).await.unwrap();
    tokio::spawn(connection);
    assert!(
        client
            .query_one(&format!("SELECT {term}"), &[])
            .await
            .unwrap()
            .get::<_, bool>(0)
    );
    owner.batch_execute(&grants).await.unwrap();
    assert!(
        !client
            .query_one(&format!("SELECT {term}"), &[])
            .await
            .unwrap()
            .get::<_, bool>(0)
    );
    let e = client
        .query("SELECT * FROM ir_blob_owner_claim", &[])
        .await
        .unwrap_err();
    assert_eq!(e.code(), Some(&SqlState::INSUFFICIENT_PRIVILEGE));
    assert_eq!(
        e.as_db_error().unwrap().message(),
        "permission denied for table ir_blob_owner_claim"
    );
    client
        .query("SELECT phase FROM ir_blob_owner_recovery", &[])
        .await
        .unwrap();
    owner
        .batch_execute("GRANT UPDATE(phase) ON ir_blob_owner_recovery TO PUBLIC")
        .await
        .unwrap();
    assert!(
        client
            .query_one(&format!("SELECT {term}"), &[])
            .await
            .unwrap()
            .get::<_, bool>(0)
    );
    owner
        .batch_execute("REVOKE UPDATE(phase) ON ir_blob_owner_recovery FROM PUBLIC")
        .await
        .unwrap();
    let e = owner
        .batch_execute("DELETE FROM ir_blob_owner_claim")
        .await
        .unwrap_err();
    assert_eq!(e.code(), Some(&SqlState::INSUFFICIENT_PRIVILEGE));
    assert_eq!(
        e.as_db_error().unwrap().message(),
        "Attachment ownership decisions are immutable"
    );
    f.h.cleanup().await.unwrap();
}

#[tokio::test]
async fn audit_failure_retains_effects_and_resume_never_invents_a_new_operation() {
    use erp_sdk::ErrorKind;
    for phase in ["decision", "owner", "resolution", "completed"] {
        let f = fixture(external_limits()).await;
        let ns = contested(&f).await;
        let before = f.store.owner(&ns).await.unwrap().unwrap();
        let db = f.h.db().client().await.unwrap();
        db.batch_execute(&format!("CREATE FUNCTION public.fail_recovery_audit() RETURNS trigger LANGUAGE plpgsql AS $fn$ BEGIN IF NEW.origin='cli' AND NEW.method='attachments.claim.{phase}' THEN RAISE EXCEPTION 'injected audit refusal' USING ERRCODE='P0001'; END IF; RETURN NEW; END $fn$; CREATE TRIGGER fail_recovery_audit BEFORE INSERT ON erp_event.ir_audit_entry FOR EACH ROW EXECUTE FUNCTION public.fail_recovery_audit()" )).await.unwrap();
        let i = recovery::inspect(f.h.orm(), "main").await.unwrap();
        if phase == "decision" {
            let e = recovery::begin(f.h.orm(), &i, Reason::RestorePrimary)
                .await
                .unwrap_err();
            assert_eq!(e.kind(), ErrorKind::Internal);
            let id = e
                .message()
                .split("decision ")
                .nth(1)
                .unwrap()
                .split(' ')
                .next()
                .unwrap();
            Namespace::parse(id).unwrap();
            assert_eq!(
                e.message(),
                format!(
                    "a later release (insertion): the audit INSERT failed (SQLSTATE P0001); attachment recovery decision {id} commit is unconfirmed; inspect this operation before retrying."
                )
            );
            assert_eq!(
                scalar(&f.h, "SELECT count(*) FROM ir_blob_owner_claim").await,
                0
            );
            assert_eq!(f.store.owner(&ns).await.unwrap().unwrap(), before);
        } else {
            let claim = recovery::begin(f.h.orm(), &i, Reason::RestorePrimary)
                .await
                .unwrap();
            let e = claim.run(f.h.orm()).await.unwrap_err();
            assert_eq!(e.kind(), ErrorKind::Internal);
            assert_eq!(
                e.message(),
                format!(
                    "a later release (insertion): the audit INSERT failed (SQLSTATE P0001); attachment recovery {} is incomplete; inspect/resume this operation and preserve markers.",
                    claim.id()
                )
            );
            assert_eq!(
                ownership::health(f.h.orm(), "main").await.unwrap(),
                ownership::Health::Unavailable
            );
            assert_eq!(
                scalar(&f.h, "SELECT count(*) FROM ir_blob_owner_claim").await,
                1
            );
            let chosen = f.store.owner(&ns).await.unwrap().unwrap();
            assert_ne!(chosen, before);
            db.batch_execute("DROP TRIGGER fail_recovery_audit ON erp_event.ir_audit_entry")
                .await
                .unwrap();
            recovery::resume(f.h.orm(), "main", claim.id())
                .await
                .unwrap()
                .run(f.h.orm())
                .await
                .unwrap();
            assert_eq!(f.store.owner(&ns).await.unwrap().unwrap(), chosen);
            assert_eq!(scalar(&f.h,"SELECT count(*) FROM erp_event.ir_audit_entry WHERE origin='cli' AND method LIKE 'attachments.claim.%'").await,4);
        }
        f.h.cleanup().await.unwrap();
    }
}

#[tokio::test]
async fn active_recovery_serializes_operator_and_heartbeat_work() {
    let f = fixture(external_limits()).await;
    contested(&f).await;
    let i = recovery::inspect(f.h.orm(), "main").await.unwrap();
    let c = recovery::begin(f.h.orm(), &i, Reason::RetireClone)
        .await
        .unwrap();
    let e = recovery::begin(f.h.orm(), &i, Reason::RetireClone)
        .await
        .unwrap_err();
    assert_eq!(e.kind(), erp_sdk::ErrorKind::Internal);
    assert_eq!(
        e.message(),
        "Attachment ownership recovery is active; resume its audited operation before maintenance."
    );
    let mut tx = f
        .h
        .orm()
        .begin_opts(BeginOptions::new(PrincipalHint::system()).isolation(Isolation::ReadCommitted))
        .await
        .unwrap();
    let e = ownership::prepare(&mut tx, "main", Purpose::Heartbeat)
        .await
        .unwrap_err();
    assert_eq!(e.kind(), erp_sdk::ErrorKind::Internal);
    assert_eq!(
        e.message(),
        "Attachment ownership recovery is active; resume its audited operation before maintenance."
    );
    tx.rollback().await.unwrap();
    drop(tx);
    c.run(f.h.orm()).await.unwrap();
    f.h.cleanup().await.unwrap();
}

#[tokio::test]
async fn recovery_refuses_generation_overflow_and_identity_change() {
    let f = fixture(external_limits()).await;
    let ns = contested(&f).await;
    let old = f.store.owner(&ns).await.unwrap().unwrap();
    let max = OwnerMarker::new(
        old.marker().identity().clone(),
        u64::MAX,
        Namespace::random().unwrap(),
        100,
    )
    .unwrap();
    f.store
        .put_owner(&ns, &max, Some(old.version()))
        .await
        .unwrap();
    let i = recovery::inspect(f.h.orm(), "main").await.unwrap();
    let e = recovery::begin(f.h.orm(), &i, Reason::RestorePrimary)
        .await
        .unwrap_err();
    assert_eq!(e.kind(), erp_sdk::ErrorKind::Internal);
    assert_eq!(
        e.message(),
        "Attachment owner generation exhausted; recovery cannot advance it."
    );
    let db = f.h.db().client().await.unwrap();
    db.execute("UPDATE ir_blob_owner_identity SET database_oid=1", &[])
        .await
        .unwrap();
    let e = recovery::inspect(f.h.orm(), "main").await.unwrap_err();
    assert_eq!(e.kind(), erp_sdk::ErrorKind::Internal);
    assert_eq!(
        e.message(),
        "Attachment ownership database identity changed; neutralize or rebase the clone before maintenance."
    );
    assert_eq!(
        scalar(&f.h, "SELECT count(*) FROM ir_blob_owner_claim").await,
        0
    );
    f.h.cleanup().await.unwrap();
}

#[tokio::test]
async fn recovery_on_selected_database_fences_the_previous_database() {
    use std::sync::Arc;
    let f = fixture(external_limits()).await;
    let ns = contested(&f).await;
    let h = erp_testkit::Harness::builder([
        &erp_test_models::base_users::ADDON,
        &erp_addon_base::ADDON,
    ])
    .attach(Arc::clone(&f.rt))
    .build()
    .await
    .unwrap();
    h.orm().check_audit_store().await.unwrap();
    let mut tx = h
        .orm()
        .begin_opts(BeginOptions::new(PrincipalHint::system()).isolation(Isolation::ReadCommitted))
        .await
        .unwrap();
    tx.execute("INSERT INTO ir_blob_store(store_key,namespace,role,fingerprint) VALUES('main',$1::text::uuid,'own',$2)",&[&ns.as_str(),&f.store.fingerprint().unwrap()]).await.unwrap();
    let p = ownership::prepare(&mut tx, "main", Purpose::Heartbeat)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let a = p.execute().await;
    let mut tx = h
        .orm()
        .begin_opts(BeginOptions::new(PrincipalHint::system()).isolation(Isolation::ReadCommitted))
        .await
        .unwrap();
    assert!(matches!(
        ownership::settle(&mut tx, &p, a).await.unwrap(),
        Settlement::Contested
    ));
    tx.commit().await.unwrap();
    let i = recovery::inspect(h.orm(), "main").await.unwrap();
    let c = recovery::begin(h.orm(), &i, Reason::RestorePrimary)
        .await
        .unwrap();
    c.run(h.orm()).await.unwrap();
    assert_eq!(
        ownership::health(h.orm(), "main").await.unwrap(),
        ownership::Health::Healthy
    );
    assert_eq!(
        ownership::health(f.h.orm(), "main").await.unwrap(),
        ownership::Health::Unavailable
    );
    let mut tx = f
        .h
        .orm()
        .begin_opts(BeginOptions::new(PrincipalHint::system()).isolation(Isolation::ReadCommitted))
        .await
        .unwrap();
    let old = ownership::prepare(&mut tx, "main", Purpose::Heartbeat)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let a = old.execute().await;
    let mut tx = f
        .h
        .orm()
        .begin_opts(BeginOptions::new(PrincipalHint::system()).isolation(Isolation::ReadCommitted))
        .await
        .unwrap();
    assert!(matches!(
        ownership::settle(&mut tx, &old, a).await.unwrap(),
        Settlement::ContestPending
    ));
    tx.commit().await.unwrap();
    old.publish_contest().await.unwrap();
    assert_eq!(
        ownership::health(h.orm(), "main").await.unwrap(),
        ownership::Health::Unavailable
    );
    h.cleanup().await.unwrap();
    f.h.cleanup().await.unwrap();
}

#[tokio::test]
async fn system_environment_does_not_replace_database_ownership() {
    let f = fixture(external_limits()).await;
    let ns = contested(&f).await;
    let config = f.h.read_only_role().await.unwrap();
    let orm = erp_orm::Orm::builder(
        f.h.registry().clone(),
        erp_orm::pool::create_pool(config, 1).unwrap(),
    )
    .build()
    .unwrap();
    let mut tx = orm
        .begin_opts(BeginOptions::new(PrincipalHint::system()).isolation(Isolation::ReadCommitted))
        .await
        .unwrap();
    let e = recovery::require_owner(&mut tx).await.unwrap_err();
    assert_eq!(e.kind(), erp_sdk::ErrorKind::Access);
    assert_eq!(
        e.message(),
        "Attachment ownership recovery requires the owner connection and the installed ir_blob_store table in public."
    );
    tx.rollback().await.unwrap();
    drop(tx);
    assert!(f.store.contested(&ns).await.unwrap().is_some());
    assert_eq!(
        scalar(&f.h, "SELECT count(*) FROM ir_blob_owner_claim").await,
        0
    );
    f.h.cleanup().await.unwrap();
}

#[tokio::test]
async fn exact_predecessor_schema_upgrades_without_inventing_ownership() {
    use erp_testkit::tokio_postgres::error::SqlState;
    let db = erp_testkit::TestDb::create().await.unwrap();
    let client = db.client().await.unwrap();
    client
        .batch_execute(include_str!("fixtures/attachment_pre_recovery.sql"))
        .await
        .unwrap();
    let absent = async || {
        client.query_one("SELECT to_regclass('public.ir_blob_owner_claim') IS NULL AND to_regclass('public.ir_blob_owner_recovery') IS NULL",&[]).await.unwrap().get::<_,bool>(0)
    };
    assert!(absent().await);
    client.batch_execute("INSERT INTO ir_blob_store(store_key,fingerprint) VALUES('main','predecessor'); INSERT INTO ir_blob(sha256,size,store_id) VALUES(decode(repeat('ab',32),'hex'),17,1); INSERT INTO ir_blob_upload(store_id,subject_user,expires_at,reserved_bytes) VALUES(1,1,now()+interval '1 day',23); INSERT INTO ir_blob_gc_hold(until,reason) VALUES(now()+interval '1 day','predecessor hold')").await.unwrap();
    let state_sql = "SELECT jsonb_build_object('store',(SELECT jsonb_agg(to_jsonb(t) ORDER BY t.id) FROM ir_blob_store t),'blob',(SELECT jsonb_agg(to_jsonb(t) ORDER BY t.id) FROM ir_blob t),'upload',(SELECT jsonb_agg(to_jsonb(t) ORDER BY t.id) FROM ir_blob_upload t),'hold',(SELECT jsonb_agg(to_jsonb(t) ORDER BY t.id) FROM ir_blob_gc_hold t),'usage',(SELECT jsonb_agg(to_jsonb(t) ORDER BY t.id) FROM ir_blob_usage_base t),'delta',(SELECT jsonb_agg(to_jsonb(t) ORDER BY t.id) FROM ir_blob_usage_delta t))::text";
    let before: String = client.query_one(state_sql, &[]).await.unwrap().get(0);
    client.batch_execute("BEGIN").await.unwrap();
    client
        .batch_execute(erp_addon_base::attachments::schema::BLOB_DDL)
        .await
        .unwrap();
    let e = client.batch_execute("SELECT 1/0").await.unwrap_err();
    assert_eq!(e.code(), Some(&SqlState::DIVISION_BY_ZERO));
    assert_eq!(e.as_db_error().unwrap().message(), "division by zero");
    client.batch_execute("ROLLBACK").await.unwrap();
    assert!(absent().await);
    for _ in 0..2 {
        client
            .batch_execute(erp_addon_base::attachments::schema::BLOB_DDL)
            .await
            .unwrap();
    }
    let after: String = client.query_one(state_sql, &[]).await.unwrap().get(0);
    assert_eq!(after, before);
    assert_eq!(client.query_one("SELECT (SELECT count(*) FROM ir_blob_owner_claim)+(SELECT count(*) FROM ir_blob_owner_recovery)+(SELECT count(*) FROM ir_blob_owner_state)+(SELECT count(*) FROM ir_blob_owner_identity)",&[]).await.unwrap().get::<_,i64>(0),0);
    drop(client);
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn recovery_refuses_changed_coordinator_binding() {
    let f = fixture(external_limits()).await;
    contested(&f).await;
    let db = f.h.db().client().await.unwrap();
    db.execute("UPDATE ir_blob_owner_state SET fingerprint='changed'", &[])
        .await
        .unwrap();
    let e = recovery::inspect(f.h.orm(), "main").await.unwrap_err();
    assert_eq!(e.kind(), erp_sdk::ErrorKind::Internal);
    assert_eq!(
        e.message(),
        "Attachment ownership coordinator binding changed; maintenance admission is required."
    );
    assert_eq!(
        scalar(&f.h, "SELECT count(*) FROM ir_blob_owner_claim").await,
        0
    );
    f.h.cleanup().await.unwrap();
}

// Schema ownership must reserve the existing peer table as well as the new recovery tables.
// Otherwise model-table collision checks and required-registration admission miss peer state.
#[tokio::test]
async fn recovery_registration_preserves_peer_table_ownership() {
    const REQUIRED: &[erp_orm::CoreTableRequirement] = &[
        erp_orm::CoreTableRequirement::new("ir_blob_peer_probe", "base"),
        erp_orm::CoreTableRequirement::new("ir_blob_owner_claim", "base"),
        erp_orm::CoreTableRequirement::new("ir_blob_owner_recovery", "base"),
    ];
    let f = fixture(external_limits()).await;
    let admitted = erp_orm::Orm::builder(f.h.registry().clone(), f.h.db().pool().clone())
        .require_core_tables(REQUIRED)
        .build();
    admitted.expect("peer and recovery tables must retain base core-table registration");
    f.h.cleanup().await.unwrap();
}

/// decision under the coordinator lock; once settled as contested the same inspection is claimed)
#[tokio::test]
async fn force_claim_waits_for_an_in_flight_owner_advancement() {
    let f = fixture(external_limits()).await;
    contested(&f).await;
    let mut tx = f
        .h
        .orm()
        .begin_opts(BeginOptions::new(PrincipalHint::system()).isolation(Isolation::ReadCommitted))
        .await
        .unwrap();
    let p = ownership::prepare(&mut tx, "main", Purpose::Heartbeat)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let i = recovery::inspect(f.h.orm(), "main").await.unwrap();
    let e = recovery::begin(f.h.orm(), &i, Reason::RestorePrimary)
        .await
        .unwrap_err();
    assert_eq!(e.kind(), erp_sdk::ErrorKind::Internal);
    assert_eq!(
        e.message(),
        "An attachment ownership advancement is in flight; let the heartbeat settle it, then inspect and confirm again."
    );
    assert_eq!(
        scalar(&f.h, "SELECT count(*) FROM ir_blob_owner_claim").await,
        0
    );
    let attempt = p.execute().await;
    let mut tx = f
        .h
        .orm()
        .begin_opts(BeginOptions::new(PrincipalHint::system()).isolation(Isolation::ReadCommitted))
        .await
        .unwrap();
    assert!(matches!(
        ownership::settle(&mut tx, &p, attempt).await.unwrap(),
        Settlement::Contested
    ));
    tx.commit().await.unwrap();
    drop(tx);
    let c = recovery::begin(f.h.orm(), &i, Reason::RestorePrimary)
        .await
        .unwrap();
    c.run(f.h.orm()).await.unwrap();
    assert_eq!(
        ownership::health(f.h.orm(), "main").await.unwrap(),
        ownership::Health::Healthy
    );
    f.h.cleanup().await.unwrap();
}

const IN_FLIGHT: &str = "An attachment ownership advancement is in flight; let the heartbeat settle it, then inspect and confirm again.";

/// The job deadline the recovery ORM was built with, never a literal.
fn deadline_secs(f: &Fixture) -> i64 {
    i64::try_from(f.h.orm().job_settings().job_deadline().as_secs()).unwrap()
}

/// Prepare and commit one heartbeat advancement, then re-date its persisted pending marker by
/// `shift` seconds (no sleep). `prepare` leaves the coordinator in mode `recovering` with the
/// marker pending, which is the state a store that keeps failing leaves behind as well
/// (`settle(Failed)` keeps the marker and sets `recovering`).
async fn redated_pending(f: &Fixture, shift: i64) -> OwnerMarker {
    let mut tx = f
        .h
        .orm()
        .begin_opts(BeginOptions::new(PrincipalHint::system()).isolation(Isolation::ReadCommitted))
        .await
        .unwrap();
    let p = ownership::prepare(&mut tx, "main", Purpose::Heartbeat)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let db = f.h.db().client().await.unwrap();
    let row = db
        .query_one("SELECT pending_marker,mode FROM ir_blob_owner_state", &[])
        .await
        .unwrap();
    let bytes: Vec<u8> = row.get(0);
    let mode: String = row.get(1);
    assert_eq!(mode, "recovering");
    let stored = OwnerMarker::decode(&bytes).unwrap();
    assert_eq!(&stored, p.marker());
    let redated = OwnerMarker::new(
        stored.identity().clone(),
        stored.generation(),
        stored.operation().clone(),
        stored.observed_at().checked_add_signed(shift).unwrap(),
    )
    .unwrap();
    db.execute(
        "UPDATE ir_blob_owner_state SET pending_marker=$1 WHERE pending_marker IS NOT NULL",
        &[&redated.encode()],
    )
    .await
    .unwrap();
    redated
}

/// A `running` row of the ownership singleton whose lease ends `lease` from now (negative: expired).
async fn running_owner_job(f: &Fixture, lease: &str) {
    let db = f.h.db().client().await.unwrap();
    db.execute(
        "INSERT INTO erp_job(kind,kind_version,max_attempts,run_as_kind,run_as_user,enqueued_by_kind,enqueued_by_user,singleton_key,state,lease_owner,lease_until,started_at,attempts) VALUES('base.attachment_owner_heartbeat',1,5,'system',1,'system',1,$1,'running',gen_random_uuid(),clock_timestamp()+$2::text::interval,clock_timestamp(),1)",
        &[&ownership::SINGLETON, &lease],
    )
    .await
    .unwrap();
}

async fn assert_in_flight(f: &Fixture) {
    let i = recovery::inspect(f.h.orm(), "main").await.unwrap();
    let e = recovery::begin(f.h.orm(), &i, Reason::RestorePrimary)
        .await
        .unwrap_err();
    assert_eq!(e.kind(), erp_sdk::ErrorKind::Internal);
    assert_eq!(e.message(), IN_FLIGHT);
    assert_eq!(
        scalar(&f.h, "SELECT count(*) FROM ir_blob_owner_claim").await,
        0
    );
}

#[tokio::test]
async fn force_claim_supersedes_a_stale_owner_advancement() {
    let f = fixture(external_limits()).await;
    contested(&f).await;
    let stale = redated_pending(&f, -(deadline_secs(&f) + 60)).await;
    let i = recovery::inspect(f.h.orm(), "main").await.unwrap();
    let c = recovery::begin(f.h.orm(), &i, Reason::RestorePrimary)
        .await
        .unwrap();
    c.run(f.h.orm()).await.unwrap();
    assert_eq!(
        scalar(
            &f.h,
            "SELECT count(*) FROM ir_blob_owner_state WHERE pending_marker IS NULL AND mode='healthy'"
        )
        .await,
        1
    );
    let db = f.h.db().client().await.unwrap();
    let chosen: Vec<u8> = db
        .query_one("SELECT chosen FROM ir_blob_owner_claim", &[])
        .await
        .unwrap()
        .get(0);
    let chosen = OwnerMarker::decode(&chosen).unwrap();
    assert!(chosen.generation() > stale.generation());
    let mut tx = f
        .h
        .orm()
        .begin_opts(BeginOptions::new(PrincipalHint::system()).isolation(Isolation::ReadCommitted))
        .await
        .unwrap();
    let next = ownership::prepare(&mut tx, "main", Purpose::Heartbeat)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    assert!(next.marker().generation() > chosen.generation());
    assert_ne!(next.marker().operation(), stale.operation());
    f.h.cleanup().await.unwrap();
}

#[tokio::test]
async fn force_claim_waits_for_a_live_singleton_lease() {
    let f = fixture(external_limits()).await;
    contested(&f).await;
    redated_pending(&f, -(deadline_secs(&f) + 60)).await;
    running_owner_job(&f, "10 minutes").await;
    assert_in_flight(&f).await;
    f.h.cleanup().await.unwrap();
}

#[tokio::test]
async fn force_claim_proceeds_past_an_expired_singleton_lease() {
    let f = fixture(external_limits()).await;
    contested(&f).await;
    redated_pending(&f, -(deadline_secs(&f) + 60)).await;
    running_owner_job(&f, "-1 second").await;
    let i = recovery::inspect(f.h.orm(), "main").await.unwrap();
    let c = recovery::begin(f.h.orm(), &i, Reason::RestorePrimary)
        .await
        .unwrap();
    c.run(f.h.orm()).await.unwrap();
    assert_eq!(
        ownership::health(f.h.orm(), "main").await.unwrap(),
        ownership::Health::Healthy
    );
    f.h.cleanup().await.unwrap();
}

#[tokio::test]
async fn force_claim_refuses_a_future_dated_advancement() {
    let f = fixture(external_limits()).await;
    contested(&f).await;
    redated_pending(&f, deadline_secs(&f) + 3600).await;
    assert_in_flight(&f).await;
    f.h.cleanup().await.unwrap();
}

const CLAIM_UNREADABLE: &str =
    "Attachment claim audit requires the connection that owns ir_blob_owner_claim.";
const CLAIM_UNKNOWN: &str = "Attachment claim audit names no recorded claim.";

#[tokio::test]
async fn claim_audit_refuses_a_request_role() {
    let f = fixture(external_limits()).await;
    // The fixture role can SELECT every table, including ir_blob_owner_claim: the gate is table
    // ownership, so read access alone must not admit a CLI-origin claim entry.
    let config = f.h.read_only_role().await.unwrap();
    let orm = erp_orm::Orm::builder(
        f.h.registry().clone(),
        erp_orm::pool::create_pool(config, 1).unwrap(),
    )
    .build()
    .unwrap();
    let mut tx = orm
        .begin_opts(BeginOptions::new(PrincipalHint::system()).isolation(Isolation::ReadCommitted))
        .await
        .unwrap();
    let e = tx.audit_attachment_claim("decision", 1).await.unwrap_err();
    assert_eq!(e.kind(), erp_sdk::ErrorKind::Access);
    assert_eq!(e.message(), CLAIM_UNREADABLE);
    // The refusal came from a catalog read, not a permission error: the transaction still runs.
    let rows = tx.query("SELECT 1::int4", &[]).await.unwrap();
    assert_eq!(rows[0].get::<_, i32>(0), 1);
    tx.rollback().await.unwrap();
    drop(tx);
    f.h.cleanup().await.unwrap();
}

#[tokio::test]
async fn claim_audit_refuses_an_unknown_claim_without_relabelling() {
    let f = fixture(external_limits()).await;
    f.h.orm().check_audit_store().await.unwrap();
    let mut tx =
        f.h.orm()
            .begin_opts(
                BeginOptions::new(PrincipalHint::system())
                    .isolation(Isolation::ReadCommitted)
                    .operation_identity(erp_orm::OperationIdentity::generate().unwrap()),
            )
            .await
            .unwrap();
    let e = tx
        .audit_attachment_claim("decision", 999_999)
        .await
        .unwrap_err();
    assert_eq!(e.kind(), erp_sdk::ErrorKind::Programming);
    assert_eq!(e.message(), CLAIM_UNKNOWN);
    tx.audit_call(
        erp_orm::audit::AuditCall::new(
            None,
            "test.origin_probe".into(),
            erp_orm::rpc::Effect::Write,
            erp_orm::audit::TargetIds::none(),
            None,
        )
        .unwrap(),
    )
    .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let db = f.h.db().client().await.unwrap();
    let origins: Vec<String> = db
        .query(
            "SELECT origin::text FROM erp_event.ir_audit_entry WHERE method='test.origin_probe'",
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect();
    assert_eq!(origins, vec!["internal".to_owned()]);
    f.h.cleanup().await.unwrap();
}
