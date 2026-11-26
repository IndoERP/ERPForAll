// SPDX-License-Identifier: MIT OR Apache-2.0
//! printing a few records at once and many in the background (`report.job`,
//! `addons/base/src/report_job.rs`). The report runtime
//! (/3b) is not in this build, so a fake [`ReportBackend`] stands in for it: it renders
//! nothing, names part attachments, fails one chosen record the way a render deadline does, and
//! merges by creating one attachment on the job. What is under test is the dispatch, the chunked
//! job and what the row tells the client.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod support;

use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use erp_addon_base::report_job::{
    JOB_MAX_KEY, PrintableReport, RenderedChunk, ReportBackend, install_backend,
};
use erp_orm::{StaticPolicy, UserId};
use erp_sdk::{BoxFuture, Cell, Error, RawVals, Result, Transaction, Value};
use erp_test_models::base_users;
use erp_testkit::Harness;
use support::jobs::{cell, exec, runner};

/// The record whose document fails, as a render deadline would.
const FAILING: i64 = 13;
const DEADLINE: &str = "The document did not finish within the render deadline.";

/// The parts each merge received, in order.
static MERGED: Mutex<Vec<Vec<i64>>> = Mutex::new(Vec::new());

struct Fake;

impl ReportBackend for Fake {
    fn reports<'a>(
        &'a self,
        _tx: &'a mut Transaction,
        model: &'a str,
    ) -> BoxFuture<'a, Result<Vec<PrintableReport>>> {
        Box::pin(async move {
            Ok(vec![PrintableReport {
                report: format!("{model}.document"),
                name: "Order".into(),
            }])
        })
    }

    fn check<'a>(
        &'a self,
        _tx: &'a mut Transaction,
        report: &'a str,
        _model: &'a str,
        _ids: &'a [i64],
    ) -> BoxFuture<'a, Result<PrintableReport>> {
        Box::pin(async move {
            if report == "unknown" {
                return Err(Error::validation("Report unknown does not exist."));
            }
            Ok(PrintableReport {
                report: report.to_owned(),
                name: "Order".into(),
            })
        })
    }

    fn download_url(&self, report: &str, ids: &[i64]) -> Result<String> {
        let ids: Vec<String> = ids.iter().map(i64::to_string).collect();
        Ok(format!("/report/pdf/{report}/{}", ids.join(",")))
    }

    fn render<'a>(
        &'a self,
        _tx: &'a mut Transaction,
        _job: i64,
        _report: &'a str,
        ids: &'a [i64],
    ) -> BoxFuture<'a, Result<RenderedChunk>> {
        Box::pin(async move {
            let failures = ids
                .iter()
                .filter(|id| **id == FAILING)
                .map(|id| (*id, DEADLINE.to_owned()))
                .collect();
            Ok(RenderedChunk {
                part: Some(1000 + ids[0]),
                failures,
                warnings: vec!["The terms field printed without its image.".into()],
            })
        })
    }

    fn merge<'a>(
        &'a self,
        tx: &'a mut Transaction,
        job: i64,
        name: &'a str,
        parts: &'a [i64],
    ) -> BoxFuture<'a, Result<i64>> {
        Box::pin(async move {
            MERGED
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(parts.to_vec());
            let vals = RawVals::new()
                .set_cell("name", Cell::Str(name.into()))
                .set_cell("res_model", Cell::Str("report.job".into()))
                .set_cell("res_id", Cell::Id(job));
            let created = tx
                .env()
                .records("ir.attachment")?
                .create(tx, vec![vals])
                .await?;
            Ok(created.ids()[0])
        })
    }
}

fn install() {
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| install_backend(Arc::new(Fake)).unwrap());
}

async fn world() -> (Harness, i64) {
    install();
    let h = Harness::builder([&base_users::ADDON, &erp_addon_base::ADDON])
        .policy(StaticPolicy::AllowAll)
        .build()
        .await
        .expect("harness");
    exec(
        &h,
        "INSERT INTO res_users (name, login, active, share) \
         VALUES ('printer', 'printer', true, false)",
    )
    .await;
    let uid: i64 = cell(&h, "SELECT id::text FROM res_users WHERE login = 'printer'")
        .await
        .parse()
        .unwrap();
    (h, uid)
}

async fn print(h: &Harness, uid: i64, report: &str, ids: Vec<i64>) -> Result<serde_json::Value> {
    let mut tx = h.orm().begin(UserId::new(uid).unwrap()).await.unwrap();
    let records = tx.env().records("report.job").unwrap();
    let answer: Value = records
        .call(
            &mut tx,
            "erp_print",
            (report.to_owned(), "sale.order".to_owned(), ids),
        )
        .await?;
    tx.commit().await.unwrap();
    match answer {
        Value::Json(json) => Ok((*json).clone()),
        other => panic!("erp_print answered {other:?}"),
    }
}

/// A request up to the threshold is the synchronous download: no row, no job.
#[tokio::test]
async fn a_small_print_is_the_synchronous_download() {
    let (h, uid) = world().await;
    let answer = print(&h, uid, "sale.order.document", vec![3, 1, 2])
        .await
        .unwrap();
    assert_eq!(
        answer,
        serde_json::json!({"mode": "sync", "url": "/report/pdf/sale.order.document/3,1,2", "open": "download"})
    );
    assert_eq!(cell(&h, "SELECT count(*)::text FROM report_job").await, "0");
    assert_eq!(cell(&h, "SELECT count(*)::text FROM erp_job").await, "0");
}

/// With `base.report_print_preview` on, every print answer asks the client to show the document
/// in its viewer; the default (the reference behaviour) asks for a download.
#[tokio::test]
async fn the_preview_parameter_decides_how_the_printed_document_opens() {
    let (h, uid) = world().await;
    exec(
        &h,
        "WITH updated AS (UPDATE ir_config_parameter SET value = 'True' \
         WHERE key = 'base.report_print_preview' RETURNING 1) \
         INSERT INTO ir_config_parameter (key, value) \
         SELECT 'base.report_print_preview', 'True' WHERE NOT EXISTS (SELECT 1 FROM updated)",
    )
    .await;
    let small = print(&h, uid, "sale.order.document", vec![1])
        .await
        .unwrap();
    assert_eq!(small["mode"], "sync");
    assert_eq!(small["open"], "preview");
    let large = print(&h, uid, "sale.order.document", (1..=51).collect())
        .await
        .unwrap();
    assert_eq!(large["mode"], "job");
    assert_eq!(large["open"], "preview");
}

/// 51 records (one over the default threshold of 50) print in the background, in chunks of 20;
/// the failing document is listed with its reason and not retried, the rest merge in order.
#[tokio::test]
async fn a_large_print_is_a_job_that_lists_the_failure_and_merges_the_rest() {
    let (h, uid) = world().await;
    let ids: Vec<i64> = (1..=51).collect();
    let answer = print(&h, uid, "sale.order.document", ids).await.unwrap();
    assert_eq!(answer["mode"], "job");
    assert_eq!(answer["poll_ms"], 1000);
    assert_eq!(answer["open"], "download");
    let id = answer["id"].as_i64().unwrap();
    for _ in 0..10 {
        runner(&h).tick().await.unwrap();
        if cell(&h, "SELECT count(*)::text FROM erp_job").await == "0" {
            break;
        }
    }
    let row = cell(
        &h,
        &format!(
            "SELECT concat_ws('|', j.state, j.total, j.processed, j.failed, j.user_id, a.name) \
             FROM report_job j JOIN ir_attachment a ON a.id = j.attachment_id WHERE j.id = {id}"
        ),
    )
    .await;
    assert_eq!(row, format!("done|51|51|1|{uid}|Order.pdf"));
    let failures: serde_json::Value = serde_json::from_str(
        &cell(
            &h,
            &format!("SELECT failures FROM report_job WHERE id = {id}"),
        )
        .await,
    )
    .unwrap();
    assert_eq!(
        failures,
        serde_json::json!([{"id": FAILING, "error": DEADLINE}])
    );
    let warnings = cell(
        &h,
        &format!("SELECT warnings FROM report_job WHERE id = {id}"),
    )
    .await;
    assert_eq!(
        warnings,
        r#"["The terms field printed without its image."]"#
    );
    assert!(
        MERGED
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains(&vec![1001, 1021, 1041])
    );
    assert_eq!(cell(&h, "SELECT count(*)::text FROM erp_job").await, "0");
}

/// Bad requests are refused before anything is stored.
#[tokio::test]
async fn bad_requests_are_refused_by_name() {
    let (h, uid) = world().await;
    let over: Vec<i64> = (1..=10_001).collect();
    let error = print(&h, uid, "sale.order.document", over)
        .await
        .unwrap_err();
    assert!(error.message().contains(JOB_MAX_KEY), "{}", error.message());
    let error = print(&h, uid, "sale.order.document", vec![2, 2])
        .await
        .unwrap_err();
    assert_eq!(
        error.message(),
        "Record 2 is listed twice in the print request."
    );
    let error = print(&h, uid, "unknown", vec![1]).await.unwrap_err();
    assert_eq!(error.message(), "Report unknown does not exist.");
    assert_eq!(cell(&h, "SELECT count(*)::text FROM report_job").await, "0");
}

/// The Print menu lists the backend's reports for the model.
#[tokio::test]
async fn the_print_menu_lists_the_reports_of_the_model() {
    let (h, uid) = world().await;
    let mut tx = h.orm().begin(UserId::new(uid).unwrap()).await.unwrap();
    let records = tx.env().records("report.job").unwrap();
    let answer: Value = records
        .call(&mut tx, "erp_print_reports", ("sale.order".to_owned(),))
        .await
        .unwrap();
    let Value::Json(json) = answer else {
        panic!("not JSON")
    };
    assert_eq!(
        *json,
        serde_json::json!([{"report": "sale.order.document", "name": "Order"}])
    );
}
