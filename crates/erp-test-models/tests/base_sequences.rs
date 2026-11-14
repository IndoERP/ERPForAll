//! Parity tests for `ir.sequence` and numbering.
//! Specs: and 014 (the pure SPEC-SEQ cases are unit tests in
//! `crates/erp-orm/src/sequence/pattern.rs`).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod support;

use std::sync::Mutex;
use std::time::{Duration, Instant};

use erp_fields::{Condition, Date, Error, ErrorKind, RecId};
use erp_orm::{BeginOptions, Timeouts, sequence};
use erp_test_models::base_users;
use erp_testkit::{Harness, assert_query_count};

async fn seq_harness() -> Harness {
    Harness::builder([&base_users::ADDON, &erp_addon_base::ADDON])
        .build()
        .await
        .expect("harness")
}

fn rid(id: i64) -> RecId {
    RecId::from_raw(id).expect("valid rec id")
}

/// Helper to insert an ir.sequence row via ORM create with optional wait_mode.
#[allow(clippy::too_many_arguments)]
async fn create_seq_with_wait_mode(
    tx: &mut erp_orm::Transaction,
    name: &str,
    code: &str,
    implementation: &str,
    prefix: Option<&str>,
    suffix: Option<&str>,
    padding: i32,
    number_increment: i32,
    number_next: i32,
    company_id: Option<i64>,
    use_date_range: bool,
    wait_mode: Option<&str>,
) -> i64 {
    let mut vals = erp_orm::RawVals::new();
    vals.insert(
        "name".into(),
        erp_fields::Cell::Str(name.to_string().into()),
    );
    vals.insert(
        "code".into(),
        erp_fields::Cell::Str(code.to_string().into()),
    );
    vals.insert(
        "implementation".into(),
        erp_fields::Cell::Str(implementation.to_string().into()),
    );
    if let Some(p) = prefix {
        vals.insert("prefix".into(), erp_fields::Cell::Str(p.to_string().into()));
    }
    if let Some(s) = suffix {
        vals.insert("suffix".into(), erp_fields::Cell::Str(s.to_string().into()));
    }
    vals.insert("padding".into(), erp_fields::Cell::Int(padding));
    vals.insert(
        "number_increment".into(),
        erp_fields::Cell::Int(number_increment),
    );
    vals.insert("number_next".into(), erp_fields::Cell::Int(number_next));
    if let Some(cid) = company_id {
        vals.insert("company_id".into(), erp_fields::Cell::Id(cid));
    }
    vals.insert(
        "use_date_range".into(),
        erp_fields::Cell::Bool(use_date_range),
    );
    if let Some(wm) = wait_mode {
        vals.insert(
            "wait_mode".into(),
            erp_fields::Cell::Str(wm.to_string().into()),
        );
    }

    let seq_model = tx.env().records("ir.sequence").unwrap();
    let records = seq_model.create(tx, vec![vals]).await.unwrap();
    records.rec_ids()[0].get()
}

/// Helper to insert an ir.sequence row via ORM create.
#[allow(clippy::too_many_arguments)]
async fn create_seq(
    tx: &mut erp_orm::Transaction,
    name: &str,
    code: &str,
    implementation: &str,
    prefix: Option<&str>,
    suffix: Option<&str>,
    padding: i32,
    number_increment: i32,
    number_next: i32,
    company_id: Option<i64>,
    use_date_range: bool,
) -> i64 {
    create_seq_with_wait_mode(
        tx,
        name,
        code,
        implementation,
        prefix,
        suffix,
        padding,
        number_increment,
        number_next,
        company_id,
        use_date_range,
        None,
    )
    .await
}

#[tokio::test]
async fn spec_base_031_create_standard_sequence_and_pg_backing_sequence() {
    let h = seq_harness().await;
    let mut tx = h.begin().await.unwrap();

    let seq_id = create_seq(
        &mut tx,
        "Test Std",
        "test_seq_std",
        "standard",
        None,
        None,
        0,
        1,
        1,
        None,
        false,
    )
    .await;

    // Verify backing sequence exists in PostgreSQL pg_class
    let seq_name = format!("ir_sequence_{seq_id:03}");
    let check_sql = "SELECT 1 FROM pg_class WHERE relkind = 'S' AND relname = $1";
    let rows = tx.query(check_sql, &[&seq_name]).await.unwrap();
    assert_eq!(rows.len(), 1, "Backing sequence {seq_name} must exist");

    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_base_032_sequential_draw_on_standard_sequence() {
    let h = seq_harness().await;
    let mut tx = h.begin().await.unwrap();

    create_seq(
        &mut tx,
        "Test Std",
        "test_seq_std_draw",
        "standard",
        None,
        None,
        0,
        1,
        1,
        None,
        false,
    )
    .await;

    let env = tx.env();
    let mut drawn = Vec::new();
    for _ in 0..5 {
        let num = sequence::next_by_code(&mut tx, &env, "test_seq_std_draw", None)
            .await
            .unwrap()
            .expect("sequence drawn");
        drawn.push(num);
    }

    assert_eq!(drawn, vec!["1", "2", "3", "4", "5"]);

    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_base_033_concurrent_draw_on_standard_sequence_does_not_block() {
    let h = seq_harness().await;

    // Setup sequence
    {
        let mut tx = h.begin().await.unwrap();
        create_seq(
            &mut tx,
            "Concurrent Std",
            "test_concurrent_std",
            "standard",
            None,
            None,
            0,
            1,
            1,
            None,
            false,
        )
        .await;
        tx.commit().await.unwrap();
    }

    // Deliberate overlap: tx1 draws, holds tx1 open, tx2 draws and commits, then tx1 commits.
    let orm1 = h.orm().clone();
    let orm2 = h.orm().clone();

    let (tx1_drawn_tx, tx1_drawn_rx) = tokio::sync::oneshot::channel::<()>();
    let (tx2_done_tx, tx2_done_rx) = tokio::sync::oneshot::channel::<()>();

    let task1 = tokio::spawn(async move {
        let mut tx1 = orm1.begin(erp_orm::SUPERUSER_ID).await.unwrap();
        let env1 = tx1.env();
        let num1 = sequence::next_by_code(&mut tx1, &env1, "test_concurrent_std", None)
            .await
            .unwrap()
            .unwrap();
        // Signal task2 that tx1 has drawn while holding tx1 open
        tx1_drawn_tx.send(()).unwrap();
        // Wait until task2 finishes drawing and commits
        tx2_done_rx.await.unwrap();
        tx1.commit().await.unwrap();
        num1
    });

    let task2 = tokio::spawn(async move {
        // Wait until tx1 has drawn and is holding its transaction open
        tx1_drawn_rx.await.unwrap();
        let mut tx2 = orm2.begin(erp_orm::SUPERUSER_ID).await.unwrap();
        let env2 = tx2.env();
        let num2 = sequence::next_by_code(&mut tx2, &env2, "test_concurrent_std", None)
            .await
            .unwrap()
            .unwrap();
        // tx2 commits while tx1 is still open and uncommitted
        tx2.commit().await.unwrap();
        tx2_done_tx.send(()).unwrap();
        num2
    });

    let (res1, res2) = tokio::join!(task1, task2);
    let mut nums = vec![res1.unwrap(), res2.unwrap()];
    nums.sort();
    assert_eq!(nums, vec!["1", "2"]);

    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_base_034_create_no_gap_sequence_without_backing_pg_sequence() {
    let h = seq_harness().await;
    let mut tx = h.begin().await.unwrap();

    let seq_id = create_seq(
        &mut tx,
        "No Gap",
        "test_nogap_seq",
        "no_gap",
        None,
        None,
        0,
        1,
        1,
        None,
        false,
    )
    .await;

    // Verify backing sequence does NOT exist in pg_class
    let seq_name = format!("ir_sequence_{seq_id:03}");
    let check_sql = "SELECT 1 FROM pg_class WHERE relkind = 'S' AND relname = $1";
    let rows = tx.query(check_sql, &[&seq_name]).await.unwrap();
    assert_eq!(rows.len(), 0, "No backing sequence for no_gap");

    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_base_035_sequential_draw_on_no_gap_sequence() {
    let h = seq_harness().await;
    let mut tx = h.begin().await.unwrap();

    let seq_id = create_seq(
        &mut tx,
        "No Gap Draw",
        "test_nogap_draw",
        "no_gap",
        None,
        None,
        0,
        1,
        1,
        None,
        false,
    )
    .await;

    let env = tx.env();
    let mut drawn = Vec::new();
    for _ in 0..3 {
        let num = sequence::next_by_code(&mut tx, &env, "test_nogap_draw", None)
            .await
            .unwrap()
            .expect("drawn");
        drawn.push(num);
    }
    assert_eq!(drawn, vec!["1", "2", "3"]);

    // Verify number_next in ir_sequence table was updated to 4
    let rows = tx
        .query(
            "SELECT number_next FROM ir_sequence WHERE id = $1",
            &[&seq_id],
        )
        .await
        .unwrap();
    let next: i32 = rows[0].try_get(0).unwrap();
    assert_eq!(next, 4);

    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_base_036_concurrency_lock_contention_on_no_gap_sequence() {
    let h = seq_harness().await;

    {
        let mut tx = h.begin().await.unwrap();
        create_seq(
            &mut tx,
            "Contention",
            "test_nogap_contention",
            "no_gap",
            None,
            None,
            0,
            1,
            1,
            None,
            false,
        )
        .await;
        tx.commit().await.unwrap();
    }

    let mut tx1 = h.begin().await.unwrap();
    // Tx1 draws from no_gap sequence, locking the row FOR UPDATE
    let env1 = tx1.env();
    let num1 = sequence::next_by_code(&mut tx1, &env1, "test_nogap_contention", None)
        .await
        .unwrap();
    assert_eq!(num1, Some("1".to_string()));

    // Tx2 attempts to draw before Tx1 commits: fails with Lock (SQLSTATE 55P03 / NOWAIT)
    let mut tx2 = h.begin().await.unwrap();
    let env2 = tx2.env();
    let err = sequence::next_by_code(&mut tx2, &env2, "test_nogap_contention", None)
        .await
        .unwrap_err();

    assert_eq!(err.kind(), ErrorKind::Lock);
    assert_eq!(err.message(), "could not obtain lock on no_gap sequence");
    assert_eq!(err.sqlstate(), Some("55P03"));

    drop(tx1);
    drop(tx2);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_base_037_switch_standard_to_no_gap_drops_sequence() {
    let h = seq_harness().await;
    let mut tx = h.begin().await.unwrap();

    let seq_id = create_seq(
        &mut tx,
        "Switch to NoGap",
        "test_switch_nogap",
        "standard",
        None,
        None,
        0,
        1,
        1,
        None,
        false,
    )
    .await;

    let seq_name = format!("ir_sequence_{seq_id:03}");
    // Verify backing sequence exists
    let check_sql = "SELECT 1 FROM pg_class WHERE relkind = 'S' AND relname = $1";
    let rows = tx.query(check_sql, &[&seq_name]).await.unwrap();
    assert_eq!(rows.len(), 1);

    // Switch to no_gap via ORM write
    let seq_record = tx
        .env()
        .records("ir.sequence")
        .unwrap()
        .browse_ids([rid(seq_id)]);
    let mut vals = erp_orm::RawVals::new();
    vals.insert(
        "implementation".into(),
        erp_fields::Cell::Str("no_gap".to_string().into()),
    );
    seq_record.write(&mut tx, vals).await.unwrap();

    // Verify backing sequence is dropped
    let rows = tx.query(check_sql, &[&seq_name]).await.unwrap();
    assert_eq!(rows.len(), 0);

    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_base_038_switch_no_gap_to_standard_creates_sequence() {
    let h = seq_harness().await;
    let mut tx = h.begin().await.unwrap();

    let seq_id = create_seq(
        &mut tx,
        "Switch to Std",
        "test_switch_std",
        "no_gap",
        None,
        None,
        0,
        1,
        10,
        None,
        false,
    )
    .await;

    let seq_name = format!("ir_sequence_{seq_id:03}");
    let check_sql = "SELECT 1 FROM pg_class WHERE relkind = 'S' AND relname = $1";
    let rows = tx.query(check_sql, &[&seq_name]).await.unwrap();
    assert_eq!(rows.len(), 0);

    // Switch to standard via ORM write
    let seq_record = tx
        .env()
        .records("ir.sequence")
        .unwrap()
        .browse_ids([rid(seq_id)]);
    let mut vals = erp_orm::RawVals::new();
    vals.insert(
        "implementation".into(),
        erp_fields::Cell::Str("standard".to_string().into()),
    );
    seq_record.write(&mut tx, vals).await.unwrap();

    // Verify backing sequence exists
    let rows = tx.query(check_sql, &[&seq_name]).await.unwrap();
    assert_eq!(rows.len(), 1);

    let env = tx.env();
    let num = sequence::next_by_code(&mut tx, &env, "test_switch_std", None)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(num, "10");

    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_base_039_unlink_sequence_drops_backing_sequence() {
    let h = seq_harness().await;
    let mut tx = h.begin().await.unwrap();

    let seq_id = create_seq(
        &mut tx,
        "To Unlink",
        "test_unlink_seq",
        "standard",
        None,
        None,
        0,
        1,
        1,
        None,
        true, // use_date_range
    )
    .await;

    let seq_name = format!("ir_sequence_{seq_id:03}");
    let check_sql = "SELECT 1 FROM pg_class WHERE relkind = 'S' AND relname = $1";
    let rows = tx.query(check_sql, &[&seq_name]).await.unwrap();
    assert_eq!(rows.len(), 1);

    // Draw for two different years to create two date ranges with backing sequences
    let env = tx.env();
    let _ = sequence::next_by_code(
        &mut tx,
        &env,
        "test_unlink_seq",
        Some(Date::from_ymd(2026, 5, 1).unwrap()),
    )
    .await
    .unwrap();
    let _ = sequence::next_by_code(
        &mut tx,
        &env,
        "test_unlink_seq",
        Some(Date::from_ymd(2025, 5, 1).unwrap()),
    )
    .await
    .unwrap();

    let range_rows = tx
        .query(
            "SELECT id FROM ir_sequence_date_range WHERE sequence_id = $1 ORDER BY date_from ASC",
            &[&seq_id],
        )
        .await
        .unwrap();
    assert_eq!(range_rows.len(), 2);
    let r1_id: i64 = range_rows[0].try_get(0).unwrap();
    let r2_id: i64 = range_rows[1].try_get(0).unwrap();

    let r1_seq = format!("ir_sequence_{seq_id:03}_{r1_id:03}");
    let r2_seq = format!("ir_sequence_{seq_id:03}_{r2_id:03}");

    // All three backing sequences exist in pg_class
    assert_eq!(tx.query(check_sql, &[&seq_name]).await.unwrap().len(), 1);
    assert_eq!(tx.query(check_sql, &[&r1_seq]).await.unwrap().len(), 1);
    assert_eq!(tx.query(check_sql, &[&r2_seq]).await.unwrap().len(), 1);

    // 1. Unlink date range 1 directly -> drops its backing sequence, leaves parent and range 2
    let r1_record = tx
        .env()
        .records("ir.sequence.date_range")
        .unwrap()
        .browse_ids([rid(r1_id)]);
    r1_record.unlink(&mut tx).await.unwrap();

    assert_eq!(tx.query(check_sql, &[&r1_seq]).await.unwrap().len(), 0);
    assert_eq!(tx.query(check_sql, &[&seq_name]).await.unwrap().len(), 1);
    assert_eq!(tx.query(check_sql, &[&r2_seq]).await.unwrap().len(), 1);

    // 2. Unlink parent sequence -> drops parent backing sequence AND date range 2 backing sequence
    let seq_record = tx
        .env()
        .records("ir.sequence")
        .unwrap()
        .browse_ids([rid(seq_id)]);
    seq_record.unlink(&mut tx).await.unwrap();

    assert_eq!(tx.query(check_sql, &[&seq_name]).await.unwrap().len(), 0);
    assert_eq!(tx.query(check_sql, &[&r2_seq]).await.unwrap().len(), 0);

    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_base_040_date_interpolation_directives() {
    let h = seq_harness().await;
    let mut tx = h.begin().await.unwrap();

    create_seq(
        &mut tx,
        "Date Format",
        "test_date_fmt",
        "standard",
        Some("%(year)s/%(month)s/%(day)s/"),
        Some("/%(y)s/%(doy)s/%(woy)s"),
        0,
        1,
        1,
        None,
        false,
    )
    .await;

    let test_date = Date::from_ymd(2026, 10, 16).unwrap();
    let env = tx.env();
    let res = sequence::next_by_code(&mut tx, &env, "test_date_fmt", Some(test_date))
        .await
        .unwrap()
        .unwrap();

    // 2026/10/16/1/26/289/41
    assert_eq!(res, "2026/10/16/1/26/289/41");

    // Test range auto-creation with range_month and range_day
    create_seq(
        &mut tx,
        "Range Auto-create Format",
        "test_range_autocreate_fmt",
        "standard",
        Some("%(range_year)s-%(range_month)s-%(range_day)s/"),
        Some("/%(current_year)s-%(year)s"),
        0,
        1,
        1,
        None,
        true,
    )
    .await;

    let clock_fn_2026 = || Date::from_ymd(2026, 3, 20).unwrap();
    let target_draw_date_2028 = Date::from_ymd(2028, 7, 10).unwrap();
    let res2 = sequence::next_by_code_with_clock(
        &mut tx,
        &env,
        "test_range_autocreate_fmt",
        Some(target_draw_date_2028),
        clock_fn_2026,
    )
    .await
    .unwrap()
    .unwrap();

    // Range auto-created for full year 2028 (2028-01-01 .. 2028-12-31):
    // range_year = 2028, range_month = 01, range_day = 01, current_year = 2026 (clock), year = 2028 (draw date)
    assert_eq!(res2, "2028-01-01/1/2026-2028");

    // Test range_* and current_* interpolation directives with use_date_range = true
    // where range date_from (2027), draw date (2028), and injected clock (2029)
    // fall in three different years.
    let seq3_id = create_seq(
        &mut tx,
        "Range Current Three Years",
        "test_range_current_three_years",
        "no_gap",
        Some("%(range_year)s/%(current_year)s/"),
        Some("/%(year)s"),
        0,
        1,
        1,
        None,
        true,
    )
    .await;

    let range_from = Date::from_ymd(2027, 7, 1).unwrap();
    let range_to = Date::from_ymd(2028, 12, 31).unwrap();
    tx.execute(
        r#"
        INSERT INTO ir_sequence_date_range (sequence_id, date_from, date_to, number_next)
        VALUES ($1, $2, $3, 1)
        "#,
        &[&seq3_id, &range_from, &range_to],
    )
    .await
    .unwrap();

    let clock_fn_2029 = || Date::from_ymd(2029, 3, 20).unwrap();
    let res3 = sequence::next_by_code_with_clock(
        &mut tx,
        &env,
        "test_range_current_three_years",
        Some(target_draw_date_2028),
        clock_fn_2029,
    )
    .await
    .unwrap()
    .unwrap();

    // range_year = 2027 (date_from), current_year = 2029 (clock), year = 2028 (draw date)
    assert_eq!(res3, "2027/2029/1/2028");

    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_base_041_iso_calendar_interpolation_directives() {
    let h = seq_harness().await;
    let mut tx = h.begin().await.unwrap();

    create_seq(
        &mut tx,
        "ISO Format",
        "test_iso_fmt",
        "standard",
        Some("%(isoyear)s/%(isoy)s/"),
        Some("/%(isoweek)s/%(weekday)s"),
        0,
        1,
        1,
        None,
        false,
    )
    .await;

    let test_date = Date::from_ymd(2026, 10, 16).unwrap();
    let env = tx.env();
    let res = sequence::next_by_code(&mut tx, &env, "test_iso_fmt", Some(test_date))
        .await
        .unwrap()
        .unwrap();

    assert_eq!(res, "2026/26/1/42/5");

    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_base_042_invalid_prefix_format_raises_user_error() {
    let h = seq_harness().await;
    let mut tx = h.begin().await.unwrap();

    create_seq(
        &mut tx,
        "Bad Prefix",
        "test_bad_prefix",
        "standard",
        Some("%u"),
        None,
        0,
        1,
        1,
        None,
        false,
    )
    .await;

    let env = tx.env();
    let err = sequence::next_by_code(&mut tx, &env, "test_bad_prefix", None)
        .await
        .unwrap_err();

    assert_eq!(err.kind(), ErrorKind::UserError);
    assert_eq!(
        err.message(),
        "Invalid prefix or suffix for sequence “Bad Prefix”"
    );

    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_base_043_invalid_suffix_placeholder_raises_user_error() {
    let h = seq_harness().await;
    let mut tx = h.begin().await.unwrap();

    create_seq(
        &mut tx,
        "Bad Suffix",
        "test_bad_suffix",
        "standard",
        None,
        Some("/%(invalid)s"),
        0,
        1,
        1,
        None,
        false,
    )
    .await;

    let env = tx.env();
    let err = sequence::next_by_code(&mut tx, &env, "test_bad_suffix", None)
        .await
        .unwrap_err();

    assert_eq!(err.kind(), ErrorKind::UserError);
    assert_eq!(
        err.message(),
        "Invalid prefix or suffix for sequence “Bad Suffix”"
    );

    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn test_sequence_default_date_follows_clock() {
    let h = seq_harness().await;
    let mut tx = h.begin().await.unwrap();

    create_seq(
        &mut tx,
        "Clock Seq",
        "test_clock_seq",
        "standard",
        Some("%(year)s-%(month)s-%(day)s-"),
        None,
        0,
        1,
        1,
        None,
        false,
    )
    .await;

    let env = tx.env();
    // 1. Injected clock returns 2030-05-20
    let injected_clock = || Date::from_ymd(2030, 5, 20).unwrap();
    let res =
        sequence::next_by_code_with_clock(&mut tx, &env, "test_clock_seq", None, injected_clock)
            .await
            .unwrap()
            .unwrap();
    assert_eq!(res, "2030-05-20-1");

    // 2. Default clock (no injection, None date) follows real DateTime::now().date()
    let today = erp_fields::DateTime::now().date();
    let (ty, tm, td) = today.ymd();
    let expected_prefix = format!("{ty:04}-{tm:02}-{td:02}-");
    let res_today = sequence::next_by_code(&mut tx, &env, "test_clock_seq", None)
        .await
        .unwrap()
        .unwrap();
    assert!(res_today.starts_with(&expected_prefix));

    // 3. Context override via ir_sequence_date
    let ctx_env = env.with_context([("ir_sequence_date", erp_fields::Value::str("2028-12-25"))]);
    let res_ctx = sequence::next_by_code(&mut tx, &ctx_env, "test_clock_seq", None)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(res_ctx, "2028-12-25-3");

    drop(tx);
    h.cleanup().await.unwrap();
}

#[test]
fn test_calendar_interpolation_year_boundaries() {
    // 2020-12-29 to 2021-01-04
    // 2020-12-29 is Tuesday (weekday 2), ISO week 53, ISO year 2020, woy 52
    let d1 = Date::from_ymd(2020, 12, 29).unwrap();
    let (doy1, woy1, isoyear1, isoweek1, wd1) = sequence::date_interpolation_parts(d1).unwrap();
    assert_eq!((isoyear1, isoweek1, wd1), (2020, 53, 2));
    assert_eq!((doy1, woy1), (364, 52));

    // 2020-12-31 is Thursday, ISO week 53, ISO year 2020
    let d2 = Date::from_ymd(2020, 12, 31).unwrap();
    let (doy2, woy2, isoyear2, isoweek2, wd2) = sequence::date_interpolation_parts(d2).unwrap();
    assert_eq!((isoyear2, isoweek2, wd2), (2020, 53, 4));
    assert_eq!((doy2, woy2), (366, 52));

    // 2021-01-01 is Friday (weekday 5), ISO week 53, ISO year 2020, woy 0
    let d3 = Date::from_ymd(2021, 1, 1).unwrap();
    let (doy3, woy3, isoyear3, isoweek3, wd3) = sequence::date_interpolation_parts(d3).unwrap();
    assert_eq!((isoyear3, isoweek3, wd3), (2020, 53, 5));
    assert_eq!((doy3, woy3), (1, 0));

    let d4 = Date::from_ymd(2021, 1, 3).unwrap();
    let (doy4, woy4, isoyear4, isoweek4, wd4) = sequence::date_interpolation_parts(d4).unwrap();
    assert_eq!((isoyear4, isoweek4, wd4), (2020, 53, 0));
    assert_eq!((doy4, woy4), (3, 0));

    // 2021-01-04 is Monday (weekday 1), ISO week 1, ISO year 2021, woy 1
    let d5 = Date::from_ymd(2021, 1, 4).unwrap();
    let (doy5, woy5, isoyear5, isoweek5, wd5) = sequence::date_interpolation_parts(d5).unwrap();
    assert_eq!((isoyear5, isoweek5, wd5), (2021, 1, 1));
    assert_eq!((doy5, woy5), (4, 1));

    // 2024-12-30 is Monday, ISO week 1, ISO year 2025
    let d6 = Date::from_ymd(2024, 12, 30).unwrap();
    let (doy6, woy6, isoyear6, isoweek6, wd6) = sequence::date_interpolation_parts(d6).unwrap();
    assert_eq!((isoyear6, isoweek6, wd6), (2025, 1, 1));
    assert_eq!((doy6, woy6), (365, 53));
}

#[tokio::test]
async fn spec_base_044_padding_left_pads_with_zeros() {
    let h = seq_harness().await;
    let mut tx = h.begin().await.unwrap();

    create_seq(
        &mut tx,
        "Padding",
        "test_padding",
        "standard",
        None,
        None,
        5,
        1,
        7,
        None,
        false,
    )
    .await;

    let env = tx.env();
    let res = sequence::next_by_code(&mut tx, &env, "test_padding", None)
        .await
        .unwrap()
        .unwrap();

    assert_eq!(res, "00007");

    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_base_045_multi_company_sequence_precedence() {
    let h = seq_harness().await;
    let mut tx = h.begin().await.unwrap();

    let currency = erp_testkit::fixtures::seed_currency_in(&mut tx)
        .await
        .unwrap();

    tx.execute(
        "INSERT INTO res_company (id, name, currency_id) VALUES (1, 'Company 1', $1) ON CONFLICT DO NOTHING",
        &[&currency],
    )
    .await
    .unwrap();

    // Sequence 1: company 1 specific
    create_seq(
        &mut tx,
        "SO C1",
        "SO",
        "standard",
        Some("SO-C1-"),
        None,
        0,
        1,
        1,
        Some(1),
        false,
    )
    .await;

    // Sequence 2: global (company NULL)
    create_seq(
        &mut tx,
        "SO Global",
        "SO",
        "standard",
        Some("SO-GLOBAL-"),
        None,
        0,
        1,
        1,
        None,
        false,
    )
    .await;

    // When in company 1: company-specific sequence is used
    let env_c1 = tx.env().with_company(1).unwrap();
    let res_c1 = sequence::next_by_code(&mut tx, &env_c1, "SO", None)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(res_c1, "SO-C1-1");

    // When in company 2: global sequence is used
    let env_c2 = tx.env().with_company(2).unwrap();
    let res_c2 = sequence::next_by_code(&mut tx, &env_c2, "SO", None)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(res_c2, "SO-GLOBAL-1");

    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_base_046_next_by_code_returns_none_when_code_not_found() {
    let h = seq_harness().await;
    let mut tx = h.begin().await.unwrap();

    let env = tx.env();
    let res = sequence::next_by_code(&mut tx, &env, "NONEXISTENT", None)
        .await
        .unwrap();

    assert_eq!(res, None);

    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn test_use_date_range_creation_and_number_next() {
    let h = seq_harness().await;
    let mut tx = h.begin().await.unwrap();

    // Standard sequence with use_date_range = true
    let seq_id = create_seq(
        &mut tx,
        "Range Std",
        "range_std",
        "standard",
        Some("R-"),
        None,
        4,
        5,
        50,
        None,
        true,
    )
    .await;

    let env = tx.env();
    let n1 = sequence::next_by_code(
        &mut tx,
        &env,
        "range_std",
        Some(Date::from_ymd(2026, 4, 10).unwrap()),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(n1, "R-0050");

    let n2 = sequence::next_by_code(
        &mut tx,
        &env,
        "range_std",
        Some(Date::from_ymd(2026, 8, 10).unwrap()),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(n2, "R-0055");

    // Check ir_sequence_date_range record
    let rows = tx
        .query(
            "SELECT date_from, date_to FROM ir_sequence_date_range WHERE sequence_id = $1",
            &[&seq_id],
        )
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    let d_from: Date = rows[0].try_get(0).unwrap();
    let d_to: Date = rows[0].try_get(1).unwrap();
    assert_eq!(d_from, Date::from_ymd(2026, 1, 1).unwrap());
    assert_eq!(d_to, Date::from_ymd(2026, 12, 31).unwrap());

    // no_gap sequence with use_date_range = true
    let nogap_id = create_seq(
        &mut tx,
        "Range NoGap",
        "range_nogap",
        "no_gap",
        Some("NG-"),
        None,
        3,
        10,
        100,
        None,
        true,
    )
    .await;

    let ng1 = sequence::next_by_code(
        &mut tx,
        &env,
        "range_nogap",
        Some(Date::from_ymd(2026, 6, 1).unwrap()),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(ng1, "NG-100");

    let ng2 = sequence::next_by_code(
        &mut tx,
        &env,
        "range_nogap",
        Some(Date::from_ymd(2026, 6, 2).unwrap()),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(ng2, "NG-110");

    let ng_rows = tx
        .query(
            "SELECT number_next FROM ir_sequence_date_range WHERE sequence_id = $1",
            &[&nogap_id],
        )
        .await
        .unwrap();
    assert_eq!(ng_rows.len(), 1);
    let ng_next: i32 = ng_rows[0].try_get(0).unwrap();
    assert_eq!(ng_next, 120);

    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn test_use_date_range_clamping() {
    let h = seq_harness().await;
    let mut tx = h.begin().await.unwrap();

    let seq_id = create_seq(
        &mut tx,
        "Clamp Seq",
        "clamp_seq",
        "standard",
        None,
        None,
        0,
        1,
        1,
        None,
        true,
    )
    .await;

    // Pre-insert two date ranges: Jan 1..Mar 31 and Jul 1..Dec 31
    tx.execute(
        r#"
        INSERT INTO ir_sequence_date_range (sequence_id, date_from, date_to, number_next)
        VALUES ($1, $2, $3, 1), ($1, $4, $5, 1)
        "#,
        &[
            &seq_id,
            &Date::from_ymd(2026, 1, 1).unwrap(),
            &Date::from_ymd(2026, 3, 31).unwrap(),
            &Date::from_ymd(2026, 7, 1).unwrap(),
            &Date::from_ymd(2026, 12, 31).unwrap(),
        ],
    )
    .await
    .unwrap();

    let env = tx.env();
    // Draw for May 15: must be clamped to Apr 1..Jun 30
    let res = sequence::next_by_code(
        &mut tx,
        &env,
        "clamp_seq",
        Some(Date::from_ymd(2026, 5, 15).unwrap()),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(res, "1");

    let clamped_row = tx
        .query(
            r#"
            SELECT date_from, date_to
            FROM ir_sequence_date_range
            WHERE sequence_id = $1 AND date_from > $2 AND date_to < $3
            "#,
            &[
                &seq_id,
                &Date::from_ymd(2026, 3, 31).unwrap(),
                &Date::from_ymd(2026, 7, 1).unwrap(),
            ],
        )
        .await
        .unwrap();
    assert_eq!(clamped_row.len(), 1);
    let d_from: Date = clamped_row[0].try_get(0).unwrap();
    let d_to: Date = clamped_row[0].try_get(1).unwrap();
    assert_eq!(d_from, Date::from_ymd(2026, 4, 1).unwrap());
    assert_eq!(d_to, Date::from_ymd(2026, 6, 30).unwrap());

    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn test_use_date_range_concurrent_insert() {
    let h = seq_harness().await;

    // Setup sequence
    {
        let mut tx = h.begin().await.unwrap();
        create_seq(
            &mut tx,
            "Concurrent Range",
            "concurrent_range",
            "standard",
            None,
            None,
            0,
            1,
            1,
            None,
            true,
        )
        .await;
        tx.commit().await.unwrap();
    }

    let orm1 = h.orm().clone();
    let orm2 = h.orm().clone();

    let task1 = tokio::spawn(async move {
        orm1.run(erp_orm::SUPERUSER_ID, erp_orm::Context::new(), async |tx| {
            let env = tx.env();
            let res = sequence::next_by_code(
                tx,
                &env,
                "concurrent_range",
                Some(Date::from_ymd(2026, 8, 20).unwrap()),
            )
            .await?
            .ok_or_else(|| Error::user("missing sequence"))?;
            Ok(res)
        })
        .await
        .unwrap()
    });

    let task2 = tokio::spawn(async move {
        orm2.run(erp_orm::SUPERUSER_ID, erp_orm::Context::new(), async |tx| {
            let env = tx.env();
            let res = sequence::next_by_code(
                tx,
                &env,
                "concurrent_range",
                Some(Date::from_ymd(2026, 8, 20).unwrap()),
            )
            .await?
            .ok_or_else(|| Error::user("missing sequence"))?;
            Ok(res)
        })
        .await
        .unwrap()
    });

    let (r1, r2) = tokio::join!(task1, task2);
    let mut results = vec![r1.unwrap(), r2.unwrap()];
    results.sort();
    assert_eq!(results, vec!["1", "2"]);

    // Verify only one date range was inserted
    let mut tx = h.begin().await.unwrap();
    let rows = tx
        .query(
            "SELECT count(*) FROM ir_sequence_date_range WHERE date_from = $1",
            &[&Date::from_ymd(2026, 1, 1).unwrap()],
        )
        .await
        .unwrap();
    let count: i64 = rows[0].try_get(0).unwrap();
    assert_eq!(count, 1);

    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn test_use_date_range_concurrent_insert_40001_serialization_error() {
    let h = seq_harness().await;

    // 1. Setup sequence with use_date_range = true
    {
        let mut tx = h.begin().await.unwrap();
        create_seq(
            &mut tx,
            "Date Range 40001",
            "date_range_40001",
            "standard",
            None,
            None,
            0,
            1,
            1,
            None,
            true,
        )
        .await;
        tx.commit().await.unwrap();
    }

    // 2. Open tx2 and establish its REPEATABLE READ snapshot before tx1 creates the range.
    let mut tx2 = h.begin().await.unwrap();
    // Execute a query so tx2 establishes its transaction snapshot before tx1 creates the range.
    tx2.query("SELECT 1", &[]).await.unwrap();

    let target_date = Date::from_ymd(2026, 5, 10).unwrap();

    // 3. Open tx1, draw number to create and commit the date range.
    {
        let mut tx1 = h.begin().await.unwrap();
        let env1 = tx1.env();
        let res1 = sequence::next_by_code(&mut tx1, &env1, "date_range_40001", Some(target_date))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(res1, "1");
        tx1.commit().await.unwrap();
    }

    // 4. In tx2, whose snapshot predates tx1's commit, attempting to draw hits the
    //    speculative date range insert conflict and surfaces as a 40001 Concurrency error.
    let env2 = tx2.env();
    let err = sequence::next_by_code(&mut tx2, &env2, "date_range_40001", Some(target_date))
        .await
        .unwrap_err();

    assert_eq!(err.kind(), ErrorKind::Concurrency);
    assert_eq!(err.sqlstate(), Some("40001"));
    assert!(
        err.message().contains("could not serialize access"),
        "expected serialization failure message, got: {}",
        err.message()
    );

    // Roll back explicitly before retry so parent-row lock release is synchronous.
    tx2.rollback().await.unwrap();

    // 5. Run a draw through Orm::run (which retries on Concurrency / 40001) for a new date (2027).
    //    We force the first attempt into a stale-snapshot conflict to exercise the retry loop.
    let target_date_2027 = Date::from_ymd(2027, 5, 10).unwrap();
    let attempts = std::sync::atomic::AtomicU32::new(0);
    let orm = h.orm().clone();
    let res2 = orm
        .run(erp_orm::SUPERUSER_ID, erp_orm::Context::new(), async |tx| {
            let attempt = attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if attempt == 0 {
                // In attempt 0, take snapshot before another transaction creates the 2027 range.
                tx.query("SELECT 1", &[]).await?;
                let mut tx_other = h.begin().await.map_err(|e| Error::db(e.to_string()))?;
                let env_other = tx_other.env();
                let res_other = sequence::next_by_code(
                    &mut tx_other,
                    &env_other,
                    "date_range_40001",
                    Some(target_date_2027),
                )
                .await?
                .ok_or_else(|| Error::user("missing sequence"))?;
                assert_eq!(res_other, "1");
                tx_other
                    .commit()
                    .await
                    .map_err(|e| Error::db(e.to_string()))?;
            }
            let env = tx.env();
            let res = sequence::next_by_code(tx, &env, "date_range_40001", Some(target_date_2027))
                .await?
                .ok_or_else(|| Error::user("missing sequence"))?;
            Ok(res)
        })
        .await
        .unwrap();
    assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 2);
    assert_eq!(res2, "2");

    let mut tx_check = h.begin().await.unwrap();
    let rows = tx_check
        .query(
            "SELECT count(*) FROM ir_sequence_date_range WHERE sequence_id = (SELECT id FROM ir_sequence WHERE code = 'date_range_40001')",
            &[],
        )
        .await
        .unwrap();
    let count: i64 = rows[0].try_get(0).unwrap();
    assert_eq!(count, 2);
    drop(tx_check);

    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn test_use_date_range_primary_key_collision_23505_is_validation_error() {
    let h = seq_harness().await;

    // 1. Setup sequence with use_date_range = true
    {
        let mut tx = h.begin().await.unwrap();
        create_seq(
            &mut tx,
            "Date Range PK 23505",
            "date_range_pk_23505",
            "standard",
            None,
            None,
            0,
            1,
            1,
            None,
            true,
        )
        .await;
        tx.commit().await.unwrap();
    }

    // 2. Insert an existing date range record with an explicit id = 999.
    {
        let mut tx = h.begin().await.unwrap();
        let rows = tx
            .query(
                "SELECT id FROM ir_sequence WHERE code = 'date_range_pk_23505'",
                &[],
            )
            .await
            .unwrap();
        let seq_id: i64 = rows[0].try_get(0).unwrap();
        tx.execute(
            "INSERT INTO ir_sequence_date_range (id, sequence_id, date_from, date_to, number_next) \
             VALUES (999, $1, '2020-01-01', '2020-12-31', 1)",
            &[&seq_id],
        )
        .await
        .unwrap();

        // Move the id sequence behind the existing row so the next insert attempts id 999.
        tx.execute(
            "SELECT setval(pg_get_serial_sequence('ir_sequence_date_range', 'id'), 999, false)",
            &[],
        )
        .await
        .unwrap();

        tx.commit().await.unwrap();
    }

    // 3. Draw for a date in 2026, which attempts to auto-create a 2026 date range.
    // The insert will draw id 999 from the sequence and collide with the existing primary key (23505).
    // This must surface as a Validation error without retry.
    let target_date = Date::from_ymd(2026, 6, 1).unwrap();
    let mut tx = h.begin().await.unwrap();
    let env = tx.env();
    let err = sequence::next_by_code(&mut tx, &env, "date_range_pk_23505", Some(target_date))
        .await
        .unwrap_err();

    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(err.sqlstate(), Some("23505"));
    assert!(
        err.message()
            .starts_with("The database refused the change:"),
        "expected Validation message prefix, got: {}",
        err.message()
    );

    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn test_padding_bounds() {
    let h = seq_harness().await;
    let mut tx = h.begin().await.unwrap();

    create_seq(
        &mut tx,
        "Overpadded",
        "overpadded",
        "standard",
        None,
        None,
        65,
        1,
        1,
        None,
        false,
    )
    .await;

    let env = tx.env();
    let err = sequence::next_by_code(&mut tx, &env, "overpadded", None)
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.message(),
        "Sequence padding must be between 0 and 64, got 65"
    );

    drop(tx);
    h.cleanup().await.unwrap();
}

/// The lock statement of a gapless draw in `lock_timeout` mode.
const SEQ_LOCK_SQL: &str = "SELECT number_next FROM ir_sequence WHERE id = $1 FOR UPDATE";

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

/// A gapless sequence with `wait_mode`, committed; returns its id.
async fn gapless(
    h: &Harness,
    name: &str,
    code: &str,
    wait_mode: Option<&str>,
    ranges: bool,
) -> i64 {
    let mut tx = h.begin().await.unwrap();
    let id = create_seq_with_wait_mode(
        &mut tx, name, code, "no_gap", None, None, 0, 1, 1, None, ranges, wait_mode,
    )
    .await;
    tx.commit().await.unwrap();
    id
}

/// Wait until another session of this test database waits for a lock in statement `sql` (polled
/// in fresh transactions: `pg_stat_activity` is read once per transaction). Replaces a sleep: the
/// holder commits only once the draw is known to be blocked in its `FOR UPDATE`.
async fn wait_for_lock_waiter(h: &Harness, sql: &str) {
    for _ in 0..4000 {
        let mut tx = h.begin().await.unwrap();
        let rows = tx
            .query(
                "SELECT count(*) FROM pg_stat_activity WHERE datname = current_database() \
                 AND pid <> pg_backend_pid() AND wait_event_type = 'Lock' AND query = $1",
                &[&sql],
            )
            .await
            .unwrap();
        let waiting: i64 = rows[0].get(0);
        tx.rollback().await.unwrap();
        if waiting > 0 {
            return;
        }
        tokio::time::sleep(ms(5)).await;
    }
    panic!("no session waits for a lock in {sql:?}");
}

#[tokio::test]
async fn spec_seq_008_lock_timeout_waits_then_the_unit_of_work_retries() {
    let h = seq_harness().await;
    gapless(
        &h,
        "LockTimeout Retry",
        "seq_008",
        Some("lock_timeout"),
        false,
    )
    .await;

    // A draws 1 and holds the row lock
    let mut holder = h.begin().await.unwrap();
    let env = holder.env();
    let first = sequence::next_by_code(&mut holder, &env, "seq_008", None)
        .await
        .unwrap();
    assert_eq!(first.as_deref(), Some("1"));

    // B waits in FOR UPDATE; A commits; B's statement fails with 40001 (REPEATABLE READ: the row
    // changed after B's snapshot) and Orm::run runs B again, which draws 2
    let attempts = Mutex::new(Vec::new());
    let orm = h.orm().clone();
    let (drawn, ()) = tokio::join!(
        orm.run(erp_orm::SUPERUSER_ID, erp_orm::Context::new(), async |tx| {
            let env = tx.env();
            let before = tx.query_count();
            let result = sequence::next_by_code(tx, &env, "seq_008", None).await;
            let outcome = match &result {
                Ok(_) => "drawn".to_owned(),
                Err(e) => format!("{:?}/{}", e.kind(), e.sqlstate().unwrap_or("-")),
            };
            attempts
                .lock()
                .unwrap()
                .push((outcome, tx.query_count() - before));
            result?.ok_or_else(|| Error::user("missing sequence"))
        }),
        async {
            wait_for_lock_waiter(&h, SEQ_LOCK_SQL).await;
            holder.commit().await.unwrap();
        }
    );
    assert_eq!(drawn.unwrap(), "2");
    // attempt 1: code lookup + lock (fails after the wait); attempt 2: lookup + lock + UPDATE
    assert_eq!(
        attempts.into_inner().unwrap(),
        vec![("Concurrency/40001".to_owned(), 2), ("drawn".to_owned(), 3)]
    );

    // On a bare transaction the same wait ends in the Concurrency error itself
    let mut holder = h.begin().await.unwrap();
    let env = holder.env();
    let third = sequence::next_by_code(&mut holder, &env, "seq_008", None)
        .await
        .unwrap();
    assert_eq!(third.as_deref(), Some("3"));
    let mut waiter = h.begin().await.unwrap();
    let env = waiter.env();
    let (result, ()) = tokio::join!(
        sequence::next_by_code(&mut waiter, &env, "seq_008", None),
        async {
            wait_for_lock_waiter(&h, SEQ_LOCK_SQL).await;
            holder.commit().await.unwrap();
        }
    );
    let err = result.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Concurrency);
    assert_eq!(err.sqlstate(), Some("40001"));
    assert_eq!(err.condition(), None);
    assert_eq!(
        err.message(),
        "could not serialize access due to concurrent update"
    );
    waiter.rollback().await.unwrap();

    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_seq_009_default_nowait_mode_preserves_immediate_55p03() {
    let h = seq_harness().await;
    // no wait_mode given: the field default
    gapless(&h, "Default NoWait", "seq_009", None, false).await;

    let mut tx1 = h.begin().await.unwrap();
    let env1 = tx1.env();
    let num1 = sequence::next_by_code(&mut tx1, &env1, "seq_009", None)
        .await
        .unwrap();
    assert_eq!(num1, Some("1".to_string()));

    let mut tx2 = h.begin().await.unwrap();
    let env2 = tx2.env();
    // code lookup + FOR UPDATE NOWAIT (refused at once)
    let err = assert_query_count!(tx2, >= 2, <= 2, {
        sequence::next_by_code(&mut tx2, &env2, "seq_009", None)
            .await
            .unwrap_err()
    });
    assert_eq!(err.kind(), ErrorKind::Lock);
    assert_eq!(err.message(), "could not obtain lock on no_gap sequence");
    assert_eq!(err.sqlstate(), Some("55P03"));
    // a NOWAIT refusal is not a lock timeout
    assert_eq!(err.condition(), None);

    drop(tx1);
    drop(tx2);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_seq_009_relation_lock_timeout_under_nowait() {
    let h = seq_harness().await;
    let seq_id = gapless(
        &h,
        "NoWait Timeout",
        "seq_009_timeout",
        Some("nowait"),
        false,
    )
    .await;
    let own = Timeouts::new(ms(100), ms(30_000), ms(60_000)).unwrap();

    let mut holder = h.begin().await.unwrap();
    // EXCLUSIVE MODE conflicts with ROW SHARE (requested by FOR UPDATE NOWAIT) but permits
    // ACCESS SHARE (requested by the preceding sequence lookup query), ensuring the relation-lock
    // timeout trips during lock_number_next.
    holder
        .execute("LOCK TABLE ir_sequence IN EXCLUSIVE MODE", &[])
        .await
        .unwrap();
    // Verify the holder before drawing. Sampling the waiter's 100 ms lock wait can miss it
    // entirely under scheduler contention; the typed timeout below proves that wait completed.
    let held = holder
        .query(
            "SELECT granted FROM pg_locks WHERE pid = pg_backend_pid() \
             AND relation = 'ir_sequence'::regclass AND mode = 'ExclusiveLock'",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(held.len(), 1);
    assert!(held[0].get::<_, bool>(0));

    let mut waiter = h
        .orm()
        .begin_opts(BeginOptions::new(erp_orm::SUPERUSER_ID).timeouts(own))
        .await
        .unwrap();
    let env = waiter.env();
    let started = Instant::now();
    let result = sequence::next_by_code(&mut waiter, &env, "seq_009_timeout", None).await;
    assert!(
        started.elapsed() >= ms(100),
        "returned after {:?}",
        started.elapsed()
    );
    let err = result.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Lock);
    assert_eq!(err.sqlstate(), Some("55P03"));
    assert_eq!(err.condition(), Some(Condition::LockTimeout));
    assert_eq!(
        err.message(),
        format!(
            "could not obtain the lock on no_gap sequence “NoWait Timeout” (id {seq_id}) \
             within lock_timeout: canceling statement due to lock timeout (lock_timeout was 100 \
             ms when the transaction began)"
        )
    );
    let source = std::error::Error::source(&err).expect("source");
    assert!(
        source
            .to_string()
            .starts_with("canceling statement due to lock timeout")
    );

    waiter.rollback().await.unwrap();
    holder.rollback().await.unwrap();
    drop(waiter);
    drop(holder);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_seq_010_lock_timeout_gives_the_typed_timeout_naming_the_sequence() {
    let h = seq_harness().await;
    let seq_id = gapless(
        &h,
        "LockTimeout Abort",
        "seq_010",
        Some("lock_timeout"),
        false,
    )
    .await;
    let own = Timeouts::new(ms(100), ms(30_000), ms(60_000)).unwrap();

    let mut holder = h.begin().await.unwrap();
    let env = holder.env();
    let first = sequence::next_by_code(&mut holder, &env, "seq_010", None)
        .await
        .unwrap();
    assert_eq!(first.as_deref(), Some("1"));

    let mut waiter = h
        .orm()
        .begin_opts(BeginOptions::new(erp_orm::SUPERUSER_ID).timeouts(own))
        .await
        .unwrap();
    let env = waiter.env();
    let started = Instant::now();
    // code lookup + FOR UPDATE (cancelled by lock_timeout)
    let err = assert_query_count!(waiter, >= 2, <= 2, {
        sequence::next_by_code(&mut waiter, &env, "seq_010", None)
            .await
            .unwrap_err()
    });
    assert!(
        started.elapsed() >= ms(100),
        "returned after {:?}",
        started.elapsed()
    );
    assert_eq!(err.kind(), ErrorKind::Lock);
    assert_eq!(err.sqlstate(), Some("55P03"));
    assert_eq!(err.condition(), Some(Condition::LockTimeout));
    assert_eq!(
        err.message(),
        format!(
            "could not obtain the lock on no_gap sequence “LockTimeout Abort” (id {seq_id}) \
             within lock_timeout: canceling statement due to lock timeout (lock_timeout was 100 \
             ms when the transaction began)"
        )
    );
    // the transaction layer's error stays the source
    let source = std::error::Error::source(&err).expect("source");
    assert!(
        source
            .to_string()
            .starts_with("canceling statement due to lock timeout")
    );
    waiter.rollback().await.unwrap();
    holder.rollback().await.unwrap();

    // the same on a date range row
    let range_seq = gapless(
        &h,
        "LockTimeout Range",
        "seq_010_range",
        Some("lock_timeout"),
        true,
    )
    .await;
    let day = Date::from_ymd(2026, 3, 15).unwrap();
    {
        let mut tx = h.begin().await.unwrap();
        let env = tx.env();
        let n = sequence::next_by_code(&mut tx, &env, "seq_010_range", Some(day))
            .await
            .unwrap();
        assert_eq!(n.as_deref(), Some("1"));
        tx.commit().await.unwrap();
    }
    let range_id: i64 = {
        let mut tx = h.begin().await.unwrap();
        let rows = tx
            .query(
                "SELECT id FROM ir_sequence_date_range WHERE sequence_id = $1",
                &[&range_seq],
            )
            .await
            .unwrap();
        tx.rollback().await.unwrap();
        rows[0].get(0)
    };
    let mut holder = h.begin().await.unwrap();
    let env = holder.env();
    let second = sequence::next_by_code(&mut holder, &env, "seq_010_range", Some(day))
        .await
        .unwrap();
    assert_eq!(second.as_deref(), Some("2"));
    let mut waiter = h
        .orm()
        .begin_opts(BeginOptions::new(erp_orm::SUPERUSER_ID).timeouts(own))
        .await
        .unwrap();
    let env = waiter.env();
    let err = sequence::next_by_code(&mut waiter, &env, "seq_010_range", Some(day))
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Lock);
    assert_eq!(err.condition(), Some(Condition::LockTimeout));
    assert_eq!(
        err.message(),
        format!(
            "could not obtain the lock on no_gap sequence date range “LockTimeout Range” (id \
             {range_seq}, date range {range_id}) within lock_timeout: canceling statement due to \
             lock timeout (lock_timeout was 100 ms when the transaction began)"
        )
    );
    waiter.rollback().await.unwrap();
    holder.rollback().await.unwrap();

    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_seq_014_unknown_wait_mode_is_an_error() {
    let h = seq_harness().await;
    let seq_id = gapless(&h, "Bad Mode", "seq_014", Some("lock_timeout"), false).await;
    {
        // a value no selection allows, written past the ORM (raw SQL, a data file of a later version)
        let mut tx = h.begin().await.unwrap();
        tx.execute(
            "UPDATE ir_sequence SET wait_mode = 'lock_timout' WHERE id = $1",
            &[&seq_id],
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
    }
    let mut tx = h.begin().await.unwrap();
    let env = tx.env();
    for err in [
        sequence::next_by_code(&mut tx, &env, "seq_014", None)
            .await
            .unwrap_err(),
        sequence::next_by_id(&mut tx, &env, seq_id, None)
            .await
            .unwrap_err(),
    ] {
        assert_eq!(err.kind(), ErrorKind::Validation);
        assert_eq!(
            err.message(),
            format!(
                "ir.sequence {seq_id} (“Bad Mode”) has wait_mode \"lock_timout\"; expected \
                 'nowait' or 'lock_timeout'"
            )
        );
    }
    tx.rollback().await.unwrap();
    drop(tx);

    // A NULL wait_mode column (pre-migration row or raw SQL) resolves via COALESCE to 'nowait'.
    {
        let mut tx = h.begin().await.unwrap();
        tx.execute(
            "UPDATE ir_sequence SET wait_mode = NULL WHERE id = $1",
            &[&seq_id],
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
    }
    let mut tx1 = h.begin().await.unwrap();
    let env1 = tx1.env();
    let num1 = sequence::next_by_code(&mut tx1, &env1, "seq_014", None)
        .await
        .unwrap();
    assert_eq!(num1, Some("1".to_string()));

    let mut tx2 = h.begin().await.unwrap();
    let env2 = tx2.env();
    let err = sequence::next_by_code(&mut tx2, &env2, "seq_014", None)
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Lock);
    assert_eq!(err.message(), "could not obtain lock on no_gap sequence");
    assert_eq!(err.sqlstate(), Some("55P03"));
    assert_eq!(err.condition(), None);

    // Await server rollback before the next NOWAIT phase; Drop only closes the connection.
    tx1.rollback().await.unwrap();
    tx2.rollback().await.unwrap();
    drop(tx1);
    drop(tx2);

    // The same COALESCE(wait_mode, 'nowait') resolution exercised via next_by_id (L2)
    let mut tx3 = h.begin().await.unwrap();
    let env3 = tx3.env();
    let num2 = sequence::next_by_id(&mut tx3, &env3, seq_id, None)
        .await
        .unwrap();
    assert_eq!(num2, Some("1".to_string()));

    let mut tx4 = h.begin().await.unwrap();
    let env4 = tx4.env();
    let err = sequence::next_by_id(&mut tx4, &env4, seq_id, None)
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Lock);
    assert_eq!(err.message(), "could not obtain lock on no_gap sequence");
    assert_eq!(err.sqlstate(), Some("55P03"));
    assert_eq!(err.condition(), None);

    tx3.rollback().await.unwrap();
    tx4.rollback().await.unwrap();
    drop(tx3);
    drop(tx4);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn test_next_by_id_and_wait_mode_overrides() {
    let h = seq_harness().await;
    let seq_id = {
        let mut tx = h.begin().await.unwrap();
        let id = create_seq_with_wait_mode(
            &mut tx,
            "By ID Test",
            "test_by_id",
            "no_gap",
            Some("SEQ/"),
            None,
            4,
            1,
            1,
            None,
            false,
            Some("nowait"),
        )
        .await;
        tx.commit().await.unwrap();
        id
    };

    let mut tx = h.begin().await.unwrap();
    let env = tx.env();
    // next_by_id
    let res1 = sequence::next_by_id(&mut tx, &env, seq_id, None)
        .await
        .unwrap();
    assert_eq!(res1, Some("SEQ/0001".to_string()));

    // next_by_id_with_wait_mode
    let res2 = sequence::next_by_id_with_wait_mode(
        &mut tx,
        &env,
        seq_id,
        None,
        erp_orm::sequence::SequenceWaitMode::LockTimeout,
    )
    .await
    .unwrap();
    assert_eq!(res2, Some("SEQ/0002".to_string()));

    // next_by_code_with_wait_mode
    let res3 = sequence::next_by_code_with_wait_mode(
        &mut tx,
        &env,
        "test_by_id",
        None,
        erp_orm::sequence::SequenceWaitMode::LockTimeout,
    )
    .await
    .unwrap();
    assert_eq!(res3, Some("SEQ/0003".to_string()));

    drop(tx);
    h.cleanup().await.unwrap();
}
