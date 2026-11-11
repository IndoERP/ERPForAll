//! Company consistency (`_check_company`, CRUD §4.9, REL §4.12).
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

mod support;

use erp_sdk::{ErrorKind, Records, Transaction, Value};
use erp_testkit::assert_query_count;
use support::map;

async fn create(tx: &mut Transaction, model: &str, rows: &[&[(&str, Value)]]) -> Records {
    let records = tx.env().records(model).unwrap();
    let vals: Vec<_> = rows.iter().map(|r| map(r)).collect();
    records.create_values(tx, &vals).await.unwrap()
}

struct Fixture {
    company_a: i64,
    company_b: i64,
    p0: i64,
    pa: i64,
    pb: i64,
}

async fn fixture(tx: &mut Transaction) -> Fixture {
    let companies = create(
        tx,
        "res.company",
        &[
            &[("name", Value::str("Company A"))],
            &[("name", Value::str("Company B"))],
        ],
    )
    .await;
    let (company_a, company_b) = (companies.ids()[0], companies.ids()[1]);
    let parents = create(
        tx,
        "test_rel.model_parent",
        &[
            &[("name", Value::str("P0"))],
            &[
                ("name", Value::str("PA")),
                ("company_id", Value::Int(company_a)),
            ],
            &[
                ("name", Value::str("PB")),
                ("company_id", Value::Int(company_b)),
            ],
        ],
    )
    .await;
    Fixture {
        company_a,
        company_b,
        p0: parents.ids()[0],
        pa: parents.ids()[1],
        pb: parents.ids()[2],
    }
}

fn many(ids: &[i64]) -> Value {
    Value::list(ids.iter().map(|i| Value::Int(*i)))
}

#[tokio::test]
async fn spec_crud_059_company_check_on_create() {
    let h = support::rel_orm().await;
    let mut tx = h.begin().await.unwrap();
    let f = fixture(&mut tx).await;
    let children = tx.env().records("test_rel.model_child").unwrap();
    let ok: Vec<Vec<(&str, Value)>> = vec![
        vec![
            ("company_id", Value::Int(f.company_a)),
            ("parent_id", Value::Int(f.pa)),
        ],
        vec![
            ("company_id", Value::Int(f.company_a)),
            ("parent_id", Value::Int(f.pa)),
            ("parent_ids", many(&[f.pa])),
        ],
        vec![
            ("company_id", Value::Int(f.company_a)),
            ("parent_id", Value::Int(f.p0)),
        ],
        vec![
            ("company_id", Value::Int(f.company_a)),
            ("parent_id", Value::Int(f.p0)),
            ("parent_ids", many(&[f.p0])),
        ],
        vec![("parent_id", Value::Int(f.p0))],
        vec![
            ("parent_id", Value::Int(f.p0)),
            ("parent_ids", many(&[f.p0])),
        ],
    ];
    for row in &ok {
        children.create_values(&mut tx, &[map(row)]).await.unwrap();
    }
    let bad: Vec<Vec<(&str, Value)>> = vec![
        vec![
            ("company_id", Value::Int(f.company_b)),
            ("parent_id", Value::Int(f.pa)),
        ],
        vec![
            ("company_id", Value::Int(f.company_b)),
            ("parent_ids", many(&[f.pa, f.pb])),
        ],
        vec![("parent_id", Value::Int(f.pa))],
        vec![("parent_ids", many(&[f.p0, f.pa]))],
    ];
    for row in &bad {
        let err = tx
            .savepoint(async |tx| children.create_values(tx, &[map(row)]).await)
            .await
            .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::UserError, "{err}");
        assert!(
            err.message()
                .starts_with("Uh-oh! You’ve got some company inconsistencies here:"),
            "{err}"
        );
        assert!(
            err.message()
                .ends_with("To avoid a mess, no company crossover is allowed!"),
            "{err}"
        );
    }
    // one bad row in full: the message names the record, its company and the linked record
    let err = tx
        .savepoint(async |tx| {
            children
                .create_values(
                    tx,
                    &[map(&[
                        ("name", Value::str("N0")),
                        ("company_id", Value::Int(f.company_b)),
                        ("parent_id", Value::Int(f.pa)),
                    ])],
                )
                .await
        })
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::UserError);
    assert_eq!(
        err.message(),
        "Uh-oh! You’ve got some company inconsistencies here:\n\
         - “N0” belongs to company “Company B” while “Parent” (parent_id: 'PA') belongs to \
         another company.\n\
         To avoid a mess, no company crossover is allowed!"
    );
    // the model without the auto check accepts the inconsistency
    let nocheck = tx.env().records("test_rel.model_child_nocheck").unwrap();
    nocheck
        .create_values(
            &mut tx,
            &[map(&[
                ("company_id", Value::Int(f.company_b)),
                ("parent_id", Value::Int(f.pa)),
            ])],
        )
        .await
        .unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_crud_060_company_check_on_write_and_its_cost() {
    let h = support::rel_orm().await;
    let mut tx = h.begin().await.unwrap();
    let f = fixture(&mut tx).await;
    let child = create(
        &mut tx,
        "test_rel.model_child",
        &[&[
            ("company_id", Value::Int(f.company_a)),
            ("parent_id", Value::Int(f.pa)),
            ("parent_ids", many(&[f.pa])),
        ]],
    )
    .await;
    for (field, value) in [
        ("company_id", Value::Int(f.company_b)),
        ("parent_id", Value::Int(f.pb)),
    ] {
        let err = tx
            .savepoint(async |tx| child.set_value(tx, field, value.clone()).await)
            .await
            .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::UserError, "{field}");
    }
    let mut vals = std::collections::BTreeMap::new();
    vals.insert("parent_id".to_owned(), Value::Int(f.pb));
    vals.insert("parent_ids".to_owned(), many(&[f.pb]));
    vals.insert("company_id".to_owned(), Value::Int(f.company_b));
    child.write_values(&mut tx, &vals).await.unwrap();
    // the cost of one check
    let other = create(
        &mut tx,
        "test_rel.model_child",
        &[&[("company_id", Value::Int(f.company_b))]],
    )
    .await;
    tx.invalidate_all().await.unwrap();
    assert_query_count!(tx, >= 1, <= 1, {
        other.set_value(&mut tx, "parent_id", Value::Int(f.pb)).await.unwrap();
    });
    assert_query_count!(tx, >= 1, <= 1, { tx.flush().await.unwrap() });
    // everything cached (records and targets created in this transaction): the check reads nothing
    // and the assignment waits for the flush
    let warm = create(
        &mut tx,
        "test_rel.model_child",
        &[&[("company_id", Value::Int(f.company_b))]],
    )
    .await;
    assert_query_count!(tx, <= 0, {
        warm.set_value(&mut tx, "parent_id", Value::Int(f.pb)).await.unwrap();
    });
    drop(tx);
    h.cleanup().await.unwrap();
}

/// listed; (b) awaits the spec question open question 2)
#[tokio::test]
async fn spec_crud_061_company_check_message_forms() {
    let h = support::rel_orm().await;
    let mut tx = h.begin().await.unwrap();
    let f = fixture(&mut tx).await;
    let children = tx.env().records("test_rel.model_child").unwrap();
    let rows: Vec<_> = (0..7)
        .map(|i| {
            map(&[
                ("name", Value::str(format!("C{i}"))),
                ("company_id", Value::Int(f.company_b)),
                ("parent_id", Value::Int(f.pa)),
            ])
        })
        .collect();
    let err = tx
        .savepoint(async |tx| children.create_values(tx, &rows).await)
        .await
        .unwrap_err();
    let lines: Vec<&str> = err.message().lines().collect();
    assert_eq!(
        lines.len(),
        7,
        "header, 5 inconsistencies, footer: {lines:?}"
    );
    assert_eq!(
        lines[1],
        "- “C0” belongs to company “Company B” while “Parent” (parent_id: 'PA') belongs to another \
         company."
    );
    // form (a): a company is its own company
    let companies = tx.env().records("res.company").unwrap();
    let err = tx
        .savepoint(async |tx| {
            companies
                .create_values(
                    tx,
                    &[map(&[
                        ("name", Value::str("Self")),
                        ("model_parent_id", Value::Int(f.pb)),
                    ])],
                )
                .await
        })
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::UserError);
    assert_eq!(
        err.message(),
        "Uh-oh! You’ve got some company inconsistencies here:\n\
         - Record is company “Self” while “Parent Record” (model_parent_id: 'PB') belongs to \
         another company.\n\
         To avoid a mess, no company crossover is allowed!"
    );
    drop(tx);
    h.cleanup().await.unwrap();
}
