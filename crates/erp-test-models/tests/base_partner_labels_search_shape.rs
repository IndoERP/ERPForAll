//! Project semantic search contracts, independently of unmeasured PARTNER Q4.
//!
//! These functional checks preserve SQL predicate composition and batched label
//! resolution.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use std::sync::Arc;

use erp_orm::{NameSearch, Operation, Records, Transaction};
use erp_sdk::{Cell, Domain, Operator, RawVals};
use erp_test_models::base_users;
use erp_testkit::{DomainPolicy, Harness, Otherwise};

async fn harness() -> Harness {
    let allowed = Domain::condition("ref", Operator::Ne, "blocked").unwrap();
    let policy = DomainPolicy::builder(Otherwise::Allow)
        .rule("res.partner", Operation::Read, allowed)
        .build()
        .unwrap();
    Harness::builder([&base_users::ADDON, &erp_addon_base::ADDON])
        .policy(policy)
        .build()
        .await
        .unwrap()
}

fn partners(tx: &Transaction) -> Records {
    tx.env().records("res.partner").unwrap()
}

async fn fixture(tx: &mut Transaction) -> Vec<i64> {
    tx.batch("INSERT INTO res_users (id, name, login, active) VALUES (2, 'Lookup reader', 'lookup-reader', true)")
        .await
        .unwrap();
    let root = partners(tx)
        .create(
            tx,
            vec![RawVals::new().set_cell("name", Cell::Str("Scope".into()))],
        )
        .await
        .unwrap();
    let parent = root.ids()[0];
    let values = (0..8)
        .map(|index| {
            RawVals::new()
                .set_cell("name", Cell::Str(format!("needle {index:02}").into()))
                .set_cell("parent_id", Cell::Id(parent))
                .set_cell(
                    "email",
                    Cell::Str(format!("needle{index}@example.com").into()),
                )
                .set_cell("ref", Cell::Str(format!("needle-{index}").into()))
                .set_cell("vat", Cell::Str(format!("NEEDLE-{index}").into()))
                .set_cell("color", Cell::Int(7))
        })
        .collect();
    let rows = partners(tx).create(tx, values).await.unwrap();
    partners(tx)
        .create(
            tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("needle archived".into()))
                    .set_cell("active", Cell::Bool(false))
                    .set_cell("color", Cell::Int(7)),
                RawVals::new()
                    .set_cell("name", Cell::Str("needle rule denied".into()))
                    .set_cell("ref", Cell::Str("blocked".into()))
                    .set_cell("color", Cell::Int(7)),
                RawVals::new()
                    .set_cell("name", Cell::Str("needle domain excluded".into()))
                    .set_cell("color", Cell::Int(8)),
            ],
        )
        .await
        .unwrap();
    tx.flush().await.unwrap();
    rows.ids()
}

async fn capture_lookup(
    tx: &mut Transaction,
    model: &Records,
    limit: u64,
) -> (Vec<(i64, Option<Arc<str>>)>, Vec<String>) {
    tx.invalidate_all().await.unwrap();
    tx.capture_statements(true);
    let options = NameSearch::new()
        .domain(Domain::condition("color", Operator::Eq, 7_i64).unwrap())
        .limit(Some(limit));
    let found = model
        .name_search_with(tx, "needle", &options)
        .await
        .unwrap();
    let statements = tx.captured().to_vec();
    tx.capture_statements(false);
    (found, statements)
}

fn partner_data_reads(statements: &[String]) -> Vec<&str> {
    statements
        .iter()
        .filter(|sql| sql.starts_with("SELECT ") && sql.contains("FROM \"res_partner\""))
        .map(String::as_str)
        .collect()
}

fn assert_selection_shape(sql: &str) {
    let (_, predicate_and_order) = sql.split_once(" WHERE ").expect("selection has WHERE");
    let (predicate, order) = predicate_and_order
        .split_once(" ORDER BY ")
        .expect("selection has ORDER BY");
    for field in ["complete_name", "email", "ref", "vat"] {
        assert!(
            predicate.contains(&format!("\"{field}\" ILIKE")),
            "missing {field} match in {sql}"
        );
    }
    assert_eq!(
        predicate.matches(" ILIKE ").count(),
        4,
        "four-field search predicate: {sql}"
    );
    assert!(
        predicate.matches(" OR ").count() >= 3,
        "four-field OR: {sql}"
    );
    assert!(
        predicate.contains("\"active\" IS TRUE"),
        "active-test predicate: {sql}"
    );
    assert!(predicate.contains("\"color\""), "caller domain: {sql}");
    assert!(
        predicate.contains("\"ref\" <>"),
        "record-access domain: {sql}"
    );
    let complete_name = order
        .find("\"complete_name\"")
        .expect("complete-name order");
    let id = order
        .find("\"id\" DESC")
        .expect("descending id tie-breaker");
    assert!(complete_name < id, "default order: {sql}");
    assert!(
        order.contains(" LIMIT "),
        "limited ordered selection: {sql}"
    );
}

#[tokio::test]
async fn project_partner_lookup_composes_four_matches_domain_access_order_and_limit_in_sql() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let ids = fixture(&mut tx).await;
    let model = partners(&tx).with_user(2_i64);
    let (cold, cold_sql) = capture_lookup(&mut tx, &model, 2).await;
    assert_eq!(
        cold,
        vec![
            (ids[0], Some("Scope, needle 00".into())),
            (ids[1], Some("Scope, needle 01".into()))
        ]
    );
    let selections = partner_data_reads(&cold_sql);
    let selection = selections
        .iter()
        .find(|sql| sql.contains(" ILIKE "))
        .expect("recorded lookup selection");
    assert_eq!(
        selections
            .iter()
            .filter(|sql| sql.contains(" ILIKE "))
            .count(),
        1,
        "one logical lookup selection: {cold_sql:#?}"
    );
    assert_selection_shape(selection);
    // Do not invalidate between these calls: the warm path must apply the same
    // domain and access policy rather than returning a cached broad result.
    tx.capture_statements(true);
    let warm = model
        .name_search_with(
            &mut tx,
            "needle",
            &NameSearch::new()
                .domain(Domain::condition("color", Operator::Eq, 7_i64).unwrap())
                .limit(Some(2)),
        )
        .await
        .unwrap();
    assert_eq!(warm, cold);
    let warm_sql = tx.captured();
    let warm_reads = partner_data_reads(warm_sql);
    let selection = warm_reads
        .iter()
        .find(|sql| sql.contains(" ILIKE "))
        .expect("warm lookup selection remains authorized");
    assert_selection_shape(selection);
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn project_partner_label_reads_are_batched_for_a_shared_dependency_footprint() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let ids = fixture(&mut tx).await;
    let model = partners(&tx).with_user(2_i64);
    // Warm method/statement setup independently from the captured functional
    // checks, then use cold value caches for each cardinality.
    capture_lookup(&mut tx, &model, 8).await;
    let (one, one_sql) = capture_lookup(&mut tx, &model, 1).await;
    let (many, many_sql) = capture_lookup(&mut tx, &model, 8).await;
    assert_eq!(one.len(), 1);
    assert_eq!(many.iter().map(|(id, _)| *id).collect::<Vec<_>>(), ids);
    for (index, (_, label)) in many.iter().enumerate() {
        assert_eq!(
            label.as_deref(),
            Some(format!("Scope, needle {index:02}").as_str())
        );
    }
    // Project semantic contract: with one parent and the same required fields,
    // adding seven result rows must not add seven individual partner fetches.
    assert_eq!(
        partner_data_reads(&many_sql).len(),
        partner_data_reads(&one_sql).len(),
        "label reads grew with rows; singleton={one_sql:#?}, batch={many_sql:#?}"
    );
    drop(tx);
    h.cleanup().await.unwrap();
}
