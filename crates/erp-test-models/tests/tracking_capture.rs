//! Set-based capture of tracked-field changes (design;
//! behavior
//! the display names), so every tag is `partial`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod support;

use std::sync::Mutex;

use erp_sdk::{
    BoxFuture, Cell, Command, Date, DateTime, ErrorKind, RawVals, RecId, RecordSet, Result,
    TrackedChange, TrackedRecord, Transaction, Upsert, Vals, Value,
};
use erp_test_models::base_users;
use erp_test_models::test_tracking::{self as tracking, Partner, Stock, Tag, Task};
use erp_testkit::Harness;
use support::one;

async fn harness() -> Harness {
    Harness::builder([&base_users::ADDON, &tracking::ADDON])
        .build()
        .await
        .unwrap()
}

fn ctx(key: &str) -> [(String, Value); 1] {
    [(key.to_owned(), Value::Bool(true))]
}

fn real(id: RecId) -> i64 {
    id.as_real().unwrap().get()
}

/// Create tasks and flush, so the rows exist in the database.
async fn make(tx: &mut Transaction, vals: Vec<Vals<Task>>) -> RecordSet<Task> {
    let created = tx
        .env()
        .model::<Task>()
        .unwrap()
        .create(tx, vals)
        .await
        .unwrap();
    tx.flush().await.unwrap();
    created
}

/// Commit `n` tasks named `t<i>` and return their ids: what the next transaction writes is a
/// record that existed before it.
async fn committed(h: &Harness, n: usize) -> Vec<i64> {
    let mut tx = h.begin().await.unwrap();
    let tasks = make(
        &mut tx,
        (0..n)
            .map(|i| Vals::new().set(Task::NAME, format!("t{i}")))
            .collect(),
    )
    .await;
    let ids = tasks.rec_ids().iter().map(|id| real(*id)).collect();
    tx.commit().await.unwrap();
    ids
}

fn tasks(tx: &Transaction, ids: &[i64]) -> RecordSet<Task> {
    tx.env()
        .model::<Task>()
        .unwrap()
        .browse(ids.iter().copied())
        .unwrap()
}

fn change<'a>(record: &'a TrackedRecord, field: &str) -> &'a TrackedChange {
    record
        .changes()
        .iter()
        .find(|c| c.field() == field)
        .unwrap_or_else(|| panic!("no change on {field}: {record:?}"))
}

fn fields(record: &TrackedRecord) -> Vec<&'static str> {
    record.changes().iter().map(TrackedChange::field).collect()
}

#[tokio::test]
async fn spec_mail_050_fields_get_exposes_tracking_and_sequence() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let described = tx
        .env()
        .records("track.task")
        .unwrap()
        .fields_get(&mut tx, Some(&["state", "name", "note"]), None)
        .await
        .unwrap();
    assert_eq!(described["state"]["tracking"], Value::Bool(true));
    assert_eq!(described["state"]["tracking_sequence"], Value::Int(20));
    assert_eq!(described["name"]["tracking_sequence"], Value::Int(100));
    assert!(!described["note"].contains_key("tracking"));
    assert!(!described["note"].contains_key("tracking_sequence"));
}

/// sequence; the message and the precommit hook that calls it are the mail addon's)
#[tokio::test]
async fn spec_mail_052_one_write_is_one_record_of_ordered_changes() {
    let h = harness().await;
    let ids = committed(&h, 1).await;
    let mut tx = h.begin().await.unwrap();
    let partner = tx
        .env()
        .model::<Partner>()
        .unwrap()
        .create(&mut tx, vec![Vals::new().set(Partner::NAME, "P")])
        .await
        .unwrap();
    let pid = real(partner.rec_ids()[0]);
    let due = Date::from_ymd(2026, 11, 5).unwrap();
    let seen = DateTime::from_parts(due, 9, 30, 0, 0).unwrap();
    let task = tasks(&tx, &ids);
    task.write(
        &mut tx,
        Vals::new()
            .set(Task::SEEN, seen)
            .set(Task::QTY, 2000)
            .set(Task::NAME, "renamed")
            .set(Task::STATE, "done")
            .set(Task::PARTNER_ID, partner.rec_ids()[0])
            .set(Task::FLAGGED, true)
            .set(Task::DUE, due)
            .set(Task::WEIGHT, 1.5)
            .set(Task::NOTE, "not tracked"),
    )
    .await
    .unwrap();
    let out = tx.take_tracked_changes().await.unwrap();
    assert_eq!(out.len(), 1);
    let record = &out[0];
    assert_eq!(record.model(), "track.task");
    assert_eq!(real(record.id()), ids[0]);
    // by sequence (state 20, qty 30, weight 35, partner 40, flagged 50, due 60, seen 70, name 100)
    assert_eq!(
        fields(record),
        [
            "state",
            "qty",
            "weight",
            "partner_id",
            "flagged",
            "due",
            "seen",
            "name"
        ]
    );
    let c = change(record, "qty");
    assert_eq!((c.old(), c.new_value()), (&Cell::Null, &Cell::Int(2000)));
    let c = change(record, "state");
    assert_eq!(
        (c.old(), c.new_value()),
        (&Cell::Null, &Cell::Str("done".into()))
    );
    let c = change(record, "partner_id");
    assert_eq!((c.old(), c.new_value()), (&Cell::Null, &Cell::Id(pid)));
    let c = change(record, "flagged");
    assert_eq!((c.old(), c.new_value()), (&Cell::Null, &Cell::Bool(true)));
    let c = change(record, "seen");
    assert_eq!(
        (c.old(), c.new_value()),
        (&Cell::Null, &Cell::DateTime(seen))
    );
    let c = change(record, "name");
    assert_eq!(
        (c.old(), c.new_value()),
        (&Cell::Str("t0".into()), &Cell::Str("renamed".into()))
    );
    assert_eq!(c.sequence(), 100);
    // draining took them: a second take is empty
    assert!(tx.take_tracked_changes().await.unwrap().is_empty());
}

/// write that follows is flushed or not; the creation message is the mail addon's)
#[tokio::test]
async fn spec_mail_052_create_is_never_a_diff() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let created = tx
        .env()
        .model::<Task>()
        .unwrap()
        .create(
            &mut tx,
            vec![Vals::new().set(Task::NAME, "a").set(Task::QTY, 1)],
        )
        .await
        .unwrap();
    // written before and after the INSERT reached the database
    created
        .write(&mut tx, Vals::new().set(Task::QTY, 2))
        .await
        .unwrap();
    tx.flush().await.unwrap();
    created
        .write(&mut tx, Vals::new().set(Task::QTY, 3))
        .await
        .unwrap();
    assert!(tx.take_tracked_changes().await.unwrap().is_empty());
}

/// change, and the first old and last new value are kept otherwise)
#[tokio::test]
async fn spec_mail_052_changes_are_compared_when_taken() {
    let h = harness().await;
    let ids = committed(&h, 1).await;
    let mut tx = h.begin().await.unwrap();
    let task = tasks(&tx, &ids);
    for name in ["b", "t0"] {
        task.write(&mut tx, Vals::new().set(Task::NAME, name))
            .await
            .unwrap();
        tx.flush().await.unwrap();
    }
    assert!(tx.take_tracked_changes().await.unwrap().is_empty());
    // first old, last new
    for qty in [5, 7, 9] {
        task.write(&mut tx, Vals::new().set(Task::QTY, qty))
            .await
            .unwrap();
        tx.flush().await.unwrap();
    }
    let out = tx.take_tracked_changes().await.unwrap();
    let c = change(&out[0], "qty");
    assert_eq!((c.old(), c.new_value()), (&Cell::Null, &Cell::Int(9)));
}

/// change)
#[tokio::test]
async fn spec_mail_052_falsy_values_are_equal() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let created = make(&mut tx, vec![Vals::new().set(Task::QTY, 0)]).await;
    let id = real(created.rec_ids()[0]);
    tx.commit().await.unwrap();
    let mut tx = h.begin().await.unwrap();
    let task = tasks(&tx, &[id]);
    task.write(
        &mut tx,
        Vals::new()
            .set(Task::NAME, "")
            .set(Task::FLAGGED, false)
            .set(Task::QTY, 0),
    )
    .await
    .unwrap();
    assert!(tx.take_tracked_changes().await.unwrap().is_empty());
}

#[tokio::test]
async fn spec_mail_052_unlink_drops_the_changes() {
    let h = harness().await;
    let ids = committed(&h, 2).await;
    let mut tx = h.begin().await.unwrap();
    let all = tasks(&tx, &ids);
    all.write(&mut tx, Vals::new().set(Task::QTY, 4))
        .await
        .unwrap();
    tx.flush().await.unwrap();
    tasks(&tx, &ids[..1]).unlink(&mut tx).await.unwrap();
    let out = tx.take_tracked_changes().await.unwrap();
    assert_eq!(out.len(), 1);
    assert_eq!(real(out[0].id()), ids[1]);
}

/// creation-message half is the mail addon's)
#[tokio::test]
async fn spec_mail_053_context_keys_switch_tracking_off() {
    let h = harness().await;
    let ids = committed(&h, 1).await;
    for key in ["tracking_disable", "mail_notrack"] {
        let mut tx = h.begin().await.unwrap();
        tasks(&tx, &ids)
            .with_context(ctx(key))
            .write(&mut tx, Vals::new().set(Task::QTY, 11))
            .await
            .unwrap();
        assert!(
            tx.take_tracked_changes().await.unwrap().is_empty(),
            "{key} must switch tracking off"
        );
        tx.rollback().await.unwrap();
    }
}

/// flush is sent first when a tracked write of the same record follows, so it is never reported)
#[tokio::test]
async fn spec_mail_053_a_pending_untracked_write_is_flushed_first() {
    let h = harness().await;
    let ids = committed(&h, 1).await;
    let mut tx = h.begin().await.unwrap();
    let task = tasks(&tx, &ids);
    task.with_context(ctx("mail_notrack"))
        .write(&mut tx, Vals::new().set(Task::QTY, 8))
        .await
        .unwrap();
    // not flushed yet: the tracked write below sends it first (one extra statement)
    let before = tx.query_count();
    task.write(&mut tx, Vals::new().set(Task::NAME, "tracked"))
        .await
        .unwrap();
    assert_eq!(
        one(
            &mut tx,
            &format!("SELECT qty::text FROM track_task WHERE id = {}", ids[0])
        )
        .await
        .as_deref(),
        Some("8"),
        "the untracked value was sent before the tracked write was prepared"
    );
    // since `before`: the one flush UPDATE of the pending untracked value, then the SELECT of
    // the check above
    assert_eq!(
        tx.query_count() - before,
        2,
        "the flush UPDATE, then the SELECT of the check"
    );
    let out = tx.take_tracked_changes().await.unwrap();
    assert_eq!(out.len(), 1);
    assert_eq!(fields(&out[0]), ["name"]);
}

#[tokio::test]
async fn spec_mail_052_a_deleted_prepared_record_reports_nothing() {
    let h = harness().await;
    let ids = committed(&h, 1).await;
    let mut tx = h.begin().await.unwrap();
    let task = tasks(&tx, &ids);
    task.write(&mut tx, Vals::new().set(Task::QTY, 1))
        .await
        .unwrap();
    task.unlink(&mut tx).await.unwrap();
    assert!(tx.take_tracked_changes().await.unwrap().is_empty());
}

/// the sets come from the relation write in memory)
#[tokio::test]
async fn spec_mail_062_many2many_records_the_old_and_new_sets() {
    let h = harness().await;
    let ids = committed(&h, 1).await;
    let mut tx = h.begin().await.unwrap();
    let tags = tx
        .env()
        .model::<Tag>()
        .unwrap()
        .create(
            &mut tx,
            vec![
                Vals::new().set(Tag::NAME, "T1"),
                Vals::new().set(Tag::NAME, "T2"),
            ],
        )
        .await
        .unwrap();
    let (t1, t2) = (tags.rec_ids()[0], tags.rec_ids()[1]);
    let task = tasks(&tx, &ids);
    task.as_records()
        .write(
            &mut tx,
            RawVals::new().set_commands("tag_ids", [Command::Set(vec![t1])]),
        )
        .await
        .unwrap();
    tx.flush().await.unwrap();
    tx.take_tracked_changes().await.unwrap();
    // [T1] -> [T1, T2]
    let statements = tx.query_count();
    task.as_records()
        .write(
            &mut tx,
            RawVals::new().set_commands("tag_ids", [Command::Link(t2)]),
        )
        .await
        .unwrap();
    let tracked_cost = tx.query_count() - statements;
    let out = tx.take_tracked_changes().await.unwrap();
    assert_eq!(out.len(), 1);
    let c = change(&out[0], "tag_ids");
    assert_eq!(c.old(), &Cell::Ids(vec![t1].into()));
    assert_eq!(c.new_value(), &Cell::Ids(vec![t1, t2].into()));
    // the same link under tracking_disable costs the same: capture adds no statement (D3)
    let silent = task.with_context(ctx("tracking_disable"));
    silent
        .as_records()
        .write(
            &mut tx,
            RawVals::new().set_commands("tag_ids", [Command::Set(vec![t1])]),
        )
        .await
        .unwrap();
    let statements = tx.query_count();
    silent
        .as_records()
        .write(
            &mut tx,
            RawVals::new().set_commands("tag_ids", [Command::Link(t2)]),
        )
        .await
        .unwrap();
    assert_eq!(tx.query_count() - statements, tracked_cost);
    tx.take_tracked_changes().await.unwrap();
    // removing and re-adding the same tag is no change
    task.as_records()
        .write(
            &mut tx,
            RawVals::new().set_commands("tag_ids", [Command::Unlink(t2)]),
        )
        .await
        .unwrap();
    task.as_records()
        .write(
            &mut tx,
            RawVals::new().set_commands("tag_ids", [Command::Link(t2)]),
        )
        .await
        .unwrap();
    assert!(tx.take_tracked_changes().await.unwrap().is_empty());
}

/// a released one reports its changes, and taking inside a savepoint is refused)
#[tokio::test]
async fn spec_mail_052_savepoints_scope_the_log() {
    let h = harness().await;
    let ids = committed(&h, 1).await;
    let mut tx = h.begin().await.unwrap();
    let task = tasks(&tx, &ids);

    let rolled: Result<()> = tx
        .savepoint(async |tx| {
            task.write(tx, Vals::new().set(Task::QTY, 3)).await?;
            tx.flush().await?;
            Err(erp_sdk::Error::validation("undo"))
        })
        .await;
    assert!(rolled.is_err());
    assert!(tx.take_tracked_changes().await.unwrap().is_empty());

    let refusal = tx
        .savepoint(async |tx| {
            task.write(tx, Vals::new().set(Task::QTY, 4)).await?;
            Ok(tx.take_tracked_changes().await.unwrap_err())
        })
        .await
        .unwrap();
    assert_eq!(refusal.kind(), ErrorKind::Programming);
    assert_eq!(
        refusal.message(),
        "take_tracked_changes: a savepoint is open; its changes can still be rolled back, take \
         the tracked changes after it is released"
    );
    // the savepoint above was released with its write: reported now
    let out = tx.take_tracked_changes().await.unwrap();
    let c = change(&out[0], "qty");
    assert_eq!((c.old(), c.new_value()), (&Cell::Null, &Cell::Int(4)));
}

/// one is a creation and never a diff)
#[tokio::test]
async fn spec_mail_052_upsert_updates_are_tracked_inserts_are_not() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let stock = tx.env().model::<Stock>().unwrap();
    let row = |code: &str, qty: i32| {
        Vals::<Stock>::new()
            .set(Stock::CODE, code)
            .set(Stock::QTY, qty)
    };
    stock
        .upsert(
            &mut tx,
            Upsert::on(Stock::CODE_KEY)
                .rows([row("A", 1)])
                .update([Stock::QTY]),
        )
        .await
        .unwrap();
    assert!(tx.take_tracked_changes().await.unwrap().is_empty());
    tx.commit().await.unwrap();

    let mut tx = h.begin().await.unwrap();
    let stock = tx.env().model::<Stock>().unwrap();
    stock
        .upsert(
            &mut tx,
            Upsert::on(Stock::CODE_KEY)
                .rows([row("A", 5), row("B", 9)])
                .update([Stock::QTY]),
        )
        .await
        .unwrap();
    let out = tx.take_tracked_changes().await.unwrap();
    assert_eq!(out.len(), 1, "{out:?}");
    let c = change(&out[0], "qty");
    assert_eq!((c.old(), c.new_value()), (&Cell::Int(1), &Cell::Int(5)));
}

/// cell: a decimal written as 1.234 into a two-place column is stored as 1.23, so rewriting the
/// stored value is no change and a real change reports the rounded value; floats keep their bits)
#[tokio::test]
async fn spec_mail_052_upsert_reports_stored_values() {
    let h = harness().await;
    let dec = |s: &str| erp_sdk::Decimal::from_str_exact(s).unwrap();
    let row = |price: &str, ratio: f64| {
        Vals::<Stock>::new()
            .set(Stock::CODE, "A")
            .set(Stock::PRICE, dec(price))
            .set(Stock::RATIO, ratio)
    };
    let up = |price: &str, ratio: f64| {
        Upsert::on(Stock::CODE_KEY)
            .rows([row(price, ratio)])
            .update([Stock::PRICE])
            .update([Stock::RATIO])
    };
    let mut tx = h.begin().await.unwrap();
    let stock = tx.env().model::<Stock>().unwrap();
    stock.upsert(&mut tx, up("1.23", 0.5)).await.unwrap();
    tx.commit().await.unwrap();

    // 1.234 is stored as 1.23: the stored value did not change
    let mut tx = h.begin().await.unwrap();
    let stock = tx.env().model::<Stock>().unwrap();
    stock.upsert(&mut tx, up("1.234", 0.5)).await.unwrap();
    let out = tx.take_tracked_changes().await.unwrap();
    assert!(out.is_empty(), "false change from the raw cell: {out:?}");

    // a real change reports the stored (rounded) new value and the float as written
    stock.upsert(&mut tx, up("2.346", 0.75)).await.unwrap();
    let out = tx.take_tracked_changes().await.unwrap();
    assert_eq!(out.len(), 1, "{out:?}");
    let c = change(&out[0], "price");
    assert_eq!(
        (c.old(), c.new_value()),
        (&Cell::Decimal(dec("1.23")), &Cell::Decimal(dec("2.35")))
    );
    let c = change(&out[0], "ratio");
    assert_eq!(
        (c.old(), c.new_value()),
        (&Cell::Float(0.5), &Cell::Float(0.75))
    );
}

/// The statements of `flush` after writing `n` tasks, with or without tracking disabled.
async fn flush_cost(
    h: &Harness,
    ids: &[i64],
    field_tracked: bool,
    disabled: bool,
) -> (u64, String) {
    let mut tx = h.begin().await.unwrap();
    let all = tasks(&tx, ids);
    let all = if disabled {
        all.with_context(ctx("tracking_disable"))
    } else {
        all
    };
    let vals = if field_tracked {
        Vals::new().set(Task::QTY, 6)
    } else {
        Vals::new().set(Task::NOTE, "n")
    };
    all.write(&mut tx, vals).await.unwrap();
    tx.capture_statements(true);
    let before = tx.query_count();
    tx.flush().await.unwrap();
    let cost = tx.query_count() - before;
    let sql = tx
        .captured()
        .iter()
        .find(|s| s.contains("UPDATE"))
        .cloned()
        .unwrap_or_default();
    tx.capture_statements(false);
    tx.rollback().await.unwrap();
    (cost, sql)
}

/// writes costs what the same flush under `tracking_disable` costs, and the statement returns old
/// and new values only when a tracked column is written)
#[tokio::test]
async fn spec_mail_052_capture_adds_no_statement() {
    let h = harness().await;
    for n in [1, 100, 1000] {
        let ids = committed(&h, n).await;
        let (tracked, sql) = flush_cost(&h, &ids, true, false).await;
        let (twin, _) = flush_cost(&h, &ids, true, true).await;
        assert_eq!(tracked, twin, "N = {n}");
        assert!(sql.contains("RETURNING WITH (OLD AS"), "N = {n}: {sql}");
        // a write of an untracked column keeps the plain statement
        let (untracked, plain) = flush_cost(&h, &ids, false, false).await;
        assert_eq!(untracked, tracked, "N = {n}");
        assert!(
            !plain.contains("RETURNING WITH (OLD AS"),
            "N = {n}: {plain}"
        );
    }
}

static LOG: Mutex<Vec<String>> = Mutex::new(Vec::new());

fn log_hook(tx: &mut Transaction) -> BoxFuture<'_, Result<()>> {
    Box::pin(async move {
        for record in tx.take_tracked_changes().await? {
            for c in record.changes() {
                LOG.lock().unwrap().push(format!(
                    "{}#{}.{}:{:?}->{:?}",
                    record.model(),
                    real(record.id()),
                    c.field(),
                    c.old(),
                    c.new_value()
                ));
            }
        }
        Ok(())
    })
}

/// flush; the mail addon registers its own hook and builds the messages)
#[tokio::test]
async fn spec_mail_051_the_precommit_hook_runs_at_commit() {
    let h = Harness::builder([&base_users::ADDON, &tracking::ADDON])
        .precommit_hook(erp_sdk::PrecommitHook::new("log", log_hook))
        .build()
        .await
        .unwrap();
    let ids = committed(&h, 1).await;
    LOG.lock().unwrap().clear();
    let mut tx = h.begin().await.unwrap();
    tasks(&tx, &ids)
        .write(&mut tx, Vals::new().set(Task::QTY, 12))
        .await
        .unwrap();
    // pending, not yet flushed: the commit flushes, then runs the hook
    tx.commit().await.unwrap();
    assert_eq!(
        *LOG.lock().unwrap(),
        [format!("track.task#{}.qty:Null->Int(12)", ids[0])]
    );
    LOG.lock().unwrap().clear();
    let mut tx = h.begin().await.unwrap();
    tasks(&tx, &ids)
        .write(&mut tx, Vals::new().set(Task::QTY, 13))
        .await
        .unwrap();
    tx.rollback().await.unwrap();
    assert!(LOG.lock().unwrap().is_empty());
}

fn churn_hook(tx: &mut Transaction) -> BoxFuture<'_, Result<()>> {
    Box::pin(async move {
        for record in tx.take_tracked_changes().await? {
            let task = tx.env().model::<Task>()?.browse([real(record.id())])?;
            let qty = task.get(tx, Task::QTY).await?;
            task.write(tx, Vals::new().set(Task::QTY, qty + 1)).await?;
        }
        Ok(())
    })
}

/// rounds and fails loudly instead of committing)
#[tokio::test]
async fn spec_mail_052_a_hook_that_never_settles_fails_the_commit() {
    let h = Harness::builder([&base_users::ADDON, &tracking::ADDON])
        .precommit_hook(erp_sdk::PrecommitHook::new("churn", churn_hook))
        .build()
        .await
        .unwrap();
    let ids = committed(&h, 1).await;
    let mut tx = h.begin().await.unwrap();
    tasks(&tx, &ids)
        .write(&mut tx, Vals::new().set(Task::QTY, 1))
        .await
        .unwrap();
    let error = tx.commit().await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.message(),
        "the precommit hooks [churn] did not settle after 16 rounds: tracked changes are still \
         pending"
    );
}

static COUNT: Mutex<usize> = Mutex::new(0);

fn count_hook(_tx: &mut Transaction) -> BoxFuture<'_, Result<()>> {
    Box::pin(async move {
        *COUNT.lock().unwrap() += 1;
        Ok(())
    })
}

/// tracked changes are pending: the hooks run again only after a round in which one of them
/// drained)
#[tokio::test]
async fn spec_mail_051_a_hook_that_drains_nothing_runs_once() {
    let h = Harness::builder([&base_users::ADDON, &tracking::ADDON])
        .precommit_hook(erp_sdk::PrecommitHook::new("count", count_hook))
        .build()
        .await
        .unwrap();
    let ids = committed(&h, 1).await;
    *COUNT.lock().unwrap() = 0;
    let mut tx = h.begin().await.unwrap();
    tasks(&tx, &ids)
        .write(&mut tx, Vals::new().set(Task::QTY, 21))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(*COUNT.lock().unwrap(), 1);
}

static ARMED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Fails only once the test arms it (the harness commits while it builds the database).
fn failing_hook(_tx: &mut Transaction) -> BoxFuture<'_, Result<()>> {
    Box::pin(async move {
        if ARMED.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(erp_sdk::Error::validation("boom"));
        }
        Ok(())
    })
}

#[tokio::test]
async fn spec_mail_051_a_failing_hook_is_named() {
    let h = Harness::builder([&base_users::ADDON, &tracking::ADDON])
        .precommit_hook(erp_sdk::PrecommitHook::new("failing", failing_hook))
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    ARMED.store(true, std::sync::atomic::Ordering::SeqCst);
    let error = tx.commit().await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(error.message(), "precommit hook 'failing': boom");
}

static ARMED_FULL: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Fails once armed with an error that carries a constraint and an invariant condition.
fn failing_full_hook(_tx: &mut Transaction) -> BoxFuture<'_, Result<()>> {
    Box::pin(async move {
        if ARMED_FULL.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(erp_sdk::Error::invariant("broken contract").with_constraint("track_c"));
        }
        Ok(())
    })
}

/// A failing hook's error keeps the kind, the full message, the condition and the constraint.
#[tokio::test]
async fn a_failing_hook_error_keeps_condition_and_constraint() {
    let h = Harness::builder([&base_users::ADDON, &tracking::ADDON])
        .precommit_hook(erp_sdk::PrecommitHook::new("full", failing_full_hook))
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    ARMED_FULL.store(true, std::sync::atomic::Ordering::SeqCst);
    let error = tx.commit().await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(error.message(), "precommit hook 'full': broken contract");
    assert_eq!(
        error.condition(),
        Some(erp_sdk::Condition::InvariantViolation)
    );
    assert_eq!(error.constraint(), Some("track_c"));
}

/// once, under the parent model; the mail-side message is the mail addon's)
#[tokio::test]
async fn spec_mail_052_a_delegated_write_is_reported_under_the_parent() {
    let h = Harness::builder([&base_users::ADDON, &tracking::ADDON])
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    let child = tx
        .env()
        .model::<tracking::Child>()
        .unwrap()
        .create(&mut tx, vec![Vals::new().set(tracking::Child::EXTRA, "x")])
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let child_id = real(child.rec_ids()[0]);
    let mut tx = h.begin().await.unwrap();
    let child = tx
        .env()
        .model::<tracking::Child>()
        .unwrap()
        .browse([child_id])
        .unwrap();
    child
        .as_records()
        .write(
            &mut tx,
            RawVals::new().set_cell("name", Cell::Str("via child".into())),
        )
        .await
        .unwrap();
    let out = tx.take_tracked_changes().await.unwrap();
    assert_eq!(out.len(), 1, "{out:?}");
    assert_eq!(out[0].model(), "track.task");
    assert_eq!(fields(&out[0]), ["name"]);
    assert_eq!(
        change(&out[0], "name").new_value(),
        &Cell::Str("via child".into())
    );
}
