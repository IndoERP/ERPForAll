//! administrative interface over erp_job / erp_job_run.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::collapsible_if,
    reason = "tests"
)]

mod support;

use erp_domain::Domain;
use erp_fields::{Cell, ErrorKind};
use erp_orm::jobs::{EnqueueRequest, HandlerRef, JobKindDecl, When};
use erp_orm::{Orm, RawVals, SUPERUSER_ID, StaticPolicy, UserId};
use erp_test_models::base_users;
use erp_testkit::Harness;
use support::jobs::SCRIPT;

erp_orm::register_job_kind!(
    UI_SCRIPT_KIND,
    JobKindDecl::new(
        "test_jobs",
        "test_jobs.ui_script",
        HandlerRef::Native(&SCRIPT)
    )
    .max_attempts(2)
);

async fn harness(policy: StaticPolicy) -> Harness {
    Harness::builder([&base_users::ADDON, &erp_addon_base::ADDON])
        .policy(policy)
        .build()
        .await
        .expect("harness")
}

fn orm_with_admin_capability(h: &Harness) -> Orm {
    h.orm()
        .rebuild(h.registry().clone())
        .job_admin_capability(&erp_security::jobs::JOBS_ADMIN)
        .build()
        .unwrap()
}

#[tokio::test]
async fn ir_job_model_reflects_active_queue_rows() {
    let h = harness(StaticPolicy::AllowAll).await;
    let mut tx = h.begin().await.unwrap();

    let outcome = tx
        .enqueue(
            &tx.env(),
            EnqueueRequest::new("test_jobs.ui_script")
                .at(When::Now)
                .priority(erp_orm::jobs::Priority::new(3).unwrap()),
        )
        .await
        .unwrap();

    let domain = Domain::from_json(&erp_sdk::serde_json::json!([[
        "kind",
        "=",
        "test_jobs.ui_script"
    ]]))
    .unwrap();
    let records = tx
        .env()
        .records("ir.job")
        .unwrap()
        .search(&mut tx, &domain)
        .await
        .unwrap();

    assert_eq!(records.len(), 1);
    let id = records.rec_ids()[0];
    assert_eq!(id.get(), outcome.job().id());

    let state = records.cells(&mut tx, "state").await.unwrap();
    assert_eq!(state[0], Cell::Str("ready".into()));

    let priority = records.cells(&mut tx, "priority").await.unwrap();
    assert_eq!(priority[0], Cell::Int(3));

    let kind = records.cells(&mut tx, "kind").await.unwrap();
    assert_eq!(kind[0], Cell::Str("test_jobs.ui_script".into()));

    tx.rollback().await.unwrap();
}

#[tokio::test]
async fn ir_job_refuses_direct_crud_mutations() {
    let h = harness(StaticPolicy::AllowAll).await;
    let mut tx = h.begin().await.unwrap();

    let job_model = tx.env().records("ir.job").unwrap();
    let create_err = job_model
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("kind", Cell::Str("test.kind".into()))],
        )
        .await
        .unwrap_err();
    assert_eq!(create_err.kind(), ErrorKind::UserError);
    assert!(create_err.message().contains("read-only"));

    let outcome = tx
        .enqueue(&tx.env(), EnqueueRequest::new("test_jobs.ui_script"))
        .await
        .unwrap();

    let job_rec = tx
        .env()
        .records("ir.job")
        .unwrap()
        .browse([outcome.job().id()])
        .unwrap();

    let write_err = job_rec
        .write(&mut tx, RawVals::new().set_cell("priority", Cell::Int(1)))
        .await
        .unwrap_err();
    assert_eq!(write_err.kind(), ErrorKind::UserError);
    assert!(write_err.message().contains("read-only"));

    let unlink_err = job_rec.unlink(&mut tx).await.unwrap_err();
    assert_eq!(unlink_err.kind(), ErrorKind::UserError);
    assert!(unlink_err.message().contains("read-only"));

    let run_model = tx.env().records("ir.job.run").unwrap();
    let run_create_err = run_model
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("kind", Cell::Str("test.kind".into()))],
        )
        .await
        .unwrap_err();
    assert_eq!(run_create_err.kind(), ErrorKind::UserError);
    assert!(run_create_err.message().contains("read-only"));

    tx.rollback().await.unwrap();
}

#[tokio::test]
async fn ir_job_action_hold_and_resume_transitions() {
    let h = harness(StaticPolicy::AllowAll).await;
    let orm = orm_with_admin_capability(&h);
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();

    let outcome = tx
        .enqueue(&tx.env(), EnqueueRequest::new("test_jobs.ui_script"))
        .await
        .unwrap();

    let job_rec = tx
        .env()
        .records("ir.job")
        .unwrap()
        .browse([outcome.job().id()])
        .unwrap();

    job_rec
        .call::<(), ()>(&mut tx, "action_hold", ())
        .await
        .unwrap();

    let state = job_rec.cells(&mut tx, "state").await.unwrap();
    assert_eq!(state[0], Cell::Str("held".into()));

    let hold_again = job_rec
        .call::<(), ()>(&mut tx, "action_hold", ())
        .await
        .unwrap_err();
    assert_eq!(hold_again.kind(), ErrorKind::UserError);

    job_rec
        .call::<(), ()>(&mut tx, "action_resume", ())
        .await
        .unwrap();

    let state = job_rec.cells(&mut tx, "state").await.unwrap();
    assert_eq!(state[0], Cell::Str("ready".into()));

    let resume_again = job_rec
        .call::<(), ()>(&mut tx, "action_resume", ())
        .await
        .unwrap_err();
    assert_eq!(resume_again.kind(), ErrorKind::UserError);

    tx.rollback().await.unwrap();
}

#[tokio::test]
async fn ir_job_actions_refuse_a_job_that_no_longer_exists() {
    let h = harness(StaticPolicy::AllowAll).await;
    let orm = orm_with_admin_capability(&h);
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let outcome = tx
        .enqueue(&tx.env(), EnqueueRequest::new("test_jobs.ui_script"))
        .await
        .unwrap();
    let gone = 987_654_321_i64;
    let recs = tx
        .env()
        .records("ir.job")
        .unwrap()
        .browse([outcome.job().id(), gone])
        .unwrap();
    for action in [
        "action_hold",
        "action_resume",
        "action_cancel",
        "action_requeue",
        "action_discard",
    ] {
        let error = recs.call::<(), ()>(&mut tx, action, ()).await.unwrap_err();
        assert_eq!(error.kind(), ErrorKind::UserError, "{action}");
        assert!(
            error.message().contains("987654321") && error.message().contains("already finished"),
            "{action}: {}",
            error.message()
        );
    }
    tx.rollback().await.unwrap();
}

#[tokio::test]
async fn ir_job_action_cancel_requeue_and_discard() {
    let h = harness(StaticPolicy::AllowAll).await;
    let orm = orm_with_admin_capability(&h);
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();

    let outcome1 = tx
        .enqueue(&tx.env(), EnqueueRequest::new("test_jobs.ui_script"))
        .await
        .unwrap();

    let job1 = tx
        .env()
        .records("ir.job")
        .unwrap()
        .browse([outcome1.job().id()])
        .unwrap();

    job1.call::<(), ()>(&mut tx, "action_cancel", ())
        .await
        .unwrap();

    let rows = tx
        .query(
            "SELECT state FROM erp_job WHERE id = $1",
            &[&outcome1.job().id()],
        )
        .await
        .unwrap();
    assert!(rows.is_empty(), "cancelled waiting job leaves erp_job");

    let history = tx
        .query(
            "SELECT outcome FROM erp_job_run WHERE job_uuid = $1",
            &[&outcome1.job().uuid()],
        )
        .await
        .unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].get::<_, String>(0), "cancelled");

    let outcome2 = tx
        .enqueue(&tx.env(), EnqueueRequest::new("test_jobs.ui_script"))
        .await
        .unwrap();
    tx.execute(
        "UPDATE erp_job SET state = 'dead', dead_at = clock_timestamp(), attempts = 3 WHERE id = $1",
        &[&outcome2.job().id()],
    )
    .await
    .unwrap();

    let job2 = tx
        .env()
        .records("ir.job")
        .unwrap()
        .browse([outcome2.job().id()])
        .unwrap();

    job2.call::<(), ()>(&mut tx, "action_requeue", ())
        .await
        .unwrap();

    let state = job2.cells(&mut tx, "state").await.unwrap();
    assert_eq!(state[0], Cell::Str("ready".into()));
    let attempts = job2.cells(&mut tx, "attempts").await.unwrap();
    assert_eq!(attempts[0], Cell::Int(0));

    tx.execute(
        "UPDATE erp_job SET state = 'dead', dead_at = clock_timestamp() WHERE id = $1",
        &[&outcome2.job().id()],
    )
    .await
    .unwrap();

    job2.call::<(), ()>(&mut tx, "action_discard", ())
        .await
        .unwrap();

    let rows = tx
        .query(
            "SELECT 1 FROM erp_job WHERE id = $1",
            &[&outcome2.job().id()],
        )
        .await
        .unwrap();
    assert!(rows.is_empty(), "discarded job leaves erp_job");

    let history2 = tx
        .query(
            "SELECT outcome FROM erp_job_run WHERE job_uuid = $1",
            &[&outcome2.job().uuid()],
        )
        .await
        .unwrap();
    assert_eq!(history2.len(), 1);
    assert_eq!(history2[0].get::<_, String>(0), "dead_discarded");

    tx.rollback().await.unwrap();
}

#[tokio::test]
async fn ir_job_actions_require_job_admin_capability() {
    let h = harness(StaticPolicy::SuperuserOnly).await;
    let orm = orm_with_admin_capability(&h);
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();

    let outcome = tx
        .enqueue(&tx.env(), EnqueueRequest::new("test_jobs.ui_script"))
        .await
        .unwrap();

    let normal_user = UserId::new(42).unwrap();
    let user_env = tx.env().with_user(normal_user);
    let user_job = user_env
        .records("ir.job")
        .unwrap()
        .browse([outcome.job().id()])
        .unwrap();

    let err = user_job
        .call::<(), ()>(&mut tx, "action_hold", ())
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access);
    assert!(err.message().contains("jobs.admin"));

    tx.rollback().await.unwrap();
}

#[tokio::test]
async fn ir_job_run_reflects_history_runs() {
    let h = harness(StaticPolicy::AllowAll).await;
    let mut tx = h.begin().await.unwrap();

    let uuid = erp_fields::Uuid::parse("a1b2c3d4-e5f6-4a1b-8c2d-3e4f5a6b7c8d").unwrap();
    tx.execute(
        "INSERT INTO erp_job_run (job_uuid, kind, kind_version, outcome, attempts, enqueued_at, \
         finished_at, items_done, items_failed, run_as_kind, run_as_user, enqueued_by_user) \
         VALUES ($1, 'test_jobs.ui_script', 1, 'done', 1, clock_timestamp(), clock_timestamp(), 10, 0, 'user', 1, 1)",
        &[&uuid],
    )
    .await
    .unwrap();

    let run_model = tx.env().records("ir.job.run").unwrap();
    let domain = Domain::from_json(&erp_sdk::serde_json::json!([[
        "kind",
        "=",
        "test_jobs.ui_script"
    ]]))
    .unwrap();
    let records = run_model.search(&mut tx, &domain).await.unwrap();

    assert_eq!(records.len(), 1);
    let outcome = records.cells(&mut tx, "outcome").await.unwrap();
    assert_eq!(outcome[0], Cell::Str("done".into()));

    let items_done = records.cells(&mut tx, "items_done").await.unwrap();
    assert_eq!(items_done[0], Cell::Int(10));

    tx.rollback().await.unwrap();
}

#[tokio::test]
async fn jobs_admin_technical_views_validate_and_compile() {
    let xml = include_str!("../../../addons/base/data/technical_views.xml");
    let ops = erp_data::parse_xml_str(xml, "technical_views.xml", "base", false).unwrap();

    let mut view_models = Vec::new();
    let mut action_models = Vec::new();
    let mut menu_names = Vec::new();

    for op in &ops {
        match op {
            erp_data::DataOperation::Record(rec) => {
                if rec.model() == "ir.ui.view" {
                    for f in rec.fields() {
                        if f.name() == "model" {
                            let erp_data::FieldValue::Text(s) = f.value() else {
                                continue;
                            };
                            view_models.push(s.clone());
                        }
                    }
                } else if rec.model() == "ir.actions.act_window" {
                    for f in rec.fields() {
                        if f.name() == "res_model" {
                            let erp_data::FieldValue::Text(s) = f.value() else {
                                continue;
                            };
                            action_models.push(s.clone());
                        }
                    }
                }
            }
            erp_data::DataOperation::MenuItem(m) => {
                if let Some(name) = m.name() {
                    menu_names.push(name.to_owned());
                }
            }
            _ => {}
        }
    }

    assert!(view_models.iter().any(|m| m == "ir.job"));
    assert!(view_models.iter().any(|m| m == "ir.job.run"));
    assert!(action_models.iter().any(|m| m == "ir.job"));
    assert!(action_models.iter().any(|m| m == "ir.job.run"));
    assert!(menu_names.iter().any(|n| n == "Automation"));
    assert!(menu_names.iter().any(|n| n == "Background Jobs"));
    assert!(menu_names.iter().any(|n| n == "Job Runs"));
}
