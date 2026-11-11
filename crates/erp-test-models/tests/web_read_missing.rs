//! web_read/read paths (design note).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod bench_fixture;

use bench_fixture::{create, harness, orders, vals};
use erp_fields::{ErrorKind, Result, Value};
use erp_orm::Spec;
use erp_registry::ModelSpec;
use erp_sdk::{Domain, Operation, Operator};
use erp_test_models::test_compute::{ComputeRaises, CtxKey};
use erp_test_models::{base_users, bench, test_compute};

fn val_id(v: &Value) -> i64 {
    match v {
        Value::Int(i) => *i,
        _ => match v.dict().unwrap().get("id").unwrap() {
            Value::Int(i) => i,
            other => panic!("expected Value::Int id in dict, got {other:?}"),
        },
    }
}

/// One result row as `(id, requested values in the order of the requested fields)`.
type Row = (i64, Vec<Value>);

/// The value of `field` on the order whose name is `name` and whose partner is `partner`
/// (the fixture's truth table, independent of any read).
fn truth(field: &str, name: &str, partner: &str) -> Value {
    match field {
        "name" => Value::str(name),
        "partner_name" => Value::str(partner),
        other => panic!("fixture has no truth for field {other}"),
    }
}

/// The three read representations of `recs` for `fields`: the dictionary `read`, the columnar
/// `read_rows` and `web_read`, each reduced to [`Row`]s so one assertion compares them all.
async fn three_representations(
    tx: &mut erp_orm::Transaction,
    recs: &erp_orm::Records,
    fields: &[&str],
) -> [(&'static str, Vec<Row>); 3] {
    let dict = recs
        .read(tx, fields)
        .await
        .unwrap()
        .iter()
        .map(|r| {
            (
                val_id(r.get("id").unwrap()),
                fields.iter().map(|f| r.get(*f).unwrap().clone()).collect(),
            )
        })
        .collect();
    let columnar = recs
        .read_rows(tx, fields, erp_orm::Load::Classic)
        .await
        .unwrap();
    let columnar = (0..columnar.len())
        .map(|i| {
            (
                val_id(&columnar.get(i, "id").unwrap()),
                fields.iter().map(|f| columnar.get(i, f).unwrap()).collect(),
            )
        })
        .collect();
    let mut spec = Spec::new();
    for &f in fields {
        spec = spec.field(f);
    }
    let web = recs
        .web_read(tx, spec)
        .await
        .unwrap()
        .iter()
        .map(|r| {
            (
                val_id(r),
                fields
                    .iter()
                    .map(|f| r.dict().unwrap().get(f).unwrap())
                    .collect(),
            )
        })
        .collect();
    [("read", dict), ("read_rows", columnar), ("web_read", web)]
}

#[tokio::test]
async fn deleted_rows_do_not_break_stored_dependent_computed_reads() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();

    // Distinct partners and distinct order names: a row or value mix-up cannot pass.
    let partner_rows = vec![
        vals(vec![
            ("code", Value::str("P_A")),
            ("name", Value::str("Partner of A")),
        ]),
        vals(vec![
            ("code", Value::str("P_D")),
            ("name", Value::str("Partner of D")),
        ]),
        vals(vec![
            ("code", Value::str("P_B")),
            ("name", Value::str("Partner of B")),
        ]),
    ];
    let partner_ids = create(&mut tx, "bench.partner", &partner_rows).await;
    let order_rows: Vec<_> = [("A", 0), ("D", 1), ("B", 2)]
        .iter()
        .map(|(tag, p)| {
            vals(vec![
                ("code", Value::str(format!("O_{tag}"))),
                ("name", Value::str(format!("Order {tag}"))),
                ("partner_id", Value::Int(partner_ids[*p])),
            ])
        })
        .collect();
    let order_ids = create(&mut tx, "bench.order", &order_rows).await;
    let (a_id, d_id, b_id) = (order_ids[0], order_ids[1], order_ids[2]);

    // Delete order D directly via unlink
    orders(&tx, &[d_id]).unlink(&mut tx).await.unwrap();
    tx.flush().await.unwrap();

    let field_combinations = [
        vec!["name"],
        vec!["partner_name"],
        vec!["name", "partner_name"],
        vec!["partner_name", "name"],
    ];

    for fields in &field_combinations {
        let row_of = |id: i64| -> Row {
            let (name, partner) = if id == a_id {
                ("Order A", "Partner of A")
            } else if id == b_id {
                ("Order B", "Partner of B")
            } else {
                panic!("no surviving fixture row {id}")
            };
            (id, fields.iter().map(|f| truth(f, name, partner)).collect())
        };

        // each scenario runs cold (after an invalidation) and warm (straight after, no
        // invalidation), for every representation
        tx.invalidate_all().await.unwrap();
        for pass in ["cold", "warm"] {
            // [B, D, A, A]: B, A, A in order with their own values; D omitted
            let mixed = orders(&tx, &[b_id, d_id, a_id, a_id]);
            for (repr, rows) in three_representations(&mut tx, &mixed, fields).await {
                assert_eq!(
                    rows,
                    vec![row_of(b_id), row_of(a_id), row_of(a_id)],
                    "{repr}({pass}, {fields:?}) on [B, D, A, A]"
                );
            }
        }

        tx.invalidate_all().await.unwrap();
        for pass in ["cold", "warm"] {
            // [D]: empty in every representation
            let d_only = orders(&tx, &[d_id]);
            for (repr, rows) in three_representations(&mut tx, &d_only, fields).await {
                assert!(
                    rows.is_empty(),
                    "{repr}({pass}, {fields:?}) on [D] must be empty, got {rows:?}"
                );
            }
        }

        tx.invalidate_all().await.unwrap();
        for pass in ["cold", "warm"] {
            // surviving singleton B whose prefetch set holds the deleted D and A
            let set_with_d = orders(&tx, &[b_id, d_id, a_id]);
            let b_singleton = set_with_d.slice(Some(0), Some(1), 1).unwrap();
            for (repr, rows) in three_representations(&mut tx, &b_singleton, fields).await {
                assert_eq!(
                    rows,
                    vec![row_of(b_id)],
                    "{repr}({pass}, {fields:?}) on singleton B"
                );
            }
        }
    }

    tx.rollback().await.unwrap();
}

#[tokio::test]
async fn id_only_web_reads_retain_supplied_identities() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();

    let partner_rows = vec![vals(vec![
        ("code", Value::str("P_ID_ONLY")),
        ("name", Value::str("ID Only Partner")),
    ])];
    let partner_ids = create(&mut tx, "bench.partner", &partner_rows).await;

    let order_rows = vec![
        vals(vec![
            ("code", Value::str("O_A2")),
            ("name", Value::str("Order A2")),
            ("partner_id", Value::Int(partner_ids[0])),
        ]),
        vals(vec![
            ("code", Value::str("O_D2")),
            ("name", Value::str("Order D2")),
            ("partner_id", Value::Int(partner_ids[0])),
        ]),
        vals(vec![
            ("code", Value::str("O_B2")),
            ("name", Value::str("Order B2")),
            ("partner_id", Value::Int(partner_ids[0])),
        ]),
    ];
    let order_ids = create(&mut tx, "bench.order", &order_rows).await;
    let a_id = order_ids[0];
    let d_id = order_ids[1];
    let b_id = order_ids[2];

    // Delete D2
    orders(&tx, &[d_id]).unlink(&mut tx).await.unwrap();
    tx.flush().await.unwrap();

    let input_ids = [b_id, d_id, a_id, a_id];

    // Test with {} (Spec::new()) and {'id': {}} (Spec::new().field("id"))
    for spec in [Spec::new(), Spec::new().field("id")] {
        let before_queries = tx.query_count();
        let recs = orders(&tx, &input_ids);
        let got = recs.web_read(&mut tx, spec).await.unwrap();

        assert_eq!(
            tx.query_count(),
            before_queries,
            "an id-only web_read sends no database queries"
        );

        let got_ids: Vec<i64> = got.iter().map(val_id).collect();

        assert_eq!(
            got_ids, input_ids,
            "id-only web_read must retain every supplied id without filtering deleted rows"
        );
    }

    // Empty recordset: no rows, no queries
    for spec in [Spec::new(), Spec::new().field("id")] {
        let before_queries = tx.query_count();
        let empty_recs = orders(&tx, &[]);
        let got = empty_recs.web_read(&mut tx, spec).await.unwrap();

        assert_eq!(
            tx.query_count(),
            before_queries,
            "empty recordset sends no queries"
        );
        assert!(got.is_empty(), "empty recordset returns empty result");
    }

    tx.rollback().await.unwrap();
}

#[tokio::test]
async fn invalid_fields_and_errors_propagate_loudly() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();

    let partner_rows = vec![vals(vec![
        ("code", Value::str("P_ERR")),
        ("name", Value::str("Err Partner")),
    ])];
    let partner_ids = create(&mut tx, "bench.partner", &partner_rows).await;
    let order_rows = vec![vals(vec![
        ("code", Value::str("O_ERR")),
        ("name", Value::str("Err Order")),
        ("partner_id", Value::Int(partner_ids[0])),
    ])];
    let order_ids = create(&mut tx, "bench.order", &order_rows).await;
    let recs = orders(&tx, &order_ids);

    // Invalid field on read raises Programming error
    let err_read = recs
        .read(&mut tx, &["non_existent_field"])
        .await
        .unwrap_err();
    assert_eq!(err_read.kind(), ErrorKind::Programming);
    assert!(
        err_read
            .message()
            .contains("Invalid field 'non_existent_field'"),
        "{err_read}"
    );

    // Invalid field on web_read raises Validation error
    let err_web = recs
        .web_read(&mut tx, Spec::new().field("non_existent_field"))
        .await
        .unwrap_err();
    assert_eq!(err_web.kind(), ErrorKind::Validation);
    assert!(
        err_web
            .message()
            .contains("Invalid field 'non_existent_field'"),
        "{err_web}"
    );

    tx.rollback().await.unwrap();
}

struct ForbiddenOrderRule;

impl erp_testkit::TestRules for ForbiddenOrderRule {
    fn check_model(
        &self,
        _b: &erp_orm::PrincipalBinding<'_>,
        _model: &ModelSpec,
        _op: Operation,
    ) -> Result<()> {
        Ok(())
    }

    fn access_domain(
        &self,
        b: &erp_orm::PrincipalBinding<'_>,
        model: &ModelSpec,
        _op: Operation,
    ) -> Result<Domain> {
        if model.name == "bench.order" && b.principal().subject().get() == 2 {
            Ok(Domain::condition("name", Operator::Ne, "Order Forbidden").unwrap())
        } else {
            Ok(Domain::TRUE)
        }
    }
}

#[tokio::test]
async fn forbidden_existing_record_raises_access_error() {
    let h = erp_testkit::Harness::builder([&base_users::ADDON, &bench::ADDON])
        .rules(ForbiddenOrderRule)
        .build()
        .await
        .unwrap();
    let mut su = h.begin().await.unwrap();
    su.batch("INSERT INTO res_users (id, name, login, active) VALUES (2, 'demo', 'demo', true)")
        .await
        .unwrap();

    let partner_rows = vec![vals(vec![
        ("code", Value::str("P_FORBID")),
        ("name", Value::str("Forbid Partner")),
    ])];
    let partner_ids = create(&mut su, "bench.partner", &partner_rows).await;

    let order_rows = vec![
        vals(vec![
            ("code", Value::str("O_ALLOWED")),
            ("name", Value::str("Order Allowed")),
            ("partner_id", Value::Int(partner_ids[0])),
        ]),
        vals(vec![
            ("code", Value::str("O_FORBIDDEN")),
            ("name", Value::str("Order Forbidden")),
            ("partner_id", Value::Int(partner_ids[0])),
        ]),
        vals(vec![
            ("code", Value::str("O_DELETED")),
            ("name", Value::str("Order Deleted")),
            ("partner_id", Value::Int(partner_ids[0])),
        ]),
    ];
    let order_ids = create(&mut su, "bench.order", &order_rows).await;
    let allowed_id = order_ids[0];
    let forbidden_id = order_ids[1];
    let deleted_id = order_ids[2];

    // Delete O_DELETED
    orders(&su, &[deleted_id]).unlink(&mut su).await.unwrap();
    su.commit().await.unwrap();

    let mut tx = h.begin_as(2).await.unwrap();

    // 1. Reading existing forbidden record must fail with Access error, NOT be filtered as deleted
    let forbidden_recs = orders(&tx, &[forbidden_id]);
    let err_read = forbidden_recs.read(&mut tx, &["name"]).await.unwrap_err();
    assert_eq!(err_read.kind(), ErrorKind::Access, "{err_read}");

    let err_comp = forbidden_recs
        .read(&mut tx, &["partner_name"])
        .await
        .unwrap_err();
    assert_eq!(err_comp.kind(), ErrorKind::Access, "{err_comp}");

    let err_web = forbidden_recs
        .web_read(&mut tx, Spec::new().field("name"))
        .await
        .unwrap_err();
    assert_eq!(err_web.kind(), ErrorKind::Access, "{err_web}");

    // 2. Reading deleted record returns empty result, NOT an Access error
    let deleted_recs = orders(&tx, &[deleted_id]);
    let deleted_read = deleted_recs.read(&mut tx, &["name"]).await.unwrap();
    assert!(
        deleted_read.is_empty(),
        "deleted record must be omitted, not treated as forbidden"
    );
    let deleted_comp = deleted_recs.read(&mut tx, &["partner_name"]).await.unwrap();
    assert!(
        deleted_comp.is_empty(),
        "deleted record must be omitted, not treated as forbidden"
    );
    let deleted_web = deleted_recs
        .web_read(&mut tx, Spec::new().field("name"))
        .await
        .unwrap();
    assert!(
        deleted_web.is_empty(),
        "deleted record must be omitted, not treated as forbidden"
    );

    // 3. Mixed [allowed, deleted] returns [allowed]
    let mixed_deleted = orders(&tx, &[allowed_id, deleted_id]);
    let res = mixed_deleted.read(&mut tx, &["name"]).await.unwrap();
    assert_eq!(res.len(), 1);
    assert_eq!(val_id(res[0].get("id").unwrap()), allowed_id);

    // 4. Mixed [allowed, forbidden] fails with Access error
    let mixed_forbidden = orders(&tx, &[allowed_id, forbidden_id]);
    let err_mixed = mixed_forbidden.read(&mut tx, &["name"]).await.unwrap_err();
    assert_eq!(err_mixed.kind(), ErrorKind::Access, "{err_mixed}");

    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn compute_callback_non_missing_error_propagates() {
    let h = erp_testkit::Harness::builder([&base_users::ADDON, &test_compute::ADDON])
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    let model = tx.env().model::<ComputeRaises>().unwrap();
    let rec = model
        .create(
            &mut tx,
            vec![model.vals().set(ComputeRaises::FOO, "initial")],
        )
        .await
        .unwrap();

    // Trigger error condition: foo = "boom"
    rec.set(&mut tx, ComputeRaises::FOO, "boom").await.unwrap();

    // Read on "bar" triggers compute callback which returns Error::user("boom")
    let err_read = rec.as_records().read(&mut tx, &["bar"]).await.unwrap_err();
    assert_eq!(err_read.kind(), ErrorKind::UserError);
    assert_eq!(err_read.message(), "boom");

    // web_read on "bar" also propagates Error::user("boom")
    let err_web = rec
        .as_records()
        .web_read(&mut tx, Spec::new().field("bar"))
        .await
        .unwrap_err();
    assert_eq!(err_web.kind(), ErrorKind::UserError);
    assert_eq!(err_web.message(), "boom");

    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// Counterexample to's stored-dependency rule: a compute without stored dependencies
/// has no existence probe (`probe` is `None` in `Records::fetch_plan`/`fetch_apply`), so a
/// deleted record is not filtered out: its id is returned and the constant value computed for
/// it. No query is sent for the column fetch either. The case itself does not specify this
/// (note); the test pins the behaviour so a change is a conscious one.
#[tokio::test]
async fn constant_compute_counterexample() {
    let h = erp_testkit::Harness::builder([&base_users::ADDON, &test_compute::ADDON])
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    let model = tx.env().model::<CtxKey>().unwrap();
    let kept = model.create(&mut tx, vec![model.vals()]).await.unwrap();
    let gone = model.create(&mut tx, vec![model.vals()]).await.unwrap();
    let (kept_id, gone_id) = (
        val_id(&kept.as_records().value(&mut tx, "id").await.unwrap()),
        val_id(&gone.as_records().value(&mut tx, "id").await.unwrap()),
    );
    gone.as_records().unlink(&mut tx).await.unwrap();
    tx.flush().await.unwrap();

    let both = tx
        .env()
        .records("test_orm.ctx_key")
        .unwrap()
        .browse([kept_id, gone_id])
        .unwrap();
    let before_queries = tx.query_count();
    let read_result = both.read(&mut tx, &["all"]).await.unwrap();
    let read_ids: Vec<i64> = read_result
        .iter()
        .map(|r| val_id(r.get("id").unwrap()))
        .collect();
    assert_eq!(
        read_ids,
        [kept_id, gone_id],
        "a constant compute has no probe: the deleted record's id is returned"
    );
    for row in &read_result {
        assert_eq!(row.get("all").unwrap(), &Value::str("x"));
    }

    let web_result = both
        .web_read(&mut tx, Spec::new().field("all"))
        .await
        .unwrap();
    let web_ids: Vec<i64> = web_result.iter().map(val_id).collect();
    assert_eq!(web_ids, [kept_id, gone_id]);

    // Constant compute without stored dependencies performs no database queries for column fetch
    assert_eq!(
        tx.query_count(),
        before_queries,
        "constant compute with no stored dependencies requires no SQL queries"
    );

    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// case leaves this outside its fixtures). Reading the computed field on the deleted singleton
/// through the field accessor is an error, never an empty value.
#[tokio::test]
async fn missing_asked_record_still_raises_missing() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let partner_ids = create(
        &mut tx,
        "bench.partner",
        &[vals(vec![
            ("code", Value::str("P_MA")),
            ("name", Value::str("Partner MA")),
        ])],
    )
    .await;
    let order_ids = create(
        &mut tx,
        "bench.order",
        &[vals(vec![
            ("code", Value::str("O_MA")),
            ("name", Value::str("Order MA")),
            ("partner_id", Value::Int(partner_ids[0])),
        ])],
    )
    .await;
    orders(&tx, &order_ids).unlink(&mut tx).await.unwrap();
    tx.flush().await.unwrap();
    tx.invalidate_all().await.unwrap();

    let err = orders(&tx, &order_ids)
        .value(&mut tx, "partner_name")
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Missing, "{err}");
    assert!(
        err.message()
            .contains("Record does not exist or has been deleted."),
        "{err}"
    );
    assert!(
        err.message()
            .contains(&format!("bench.order({},)", order_ids[0])),
        "{err}"
    );

    tx.rollback().await.unwrap();
}

/// field accessor, which has no existence filter of its own). B's accessor read widens to the
/// prefetch set [B, D, A]; D was removed by raw SQL, so no cache entry knows about it.
#[tokio::test]
async fn deleted_sibling_of_prefetch_set_does_not_break_the_asked_record() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let partner_rows: Vec<_> = ["A", "D", "B"]
        .iter()
        .map(|t| {
            vals(vec![
                ("code", Value::str(format!("P_SIB_{t}"))),
                ("name", Value::str(format!("Sibling partner {t}"))),
            ])
        })
        .collect();
    let partner_ids = create(&mut tx, "bench.partner", &partner_rows).await;
    let order_rows: Vec<_> = ["A", "D", "B"]
        .iter()
        .enumerate()
        .map(|(i, t)| {
            vals(vec![
                ("code", Value::str(format!("O_SIB_{t}"))),
                ("name", Value::str(format!("Sibling order {t}"))),
                ("partner_id", Value::Int(partner_ids[i])),
            ])
        })
        .collect();
    let ids = create(&mut tx, "bench.order", &order_rows).await;
    let (a_id, d_id, b_id) = (ids[0], ids[1], ids[2]);

    tx.flush().await.unwrap();
    // raw SQL: the ORM is not told, so D stays in the prefetch set
    tx.batch(&format!("DELETE FROM bench_order WHERE id = {d_id}"))
        .await
        .unwrap();
    tx.invalidate_all().await.unwrap();

    let set = orders(&tx, &[b_id, d_id, a_id]);
    let b = set.slice(Some(0), Some(1), 1).unwrap();
    let got = b.value(&mut tx, "partner_name").await.unwrap();
    assert_eq!(got, Value::str("Sibling partner B"));

    tx.rollback().await.unwrap();
}
