// SPDX-License-Identifier: MIT OR Apache-2.0
//! Effective company values across SQL operations.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

use erp_fields::{Decimal, Error, ErrorKind, Value};
use erp_orm::{Env, ReadGroup, Records, RecordsAs, SearchOptions, Transaction, ir_default};
use erp_sdk::{Domain, Operator};
use erp_test_models::base_users;
use erp_testkit::Harness;

mod fixture {
    use erp_sdk::prelude::*;
    erp_sdk::addon!(name = "property_runtime", depends = ["base"]);
    #[model(
        name = "property.runtime",
        addon = "property_runtime",
        row_version = true
    )]
    pub struct Property {
        #[field(char)]
        pub name: Option<Arc<str>>,
        #[field(char, company_dependent = true)]
        pub note: Option<Arc<str>>,
        #[field(boolean, company_dependent = true)]
        pub truth: bool,
        #[field(integer, company_dependent = true)]
        pub count: i32,
        #[field(float, company_dependent = true)]
        pub ratio: f64,
        #[field(float, digits = (0, 3), company_dependent = true)]
        pub exact: Decimal,
        #[field(float, digits = "Property Precision", company_dependent = true)]
        pub named: Decimal,
        #[field(text, company_dependent = true)]
        pub text: Option<Arc<str>>,
        #[field(selection, selection = [("a", "First"), ("b", "Second")], company_dependent = true)]
        pub choice: Option<Arc<str>>,
        #[field(date, company_dependent = true)]
        pub day: Option<Date>,
        #[field(datetime, company_dependent = true)]
        pub moment: Option<DateTime>,
        #[field(many2one, comodel = "property.runtime", company_dependent = true)]
        pub tag_id: Option<RecId>,
        #[field(
            many2one,
            comodel = "property.runtime",
            company_dependent = true,
            ondelete = "restrict"
        )]
        pub restricted_id: Option<RecId>,
        #[field(many2one, comodel = "property.ordered")]
        pub plain_tag_id: Option<RecId>,
        #[field(many2one, comodel = "res.company", company_dependent = true)]
        pub company_prop: Option<RecId>,
        #[field(html, company_dependent = true)]
        pub html: Option<Arc<str>>,
        #[field(many2one, comodel = "res.currency")]
        pub currency_id: Option<RecId>,
        #[field(monetary, currency_field = "currency_id", company_dependent = true)]
        pub amount: Decimal,
    }
    #[model(name = "property.ordered", addon = "property_runtime", order = "note")]
    pub struct OrderedProperty {
        #[field(char, company_dependent = true)]
        pub note: Option<Arc<str>>,
    }
    #[model(name = "property.required", addon = "property_runtime")]
    pub struct RequiredProperty {
        #[field(char, company_dependent = true, required)]
        pub note: Option<Arc<str>>,
    }
}

async fn harness() -> Harness {
    Harness::builder([&base_users::ADDON, &erp_addon_base::ADDON, &fixture::ADDON])
        .build()
        .await
        .unwrap()
}
fn company(env: &Env, id: i64) -> Env {
    env.with_context([("allowed_company_ids", Value::list([Value::Int(id)]))])
}
fn vals(items: &[(&str, Value)]) -> std::collections::BTreeMap<String, Value> {
    items
        .iter()
        .map(|(k, v)| ((*k).to_owned(), v.clone()))
        .collect()
}
async fn create(tx: &mut Transaction, env: &Env, items: &[(&str, Value)]) -> Records {
    env.records("property.runtime")
        .unwrap()
        .create_values(tx, &[vals(items)])
        .await
        .unwrap()
}
async fn default(
    tx: &mut Transaction,
    env: &Env,
    value: &str,
    company: Option<i64>,
    user: Option<i64>,
) {
    ir_default::set(
        tx,
        env,
        "property.runtime",
        "note",
        &Value::str(value),
        user.map(|id| erp_fields::RecId::from_real(id).unwrap()),
        company.map(|id| erp_fields::RecId::from_real(id).unwrap()),
        None,
    )
    .await
    .unwrap();
}
async fn seed(tx: &mut Transaction) -> i64 {
    let currency = erp_testkit::fixtures::seed_currency_in(tx).await.unwrap();
    tx.execute("INSERT INTO res_company (id, name, currency_id) VALUES (1, 'One', $1), (2, 'Two', $1), (3, 'Three', $1) ON CONFLICT DO NOTHING", &[&currency]).await.unwrap();
    currency
}
fn error(error: Error, kind: ErrorKind, message: &str) {
    assert_eq!(error.kind(), kind);
    assert_eq!(error.message(), message);
}

fn cond(field: &str, op: Operator, value: impl Into<erp_domain::Value>) -> Domain {
    Domain::condition(field, op, value).unwrap()
}

async fn check(tx: &mut Transaction, records: &Records, domain: Domain) {
    let expected = records.filtered_domain(tx, &domain).await.unwrap();
    let scope = cond(
        "id",
        Operator::In,
        erp_domain::Value::list(records.ids().iter().copied()),
    );
    let got = records
        .search(tx, &(scope.clone() & domain.clone()))
        .await
        .unwrap();
    let mut expected_ids = expected.ids().to_vec();
    let mut got_ids = got.ids().to_vec();
    expected_ids.sort_unstable();
    got_ids.sort_unstable();
    assert_eq!(got_ids, expected_ids, "{domain:?}");
    assert_eq!(
        records
            .search_count(tx, &(scope.clone() & domain.clone()), None)
            .await
            .unwrap(),
        got.len() as u64
    );
    let complement = records
        .search(tx, &(scope & !domain.clone()))
        .await
        .unwrap();
    assert_eq!(got.len() + complement.len(), records.len(), "{domain:?}");
    assert!(complement.ids().iter().all(|id| !got.ids().contains(id)));
}

#[tokio::test]
async fn scalar_search_matches_effective_reads_and_complements() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    seed(&mut tx).await;
    let env = company(&tx.env(), 1);
    let model = env.records("property.runtime").unwrap();
    let rows = [
        vals(&[
            ("note", Value::str("cedar")),
            ("truth", Value::Bool(true)),
            ("count", Value::Int(17)),
            ("ratio", Value::Float(2.75)),
        ]),
        vals(&[
            ("note", Value::str("birch")),
            ("truth", Value::Bool(true)),
            ("count", Value::Int(-3)),
            ("ratio", Value::Float(-1.5)),
        ]),
        vals(&[
            ("note", Value::False),
            ("truth", Value::False),
            ("count", Value::Int(0)),
            ("ratio", Value::Float(0.0)),
        ]),
        vals(&[]),
    ];
    let records = model.create_values(&mut tx, &rows).await.unwrap();
    for (phase, fallback) in [None, Some(Value::False), Some(Value::str("cedar"))]
        .into_iter()
        .enumerate()
    {
        if let Some(value) = fallback {
            ir_default::set(
                &mut tx,
                &env,
                "property.runtime",
                "note",
                &value,
                None,
                None,
                None,
            )
            .await
            .unwrap();
        }
        if phase > 0 {
            for (field, truthy) in [
                ("truth", Value::Bool(true)),
                ("count", Value::Int(17)),
                ("ratio", Value::Float(2.75)),
            ] {
                let value = if phase == 1 { Value::False } else { truthy };
                ir_default::set(
                    &mut tx,
                    &env,
                    "property.runtime",
                    field,
                    &value,
                    None,
                    None,
                    None,
                )
                .await
                .unwrap();
            }
        }
        // Reapply explicit values after changing defaults; the fourth record keeps inheriting.
        for (id, row) in records.ids()[..3].iter().zip(&rows[..3]) {
            model
                .browse([*id])
                .unwrap()
                .write_values(&mut tx, row)
                .await
                .unwrap();
        }
        for op in [Operator::Eq, Operator::Ne] {
            for value in [true, false] {
                check(&mut tx, &records, cond("truth", op, value)).await;
            }
        }
        for op in [
            Operator::Eq,
            Operator::Ne,
            Operator::Lt,
            Operator::Le,
            Operator::Gt,
            Operator::Ge,
        ] {
            for value in [17, -3, 0] {
                check(&mut tx, &records, cond("count", op, value)).await;
            }
            for value in [2.75, -1.5, 0.0] {
                check(&mut tx, &records, cond("ratio", op, value)).await;
            }
        }
        for op in [
            Operator::Eq,
            Operator::Ne,
            Operator::Like,
            Operator::ILike,
            Operator::NotLike,
            Operator::NotILike,
        ] {
            for value in ["cedar", "birch"] {
                check(&mut tx, &records, cond("note", op, value)).await;
            }
        }
        for op in [Operator::In, Operator::NotIn] {
            for values in [
                vec![],
                vec![erp_domain::Value::FALSE],
                vec!["cedar".into(), erp_domain::Value::FALSE],
                vec!["cedar".into(), "birch".into()],
            ] {
                check(
                    &mut tx,
                    &records,
                    cond("note", op, erp_domain::Value::list(values)),
                )
                .await;
            }
        }
        let two = records.with_env(company(&env, 2));
        check(&mut tx, &two, cond("note", Operator::Eq, "cedar")).await;
        check(&mut tx, &records, cond("note", Operator::Eq, false)).await;
    }
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn effective_order_groups_and_exact_aggregates() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let currency = seed(&mut tx).await;
    let env = company(&tx.env(), 1);
    default(&mut tx, &env, "middle", None, None).await;
    let rows = [
        vals(&[
            ("note", Value::str("zulu")),
            ("count", Value::Int(8)),
            ("currency_id", Value::Int(currency)),
            (
                "amount",
                Value::Decimal(Decimal::from_str_exact("9007199254740993.12").unwrap()),
            ),
            (
                "exact",
                Value::Decimal(Decimal::from_str_exact("9007199254740993.125").unwrap()),
            ),
        ]),
        vals(&[("note", Value::str("alpha")), ("count", Value::Int(2))]),
        vals(&[]),
    ];
    let model = env.records("property.runtime").unwrap();
    let rec = model.create_values(&mut tx, &rows).await.unwrap();
    let got = model
        .search_with(
            &mut tx,
            &Domain::TRUE,
            &SearchOptions::new().order("note").limit(2).offset(1),
        )
        .await
        .unwrap();
    assert_eq!(got.ids(), &[rec.ids()[2], rec.ids()[0]]);
    let groups = model
        ._read_group(
            &mut tx,
            ReadGroup::new(Domain::TRUE)
                .groupby(["note"])
                .aggregates(["count:sum"])
                .order("note"),
        )
        .await
        .unwrap();
    let got: Vec<_> = groups
        .rows()
        .iter()
        .map(|row| {
            row.iter()
                .map(|v| v.to_value(RecordsAs::Id))
                .collect::<Vec<_>>()
        })
        .collect();
    assert_eq!(
        got,
        vec![
            vec![Value::str("alpha"), Value::Int(2)],
            vec![Value::str("middle"), Value::Int(0)],
            vec![Value::str("zulu"), Value::Int(8)]
        ]
    );
    let total = model
        ._read_group(
            &mut tx,
            ReadGroup::new(Domain::TRUE).aggregates(["exact:sum"]),
        )
        .await
        .unwrap();
    assert_eq!(
        total.rows()[0][0].to_value(RecordsAs::Id),
        Value::Decimal(Decimal::from_str_exact("9007199254740993.125").unwrap())
    );
    let money = model
        ._read_group(
            &mut tx,
            ReadGroup::new(Domain::TRUE).aggregates(["amount:sum"]),
        )
        .await
        .unwrap();
    assert_eq!(
        money.rows()[0][0].to_value(RecordsAs::Id),
        Value::Decimal(Decimal::from_str_exact("9007199254740993.12").unwrap())
    );
    check(
        &mut tx,
        &rec,
        cond("amount", Operator::Eq, "9007199254740993.12"),
    )
    .await;
    let two = model.with_env(company(&env, 2));
    let got = two
        .search_with(&mut tx, &Domain::TRUE, &SearchOptions::new().order("note"))
        .await
        .unwrap();
    assert_eq!(got.ids(), rec.ids());
    default(&mut tx, &env, "aardvark", None, None).await;
    let got = model
        .search_with(&mut tx, &Domain::TRUE, &SearchOptions::new().order("note"))
        .await
        .unwrap();
    assert_eq!(got.ids(), &[rec.ids()[2], rec.ids()[1], rec.ids()[0]]);
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn relations_use_effective_ids_and_missing_rows_do_not_get_defaults() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    seed(&mut tx).await;
    let env = company(&tx.env(), 1);
    default(&mut tx, &env, "fallback", None, None).await;
    let target = create(
        &mut tx,
        &env,
        &[("name", Value::str("target")), ("note", Value::str("leaf"))],
    )
    .await;
    let owner = create(
        &mut tx,
        &env,
        &[
            ("name", Value::str("owner")),
            ("tag_id", Value::Int(target.ids()[0])),
        ],
    )
    .await;
    let absent = create(&mut tx, &env, &[("name", Value::str("absent"))]).await;
    let model = env.records("property.runtime").unwrap();
    let got = model
        .search(&mut tx, &cond("tag_id.note", Operator::Eq, "leaf"))
        .await
        .unwrap();
    assert_eq!(got.ids(), owner.ids());
    let got = model
        .search(&mut tx, &cond("tag_id.note", Operator::Eq, "fallback"))
        .await
        .unwrap();
    assert!(got.is_empty());
    check(
        &mut tx,
        &owner,
        cond(
            "tag_id",
            Operator::Any,
            erp_domain::Value::Domain(cond("name", Operator::Eq, "target")),
        ),
    )
    .await;
    let groups = model
        ._read_group(
            &mut tx,
            ReadGroup::new(cond(
                "id",
                Operator::In,
                erp_domain::Value::list([owner.ids()[0], absent.ids()[0]]),
            ))
            .groupby(["tag_id.note"])
            .aggregates(["__count"]),
        )
        .await
        .unwrap();
    assert_eq!(groups.rows().len(), 2);
    tx.flush().await.unwrap();
    tx.execute(
        "DELETE FROM property_runtime WHERE id=$1",
        &[&target.ids()[0]],
    )
    .await
    .unwrap();
    let got = model
        .search(
            &mut tx,
            &(cond("id", Operator::Eq, owner.ids()[0]) & cond("tag_id", Operator::Eq, false)),
        )
        .await
        .unwrap();
    assert_eq!(got.ids(), owner.ids());
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn temporal_and_selection_values_search_after_flush() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    seed(&mut tx).await;
    let env = company(&tx.env(), 1);
    let model = env.records("property.runtime").unwrap();
    let rows = [
        vals(&[
            ("day", Value::str("2026-02-03")),
            ("moment", Value::str("2026-02-03 04:05:06")),
            ("choice", Value::str("a")),
        ]),
        vals(&[
            ("day", Value::str("2026-02-04")),
            ("moment", Value::str("2026-02-04 04:05:06")),
            ("choice", Value::str("b")),
        ]),
        vals(&[("day", Value::False), ("moment", Value::False)]),
        vals(&[]),
    ];
    let records = model.create_values(&mut tx, &rows).await.unwrap();
    for phase in 0..3 {
        if phase > 0 {
            for (field, truthy) in [
                ("day", Value::str("2026-02-03")),
                ("moment", Value::str("2026-02-03 04:05:06")),
            ] {
                let value = if phase == 1 { Value::False } else { truthy };
                ir_default::set(
                    &mut tx,
                    &env,
                    "property.runtime",
                    field,
                    &value,
                    None,
                    None,
                    None,
                )
                .await
                .unwrap();
            }
        }
        for (id, row) in records.ids()[..3].iter().zip(&rows[..3]) {
            model
                .browse([*id])
                .unwrap()
                .write_values(&mut tx, row)
                .await
                .unwrap();
        }
        tx.flush().await.unwrap();
        for op in [
            Operator::Eq,
            Operator::Ne,
            Operator::Lt,
            Operator::Le,
            Operator::Gt,
            Operator::Ge,
        ] {
            for day in ["2026-02-03", "2026-02-04"] {
                check(&mut tx, &records, cond("day", op, day)).await;
            }
            for moment in ["2026-02-03 04:05:06", "2026-02-04 04:05:06"] {
                check(&mut tx, &records, cond("moment", op, moment)).await;
            }
        }
        for field in ["day", "moment", "choice"] {
            check(&mut tx, &records, cond(field, Operator::Eq, false)).await;
        }
    }
    let ordered = model
        .search_with(
            &mut tx,
            &Domain::TRUE,
            &SearchOptions::new().order("choice"),
        )
        .await
        .unwrap();
    assert_eq!(ordered.ids(), records.ids());
    let missing = model.with_env(env.with_context([("allowed_company_ids", Value::list([]))]));
    error(
        missing
            .search(&mut tx, &cond("note", Operator::Eq, "x"))
            .await
            .unwrap_err(),
        ErrorKind::Validation,
        "company-dependent fields require a current company",
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn relation_search_grid_inherits_defaults_and_traverses_hierarchies() {
    use erp_domain::Value as D;
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    seed(&mut tx).await;
    let env = company(&tx.env(), 1);
    let a = create(&mut tx, &env, &[("name", Value::str("cedar"))]).await;
    let b = create(&mut tx, &env, &[("name", Value::str("birch"))]).await;
    let model = env.records("property.runtime").unwrap();
    let records = model
        .create_values(&mut tx, &[vals(&[]), vals(&[]), vals(&[]), vals(&[])])
        .await
        .unwrap();
    for fallback in [None, Some(Value::False), Some(Value::Int(a.ids()[0]))] {
        if let Some(value) = fallback {
            ir_default::set(
                &mut tx,
                &env,
                "property.runtime",
                "tag_id",
                &value,
                None,
                None,
                None,
            )
            .await
            .unwrap();
        }
        for (id, value) in records.ids()[..3].iter().zip([
            Value::Int(a.ids()[0]),
            Value::Int(b.ids()[0]),
            Value::False,
        ]) {
            model
                .browse([*id])
                .unwrap()
                .write_values(&mut tx, &vals(&[("tag_id", value)]))
                .await
                .unwrap();
        }
        for op in [Operator::Eq, Operator::Ne] {
            for value in [D::Int(a.ids()[0]), D::Int(b.ids()[0]), D::FALSE] {
                check(&mut tx, &records, cond("tag_id", op, value)).await;
            }
        }
        for op in [
            Operator::Like,
            Operator::ILike,
            Operator::NotLike,
            Operator::NotILike,
        ] {
            for name in ["cedar", "birch"] {
                check(&mut tx, &records, cond("tag_id", op, name)).await;
            }
        }
        for op in [Operator::In, Operator::NotIn] {
            for values in [
                vec![],
                vec![D::FALSE],
                vec![D::Int(a.ids()[0]), D::FALSE],
                vec![D::Int(a.ids()[0]), D::Int(b.ids()[0])],
            ] {
                check(&mut tx, &records, cond("tag_id", op, D::list(values))).await;
            }
        }
        for op in [Operator::Any, Operator::NotAny] {
            for sub in [
                Domain::TRUE,
                cond("name", Operator::Eq, "cedar"),
                cond("name", Operator::Eq, false),
            ] {
                check(&mut tx, &records, cond("tag_id", op, D::Domain(sub))).await;
            }
        }
    }
    tx.execute("UPDATE res_company SET parent_id = CASE id WHEN 2 THEN 1 WHEN 3 THEN 2 ELSE NULL END, parent_path = CASE id WHEN 1 THEN '1/' WHEN 2 THEN '1/2/' WHEN 3 THEN '1/2/3/' END WHERE id IN (1,2,3)", &[]).await.unwrap();
    for (id, value) in records.ids()[..2].iter().zip([2, 3]) {
        model
            .browse([*id])
            .unwrap()
            .write_values(&mut tx, &vals(&[("company_prop", Value::Int(value))]))
            .await
            .unwrap();
    }
    for op in [Operator::ChildOf, Operator::ParentOf] {
        for id in [1, 2, 3] {
            check(&mut tx, &records, cond("company_prop", op, id)).await;
        }
    }
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn warmed_property_query_plans_retain_field_authorization() {
    let h = Harness::builder([&base_users::ADDON, &erp_addon_base::ADDON, &fixture::ADDON])
        .policy(erp_testkit::FieldPolicy::new().deny(
            "property.runtime",
            "note",
            erp_orm::FieldOp::Read,
        ))
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    seed(&mut tx).await;
    let env = company(&tx.env(), 1);
    let rec = create(&mut tx, &env, &[("note", Value::str("private"))]).await;
    let domain = cond("note", Operator::Eq, "private");
    assert_eq!(rec.search(&mut tx, &domain).await.unwrap().ids(), rec.ids());
    rec.search_with(&mut tx, &Domain::TRUE, &SearchOptions::new().order("note"))
        .await
        .unwrap();
    let reader = rec.with_env(env.with_user(2));
    let message = format!(
        "You do not have enough rights to access the field \"note\" on {} (property.runtime). Please contact your system administrator.\n\nOperation: read",
        rec.spec().description
    );
    error(
        reader.search(&mut tx, &domain).await.unwrap_err(),
        ErrorKind::Access,
        &message,
    );
    error(
        reader
            .search_with(&mut tx, &Domain::TRUE, &SearchOptions::new().order("note"))
            .await
            .unwrap_err(),
        ErrorKind::Access,
        &message,
    );
    error(
        reader
            ._read_group(&mut tx, ReadGroup::new(Domain::TRUE).groupby(["note"]))
            .await
            .unwrap_err(),
        ErrorKind::Access,
        &message,
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn default_queries_preserve_snapshot_and_rollback_semantics() {
    let h = harness().await;
    let peer = h.orm().rebuild(h.registry().clone()).build().unwrap();
    let mut setup = h.begin().await.unwrap();
    seed(&mut setup).await;
    let env = company(&setup.env(), 1);
    let rec = create(&mut setup, &env, &[]).await;
    let id = rec.ids()[0];
    default(&mut setup, &env, "old", Some(1), None).await;
    setup.commit().await.unwrap();
    let mut old = peer.begin(erp_orm::SUPERUSER_ID).await.unwrap();
    let rec = company(&old.env(), 1)
        .records("property.runtime")
        .unwrap()
        .browse([id])
        .unwrap();
    assert_eq!(
        rec.search(&mut old, &cond("note", Operator::Eq, "old"))
            .await
            .unwrap()
            .ids(),
        &[id]
    );
    let mut writer = h.begin().await.unwrap();
    let env = company(&writer.env(), 1);
    default(&mut writer, &env, "new", Some(1), None).await;
    writer.commit().await.unwrap();
    assert_eq!(
        rec.search(&mut old, &cond("note", Operator::Eq, "old"))
            .await
            .unwrap()
            .ids(),
        &[id]
    );
    assert!(
        rec.search(&mut old, &cond("note", Operator::Eq, "new"))
            .await
            .unwrap()
            .is_empty()
    );
    old.commit().await.unwrap();
    assert_eq!(
        rec.search(&mut old, &cond("note", Operator::Eq, "new"))
            .await
            .unwrap()
            .ids(),
        &[id]
    );
    let env = company(&old.env(), 1);
    let result: erp_fields::Result<()> = old
        .savepoint(async |tx| {
            default(tx, &env, "temporary", Some(1), None).await;
            assert_eq!(
                rec.search(tx, &cond("note", Operator::Eq, "temporary"))
                    .await
                    .unwrap()
                    .ids(),
                &[id]
            );
            Err(Error::validation("rollback query fallback"))
        })
        .await;
    error(
        result.unwrap_err(),
        ErrorKind::Validation,
        "rollback query fallback",
    );
    assert_eq!(
        rec.search(&mut old, &cond("note", Operator::Eq, "new"))
            .await
            .unwrap()
            .ids(),
        &[id]
    );
    drop(old);
    drop(writer);
    drop(setup);
    drop(peer);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn having_grouping_sets_and_cursor_boundaries_use_scalar_values() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    seed(&mut tx).await;
    let env = company(&tx.env(), 1);
    default(&mut tx, &env, "inherited", None, None).await;
    let model = env.records("property.runtime").unwrap();
    model
        .create_values(
            &mut tx,
            &[
                vals(&[("note", Value::str("a")), ("count", Value::Int(3))]),
                vals(&[("note", Value::str("a")), ("count", Value::Int(4))]),
                vals(&[]),
            ],
        )
        .await
        .unwrap();
    let groups = model
        ._read_group(
            &mut tx,
            ReadGroup::new(Domain::TRUE)
                .groupby(["note"])
                .aggregates(["count:sum"])
                .having(vec![Value::list([
                    Value::str("count:sum"),
                    Value::str(">"),
                    Value::Int(5),
                ])]),
        )
        .await
        .unwrap();
    assert_eq!(groups.rows().len(), 1);
    assert_eq!(groups.rows()[0][0].to_value(RecordsAs::Id), Value::str("a"));
    assert_eq!(groups.rows()[0][1].to_value(RecordsAs::Id), Value::Int(7));
    let hidden_sum = model
        ._read_group(
            &mut tx,
            ReadGroup::new(Domain::TRUE)
                .groupby(["note"])
                .having(vec![Value::list([
                    Value::str("count:sum"),
                    Value::str(">"),
                    Value::Int(5),
                ])]),
        )
        .await
        .unwrap();
    assert_eq!(hidden_sum.rows().len(), 1);
    let sets = model
        ._read_grouping_sets(
            &mut tx,
            erp_orm::ReadGroupingSets::new(Domain::TRUE, [vec!["note"], vec![]])
                .aggregates(["count:sum"]),
        )
        .await
        .unwrap();
    assert_eq!(sets.len(), 2);
    assert_eq!(sets[0].rows().len(), 2);
    assert_eq!(sets[1].rows()[0][0].to_value(RecordsAs::Id), Value::Int(7));
    error(
        model
            .search_after(
                &mut tx,
                &Domain::TRUE,
                &SearchOptions::new().order("note").limit(1),
                None,
            )
            .await
            .unwrap_err(),
        ErrorKind::Programming,
        "keyset pagination cannot order by property.runtime.note: company-dependent keys require prepared company values until a later release; use offset pagination",
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn grouping_an_ordinary_link_does_not_prepare_its_unused_property_order() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let env = tx.env();
    let target = env
        .records("property.ordered")
        .unwrap()
        .create_values(&mut tx, &[vals(&[])])
        .await
        .unwrap();
    let model = env.records("property.runtime").unwrap();
    model
        .create_values(
            &mut tx,
            &[vals(&[("plain_tag_id", Value::Int(target.ids()[0]))])],
        )
        .await
        .unwrap();
    let groups = model
        ._read_group(
            &mut tx,
            ReadGroup::new(Domain::TRUE)
                .groupby(["plain_tag_id"])
                .aggregates(["__count"]),
        )
        .await
        .unwrap();
    assert_eq!(groups.rows().len(), 1);
    assert_eq!(
        groups.rows()[0][0].to_value(RecordsAs::Id),
        Value::Int(target.ids()[0])
    );
    drop(tx);
    h.cleanup().await.unwrap();
}
