//! Commit-order safety, restart, retention and job delivery through the owner service APIs.
#![allow(clippy::unwrap_used, clippy::panic, reason = "tests")]

use std::time::Duration;

use erp_fields::{Cell, Error};
use erp_orm::jobs::{
    HandlerRef, JobContext, JobError, JobHandler, JobKindDecl, OutboxTrigger, PrincipalPolicy,
    RunOutcome, TriggerCoalesce,
};
use erp_orm::{
    BeginOptions, BoxFuture, OperationIdentity, Orm, OutboxCursor, OutboxEvent, OutboxReplay,
    OutboxRetention, RawVals, SUPERUSER_ID, Transaction,
};
use erp_registry::{AddonDescriptor, FieldDecl, ModelDecl};
use erp_test_models::base_users;
use erp_testkit::Harness;

const MODEL: &str = "test.delivery";
const CONSUMER: &str = "test.receipts";
static ADDON: AddonDescriptor = AddonDescriptor {
    name: "test_delivery",
    depends: &["base"],
    exact_only: false,
};

struct Probe;
impl JobHandler for Probe {
    fn run<'a>(
        &'a self,
        _ctx: &'a mut JobContext<'_>,
    ) -> BoxFuture<'a, Result<RunOutcome, JobError>> {
        Box::pin(async { Ok(RunOutcome::Done) })
    }
}

static PROBE: Probe = Probe;
static TRIGGERS: [OutboxTrigger; 1] = [OutboxTrigger::new(
    MODEL,
    &["create"],
    &[],
    TriggerCoalesce::PerKind,
)];
erp_orm::register_job_kind!(
    TRIGGER_KIND,
    JobKindDecl::new("core", "core.test_probe", HandlerRef::Native(&PROBE))
        .principal(PrincipalPolicy::System)
        .triggers(&TRIGGERS)
);

async fn harness() -> Harness {
    let fields = const { &[FieldDecl::char("name")] };
    let harness = Harness::builder([&base_users::ADDON, &ADDON])
        .with_decls([ModelDecl::new("test_delivery", MODEL)
            .eventful(true)
            .fields(fields)])
        .build()
        .await
        .unwrap();
    let mut tx = harness.begin().await.unwrap();
    tx.install_event_schema().await.unwrap();
    tx.batch("CREATE TABLE outbox_receipt (record_id bigint PRIMARY KEY, position bigint GENERATED ALWAYS AS IDENTITY)").await.unwrap();
    tx.commit().await.unwrap();
    harness
}

async fn writer(orm: &Orm) -> Transaction {
    orm.begin_opts(
        BeginOptions::new(SUPERUSER_ID).operation_identity(OperationIdentity::generate().unwrap()),
    )
    .await
    .unwrap()
}

async fn create(tx: &mut Transaction, name: &str) -> i64 {
    tx.env()
        .records(MODEL)
        .unwrap()
        .create(
            tx,
            vec![RawVals::new().set_cell("name", Cell::Str(name.into()))],
        )
        .await
        .unwrap()
        .ids()[0]
}

async fn receipts(harness: &Harness) -> Vec<i64> {
    let mut tx = harness.begin().await.unwrap();
    tx.query(
        "SELECT record_id FROM outbox_receipt ORDER BY position",
        &[],
    )
    .await
    .unwrap()
    .iter()
    .map(|row| row.get(0))
    .collect()
}

fn receive(
    tx: &mut Transaction,
    events: Vec<OutboxEvent>,
) -> BoxFuture<'_, erp_fields::Result<()>> {
    Box::pin(async move {
        for event in events {
            assert_eq!(event.model, MODEL);
            tx.execute(
                "INSERT INTO outbox_receipt SELECT unnest($1::bigint[])",
                &[&event.record_ids],
            )
            .await?;
        }
        Ok(())
    })
}

async fn safe_prefix(harness: &Harness) {
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let mut tx = harness.begin().await.unwrap();
            let rows = tx.query("SELECT COALESCE(max(xid),'0'::xid8) < pg_snapshot_xmin(pg_current_snapshot()) FROM erp_event.ir_event_outbox", &[]).await.unwrap();
            tx.rollback().await.unwrap();
            if rows[0].get(0) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }).await.unwrap();
}

#[tokio::test]
async fn late_commit_is_delivered_once_and_failed_effects_are_retried() {
    let harness = harness().await;
    let orm = harness.orm();
    assert_eq!(
        orm.replay_outbox(CONSUMER, 1, receive).await.unwrap(),
        OutboxReplay::Idle
    );
    let mut late = writer(orm).await;
    let first = create(&mut late, "late commit").await;
    let mut early = writer(orm).await;
    let second = create(&mut early, "early commit").await;
    early.commit().await.unwrap();
    assert_eq!(
        orm.replay_outbox(CONSUMER, 1, receive).await.unwrap(),
        OutboxReplay::Idle
    );
    late.commit().await.unwrap();
    safe_prefix(&harness).await;

    let error = orm
        .replay_outbox(CONSUMER, 1, |tx, events| {
            Box::pin(async move {
                receive(tx, events).await?;
                Err(Error::user("simulated worker interruption"))
            })
        })
        .await
        .unwrap_err();
    assert_eq!(error.message(), "simulated worker interruption");
    assert!(receipts(&harness).await.is_empty());

    let (entered, fenced) = tokio::sync::oneshot::channel();
    let (resume, waiting) = tokio::sync::oneshot::channel();
    let node = orm.clone();
    let worker = tokio::spawn(async move {
        node.replay_outbox(CONSUMER, 1, |tx, events| {
            Box::pin(async move {
                receive(tx, events).await?;
                entered.send(()).unwrap();
                waiting.await.unwrap();
                Ok(())
            })
        })
        .await
        .unwrap()
    });
    fenced.await.unwrap();
    assert_eq!(
        orm.replay_outbox(CONSUMER, 1, receive).await.unwrap(),
        OutboxReplay::Busy
    );
    assert!(receipts(&harness).await.is_empty());
    resume.send(()).unwrap();
    assert!(matches!(
        worker.await.unwrap(),
        OutboxReplay::Delivered { count: 1, .. }
    ));
    assert_eq!(receipts(&harness).await, [first]);
    assert!(matches!(
        orm.replay_outbox(CONSUMER, 1, receive).await.unwrap(),
        OutboxReplay::Delivered { count: 1, .. }
    ));
    assert_eq!(
        orm.replay_outbox(CONSUMER, 1, receive).await.unwrap(),
        OutboxReplay::Idle
    );
    assert_eq!(receipts(&harness).await, [first, second]);
}

#[tokio::test]
async fn retention_respects_hold_slowest_consumer_and_restore_floor() {
    let harness = harness().await;
    let orm = harness.orm();
    assert_eq!(orm.outbox_status(CONSUMER).await.unwrap(), None);
    assert_eq!(
        orm.replay_outbox("test.slow", 1, discard).await.unwrap(),
        OutboxReplay::Idle
    );
    let mut changes = writer(orm).await;
    create(&mut changes, "old").await;
    changes.commit().await.unwrap();
    let mut tx = harness.begin().await.unwrap();
    tx.batch(
        "CREATE TABLE erp_event.ir_event_outbox_p20200101 PARTITION OF erp_event.ir_event_outbox
      FOR VALUES FROM ('2020-01-01 00:00:00+00') TO ('2020-01-02 00:00:00+00');
      UPDATE erp_event.ir_event_outbox SET create_date = '2020-01-01 00:00:00+00'",
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    safe_prefix(&harness).await;
    let OutboxReplay::Delivered { cursor, .. } =
        orm.replay_outbox(CONSUMER, 1, discard).await.unwrap()
    else {
        panic!("expected delivery")
    };
    assert!(matches!(
        orm.retain_outbox(1, 1).await.unwrap(),
        OutboxRetention::Purged { count: 0, .. }
    ));
    orm.hold_outbox_retention(true).await.unwrap();
    let status = orm.outbox_status(CONSUMER).await.unwrap().unwrap();
    assert!(status.held);
    assert_eq!(status.acknowledgement, Some(cursor));
    assert!(status.acknowledgement_age_seconds.is_some());
    assert_eq!(status.next_event_age_seconds, None);
    assert_eq!(status.floor, OutboxCursor::default());
    let slow = orm.outbox_status("test.slow").await.unwrap().unwrap();
    assert!(slow.next_event_age_seconds.is_some_and(|age| age > 86_400));
    assert_eq!(
        orm.retain_outbox(1, 1).await.unwrap(),
        OutboxRetention::Held
    );
    orm.restore_outbox_cursor(CONSUMER, OutboxCursor::default())
        .await
        .unwrap();
    for name in ["test.slow", CONSUMER] {
        assert_eq!(
            orm.replay_outbox(name, 1, discard).await.unwrap(),
            OutboxReplay::Delivered { count: 1, cursor }
        );
    }
    orm.hold_outbox_retention(false).await.unwrap();
    assert_eq!(
        orm.retain_outbox(1, 1).await.unwrap(),
        OutboxRetention::Purged {
            count: 1,
            floor: cursor
        }
    );
    orm.hold_outbox_retention(true).await.unwrap();
    assert!(
        orm.restore_outbox_cursor(CONSUMER, OutboxCursor::default())
            .await
            .is_err()
    );
    assert_eq!(
        orm.replay_outbox("test.new", 1, discard).await.unwrap(),
        OutboxReplay::Idle
    );
}

fn discard(_: &mut Transaction, _: Vec<OutboxEvent>) -> BoxFuture<'_, erp_fields::Result<()>> {
    Box::pin(async { Ok(()) })
}

#[tokio::test]
async fn job_tick_coalesces_committed_events_and_advances_cursor() {
    let harness = harness().await;
    let orm = harness.orm();
    for event in 0..16 {
        let mut tx = writer(orm).await;
        create(&mut tx, &format!("burst {event}")).await;
        tx.commit().await.unwrap();
    }
    safe_prefix(&harness).await;
    assert!(matches!(
        orm.deliver_outbox_jobs(orm).await.unwrap(),
        OutboxReplay::Delivered { count: 16, .. }
    ));
    assert_eq!(
        orm.deliver_outbox_jobs(orm).await.unwrap(),
        OutboxReplay::Idle
    );
    let mut tx = harness.begin().await.unwrap();
    let rows = tx
        .query(
            "SELECT payload FROM erp_job WHERE kind = 'core.test_probe'",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].get::<_, serde_json::Value>(0),
        serde_json::json!({"model": MODEL, "kind": "create", "record_id": null})
    );
    tx.rollback().await.unwrap();
    let mut write = writer(orm).await;
    create(&mut write, "follow-up").await;
    write.commit().await.unwrap();
    safe_prefix(&harness).await;
    assert!(matches!(
        orm.deliver_outbox_jobs(orm).await.unwrap(),
        OutboxReplay::Delivered { count: 1, .. }
    ));
    let mut tx = harness.begin().await.unwrap();
    let rows = tx
        .query(
            "SELECT count(*) FROM erp_job WHERE kind = 'core.test_probe'",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(rows[0].get::<_, i64>(0), 2);
}
