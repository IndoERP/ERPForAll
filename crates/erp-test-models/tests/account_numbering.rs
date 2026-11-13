// SPDX-License-Identifier: MIT OR Apache-2.0
//! Journal entry numbering: the chain rows of `account` on independent
//! database connections, numbering the `test_numbering` stand-in until `account.move` exists
//! (wires the same calls into posting). Functional tests, not performance tests.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use erp_addon_account::numbering::{
    AssignedNumber, LOCK_WAIT_SETTING, LockWait, NumberRequest, assign_numbers,
};
use erp_sdk::prelude::*;
use erp_sdk::sequence::{FiscalYear, SequencePattern, SequenceReset};
use erp_sdk::{Condition, ErrorKind};
use erp_testkit::{Harness, HarnessError};

fn d(y: i32, m: u32, day: u32) -> Date {
    Date::from_ymd(y, m, day).unwrap()
}

fn pattern(reset: SequenceReset, prefix: &str, padding: usize) -> SequencePattern {
    SequencePattern::builder(reset)
        .prefix(prefix)
        .padding(padding)
        .build()
        .unwrap()
}

fn invoices() -> SequencePattern {
    pattern(SequenceReset::Year, "INV/", 5)
}

/// A harness with `account` and the stand-in, and one company.
async fn harness() -> (Harness, i64) {
    let h = Harness::builder([
        &erp_addon_base::ADDON,
        &erp_addon_uom::ADDON,
        &erp_addon_product::ADDON,
        &erp_addon_analytic::ADDON,
        &erp_addon_account::ADDON,
        &erp_test_models::test_numbering::ADDON,
    ])
    .build()
    .await
    .unwrap();
    let mut tx = h.begin().await.unwrap();
    let currency = tx
        .env()
        .records("res.currency")
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("USD".into()))
                    .set_cell("symbol", Cell::Str("$".into()))
                    .set_cell("rounding", Cell::Decimal(Decimal::new(1, 2))),
            ],
        )
        .await
        .unwrap()
        .rec_ids()[0]
        .get();
    let company = tx
        .env()
        .records("res.company")
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Numbering Co".into()))
                    .set_cell("currency_id", Cell::Id(currency)),
            ],
        )
        .await
        .unwrap()
        .rec_ids()[0]
        .get();
    tx.commit().await.unwrap();
    (h, company)
}

/// Committed drafts `(journal, date, reference)`, in order.
async fn drafts(h: &Harness, company: i64, rows: &[(i64, Date, Option<&str>)]) -> Vec<i64> {
    let mut tx = h.begin().await.unwrap();
    let vals = rows
        .iter()
        .map(|(journal, date, reference)| {
            let vals = RawVals::new()
                .set_cell("company_id", Cell::Id(company))
                .set_cell("journal_id", Cell::Int(i32::try_from(*journal).unwrap()))
                .set_cell("date", Cell::Date(*date));
            match reference {
                Some(r) => vals.set_cell("reference", Cell::Str((*r).into())),
                None => vals,
            }
        })
        .collect();
    let ids = tx
        .env()
        .records("test_numbering.document")
        .unwrap()
        .create(&mut tx, vals)
        .await
        .unwrap()
        .rec_ids()
        .iter()
        .map(|id| id.get())
        .collect();
    tx.commit().await.unwrap();
    ids
}

/// What the posting step does with the service: read the documents, number them, write the
/// numbers (3 step 7a).
async fn number(
    tx: &mut Transaction,
    wait: LockWait,
    docs: &[(i64, &str, &SequencePattern)],
) -> erp_sdk::Result<Vec<AssignedNumber>> {
    let ids: Vec<i64> = docs.iter().map(|(id, _, _)| *id).collect();
    let rows = tx
        .query(
            "SELECT id::int8, company_id::int8, journal_id::int8, date, reference \
             FROM test_numbering_document WHERE id = ANY($1)",
            &[&ids],
        )
        .await?;
    let read: Vec<(i64, i64, i64, Date, Option<String>)> = rows
        .iter()
        .map(|r| (r.get(0), r.get(1), r.get(2), r.get(3), r.get(4)))
        .collect();
    let requests: Vec<NumberRequest<'_>> = docs
        .iter()
        .map(|(id, kind, pattern)| {
            let (_, company, journal, date, reference) =
                read.iter().find(|row| row.0 == *id).expect("document read");
            NumberRequest {
                document_id: *id,
                company_id: *company,
                journal_id: *journal,
                kind,
                pattern,
                date: *date,
                fiscal_year: None,
                reference: reference.as_deref(),
            }
        })
        .collect();
    let assigned = assign_numbers(tx, wait, &requests).await?;
    let ids: Vec<i64> = assigned.iter().map(|a| a.document_id).collect();
    let names: Vec<&str> = assigned.iter().map(|a| a.name.as_str()).collect();
    let chains: Vec<i64> = assigned.iter().map(|a| a.chain_id).collect();
    let numbers: Vec<i32> = assigned
        .iter()
        .map(|a| i32::try_from(a.sequence_number).unwrap())
        .collect();
    tx.execute(
        "UPDATE test_numbering_document AS d SET name = v.name, chain_id = v.chain, \
         sequence_number = v.n FROM unnest($1::int8[], $2::text[], $3::int8[], $4::int4[]) \
         AS v(id, name, chain, n) WHERE d.id = v.id",
        &[&ids, &names, &chains, &numbers],
    )
    .await?;
    Ok(assigned)
}

/// The committed names of `ids`, in that order.
async fn names(h: &Harness, ids: &[i64]) -> Vec<Option<String>> {
    let client = h.db().client().await.unwrap();
    let mut out = Vec::new();
    for id in ids {
        let row = client
            .query_one(
                "SELECT name FROM test_numbering_document WHERE id = $1",
                &[id],
            )
            .await
            .unwrap();
        out.push(row.get(0));
    }
    out
}

/// `next_number` of every chain row, by `(journal_id, kind, period_key)`.
async fn counters(h: &Harness) -> Vec<(i64, String, String, i32)> {
    let client = h.db().client().await.unwrap();
    client
        .query(
            "SELECT journal_id::int8, kind, period_key, next_number FROM account_sequence_chain \
             ORDER BY journal_id, kind, period_key",
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| (r.get(0), r.get(1), r.get(2), r.get(3)))
        .collect()
}

/// Wait until some session of the test database waits for a lock.
async fn until_a_session_waits(h: &Harness) {
    let client = h.db().client().await.unwrap();
    let started = Instant::now();
    loop {
        let waiting: i64 = client
            .query_one(
                "SELECT count(*) FROM pg_stat_activity \
                 WHERE datname = current_database() AND wait_event_type = 'Lock'",
                &[],
            )
            .await
            .unwrap()
            .get(0);
        if waiting > 0 {
            return;
        }
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "no session waited for a lock"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn wait() -> LockWait {
    LockWait::from_millis(10_000).unwrap()
}

#[tokio::test]
async fn a_concurrent_allocation_waits_and_takes_the_next_number() {
    let (h, company) = harness().await;
    let docs = drafts(
        &h,
        company,
        &[(1, d(2026, 3, 1), None), (1, d(2026, 3, 2), None)],
    )
    .await;
    let inv = invoices();

    let mut a = h.begin().await.unwrap();
    let first = number(&mut a, wait(), &[(docs[0], "document", &inv)])
        .await
        .unwrap();
    assert_eq!(first[0].name, "INV/2026/00001");
    assert_eq!(first[0].sequence_number, 1);

    // the chain row is new: the second poster waits on the first one's insert, then retries
    let attempts = AtomicUsize::new(0);
    let second = h.run(async |tx| {
        attempts.fetch_add(1, Ordering::SeqCst);
        number(tx, wait(), &[(docs[1], "document", &inv)]).await
    });
    let release = async {
        until_a_session_waits(&h).await;
        a.commit().await.unwrap();
    };
    let (second, ()) = tokio::join!(second, release);
    let second = second.unwrap();

    assert_eq!(second[0].name, "INV/2026/00002");
    assert_eq!(second[0].sequence_number, 2);
    assert_eq!(second[0].chain_id, first[0].chain_id);
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        2,
        "the waiting attempt fails with 40001 after the commit and the next one numbers"
    );
    assert_eq!(
        names(&h, &docs).await,
        [Some("INV/2026/00001".into()), Some("INV/2026/00002".into())]
    );
    assert_eq!(
        counters(&h).await,
        [(1, "document".into(), "2026".into(), 3)]
    );
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn a_rolled_back_allocation_leaves_no_gap() {
    let (h, company) = harness().await;
    let docs = drafts(
        &h,
        company,
        &[(1, d(2026, 3, 1), None), (1, d(2026, 3, 2), None)],
    )
    .await;
    let inv = invoices();

    let mut a = h.begin().await.unwrap();
    let taken = number(&mut a, wait(), &[(docs[0], "document", &inv)])
        .await
        .unwrap();
    assert_eq!(taken[0].name, "INV/2026/00001");
    a.rollback().await.unwrap();

    let mut b = h.begin().await.unwrap();
    let again = number(&mut b, wait(), &[(docs[1], "document", &inv)])
        .await
        .unwrap();
    b.commit().await.unwrap();
    assert_eq!(again[0].name, "INV/2026/00001");
    assert_eq!(
        names(&h, &docs).await,
        [None, Some("INV/2026/00001".into())]
    );
    assert_eq!(
        counters(&h).await,
        [(1, "document".into(), "2026".into(), 2)]
    );
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn a_savepoint_rollback_gives_the_same_number_again() {
    let (h, company) = harness().await;
    let docs = drafts(&h, company, &[(1, d(2026, 3, 1), None)]).await;
    let inv = invoices();

    let mut tx = h.begin().await.unwrap();
    let inner = tx
        .savepoint(async |tx| {
            let taken = number(tx, wait(), &[(docs[0], "document", &inv)]).await?;
            assert_eq!(taken[0].name, "INV/2026/00001");
            Err::<(), _>(Error::validation("posting check failed"))
        })
        .await;
    assert_eq!(inner.unwrap_err().message(), "posting check failed");
    let again = number(&mut tx, wait(), &[(docs[0], "document", &inv)])
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(again[0].name, "INV/2026/00001");
    assert_eq!(
        counters(&h).await,
        [(1, "document".into(), "2026".into(), 2)]
    );
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn a_duplicate_is_refused_by_the_database() {
    let (h, company) = harness().await;
    let docs = drafts(
        &h,
        company,
        &[(1, d(2026, 3, 1), None), (1, d(2026, 3, 2), None)],
    )
    .await;
    let inv = invoices();

    let mut a = h.begin().await.unwrap();
    let taken = number(&mut a, wait(), &[(docs[0], "document", &inv)])
        .await
        .unwrap();

    // a writer that bypasses the service and reuses the number waits for the holder's index
    // entry, then the database refuses it
    let other = h.db().client().await.unwrap();
    let reused = i32::try_from(taken[0].sequence_number).unwrap();
    let params: [&(dyn erp_testkit::tokio_postgres::types::ToSql + Sync); 4] =
        [&docs[1], &"INV/2026/X", &taken[0].chain_id, &reused];
    let duplicate = other.execute(
        "UPDATE test_numbering_document SET name = $2, chain_id = $3, sequence_number = $4 \
         WHERE id = $1",
        &params,
    );
    let release = async {
        until_a_session_waits(&h).await;
        a.commit().await.unwrap();
    };
    let (duplicate, ()) = tokio::join!(duplicate, release);
    let error = duplicate.unwrap_err();
    assert_eq!(error.code().unwrap().code(), "23505");
    assert_eq!(
        error.as_db_error().unwrap().constraint(),
        Some("test_numbering_document_number_unique")
    );

    // a second chain row for the same journal, kind and period
    let error = other
        .execute(
            "INSERT INTO account_sequence_chain (company_id, journal_id, kind, period_key, \
             next_number) VALUES ($1, 1, 'document', '2026', 7)",
            &[&company],
        )
        .await
        .unwrap_err();
    assert_eq!(error.code().unwrap().code(), "23505");
    assert_eq!(
        error.as_db_error().unwrap().constraint(),
        Some("account_sequence_chain_chain_key_unique")
    );
    assert_eq!(
        names(&h, &docs).await,
        [Some("INV/2026/00001".into()), None]
    );
    h.cleanup().await.unwrap();
}

/// retried; the posting wiring is)
#[tokio::test]
async fn a_chain_held_past_the_wait_is_refused_without_retry() {
    let (h, company) = harness().await;
    let docs = drafts(
        &h,
        company,
        &[
            (1, d(2026, 3, 1), None),
            (1, d(2026, 3, 2), None),
            (1, d(2026, 3, 3), None),
        ],
    )
    .await;
    let inv = invoices();
    let mut tx = h.begin().await.unwrap();
    number(&mut tx, wait(), &[(docs[0], "document", &inv)])
        .await
        .unwrap();
    tx.commit().await.unwrap();

    let mut holder = h.begin().await.unwrap();
    number(&mut holder, wait(), &[(docs[1], "document", &inv)])
        .await
        .unwrap();

    let floor = LockWait::from_millis(100).unwrap();
    let attempts = AtomicUsize::new(0);
    let started = Instant::now();
    let refused = h
        .run(async |tx| {
            attempts.fetch_add(1, Ordering::SeqCst);
            number(tx, floor, &[(docs[2], "document", &inv)]).await
        })
        .await;
    let HarnessError::Orm(error) = refused.unwrap_err() else {
        panic!("expected the ORM error");
    };
    assert_eq!(error.kind(), ErrorKind::Lock);
    assert_eq!(error.condition(), Some(Condition::LockTimeout));
    assert_eq!(error.sqlstate(), Some("55P03"));
    assert!(
        error
            .message()
            .contains("number chain (journal 1, kind document, period 2026)"),
        "{}",
        error.message()
    );
    assert!(
        error.message().contains(LOCK_WAIT_SETTING),
        "{}",
        error.message()
    );
    assert!(error.message().contains("100 ms"), "{}", error.message());
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    assert!(started.elapsed() < Duration::from_secs(5));

    holder.commit().await.unwrap();
    let mut tx = h.begin().await.unwrap();
    let next = number(&mut tx, floor, &[(docs[2], "document", &inv)])
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(next[0].name, "INV/2026/00003");
    h.cleanup().await.unwrap();
}

/// chain lock; once another posting numbered, it must run again)
#[tokio::test]
async fn work_before_the_numbering_step_blocks_no_other_poster() {
    let (h, company) = harness().await;
    let docs = drafts(
        &h,
        company,
        &[(1, d(2026, 3, 1), None), (1, d(2026, 3, 2), None)],
    )
    .await;
    let inv = invoices();
    let mut tx = h.begin().await.unwrap();
    number(&mut tx, wait(), &[(docs[0], "document", &inv)])
        .await
        .unwrap();
    tx.commit().await.unwrap();

    // `a` is busy with its posting (its snapshot is taken) but has not numbered yet
    let mut a = h.begin().await.unwrap();
    a.query("SELECT count(*) FROM test_numbering_document", &[])
        .await
        .unwrap();

    let started = Instant::now();
    let mut b = h.begin().await.unwrap();
    let floor = LockWait::from_millis(100).unwrap();
    let taken = number(&mut b, floor, &[(docs[1], "document", &inv)])
        .await
        .unwrap();
    b.commit().await.unwrap();
    assert_eq!(taken[0].name, "INV/2026/00002");
    assert!(started.elapsed() < Duration::from_secs(5));

    // the chain moved after `a`'s snapshot: its numbering step asks for a new attempt
    let error = number(&mut a, wait(), &[(docs[1], "document", &inv)])
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Concurrency);
    assert_eq!(error.sqlstate(), Some("40001"));
    a.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn a_batch_is_numbered_per_chain_in_date_reference_id_order() {
    let (h, company) = harness().await;
    let docs = drafts(
        &h,
        company,
        &[
            (1, d(2026, 3, 5), None),
            (1, d(2026, 3, 6), None),
            (1, d(2026, 3, 4), None),
            (1, d(2026, 3, 5), Some("B")),
            (1, d(2026, 3, 5), Some("A")),
            (1, d(2027, 1, 2), None),
            (2, d(2026, 3, 1), None),
            (2, d(2026, 4, 1), None),
            (1, d(2026, 3, 7), None),
            (1, d(2026, 3, 8), None),
        ],
    )
    .await;
    let inv = invoices();
    let refunds = pattern(SequenceReset::Year, "RINV/", 5);
    let payments = pattern(SequenceReset::Year, "PINV/", 5);
    let misc = pattern(SequenceReset::Month, "MISC/", 4);
    let batch = [
        (docs[0], "document", &inv),
        (docs[1], "document", &inv),
        (docs[2], "document", &inv),
        (docs[3], "document", &inv),
        (docs[4], "document", &inv),
        (docs[5], "document", &inv),
        (docs[6], "document", &misc),
        (docs[7], "document", &misc),
        (docs[8], "refund", &refunds),
        (docs[9], "payment", &payments),
    ];
    let mut tx = h.begin().await.unwrap();
    let assigned = number(&mut tx, wait(), &batch).await.unwrap();
    tx.commit().await.unwrap();

    // returned in request order
    let returned: Vec<i64> = assigned.iter().map(|a| a.document_id).collect();
    assert_eq!(returned, docs);
    assert_eq!(
        names(&h, &docs).await,
        [
            // 03-05 without reference sorts before 'A' and 'B'
            Some("INV/2026/00002".into()),
            Some("INV/2026/00005".into()),
            Some("INV/2026/00001".into()),
            Some("INV/2026/00004".into()),
            Some("INV/2026/00003".into()),
            Some("INV/2027/00001".into()),
            Some("MISC/2026/03/0001".into()),
            Some("MISC/2026/04/0001".into()),
            Some("RINV/2026/00001".into()),
            Some("PINV/2026/00001".into()),
        ]
    );
    assert_eq!(
        counters(&h).await,
        [
            (1, "document".into(), "2026".into(), 6),
            (1, "document".into(), "2027".into(), 2),
            (1, "payment".into(), "2026".into(), 2),
            (1, "refund".into(), "2026".into(), 2),
            (2, "document".into(), "2026-03".into(), 2),
            (2, "document".into(), "2026-04".into(), 2),
        ]
    );
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn a_fiscal_year_pattern_uses_the_fiscal_year_given_per_document() {
    let (h, company) = harness().await;
    let docs = drafts(
        &h,
        company,
        &[(1, d(2026, 3, 15), None), (1, d(2026, 4, 15), None)],
    )
    .await;
    let fiscal = pattern(SequenceReset::YearRange, "INV/", 4);
    let fy26 = FiscalYear::new(d(2025, 4, 1), d(2026, 3, 31)).unwrap();
    let fy27 = FiscalYear::new(d(2026, 4, 1), d(2027, 3, 31)).unwrap();
    let mut tx = h.begin().await.unwrap();
    let requests = [
        NumberRequest {
            document_id: docs[0],
            company_id: company,
            journal_id: 1,
            kind: "document",
            pattern: &fiscal,
            date: d(2026, 3, 15),
            fiscal_year: Some(fy26),
            reference: None,
        },
        NumberRequest {
            document_id: docs[1],
            company_id: company,
            journal_id: 1,
            kind: "document",
            pattern: &fiscal,
            date: d(2026, 4, 15),
            fiscal_year: Some(fy27),
            reference: None,
        },
    ];
    let assigned = assign_numbers(&mut tx, wait(), &requests).await.unwrap();
    tx.commit().await.unwrap();
    let names: Vec<&str> = assigned.iter().map(|a| a.name.as_str()).collect();
    assert_eq!(names, ["INV/25-26/0001", "INV/26-27/0001"]);
    assert_eq!(
        counters(&h).await,
        [
            (1, "document".into(), "2025-26".into(), 2),
            (1, "document".into(), "2026-27".into(), 2),
        ]
    );
    h.cleanup().await.unwrap();
}

/// Refusals made before any statement: an undeclared kind, one chain asked for two companies
/// or with two patterns, a document listed twice, a missing fiscal year.
#[tokio::test]
async fn inconsistent_batches_are_refused_by_name() {
    let (h, company) = harness().await;
    let docs = drafts(
        &h,
        company,
        &[(1, d(2026, 3, 1), None), (1, d(2026, 3, 2), None)],
    )
    .await;
    let inv = invoices();
    let other = pattern(SequenceReset::Year, "OUT/", 5);
    let fiscal = pattern(SequenceReset::YearRange, "INV/", 4);
    fn request<'a>(
        id: i64,
        company_id: i64,
        kind: &'a str,
        pattern: &'a SequencePattern,
    ) -> NumberRequest<'a> {
        NumberRequest {
            document_id: id,
            company_id,
            journal_id: 1,
            kind,
            pattern,
            date: d(2026, 3, 1),
            fiscal_year: None,
            reference: None,
        }
    }
    let cases: [(Vec<NumberRequest<'_>>, &str); 5] = [
        (
            vec![request(docs[0], company, "debit", &inv)],
            "sequence kind \"debit\" is not declared on account.sequence.chain.kind",
        ),
        (
            vec![
                request(docs[0], company, "document", &inv),
                request(docs[1], company + 1, "document", &inv),
            ],
            "is asked for companies",
        ),
        (
            vec![
                request(docs[0], company, "document", &inv),
                request(docs[1], company, "document", &other),
            ],
            "with two different patterns",
        ),
        (
            vec![
                request(docs[0], company, "document", &inv),
                request(docs[0], company, "document", &inv),
            ],
            "is listed twice",
        ),
        (
            vec![request(docs[0], company, "document", &fiscal)],
            "fiscal",
        ),
    ];
    for (requests, expected) in cases {
        let mut tx = h.begin().await.unwrap();
        let error = assign_numbers(&mut tx, wait(), &requests)
            .await
            .unwrap_err();
        assert!(error.message().contains(expected), "{}", error.message());
        tx.rollback().await.unwrap();
    }
    assert!(counters(&h).await.is_empty(), "nothing was written");
    h.cleanup().await.unwrap();
}

/// The caller's `lock_timeout` is back after a refusal that comes after the chains were locked (a
/// caller that catches the error without a savepoint keeps working under its own timeout).
#[tokio::test]
async fn a_refusal_after_the_lock_leaves_the_callers_lock_timeout() {
    let (h, company) = harness().await;
    let docs = drafts(&h, company, &[(1, d(2026, 3, 1), None)]).await;
    let inv = invoices();
    let mut tx = h.begin().await.unwrap();
    number(&mut tx, wait(), &[(docs[0], "document", &inv)])
        .await
        .unwrap();
    tx.commit().await.unwrap();

    let mut tx = h.begin().await.unwrap();
    tx.query("SELECT set_config('lock_timeout', '7000ms', true)", &[])
        .await
        .unwrap();
    let current = async |tx: &mut Transaction| -> String {
        tx.query("SELECT current_setting('lock_timeout')", &[])
            .await
            .unwrap()[0]
            .get(0)
    };
    let before = current(&mut tx).await;
    // the chain of journal 1 exists and belongs to `company`: asking for another company is refused
    // once the row is locked
    let request = NumberRequest {
        document_id: docs[0],
        company_id: company + 1,
        journal_id: 1,
        kind: "document",
        pattern: &inv,
        date: d(2026, 3, 1),
        fiscal_year: None,
        reference: None,
    };
    let error = assign_numbers(&mut tx, wait(), &[request])
        .await
        .unwrap_err();
    assert!(error.message().contains("belongs to company"), "{error:?}");
    assert_eq!(current(&mut tx).await, before, "the caller's lock_timeout");
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}
