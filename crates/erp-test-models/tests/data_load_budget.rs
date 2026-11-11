//! The query budget of the data loader: the statements a file of
//! `<record>` elements of one model costs do not grow with the number of records. At most 10
//! statements per model, in init mode and in update mode, for the fixture model without computed
//! fields or constraints (design §3.4), and for the end-of-run cleanup of the same records.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use std::fmt::Write as _;
use std::sync::Arc;

use erp_data::load::{BatchLimits, FileContext, LoadError, LoadSession, NoFiles, SessionOptions};
use erp_data::{ConvertMode, XmlDataReader};
use erp_sdk::{Cell, Domain, Transaction};
use erp_test_models::{base_users, test_convert, test_convert_ui};
use erp_testkit::{Harness, assert_query_count};

const M: &str = "test_convert";
const RECORD: &str = "test_convert.record";

async fn harness() -> Harness {
    Harness::builder([
        &base_users::ADDON,
        &test_convert::ADDON,
        &test_convert_ui::ADDON,
        &erp_data::model_data::ADDON,
    ])
    .with_decls([erp_data::ir_model_data_decl()])
    .unlink_hook(erp_data::model_data::UNLINK_HOOK)
    .build()
    .await
    .expect("harness")
}

fn session() -> LoadSession {
    LoadSession::new(SessionOptions::new("20.0", Arc::new(NoFiles)))
}

async fn load(
    session: &mut LoadSession,
    tx: &mut Transaction,
    xml: &str,
    mode: ConvertMode,
) -> Result<(), LoadError> {
    let file = FileContext::new(M, "data/big.xml", mode);
    let mut ops = XmlDataReader::new(xml, "data/big.xml", M, false);
    session.load_file(tx, &file, &mut ops).await
}

/// The name of record `i`: every seventh has a CRLF line end (written as a character reference,
/// which the XML reader keeps; a literal one it normalises to LF) and non-ASCII text.
fn name_of(i: usize, version: &str) -> String {
    if i.is_multiple_of(7) {
        format!("row {i} {version}\r\nline two: é 漢字 🙂")
    } else {
        format!("row {i} {version}")
    }
}

fn escape(text: &str) -> String {
    text.replace('\r', "&#13;")
}

/// `n` records of the fixture model with scalar fields and a many2one to `test_convert.u1`; the
/// record `i` has the name `name_of(i, version)` and `ratio = i`.
fn records_xml(n: usize, version: &str) -> String {
    let mut xml = String::from(
        "<odoo>\n<record id=\"u1\" model=\"test_convert.usered\"><field name=\"name\">U</field></record>",
    );
    for i in 0..n {
        writeln!(
            xml,
            "<record id=\"r{i}\" model=\"test_convert.record\">\
             <field name=\"name\">{}</field><field name=\"count\">{i}</field>\
             <field name=\"ratio\" eval=\"{i}.5\"/><field name=\"flag\">1</field>\
             <field name=\"usered_id\" ref=\"u1\"/></record>",
            escape(&name_of(i, version)),
        )
        .unwrap();
    }
    xml.push_str("</odoo>");
    xml
}

async fn names(tx: &mut Transaction) -> Vec<String> {
    let env = tx.env();
    let all = env
        .records(RECORD)
        .unwrap()
        .search(tx, &Domain::TRUE)
        .await
        .unwrap();
    all.cells(tx, "name")
        .await
        .unwrap()
        .into_iter()
        .map(|c| match c {
            Cell::Str(s) => s.to_string(),
            other => panic!("a name: {other:?}"),
        })
        .collect()
}

/// A statement budget per model,.
const BUDGET: u64 = 10;

/// 1,000 records of one model cost at most 10 statements, in init and in update
/// mode (all records existing, half of them changed).
#[tokio::test]
async fn a_thousand_records_cost_at_most_ten_statements_in_init_and_in_update_mode() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    // the many2one target and the session's cache are warm in neither mode: the first file
    // creates `u1` too (a second model, so two batches)
    let mut first = session();
    let xml = records_xml(1000, "v1");
    let used = assert_query_count!(tx, >= 1, <= 2 * BUDGET, {
        let before = tx.query_count();
        load(&mut first, &mut tx, &xml, ConvertMode::Init).await.unwrap();
        tx.flush().await.unwrap();
        tx.query_count() - before
    });
    eprintln!("init: {used} statements for 1,000 records and 1 record of a second model");
    let expected: Vec<String> = (0..1000).map(|i| name_of(i, "v1")).collect();
    assert_eq!(names(&mut tx).await, expected);

    // update mode: a new session, half of the records have a new name
    let mut second = session();
    let xml2: String = {
        let mut x = String::from("<odoo>\n");
        for i in 0..1000usize {
            let version = if i.is_multiple_of(2) { "v2" } else { "v1" };
            writeln!(
                x,
                "<record id=\"r{i}\" model=\"test_convert.record\">\
                 <field name=\"name\">{}</field><field name=\"count\">{i}</field>\
                 <field name=\"ratio\" eval=\"{i}.5\"/><field name=\"flag\">1</field>\
                 <field name=\"usered_id\" ref=\"u1\"/></record>",
                escape(&name_of(i, version)),
            )
            .unwrap();
        }
        x.push_str("</odoo>");
        x
    };
    let used = assert_query_count!(tx, >= 1, <= BUDGET, {
        let before = tx.query_count();
        load(&mut second, &mut tx, &xml2, ConvertMode::Update)
            .await
            .unwrap();
        // the ORM writes pending updates when it is flushed: count that statement too
        tx.flush().await.unwrap();
        tx.query_count() - before
    });
    eprintln!("update: {used} statements for 1,000 records");
    let expected: Vec<String> = (0..1000usize)
        .map(|i| name_of(i, if i.is_multiple_of(2) { "v2" } else { "v1" }))
        .collect();
    assert_eq!(names(&mut tx).await, expected);
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// The window is flushed at exactly 1,000 records: 1,000 records are one batch, 1,001 are two.
#[tokio::test]
async fn the_batch_window_flushes_at_one_thousand_records() {
    let h = harness().await;
    let mut counts = Vec::new();
    for n in [1000usize, 1001] {
        let mut tx = h.begin().await.unwrap();
        // the same reference target in both runs, created before the measured file
        let mut warm = session();
        load(
            &mut warm,
            &mut tx,
            "<odoo><record id=\"u1\" model=\"test_convert.usered\"/></odoo>",
            ConvertMode::Init,
        )
        .await
        .unwrap();
        let xml = records_xml(n, "v1").replace(
            "<record id=\"u1\" model=\"test_convert.usered\"><field name=\"name\">U</field></record>",
            "",
        );
        let mut measured = session();
        let before = tx.query_count();
        load(&mut measured, &mut tx, &xml, ConvertMode::Init)
            .await
            .unwrap();
        tx.flush().await.unwrap();
        counts.push(tx.query_count() - before);
        assert_eq!(names(&mut tx).await.len(), n);
        tx.rollback().await.unwrap();
    }
    eprintln!(
        "1000 records: {} statements, 1001 records: {}",
        counts[0], counts[1]
    );
    assert!(counts[0] <= BUDGET, "{counts:?}");
    assert!(
        counts[1] <= 2 * BUDGET,
        "the second batch has the same fixed cost: {counts:?}"
    );
    assert!(
        counts[1] > counts[0],
        "the 1,001st record is a second batch: {counts:?}"
    );
    h.cleanup().await.unwrap();
}

/// Lowering the limits writes smaller batches: the number of records per batch is a ceiling the
/// host can only lower, and zero or above-ceiling values are refused.
#[tokio::test]
async fn batch_limits_can_only_be_lowered() {
    let err = BatchLimits::default().with_max_records(0).unwrap_err();
    assert_eq!(
        err.message(),
        format!(
            "a batch holds 1 to {} records, not 0",
            BatchLimits::MAX_RECORDS
        )
    );
    let err = BatchLimits::default()
        .with_max_records(BatchLimits::MAX_RECORDS + 1)
        .unwrap_err();
    assert_eq!(
        err.message(),
        format!(
            "a batch holds 1 to {} records, not {}",
            BatchLimits::MAX_RECORDS,
            BatchLimits::MAX_RECORDS + 1
        )
    );
    let err = BatchLimits::default().with_max_bytes(0).unwrap_err();
    assert_eq!(
        err.message(),
        format!(
            "a batch holds 1 to {} bytes of field values, not 0",
            BatchLimits::MAX_BYTES
        )
    );

    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let mut s = session();
    s.set_batch_limits(BatchLimits::default().with_max_records(100).unwrap());
    let xml = records_xml(250, "v1");
    let before = tx.query_count();
    load(&mut s, &mut tx, &xml, ConvertMode::Init)
        .await
        .unwrap();
    let small = tx.query_count() - before;
    // 250 records at 100 per batch: three batches of `test_convert.record`, plus `u1`
    assert!(small > BUDGET, "{small} statements for three batches");
    assert_eq!(names(&mut tx).await.len(), 250);
    tx.rollback().await.unwrap();

    let mut tx = h.begin().await.unwrap();
    let mut s = session();
    s.set_batch_limits(BatchLimits::default().with_max_bytes(2_000).unwrap());
    load(&mut s, &mut tx, &xml, ConvertMode::Init)
        .await
        .unwrap();
    assert_eq!(names(&mut tx).await.len(), 250);
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// the cleanup of 1,000 obsolete records costs a fixed number of statements.
#[tokio::test]
async fn the_cleanup_of_a_thousand_obsolete_records_costs_at_most_ten_statements() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let mut v1 = session();
    load(
        &mut v1,
        &mut tx,
        &records_xml(1000, "v1"),
        ConvertMode::Init,
    )
    .await
    .unwrap();
    tx.execute(
        "UPDATE ir_model_data SET write_date = write_date - interval '1 day'",
        &[],
    )
    .await
    .unwrap();
    let mut v2 = session();
    load(
        &mut v2,
        &mut tx,
        "<odoo><record id=\"u1\" model=\"test_convert.usered\"/></odoo>",
        ConvertMode::Update,
    )
    .await
    .unwrap();
    let used = assert_query_count!(tx, >= 1, <= BUDGET, {
        let before = tx.query_count();
        v2.process_end(&mut tx, &[M]).await.unwrap();
        tx.query_count() - before
    });
    eprintln!("process_end: {used} statements for 1,000 obsolete records");
    assert!(names(&mut tx).await.is_empty());
    let left: Vec<i64> = tx
        .query(
            "SELECT count(*) FROM ir_model_data WHERE module = 'test_convert' AND model = $1",
            &[&RECORD],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect();
    assert_eq!(left, [0]);
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

// ---- the other paths of the budget (database review D8) ---------------------------------------

/// `n` records `r{from}..r{from+n}` (name, count) of the fixture model.
fn range_xml(from: usize, n: usize, version: &str, noupdate: bool) -> String {
    let mut xml = format!(
        "<odoo><data noupdate=\"{}\">\n",
        if noupdate { "1" } else { "0" }
    );
    for i in from..from + n {
        writeln!(
            xml,
            "<record id=\"r{i}\" model=\"test_convert.record\">\
             <field name=\"name\">row {i} {version}</field><field name=\"count\">{i}</field></record>",
        )
        .unwrap();
    }
    xml.push_str("</data></odoo>");
    xml
}

/// The statements `xml` costs in `mode` (flushed), in a fresh session.
async fn cost(tx: &mut Transaction, xml: &str, mode: ConvertMode) -> u64 {
    let mut s = session();
    let before = tx.query_count();
    load(&mut s, tx, xml, mode).await.unwrap();
    tx.flush().await.unwrap();
    tx.query_count() - before
}

/// The cost of the same path at 100 and at 1,000 records: within the budget and the same.
fn assert_flat(what: &str, small: u64, large: u64) {
    eprintln!("{what}: {small} statements for 100 records, {large} for 1,000");
    assert!(
        large <= BUDGET,
        "{what}: {large} statements for 1,000 records"
    );
    assert_eq!(small, large, "{what}: the cost grows with the records");
}

/// Update mode under `noupdate` with half of the records existing: the existing ones are skipped
/// (one lookup and one `exists` for the window), the others created.
#[tokio::test]
async fn update_under_noupdate_with_half_existing_costs_a_fixed_number_of_statements() {
    let h = harness().await;
    let mut costs = Vec::new();
    for n in [100usize, 1000] {
        let mut tx = h.begin().await.unwrap();
        cost(
            &mut tx,
            &range_xml(0, n / 2, "v1", false),
            ConvertMode::Init,
        )
        .await;
        costs.push(cost(&mut tx, &range_xml(0, n, "v2", true), ConvertMode::Update).await);
        assert_eq!(names(&mut tx).await.len(), n);
        tx.rollback().await.unwrap();
    }
    assert_flat("noupdate update, half existing", costs[0], costs[1]);
    h.cleanup().await.unwrap();
}

/// Update mode with 500 existing and 500 new records: the update group and the create group.
#[tokio::test]
async fn update_with_existing_and_new_records_costs_a_fixed_number_of_statements() {
    let h = harness().await;
    let mut costs = Vec::new();
    for n in [100usize, 1000] {
        let mut tx = h.begin().await.unwrap();
        cost(
            &mut tx,
            &range_xml(0, n / 2, "v1", false),
            ConvertMode::Init,
        )
        .await;
        costs.push(cost(&mut tx, &range_xml(0, n, "v2", false), ConvertMode::Update).await);
        assert_eq!(names(&mut tx).await.len(), n);
        tx.rollback().await.unwrap();
    }
    assert_flat("update, existing and new", costs[0], costs[1]);
    h.cleanup().await.unwrap();
}

/// Records deleted behind their external ids are created again in update mode:
/// the dangling rows go in one statement, the records are created in one batch.
#[tokio::test]
async fn recreating_deleted_records_costs_a_fixed_number_of_statements() {
    let h = harness().await;
    let mut costs = Vec::new();
    for n in [100usize, 1000] {
        let mut tx = h.begin().await.unwrap();
        cost(&mut tx, &range_xml(0, n, "v1", true), ConvertMode::Init).await;
        let table = tx.registry().model(RECORD).unwrap().table.clone();
        tx.execute(&format!("DELETE FROM \"{table}\""), &[])
            .await
            .unwrap();
        tx.invalidate_all().await.unwrap();
        costs.push(cost(&mut tx, &range_xml(0, n, "v2", true), ConvertMode::Update).await);
        assert_eq!(names(&mut tx).await.len(), n);
        tx.rollback().await.unwrap();
    }
    assert_flat("recreating deleted records", costs[0], costs[1]);
    h.cleanup().await.unwrap();
}

/// A model with `_inherits`: the parent rows are created with the records and get their own
/// external ids, still in a fixed number of statements.
#[tokio::test]
async fn records_of_an_inherits_model_cost_a_fixed_number_of_statements() {
    use erp_registry::{AddonDescriptor, FieldDecl, ModelDecl};
    use erp_sdk::OnDelete;
    static ADDON: AddonDescriptor = AddonDescriptor {
        name: "t_budget",
        depends: &["base"],
        exact_only: false,
    };
    let h = Harness::builder([&base_users::ADDON, &ADDON, &erp_data::model_data::ADDON])
        .with_decls([
            erp_data::ir_model_data_decl(),
            ModelDecl::new("t_budget", "t.parent")
                .description("Parent")
                .fields(const { &[FieldDecl::char("name")] }),
            ModelDecl::new("t_budget", "t.child")
                .description("Child")
                .inherits(&[("t.parent", "parent_id")])
                .fields(
                    const {
                        &[
                            FieldDecl::many2one("parent_id", "t.parent")
                                .required(true)
                                .ondelete(OnDelete::Cascade),
                            FieldDecl::integer("extra"),
                        ]
                    },
                ),
        ])
        .unlink_hook(erp_data::model_data::UNLINK_HOOK)
        .build()
        .await
        .unwrap();
    let mut costs = Vec::new();
    for n in [100usize, 1000] {
        let mut tx = h.begin().await.unwrap();
        let mut xml = String::from("<odoo>\n");
        for i in 0..n {
            writeln!(
                xml,
                "<record id=\"c{i}\" model=\"t.child\"><field name=\"name\">c {i}</field>\
                 <field name=\"extra\">{i}</field></record>",
            )
            .unwrap();
        }
        xml.push_str("</odoo>");
        costs.push(cost(&mut tx, &xml, ConvertMode::Init).await);
        let count: i64 = tx
            .query(
                "SELECT count(*) FROM ir_model_data WHERE model = 't.parent'",
                &[],
            )
            .await
            .unwrap()[0]
            .get(0);
        assert_eq!(
            usize::try_from(count).unwrap(),
            n,
            "a parent external id per record"
        );
        tx.rollback().await.unwrap();
    }
    assert_flat("_inherits model", costs[0], costs[1]);
    h.cleanup().await.unwrap();
}
