// SPDX-License-Identifier: MIT OR Apache-2.0
//! Runtime property storage; SQL query parity remains with QUERY.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

mod support;

use erp_fields::{Cell, Decimal, Error, ErrorKind, Value};
use erp_orm::{Env, ReadGroup, Records, RecordsAs, SearchOptions, Transaction, ir_default};
use erp_test_models::base_users;
use erp_testkit::Harness;
use serde_json::json;

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
        #[field(html, company_dependent = true)]
        pub html: Option<Arc<str>>,
        #[field(many2one, comodel = "res.currency")]
        pub currency_id: Option<RecId>,
        #[field(monetary, currency_field = "currency_id", company_dependent = true)]
        pub amount: Decimal,
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

#[tokio::test]
async fn scalar_crud_projects_every_public_read_and_keeps_company_deltas() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    seed(&mut tx).await;
    let env = tx.env();
    let one = company(&env, 1);
    let rec = create(
        &mut tx,
        &one,
        &[("name", Value::str("Record")), ("note", Value::str("one"))],
    )
    .await;
    let two = rec.with_env(company(&env, 2));
    let three = rec.with_env(company(&env, 3));
    assert_eq!(two.value(&mut tx, "note").await.unwrap(), Value::False);
    two.write_values(&mut tx, &vals(&[("note", Value::str("two"))]))
        .await
        .unwrap();
    rec.write_values(&mut tx, &vals(&[("note", Value::str("one updated"))]))
        .await
        .unwrap();
    assert_eq!(
        rec.value(&mut tx, "note").await.unwrap(),
        Value::str("one updated")
    );
    assert_eq!(two.value(&mut tx, "note").await.unwrap(), Value::str("two"));
    assert_eq!(three.value(&mut tx, "note").await.unwrap(), Value::False);
    let read = two.read(&mut tx, &["name", "note"]).await.unwrap();
    assert_eq!(read[0].get("note"), Some(&Value::str("two")));
    tx.flush().await.unwrap();
    let stored = tx
        .query(
            "SELECT note FROM property_runtime WHERE id=$1",
            &[&rec.ids()[0]],
        )
        .await
        .unwrap();
    assert_eq!(
        stored[0].get::<_, serde_json::Value>(0),
        json!({"1":"one updated", "2":"two"})
    );
    let typed = one
        .model::<fixture::Property>()
        .unwrap()
        .browse(rec.ids())
        .unwrap();
    assert_eq!(
        typed.get(&mut tx, fixture::Property::NOTE).await.unwrap(),
        Some("one updated".into())
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn omitted_and_falsy_numeric_values_leave_null_columns() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    seed(&mut tx).await;
    let env = company(&tx.env(), 1);
    let rec = create(&mut tx, &env, &[("name", Value::str("Falsy"))]).await;
    for phase in 0..2 {
        if phase == 1 {
            rec.write_values(
                &mut tx,
                &vals(&[
                    ("truth", Value::Bool(false)),
                    ("count", Value::Int(0)),
                    ("ratio", Value::Float(0.0)),
                ]),
            )
            .await
            .unwrap();
            tx.flush().await.unwrap();
        }
        let row = tx.query("SELECT truth IS NULL, count IS NULL, ratio IS NULL FROM property_runtime WHERE id=$1", &[&rec.ids()[0]]).await.unwrap();
        assert!(row[0].get::<_, bool>(0));
        assert!(row[0].get::<_, bool>(1));
        assert!(row[0].get::<_, bool>(2));
        for cid in [1, 2, 3] {
            let r = rec.with_env(company(&env, cid));
            assert_eq!(r.cell(&mut tx, "truth").await.unwrap(), Cell::Bool(false));
            assert_eq!(r.cell(&mut tx, "count").await.unwrap(), Cell::Int(0));
            assert_eq!(r.cell(&mut tx, "ratio").await.unwrap(), Cell::Float(0.0));
        }
    }
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn runtime_defaults_are_not_caller_defaults_and_rollback_reprepares() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    seed(&mut tx).await;
    tx.execute("INSERT INTO res_users (id, login, name, active, share) VALUES (2, 'property-user', 'Reader', true, false) ON CONFLICT DO NOTHING", &[]).await.unwrap();
    let env = company(&tx.env(), 1);
    let rec = create(&mut tx, &env, &[("name", Value::str("Inherited"))]).await;
    default(&mut tx, &env, "global", None, None).await;
    default(&mut tx, &env, "caller", Some(1), Some(2)).await;
    let reader = rec.with_env(env.with_user(2));
    assert_eq!(
        reader.value(&mut tx, "note").await.unwrap(),
        Value::str("global")
    );
    assert_eq!(
        reader
            .default_get(&mut tx, &["note"])
            .await
            .unwrap()
            .get("note"),
        Some(&Value::str("caller"))
    );
    default(&mut tx, &env, "company", Some(1), None).await;
    assert_eq!(
        reader.value(&mut tx, "note").await.unwrap(),
        Value::str("company")
    );
    reader
        .write_values(&mut tx, &vals(&[("note", Value::False)]))
        .await
        .unwrap();
    tx.flush().await.unwrap();
    assert_eq!(reader.value(&mut tx, "note").await.unwrap(), Value::False);
    let null = tx
        .query(
            "SELECT note FROM property_runtime WHERE id=$1",
            &[&rec.ids()[0]],
        )
        .await
        .unwrap();
    assert_eq!(null[0].get::<_, serde_json::Value>(0), json!({"1":null}));
    rec.write_values(&mut tx, &vals(&[("note", Value::str("company"))]))
        .await
        .unwrap();
    tx.flush().await.unwrap();
    let result: erp_fields::Result<()> = tx
        .savepoint(async |tx| {
            default(tx, &env, "temporary", Some(1), None).await;
            assert_eq!(
                rec.value(tx, "note").await.unwrap(),
                Value::str("temporary")
            );
            Err(Error::validation("rollback property default"))
        })
        .await;
    error(
        result.unwrap_err(),
        ErrorKind::Validation,
        "rollback property default",
    );
    assert_eq!(
        rec.value(&mut tx, "note").await.unwrap(),
        Value::str("company")
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn exact_and_monetary_values_remain_exact_and_html_is_admitted() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let currency = seed(&mut tx).await;
    let env = company(&tx.env(), 1);
    let exact: Decimal = "9007199254740993.125".parse().unwrap();
    let rec = create(
        &mut tx,
        &env,
        &[
            ("name", Value::str("Exact")),
            ("currency_id", Value::Int(currency)),
            ("exact", Value::Decimal(exact)),
            ("amount", Value::Decimal("12.345".parse().unwrap())),
            ("html", Value::str("<p>safe<script>alert(1)</script></p>")),
        ],
    )
    .await;
    assert_eq!(
        rec.cell(&mut tx, "exact").await.unwrap(),
        Cell::Decimal(exact)
    );
    assert_eq!(
        rec.cell(&mut tx, "amount").await.unwrap(),
        Cell::Decimal("12.35".parse().unwrap())
    );
    let html = rec.value(&mut tx, "html").await.unwrap();
    assert_eq!(html, Value::str("<p>safe</p>"));
    let row = tx
        .query(
            "SELECT exact FROM property_runtime WHERE id=$1",
            &[&rec.ids()[0]],
        )
        .await
        .unwrap();
    assert_eq!(
        row[0].get::<_, serde_json::Value>(0),
        json!({"1":"9007199254740993.125"})
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn rollback_discards_pending_company_mutations() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    seed(&mut tx).await;
    let env = company(&tx.env(), 1);
    let rec = create(
        &mut tx,
        &env,
        &[
            ("name", Value::str("Rollback")),
            ("note", Value::str("kept")),
        ],
    )
    .await;
    let result: erp_fields::Result<()> = tx
        .savepoint(async |tx| {
            rec.with_env(company(&env, 2))
                .write_values(tx, &vals(&[("note", Value::str("discarded"))]))
                .await?;
            Err(Error::validation("rollback company write"))
        })
        .await;
    error(
        result.unwrap_err(),
        ErrorKind::Validation,
        "rollback company write",
    );
    rec.write_values(&mut tx, &vals(&[("note", Value::str("final"))]))
        .await
        .unwrap();
    tx.flush().await.unwrap();
    let row = tx
        .query(
            "SELECT note FROM property_runtime WHERE id=$1",
            &[&rec.ids()[0]],
        )
        .await
        .unwrap();
    assert_eq!(row[0].get::<_, serde_json::Value>(0), json!({"1":"final"}));
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn queries_project_properties_and_never_find_drafts() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    seed(&mut tx).await;
    let env = company(&tx.env(), 1);
    let rec = create(
        &mut tx,
        &env,
        &[
            ("name", Value::str("Query guard")),
            ("note", Value::str("hidden")),
        ],
    )
    .await;
    let other = create(&mut tx, &env, &[("name", Value::str("Other"))]).await;
    // Company 2 gives the second record the same value; company 1 still has none for it.
    other
        .with_env(company(&env, 2))
        .write_values(&mut tx, &vals(&[("note", Value::str("hidden"))]))
        .await
        .unwrap();
    let domain = erp_sdk::Domain::condition("note", erp_sdk::Operator::Eq, "hidden").unwrap();
    assert_eq!(rec.search(&mut tx, &domain).await.unwrap().ids(), rec.ids());
    assert_eq!(
        rec.with_env(company(&env, 2))
            .search(&mut tx, &domain)
            .await
            .unwrap()
            .ids(),
        other.ids()
    );
    // a draft holding the searched value is not a row: the search still finds two records
    let draft = tx
        .new_record(
            &rec,
            erp_orm::RawVals::new().set_cell("note", Cell::Str("hidden".into())),
            None,
            None,
        )
        .await
        .unwrap();
    assert_eq!(
        draft.value(&mut tx, "note").await.unwrap(),
        Value::str("hidden")
    );
    assert_eq!(rec.search(&mut tx, &domain).await.unwrap().ids(), rec.ids());
    assert_eq!(
        rec.search(&mut tx, &erp_sdk::Domain::TRUE)
            .await
            .unwrap()
            .len(),
        2
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn malformed_maps_fail_without_disclosing_other_company_values() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    seed(&mut tx).await;
    let env = company(&tx.env(), 1);
    let rec = create(&mut tx, &env, &[("name", Value::str("Corrupt"))]).await;
    tx.execute(
        "UPDATE property_runtime SET note = $1 WHERE id=$2",
        &[&json!({"2":{"secret":"value"}}), &rec.ids()[0]],
    )
    .await
    .unwrap();
    tx.invalidate_all().await.unwrap();
    error(
        rec.value(&mut tx, "note").await.unwrap_err(),
        ErrorKind::Programming,
        "company-dependent map contains a non-scalar value",
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn independent_orms_retry_stale_company_writes_without_losing_siblings() {
    let h = harness().await;
    let peer = h.orm().rebuild(h.registry().clone()).build().unwrap();
    let mut setup = h.begin().await.unwrap();
    seed(&mut setup).await;
    let env = company(&setup.env(), 1);
    let rec = create(&mut setup, &env, &[("name", Value::str("Concurrent"))]).await;
    let id = rec.ids()[0];
    setup.commit().await.unwrap();
    let mut one = h.begin().await.unwrap();
    let mut two = peer.begin(erp_orm::SUPERUSER_ID).await.unwrap();
    let a = company(&one.env(), 1)
        .records("property.runtime")
        .unwrap()
        .browse([id])
        .unwrap();
    let b = company(&two.env(), 2)
        .records("property.runtime")
        .unwrap()
        .browse([id])
        .unwrap();
    assert_eq!(b.value(&mut two, "note").await.unwrap(), Value::False);
    a.write_values(&mut one, &vals(&[("note", Value::str("one"))]))
        .await
        .unwrap();
    one.commit().await.unwrap();
    b.write_values(&mut two, &vals(&[("note", Value::str("two"))]))
        .await
        .unwrap();
    let stale = two.commit().await.unwrap_err();
    assert_eq!(stale.kind(), ErrorKind::Concurrency);
    assert_eq!(stale.sqlstate(), Some("40001"));
    assert_eq!(
        stale.message(),
        "could not serialize access due to concurrent update"
    );
    two.rollback().await.unwrap();
    let b = company(&two.env(), 2)
        .records("property.runtime")
        .unwrap()
        .browse([id])
        .unwrap();
    b.write_values(&mut two, &vals(&[("note", Value::str("two"))]))
        .await
        .unwrap();
    two.commit().await.unwrap();
    let mut check = h.begin().await.unwrap();
    let rows = check
        .query("SELECT note FROM property_runtime WHERE id=$1", &[&id])
        .await
        .unwrap();
    assert_eq!(
        rows[0].get::<_, serde_json::Value>(0),
        json!({"1":"one", "2":"two"})
    );
    drop(check);
    drop(one);
    drop(two);
    drop(setup);
    drop(peer);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn defaults_respect_existing_snapshots_and_refresh_on_independent_nodes() {
    let h = harness().await;
    let peer = h.orm().rebuild(h.registry().clone()).build().unwrap();
    let mut setup = h.begin().await.unwrap();
    seed(&mut setup).await;
    let env = company(&setup.env(), 1);
    let rec = create(&mut setup, &env, &[("name", Value::str("Snapshot"))]).await;
    let id = rec.ids()[0];
    default(&mut setup, &env, "old", Some(1), None).await;
    setup.commit().await.unwrap();
    let mut old = peer.begin(erp_orm::SUPERUSER_ID).await.unwrap();
    let r = company(&old.env(), 1)
        .records("property.runtime")
        .unwrap()
        .browse([id])
        .unwrap();
    assert_eq!(r.value(&mut old, "note").await.unwrap(), Value::str("old"));
    let mut writer = h.begin().await.unwrap();
    let writer_env = company(&writer.env(), 1);
    default(&mut writer, &writer_env, "new", Some(1), None).await;
    writer.commit().await.unwrap();
    assert_eq!(r.value(&mut old, "note").await.unwrap(), Value::str("old"));
    old.commit().await.unwrap();
    let r = company(&old.env(), 1)
        .records("property.runtime")
        .unwrap()
        .browse([id])
        .unwrap();
    assert_eq!(r.value(&mut old, "note").await.unwrap(), Value::str("new"));
    drop(old);
    drop(writer);
    drop(setup);
    drop(peer);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn ordering_and_grouping_work_while_upsert_still_refuses() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    seed(&mut tx).await;
    let env = company(&tx.env(), 1);
    default(&mut tx, &env, "middle", None, None).await;
    let model = env.records("property.runtime").unwrap();
    let rows = [
        vals(&[("note", Value::str("zulu"))]),
        vals(&[("note", Value::str("alpha"))]),
        vals(&[]),
    ];
    let rec = model.create_values(&mut tx, &rows).await.unwrap();
    let ids = rec.ids().to_vec();
    // Company 2 overrides the first record and the third; the second falls back to the default.
    let two = model.with_env(company(&env, 2));
    two.browse([ids[0]])
        .unwrap()
        .write_values(&mut tx, &vals(&[("note", Value::str("aaa"))]))
        .await
        .unwrap();
    two.browse([ids[2]])
        .unwrap()
        .write_values(&mut tx, &vals(&[("note", Value::str("bravo"))]))
        .await
        .unwrap();
    let order = SearchOptions::new().order("note");
    let got = model
        .search_with(&mut tx, &erp_sdk::Domain::TRUE, &order)
        .await
        .unwrap();
    assert_eq!(got.ids(), &[ids[1], ids[2], ids[0]]);
    let got = two
        .search_with(&mut tx, &erp_sdk::Domain::TRUE, &order)
        .await
        .unwrap();
    assert_eq!(got.ids(), &[ids[0], ids[2], ids[1]]);
    for (scope, expected) in [
        (&model, ["alpha", "middle", "zulu"]),
        (&two, ["aaa", "bravo", "middle"]),
    ] {
        let groups = scope
            ._read_group(
                &mut tx,
                ReadGroup::new(erp_sdk::Domain::TRUE)
                    .groupby(["note"])
                    .aggregates(["__count"])
                    .order("note"),
            )
            .await
            .unwrap();
        let got: Vec<Vec<Value>> = groups
            .rows()
            .iter()
            .map(|r| r.iter().map(|v| v.to_value(RecordsAs::Id)).collect())
            .collect();
        let want: Vec<Vec<Value>> = expected
            .iter()
            .map(|n| vec![Value::str(n), Value::Int(1)])
            .collect();
        assert_eq!(got, want);
    }
    error(
        model
            .upsert(&mut tx, erp_orm::UpsertCall::new("unused"))
            .await
            .unwrap_err(),
        ErrorKind::Programming,
        "property.runtime.note: company-dependent upsert is not supported until a later release",
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn required_properties_validate_effective_values_and_missing_company_refuses() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    seed(&mut tx).await;
    let env = company(&tx.env(), 1);
    let model = env.records("property.required").unwrap();
    let fail: erp_fields::Result<()> = tx
        .savepoint(async |tx| model.create_values(tx, &[vals(&[])]).await.map(|_| ()))
        .await;
    error(
        fail.unwrap_err(),
        ErrorKind::Validation,
        "property.required.note: required company-dependent value is missing",
    );
    let record = model
        .create_values(&mut tx, &[vals(&[("note", Value::str("present"))])])
        .await
        .unwrap();
    assert_eq!(
        record.value(&mut tx, "note").await.unwrap(),
        Value::str("present")
    );
    let row = tx.query("SELECT is_nullable FROM information_schema.columns WHERE table_name='property_required' AND column_name='note'", &[]).await.unwrap();
    assert_eq!(row[0].get::<_, String>(0), "YES");
    // A fresh context with no companies is an error, never an implicit company 1.
    let no_company = record.with_env(env.with_context([("allowed_company_ids", Value::list([]))]));
    error(
        no_company.value(&mut tx, "note").await.unwrap_err(),
        ErrorKind::Validation,
        "company-dependent fields require a current company",
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn warm_cache_does_not_bypass_company_authority_under_sudo() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    seed(&mut tx).await;
    let env = company(&tx.env(), 1);
    let rec = create(
        &mut tx,
        &env,
        &[
            ("name", Value::str("Authority")),
            ("note", Value::str("private")),
        ],
    )
    .await;
    let id = rec.ids()[0];
    tx.commit().await.unwrap();
    let resolution = erp_orm::Resolution::in_id_order(
        erp_orm::GroupSet::default(),
        [erp_orm::CompanyId::new(1).unwrap()],
        std::num::NonZeroU64::MIN,
    )
    .unwrap();
    let principal = erp_orm::PrincipalBuilder::user(erp_orm::UserId::new(2).unwrap())
        .resolution(resolution)
        .companies(vec![erp_orm::CompanyId::new(1).unwrap()])
        .build()
        .unwrap();
    let mut reader = h
        .orm()
        .begin_opts(erp_orm::BeginOptions::for_principal(principal))
        .await
        .unwrap();
    let visible = reader
        .env()
        .records("property.runtime")
        .unwrap()
        .browse([id])
        .unwrap();
    assert_eq!(
        visible.value(&mut reader, "note").await.unwrap(),
        Value::str("private")
    );
    let forbidden = visible.with_env(company(&visible.env().sudo(true), 2));
    error(
        forbidden.value(&mut reader, "note").await.unwrap_err(),
        ErrorKind::Access,
        "Access to unauthorized or invalid companies.",
    );
    drop(reader);
    drop(tx);
    h.cleanup().await.unwrap();
}

#[test]
fn unsupported_company_declarations_refuse_during_composition() {
    static INVALID: erp_registry::ModelDecl =
        erp_registry::ModelDecl::new("property_runtime", "property.invalid")
            .fields(&[erp_registry::FieldDecl::json("value").company_dependent(true)]);
    let err = erp_registry::RegistryBuilder::new([
        &base_users::ADDON,
        &erp_addon_base::ADDON,
        &fixture::ADDON,
    ])
    .with_decls([&INVALID])
    .build()
    .unwrap_err();
    error(
        err,
        ErrorKind::Programming,
        "field 'property.invalid.value': this company-dependent declaration is not supported until a later release",
    );
}

#[tokio::test]
async fn text_selection_dates_and_relations_keep_scalar_types_across_flush() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    seed(&mut tx).await;
    let env = company(&tx.env(), 1);
    let tag = create(&mut tx, &env, &[("name", Value::str("Target"))]).await;
    let rec = create(
        &mut tx,
        &env,
        &[
            ("name", Value::str("Types")),
            ("text", Value::str("日本語\r\ntext")),
            ("choice", Value::str("b")),
            ("day", Value::str("2026-11-11")),
            ("moment", Value::str("2026-11-11 12:34:56")),
            ("tag_id", Value::Int(tag.ids()[0])),
            ("count", Value::Int(42)),
            ("truth", Value::Bool(true)),
            ("ratio", Value::Float(1.5)),
        ],
    )
    .await;
    tx.flush().await.unwrap();
    tx.invalidate_all().await.unwrap();
    assert_eq!(
        rec.value(&mut tx, "text").await.unwrap(),
        Value::str("日本語\r\ntext")
    );
    assert_eq!(rec.value(&mut tx, "choice").await.unwrap(), Value::str("b"));
    assert_eq!(
        rec.cell(&mut tx, "day").await.unwrap(),
        Cell::Date(erp_fields::Date::parse_odoo("2026-11-11").unwrap())
    );
    assert_eq!(
        rec.cell(&mut tx, "moment").await.unwrap(),
        Cell::DateTime(erp_fields::DateTime::parse_odoo("2026-11-11 12:34:56").unwrap())
    );
    assert_eq!(
        rec.cell(&mut tx, "tag_id").await.unwrap(),
        Cell::Id(tag.ids()[0])
    );
    assert_eq!(rec.cell(&mut tx, "count").await.unwrap(), Cell::Int(42));
    let two = rec.with_env(company(&env, 2));
    assert_eq!(two.cell(&mut tx, "tag_id").await.unwrap(), Cell::Null);
    assert_eq!(two.value(&mut tx, "text").await.unwrap(), Value::False);
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn deletion_cannot_miss_a_reference_after_an_unrelated_first_row() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    seed(&mut tx).await;
    let env = company(&tx.env(), 1);
    let first = create(&mut tx, &env, &[("name", Value::str("Other target"))]).await;
    let target = create(&mut tx, &env, &[("name", Value::str("Restricted target"))]).await;
    create(
        &mut tx,
        &env,
        &[
            ("name", Value::str("Unrelated first")),
            ("restricted_id", Value::Int(first.ids()[0])),
        ],
    )
    .await;
    create(
        &mut tx,
        &env,
        &[
            ("name", Value::str("Matching later")),
            ("restricted_id", Value::Int(target.ids()[0])),
        ],
    )
    .await;
    error(
        target.unlink(&mut tx).await.unwrap_err(),
        ErrorKind::UserError,
        "You cannot delete a record used by a company-dependent field",
    );
    assert_eq!(
        target.value(&mut tx, "name").await.unwrap(),
        Value::str("Restricted target")
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn default_removal_preserves_pending_overrides_and_empty_reads_stay_empty() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    seed(&mut tx).await;
    let env = company(&tx.env(), 1);
    let rec = create(&mut tx, &env, &[("name", Value::str("Removal"))]).await;
    default(&mut tx, &env, "global", None, None).await;
    rec.write_values(&mut tx, &vals(&[("note", Value::str("pending"))]))
        .await
        .unwrap();
    ir_default::discard_values(
        &mut tx,
        &env,
        "property.runtime",
        "note",
        &[Value::str("global")],
    )
    .await
    .unwrap();
    tx.flush().await.unwrap();
    assert_eq!(
        rec.value(&mut tx, "note").await.unwrap(),
        Value::str("pending")
    );
    assert_eq!(
        rec.with_env(company(&env, 2))
            .value(&mut tx, "note")
            .await
            .unwrap(),
        Value::False
    );
    assert!(
        rec.browse_ids([])
            .cells(&mut tx, "note")
            .await
            .unwrap()
            .is_empty()
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn privileged_warm_maps_do_not_bypass_field_access() {
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
    let rec = create(
        &mut tx,
        &env,
        &[
            ("name", Value::str("Field rights")),
            ("note", Value::str("private")),
        ],
    )
    .await;
    assert_eq!(
        rec.value(&mut tx, "note").await.unwrap(),
        Value::str("private")
    );
    let reader = rec.with_env(env.with_user(2));
    let message = format!(
        "You do not have enough rights to access the field \"note\" on {} (property.runtime). Please contact your system administrator.\n\nOperation: read",
        rec.spec().description
    );
    error(
        reader.value(&mut tx, "note").await.unwrap_err(),
        ErrorKind::Access,
        &message,
    );
    error(
        reader.read(&mut tx, &["note"]).await.unwrap_err(),
        ErrorKind::Access,
        &message,
    );
    assert_eq!(
        rec.value(&mut tx, "note").await.unwrap(),
        Value::str("private")
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn stored_property_consumers_refuse_at_runtime_assembly() {
    static CONSUMER: erp_registry::AddonDescriptor = erp_registry::AddonDescriptor {
        name: "property_consumer",
        depends: &["property_runtime"],
        exact_only: false,
    };
    static STORED: erp_registry::ModelDecl =
        erp_registry::ModelDecl::new("property_consumer", "property.runtime")
            .inherit(&["property.runtime"])
            .fields(&[erp_registry::FieldDecl::char("note_copy")
                .related("note")
                .store(true)]);
    let err = Harness::builder([
        &base_users::ADDON,
        &erp_addon_base::ADDON,
        &fixture::ADDON,
        &CONSUMER,
    ])
    .with_static_decls([&STORED])
    .check()
    .unwrap_err();
    assert!(matches!(err, erp_testkit::HarnessError::Orm(_)));
    if let erp_testkit::HarnessError::Orm(err) = err {
        error(
            err,
            ErrorKind::Programming,
            "property.runtime.note_copy: company-dependent compute dependencies without nonstored company context is not supported until a later release",
        );
    }
}

fn unused_property_hook<'a>(
    _: &'a mut Transaction,
    _: &'a erp_orm::WriteBatch,
) -> erp_orm::BoxFuture<'a, erp_fields::Result<()>> {
    Box::pin(async { Ok(()) })
}

#[tokio::test]
async fn property_hooks_refuse_before_any_connection_or_callback() {
    let err = Harness::builder([&base_users::ADDON, &erp_addon_base::ADDON, &fixture::ADDON])
        .write_hook(erp_orm::WriteHook::new(
            "property.runtime",
            &["note"],
            unused_property_hook,
        ))
        .check()
        .unwrap_err();
    assert!(matches!(err, erp_testkit::HarnessError::Orm(_)));
    if let erp_testkit::HarnessError::Orm(err) = err {
        error(
            err,
            ErrorKind::Programming,
            "property.runtime.note: company-dependent write-hook registration is not supported until a later release",
        );
    }
}

#[tokio::test]
async fn scalar_schema_adoption_refuses_and_savepoint_restores_the_old_shape() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    seed(&mut tx).await;
    let env = company(&tx.env(), 1);
    let rec = create(
        &mut tx,
        &env,
        &[
            ("name", Value::str("Migration")),
            ("note", Value::str("kept")),
        ],
    )
    .await;
    let result = tx
        .savepoint(async |tx| {
            tx.execute(
                "ALTER TABLE property_runtime ALTER COLUMN note TYPE text USING note::text",
                &[],
            )
            .await?;
            tx.init_models(&[rec.model_id()]).await
        })
        .await;
    error(
        result.unwrap_err(),
        ErrorKind::Programming,
        "property.runtime.note: populated scalar conversion requires an explicit company migration policy",
    );
    assert_eq!(
        rec.value(&mut tx, "note").await.unwrap(),
        Value::str("kept")
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn invalid_hidden_fallback_never_discloses_its_value() {
    let h = Harness::builder([&base_users::ADDON, &erp_addon_base::ADDON, &fixture::ADDON])
        .policy(erp_testkit::FieldPolicy::new().deny(
            "property.runtime",
            "exact",
            erp_orm::FieldOp::Read,
        ))
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    seed(&mut tx).await;
    let env = company(&tx.env(), 1);
    let rec = create(&mut tx, &env, &[("name", Value::str("Invalid fallback"))]).await;
    ir_default::set(
        &mut tx,
        &env,
        "property.runtime",
        "exact",
        &Value::Decimal(Decimal::ZERO),
        None,
        None,
        None,
    )
    .await
    .unwrap();
    assert!(rec.spec().field("exact").unwrap().company_dependent);
    assert_eq!(tx.execute("UPDATE ir_default SET json_value=$1 WHERE field_id IN (SELECT id FROM ir_model_fields WHERE model='property.runtime' AND name='exact')", &[&"\"private fallback value\""]).await.unwrap(), 1);
    let defaults = ir_default::get_model_defaults(&mut tx, &env, "property.runtime", None)
        .await
        .unwrap();
    assert_eq!(
        defaults.get("exact"),
        Some(&Value::str("private fallback value"))
    );
    error(
        rec.with_env(env.with_user(2))
            .value(&mut tx, "note")
            .await
            .unwrap_err(),
        ErrorKind::Validation,
        "property.runtime.exact: invalid company-dependent fallback",
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn grouping_through_a_property_link_uses_effective_values() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    seed(&mut tx).await;
    let env = company(&tx.env(), 1);
    let model = env.records("property.runtime").unwrap();
    let t1 = create(&mut tx, &env, &[("name", Value::str("t1"))]).await;
    let t2 = create(&mut tx, &env, &[("name", Value::str("t2"))]).await;
    let link = |t: &Records| ("tag_id", Value::Int(t.ids()[0]));
    let o1 = create(&mut tx, &env, &[link(&t1)]).await;
    let o2 = create(&mut tx, &env, &[link(&t1)]).await;
    let o3 = create(&mut tx, &env, &[]).await;
    // Company 2 re-points o2 at t2; o1 and o3 have no company 2 value.
    o2.with_env(company(&env, 2))
        .write_values(&mut tx, &vals(&[link(&t2)]))
        .await
        .unwrap();
    let scope = erp_sdk::Domain::condition(
        "id",
        erp_sdk::Operator::In,
        erp_domain::Value::list([o1.ids()[0], o2.ids()[0], o3.ids()[0]]),
    )
    .unwrap();
    for (scoped, expected) in [
        (&model, [("t1", 2), ("", 1)]),
        (&model.with_env(company(&env, 2)), [("t2", 1), ("", 2)]),
    ] {
        let groups = scoped
            ._read_group(
                &mut tx,
                ReadGroup::new(scope.clone())
                    .groupby(["tag_id.name"])
                    .aggregates(["__count"])
                    .order("tag_id.name"),
            )
            .await
            .unwrap();
        let mut got: Vec<(Value, Value)> = groups
            .rows()
            .iter()
            .map(|r| (r[0].to_value(RecordsAs::Id), r[1].to_value(RecordsAs::Id)))
            .collect();
        got.sort_by_key(|(k, _)| format!("{k:?}"));
        let mut want: Vec<(Value, Value)> = expected
            .iter()
            .map(|(n, c)| {
                let key = if n.is_empty() {
                    Value::False
                } else {
                    Value::str(n)
                };
                (key, Value::Int(*c))
            })
            .collect();
        want.sort_by_key(|(k, _)| format!("{k:?}"));
        assert_eq!(got, want);
    }
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn company_context_compute_refreshes_after_defaults_change() {
    let h = Harness::builder([
        &base_users::ADDON,
        &erp_addon_base::ADDON,
        &native_consumer::ADDON,
    ])
    .build()
    .await
    .unwrap();
    let mut tx = h.begin().await.unwrap();
    seed(&mut tx).await;
    let env = company(&tx.env(), 1);
    let rec = env
        .records("property.computed")
        .unwrap()
        .create_values(&mut tx, &[vals(&[("note", Value::str("one"))])])
        .await
        .unwrap();
    ir_default::set(
        &mut tx,
        &env,
        "property.computed",
        "note",
        &Value::str("first"),
        None,
        None,
        None,
    )
    .await
    .unwrap();
    let two = rec.with_env(company(&env, 2));
    assert_eq!(
        rec.value(&mut tx, "note_copy").await.unwrap(),
        Value::str("one")
    );
    assert_eq!(
        two.value(&mut tx, "note_copy").await.unwrap(),
        Value::str("first")
    );
    ir_default::set(
        &mut tx,
        &env,
        "property.computed",
        "note",
        &Value::str("second"),
        None,
        None,
        None,
    )
    .await
    .unwrap();
    assert_eq!(
        two.value(&mut tx, "note_copy").await.unwrap(),
        Value::str("second")
    );
    assert_eq!(
        rec.value(&mut tx, "note_copy").await.unwrap(),
        Value::str("one")
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

mod native_consumer {
    use erp_sdk::prelude::*;
    erp_sdk::addon!(name = "property_native_consumer", depends = ["base"]);
    #[model(name = "property.computed", addon = "property_native_consumer")]
    pub struct Computed {
        #[field(char)]
        pub name: Option<Arc<str>>,
        #[field(char, company_dependent = true)]
        pub note: Option<Arc<str>>,
        #[field(char, compute = "_copy_note")]
        pub note_copy: Option<Arc<str>>,
        #[field(integer, company_dependent = true)]
        pub count: i32,
        #[field(many2one, comodel = "property.computed", company_dependent = true)]
        pub tag_id: Option<RecId>,
        #[field(integer, compute = "_double_count")]
        pub count_double: i32,
        #[field(many2one, comodel = "property.computed")]
        pub parent_id: Option<RecId>,
        #[field(one2many, comodel = "property.computed", inverse_name = "parent_id")]
        pub child_ids: Vec<RecId>,
        #[field(integer, compute = "_child_total")]
        pub child_total: i32,
        #[field(integer, compute = "_child_tagged")]
        pub child_tagged: i32,
        #[field(integer, company_dependent = true, default = 4)]
        pub quota: i32,
        #[field(html, company_dependent = true)]
        pub body: Option<Arc<str>>,
    }
    #[methods(Computed, addon = "property_native_consumer")]
    impl Computed {
        #[compute(depends = ["note"], depends_context = ["company"])]
        fn _copy_note(batch: &mut Batch<'_, Computed>) -> Result<()> {
            for i in batch.indices() {
                let note = batch.get(i, Computed::NOTE)?;
                batch.set(i, Computed::NOTE_COPY, note)?;
            }
            Ok(())
        }
        #[compute(depends = ["count"], depends_context = ["company"])]
        fn _double_count(batch: &mut Batch<'_, Computed>) -> Result<()> {
            for i in batch.indices() {
                let count = batch.get(i, Computed::COUNT)?;
                batch.set(i, Computed::COUNT_DOUBLE, count.saturating_mul(2))?;
            }
            Ok(())
        }
        #[compute(depends = ["child_ids.count"], depends_context = ["company"])]
        fn _child_total(batch: &mut Batch<'_, Computed>) -> Result<()> {
            for i in batch.indices() {
                let children = batch.get(i, Computed::CHILD_IDS)?;
                let total = batch
                    .values_of(Computed::COUNT, &children)?
                    .iter()
                    .fold(0_i32, |sum, count| sum.saturating_add(*count));
                batch.set(i, Computed::CHILD_TOTAL, total)?;
            }
            Ok(())
        }
        #[compute(depends = ["child_ids.tag_id"], depends_context = ["company"])]
        fn _child_tagged(batch: &mut Batch<'_, Computed>) -> Result<()> {
            for i in batch.indices() {
                let children = batch.get(i, Computed::CHILD_IDS)?;
                let tagged = batch
                    .values_of(Computed::TAG_ID, &children)?
                    .iter()
                    .filter(|tag| tag.is_some())
                    .count();
                batch.set(
                    i,
                    Computed::CHILD_TAGGED,
                    i32::try_from(tagged).unwrap_or(i32::MAX),
                )?;
            }
            Ok(())
        }
    }
}

#[tokio::test]
async fn named_precision_flush_uses_the_mutation_company() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    seed(&mut tx).await;
    let env = company(&tx.env(), 1);
    env.records("decimal.precision")
        .unwrap()
        .create_values(
            &mut tx,
            &[vals(&[
                ("name", Value::str("Property Precision")),
                ("digits", Value::Int(4)),
            ])],
        )
        .await
        .unwrap();
    let rec = create(
        &mut tx,
        &env,
        &[
            ("name", Value::str("Named precision")),
            ("named", Value::Decimal("1.1234".parse().unwrap())),
        ],
    )
    .await;
    let two = rec.with_env(company(&env, 2));
    two.write_values(
        &mut tx,
        &vals(&[("named", Value::Decimal("2.1234".parse().unwrap()))]),
    )
    .await
    .unwrap();
    tx.flush().await.unwrap();
    assert_eq!(
        rec.cell(&mut tx, "named").await.unwrap(),
        Cell::Decimal("1.1234".parse().unwrap())
    );
    assert_eq!(
        two.cell(&mut tx, "named").await.unwrap(),
        Cell::Decimal("2.1234".parse().unwrap())
    );
    let rows = tx
        .query(
            "SELECT named FROM property_runtime WHERE id=$1",
            &[&rec.ids()[0]],
        )
        .await
        .unwrap();
    assert_eq!(
        rows[0].get::<_, serde_json::Value>(0),
        json!({"1":"1.1234", "2":"2.1234"})
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn direct_default_crud_invalidates_property_consumers() {
    let h = Harness::builder([
        &base_users::ADDON,
        &erp_addon_base::ADDON,
        &native_consumer::ADDON,
    ])
    .build()
    .await
    .unwrap();
    let mut tx = h.begin().await.unwrap();
    seed(&mut tx).await;
    let env = company(&tx.env(), 1);
    let rec = env
        .records("property.computed")
        .unwrap()
        .create_values(&mut tx, &[vals(&[("note", Value::str("one"))])])
        .await
        .unwrap();
    ir_default::set(
        &mut tx,
        &env,
        "property.computed",
        "note",
        &Value::str("first"),
        None,
        None,
        None,
    )
    .await
    .unwrap();
    let two = rec.with_env(company(&env, 2));
    assert_eq!(
        two.value(&mut tx, "note_copy").await.unwrap(),
        Value::str("first")
    );
    let rows = tx.query("SELECT d.id, d.field_id FROM ir_default d JOIN ir_model_fields f ON f.id=d.field_id WHERE f.model='property.computed' AND f.name='note'", &[]).await.unwrap();
    assert_eq!(rows.len(), 1);
    let id: i64 = rows[0].get(0);
    let field_id: i64 = rows[0].get(1);
    let defaults = env.records("ir.default").unwrap();
    let row = defaults.browse([id]).unwrap();
    row.write_values(&mut tx, &vals(&[("json_value", Value::str("\"second\""))]))
        .await
        .unwrap();
    assert_eq!(
        two.value(&mut tx, "note_copy").await.unwrap(),
        Value::str("second")
    );
    row.unlink(&mut tx).await.unwrap();
    assert_eq!(two.value(&mut tx, "note_copy").await.unwrap(), Value::False);
    defaults
        .create_values(
            &mut tx,
            &[vals(&[
                ("field_id", Value::Int(field_id)),
                ("json_value", Value::str("\"third\"")),
            ])],
        )
        .await
        .unwrap();
    assert_eq!(
        two.value(&mut tx, "note_copy").await.unwrap(),
        Value::str("third")
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn no_company_omitted_properties_create_sql_null() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    seed(&mut tx).await;
    let env = tx.env();
    let model = env.records("property.runtime").unwrap();
    let records = model
        .create_values(
            &mut tx,
            &[
                vals(&[("name", Value::str("first"))]),
                vals(&[("name", Value::str("second"))]),
            ],
        )
        .await
        .unwrap();
    let typed = env
        .model::<fixture::Property>()
        .unwrap()
        .create(
            &mut tx,
            vec![erp_orm::Vals::new().set(
                fixture::Property::NAME,
                Some(std::sync::Arc::<str>::from("typed")),
            )],
        )
        .await
        .unwrap();
    tx.flush().await.unwrap();
    let rows = tx.query("SELECT note IS NULL AND truth IS NULL AND count IS NULL AND ratio IS NULL AND exact IS NULL AND tag_id IS NULL AND amount IS NULL FROM property_runtime ORDER BY id", &[]).await.unwrap();
    assert_eq!(rows.len(), 3);
    assert!(rows.iter().all(|row| row.get::<_, bool>(0)));
    assert_eq!(
        typed.get(&mut tx, fixture::Property::NAME).await.unwrap(),
        Some("typed".into())
    );
    let record = model.browse_ids([erp_fields::RecId::from_real(records.ids()[0]).unwrap()]);
    error(
        record.value(&mut tx, "note").await.unwrap_err(),
        ErrorKind::Validation,
        "company-dependent fields require a current company",
    );
    let one = record.with_env(company(&env, 1));
    let two = record.with_env(company(&env, 2));
    assert_eq!(one.value(&mut tx, "note").await.unwrap(), Value::False);
    one.set_value(&mut tx, "note", Value::str("one"))
        .await
        .unwrap();
    tx.flush().await.unwrap();
    assert_eq!(two.value(&mut tx, "note").await.unwrap(), Value::False);
    assert_eq!(one.value(&mut tx, "note").await.unwrap(), Value::str("one"));
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn no_company_supplied_properties_refuse_without_partial_create() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    seed(&mut tx).await;
    let env = tx.env();
    let model = env.records("property.runtime").unwrap();
    for (field, value) in [
        ("note", Value::str("value")),
        ("note", Value::False),
        ("truth", Value::False),
        ("count", Value::Int(0)),
        ("ratio", Value::Float(0.0)),
        ("tag_id", Value::Int(1)),
    ] {
        let result = tx
            .savepoint(async |tx| {
                model
                    .create_values(
                        tx,
                        &[
                            vals(&[("name", Value::str("must roll back"))]),
                            vals(&[(field, value.clone())]),
                        ],
                    )
                    .await
                    .map(|_| ())
            })
            .await;
        error(
            result.unwrap_err(),
            ErrorKind::Validation,
            "company-dependent fields require a current company",
        );
        let rows = tx
            .query("SELECT count(*) FROM property_runtime", &[])
            .await
            .unwrap();
        assert_eq!(rows[0].get::<_, i64>(0), 0);
    }
    let defaults = model.with_env(env.with_context([("default_note", Value::str("default"))]));
    let result = tx
        .savepoint(async |tx| defaults.create_values(tx, &[vals(&[])]).await.map(|_| ()))
        .await;
    error(
        result.unwrap_err(),
        ErrorKind::Validation,
        "company-dependent fields require a current company",
    );
    let record = create(
        &mut tx,
        &company(&env, 1),
        &[("name", Value::str("existing"))],
    )
    .await
    .with_env(env);
    for value in [Value::str("value"), Value::False] {
        let result = tx
            .savepoint(async |tx| record.set_value(tx, "note", value.clone()).await)
            .await;
        error(
            result.unwrap_err(),
            ErrorKind::Validation,
            "company-dependent fields require a current company",
        );
    }
    drop(tx);
    h.cleanup().await.unwrap();
}

fn raw_vals(items: &[(&str, Cell)]) -> erp_orm::RawVals {
    items
        .iter()
        .fold(erp_orm::RawVals::new(), |raw, (name, cell)| {
            raw.set_cell((*name).to_owned(), cell.clone())
        })
}
async fn stored_map(tx: &mut Transaction, table: &str, column: &str, id: i64) -> serde_json::Value {
    let sql = format!("SELECT {column} FROM {table} WHERE id = $1");
    tx.query(&sql, &[&id]).await.unwrap()[0].get(0)
}
async fn rows(tx: &mut Transaction, table: &str) -> i64 {
    let sql = format!("SELECT count(*) FROM {table}");
    tx.query(&sql, &[]).await.unwrap()[0].get(0)
}

#[tokio::test]
async fn drafts_hold_company_values_and_write_nothing() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    seed(&mut tx).await;
    let one = company(&tx.env(), 1);
    let two = company(&tx.env(), 2);
    default(&mut tx, &one, "fallback one", Some(1), None).await;
    default(&mut tx, &one, "fallback two", Some(2), None).await;
    let origin = create(
        &mut tx,
        &one,
        &[
            ("name", Value::str("Origin")),
            ("note", Value::str("origin one")),
            ("count", Value::Int(7)),
        ],
    )
    .await;
    origin
        .with_env(two.clone())
        .write_values(
            &mut tx,
            &vals(&[("note", Value::str("origin two")), ("count", Value::Int(9))]),
        )
        .await
        .unwrap();
    tx.flush().await.unwrap();
    let before = rows(&mut tx, "property_runtime").await;
    let model = one.records("property.runtime").unwrap();

    // an explicit value belongs to the environment's company; another company reads its fallback
    let draft = tx
        .new_record(
            &model,
            raw_vals(&[("note", Cell::Str("draft".into())), ("count", Cell::Int(3))]),
            None,
            None,
        )
        .await
        .unwrap();
    assert_eq!(
        draft.value(&mut tx, "note").await.unwrap(),
        Value::str("draft")
    );
    assert_eq!(draft.value(&mut tx, "count").await.unwrap(), Value::Int(3));
    let draft_two = draft.with_env(two.clone());
    assert_eq!(
        draft_two.value(&mut tx, "note").await.unwrap(),
        Value::str("fallback two")
    );
    assert_eq!(
        draft_two.value(&mut tx, "count").await.unwrap(),
        Value::Int(0)
    );

    // neither a value nor an origin: each company's fallback
    let empty = tx
        .new_record(&model, erp_orm::RawVals::new(), None, None)
        .await
        .unwrap();
    assert_eq!(
        empty.value(&mut tx, "note").await.unwrap(),
        Value::str("fallback one")
    );
    assert_eq!(
        empty
            .with_env(two.clone())
            .value(&mut tx, "note")
            .await
            .unwrap(),
        Value::str("fallback two")
    );

    // a default fills the environment company's entry only
    let defaulted = model.with_env(one.with_context([("default_note", Value::str("context"))]));
    let with_default = tx
        .new_record(&defaulted, erp_orm::RawVals::new(), None, None)
        .await
        .unwrap();
    assert_eq!(
        with_default.value(&mut tx, "note").await.unwrap(),
        Value::str("context")
    );
    assert_eq!(
        with_default
            .with_env(two.clone())
            .value(&mut tx, "note")
            .await
            .unwrap(),
        Value::str("fallback two")
    );

    // an origin: the origin's value in each company
    let copy = tx
        .new_record(
            &model,
            erp_orm::RawVals::new(),
            Some(origin.rec_ids()[0]),
            None,
        )
        .await
        .unwrap();
    assert_eq!(
        copy.value(&mut tx, "note").await.unwrap(),
        Value::str("origin one")
    );
    assert_eq!(copy.value(&mut tx, "count").await.unwrap(), Value::Int(7));
    let copy_two = copy.with_env(two.clone());
    assert_eq!(
        copy_two.value(&mut tx, "note").await.unwrap(),
        Value::str("origin two")
    );
    assert_eq!(
        copy_two.value(&mut tx, "count").await.unwrap(),
        Value::Int(9)
    );

    // a write on the draft changes its environment's company only, in either company
    copy.write_values(&mut tx, &vals(&[("note", Value::str("edited"))]))
        .await
        .unwrap();
    copy_two
        .write_values(&mut tx, &vals(&[("count", Value::Int(11))]))
        .await
        .unwrap();
    assert_eq!(
        copy.value(&mut tx, "note").await.unwrap(),
        Value::str("edited")
    );
    assert_eq!(copy.value(&mut tx, "count").await.unwrap(), Value::Int(7));
    assert_eq!(
        copy_two.value(&mut tx, "note").await.unwrap(),
        Value::str("origin two")
    );
    assert_eq!(
        copy_two.value(&mut tx, "count").await.unwrap(),
        Value::Int(11)
    );

    // no draft value reaches the database
    tx.flush().await.unwrap();
    assert_eq!(rows(&mut tx, "property_runtime").await, before);
    let id = origin.ids()[0];
    assert_eq!(
        stored_map(&mut tx, "property_runtime", "note", id).await,
        json!({"1": "origin one", "2": "origin two"})
    );
    assert_eq!(
        stored_map(&mut tx, "property_runtime", "count", id).await,
        json!({"1": 7, "2": 9})
    );

    // a draft's company values from its origin cost no more statements than the stored read
    const FIELDS: [&str; 4] = ["note", "count", "truth", "tag_id"];
    tx.invalidate_all().await.unwrap();
    let start = tx.query_count();
    for field in FIELDS {
        origin.cell(&mut tx, field).await.unwrap();
    }
    let stored_reads = tx.query_count() - start;
    tx.invalidate_all().await.unwrap();
    let fresh = tx
        .new_record(
            &model,
            erp_orm::RawVals::new(),
            Some(origin.rec_ids()[0]),
            None,
        )
        .await
        .unwrap();
    let start = tx.query_count();
    for field in FIELDS {
        fresh.cell(&mut tx, field).await.unwrap();
    }
    let draft_reads = tx.query_count() - start;
    println!(
        "company-dependent reads of {} fields: stored record {stored_reads} statements, draft \
         with origin {draft_reads} statements",
        FIELDS.len()
    );
    assert!(
        draft_reads <= stored_reads,
        "draft {draft_reads} > stored {stored_reads}"
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn draft_lines_write_the_environment_company_value() {
    let h = Harness::builder([
        &base_users::ADDON,
        &erp_addon_base::ADDON,
        &native_consumer::ADDON,
    ])
    .build()
    .await
    .unwrap();
    let mut tx = h.begin().await.unwrap();
    seed(&mut tx).await;
    let one = company(&tx.env(), 1);
    ir_default::set(
        &mut tx,
        &one,
        "property.computed",
        "count",
        &Value::Int(5),
        None,
        Some(erp_fields::RecId::from_real(2).unwrap()),
        None,
    )
    .await
    .unwrap();
    let model = one.records("property.computed").unwrap();
    let mut values = erp_orm::RawVals::new();
    values.insert_commands(
        "child_ids".into(),
        vec![erp_orm::Command::Create(raw_vals(&[
            ("name", Cell::Str("Line".into())),
            ("count", Cell::Int(3)),
        ]))],
    );
    let draft = tx.new_record(&model, values, None, None).await.unwrap();
    for (env, expected) in [(one.clone(), 3), (company(&one, 2), 5)] {
        let commands = draft
            .with_env(env)
            .x2many_commands(&mut tx, "child_ids")
            .await
            .unwrap();
        match commands.as_slice() {
            [erp_orm::Command::Set(ids), erp_orm::Command::Create(line)] => {
                assert!(ids.is_empty());
                assert_eq!(line.get("count"), Some(&Cell::Int(expected)));
            }
            other => panic!("unexpected commands {other:?}"),
        }
    }
    drop(tx);
    h.cleanup().await.unwrap();
}

async fn rpc_harness() -> erp_testkit::RpcHarness {
    support::rpc::harness_with_policy(
        &[&erp_addon_base::ADDON, &native_consumer::ADDON],
        erp_testkit::RulesPolicy::new(support::rpc::FixturePolicy).unwrap(),
        erp_rpc::RpcConfig::default(),
        false,
    )
    .await
}
fn alice_in(company: i64) -> erp_rpc::Caller {
    erp_rpc::Caller::Principal(
        support::rpc::principal_with(support::rpc::ALICE, &[company], 1)
            .build()
            .unwrap(),
    )
}
async fn json2(
    h: &erp_testkit::RpcHarness,
    method: &str,
    body: serde_json::Value,
    company: i64,
) -> serde_json::Value {
    let path = format!("/json/2/property.computed/{method}");
    let response = support::rpc::call(h, &path, &body.to_string(), alice_in(company)).await;
    assert_eq!(
        response.status(),
        200,
        "{method} in company {company}: {body}"
    );
    support::rpc::ok(&response)
}

#[tokio::test]
async fn onchange_and_create_over_rpc_keep_company_values_apart() {
    let h = rpc_harness().await;
    let mut tx = h.harness().begin().await.unwrap();
    seed(&mut tx).await;
    let env = company(&tx.env(), 1);
    let tag = env
        .records("property.computed")
        .unwrap()
        .create_values(&mut tx, &[vals(&[("name", Value::str("Tag"))])])
        .await
        .unwrap()
        .ids()[0];
    for (field, value, company) in [("tag_id", Value::Int(tag), 1), ("count", Value::Int(5), 2)] {
        ir_default::set(
            &mut tx,
            &env,
            "property.computed",
            field,
            &value,
            None,
            Some(erp_fields::RecId::from_real(company).unwrap()),
            None,
        )
        .await
        .unwrap();
    }
    tx.commit().await.unwrap();
    let mut tx = h.harness().begin().await.unwrap();
    let before = rows(&mut tx, "property_computed").await;
    drop(tx);
    let spec = json!({
        "name": {}, "count": {}, "count_double": {},
        "tag_id": {"fields": {"display_name": {}}}
    });

    // first call: each company's fallback is the default, the compute follows it
    let first = json!({"values": {}, "field_names": [], "fields_spec": spec});
    let one = json2(&h, "onchange", first.clone(), 1).await;
    let two = json2(&h, "onchange", first, 2).await;
    let stored = json2(
        &h,
        "web_read",
        json!({"ids": [tag], "specification": {"tag_id": {"fields": {"display_name": {}}}}}),
        1,
    )
    .await;
    // the draft's many2one has the wire shape of a stored record's (the tag reads the fallback)
    assert_eq!(one["value"]["tag_id"], stored[0]["tag_id"]);
    assert_eq!(one["value"]["tag_id"]["id"], json!(tag));
    assert_eq!(one["value"]["count"], json!(0));
    assert_eq!(one["value"]["count_double"], json!(0));
    assert_eq!(two["value"]["count"], json!(5));
    assert_eq!(two["value"]["count_double"], json!(10));
    assert_eq!(two["value"]["tag_id"], json!(false));

    // a changed company value is an onchange input; the compute over it recomputes in the draft
    let changed = json2(
        &h,
        "onchange",
        json!({
            "values": {"name": "Draft", "count": 4, "count_double": 0, "tag_id": tag},
            "field_names": ["count"], "fields_spec": spec
        }),
        1,
    )
    .await;
    assert_eq!(changed["value"]["count_double"], json!(8));

    // save goes through create: the caller's company only
    let id = json2(
        &h,
        "create",
        json!({"vals_list": [{"name": "Saved", "count": 4, "tag_id": tag}]}),
        1,
    )
    .await[0]
        .as_i64()
        .unwrap();
    let read = json!({"ids": [id], "fields": ["count", "count_double", "tag_id"], "load": ""});
    assert_eq!(
        json2(&h, "read", read.clone(), 1).await,
        json!([{"id": id, "count": 4, "count_double": 8, "tag_id": tag}])
    );
    assert_eq!(
        json2(&h, "read", read.clone(), 2).await,
        json!([{"id": id, "count": 5, "count_double": 10, "tag_id": false}])
    );

    // an edit form's draft starts from the stored map; its change writes nothing
    let edited = json2(
        &h,
        "onchange",
        json!({
            "ids": [id],
            "values": {"name": "Saved", "count": 6, "count_double": 10, "tag_id": null},
            "field_names": ["count"], "fields_spec": spec
        }),
        2,
    )
    .await;
    assert_eq!(edited["value"]["count_double"], json!(12));
    assert_eq!(
        json2(&h, "read", read.clone(), 2).await,
        json!([{"id": id, "count": 5, "count_double": 10, "tag_id": false}])
    );
    let mut tx = h.harness().begin().await.unwrap();
    assert_eq!(rows(&mut tx, "property_computed").await, before + 1);
    // the tag equals company 1's fallback, so only the count is an override
    assert_eq!(
        stored_map(&mut tx, "property_computed", "count", id).await,
        json!({"1": 4})
    );
    let tag_map: Option<serde_json::Value> = tx
        .query("SELECT tag_id FROM property_computed WHERE id = $1", &[&id])
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(tag_map, None);
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn edit_form_lines_keep_their_company_maps() {
    let h = rpc_harness().await;
    let mut tx = h.harness().begin().await.unwrap();
    seed(&mut tx).await;
    let env = company(&tx.env(), 1);
    let model = env.records("property.computed").unwrap();
    let tag = model
        .create_values(&mut tx, &[vals(&[("name", Value::str("Tag"))])])
        .await
        .unwrap()
        .ids()[0];
    for (field, value) in [("count", Value::Int(5)), ("tag_id", Value::Int(tag))] {
        ir_default::set(
            &mut tx,
            &env,
            "property.computed",
            field,
            &value,
            None,
            Some(erp_fields::RecId::from_real(1).unwrap()),
            None,
        )
        .await
        .unwrap();
    }
    let parent = model
        .create_values(&mut tx, &[vals(&[("name", Value::str("Parent"))])])
        .await
        .unwrap()
        .ids()[0];
    let lines = model
        .create_values(
            &mut tx,
            &[
                vals(&[
                    ("name", Value::str("Valued")),
                    ("parent_id", Value::Int(parent)),
                ]),
                vals(&[
                    ("name", Value::str("Nulled")),
                    ("parent_id", Value::Int(parent)),
                ]),
            ],
        )
        .await
        .unwrap()
        .ids();
    tx.flush().await.unwrap();
    // a value in both companies, and explicit nulls that suppress company 1's fallbacks
    for (id, count, tag_map) in [
        (lines[0], json!({"1": 7, "2": 9}), json!({"1": tag})),
        (lines[1], json!({"1": null}), json!({"1": null})),
    ] {
        tx.execute(
            "UPDATE property_computed SET count = $1, tag_id = $2 WHERE id = $3",
            &[&count, &tag_map, &id],
        )
        .await
        .unwrap();
    }
    tx.commit().await.unwrap();
    let read = json!({"ids": lines, "fields": ["count", "tag_id"], "load": ""});
    assert_eq!(
        json2(&h, "read", read, 1).await,
        json!([
            {"id": lines[0], "count": 7, "tag_id": tag},
            {"id": lines[1], "count": 0, "tag_id": false}
        ])
    );
    let spec = json!({
        "name": {}, "child_total": {}, "child_tagged": {},
        "child_ids": {"fields": {"name": {}, "count": {}, "tag_id": {}}}
    });
    // stale computed values in the form make the recomputed ones part of the answer
    // company 1: the edited line's new values, the other line's explicit nulls
    let one = json2(
        &h,
        "onchange",
        json!({
            "ids": [parent],
            "values": {
                "name": "Parent", "child_total": -1, "child_tagged": -1,
                "child_ids": [[1, lines[0], {"count": 8, "tag_id": null}]]
            },
            "field_names": ["child_ids"], "fields_spec": spec
        }),
        1,
    )
    .await;
    assert_eq!(one["value"]["child_total"], json!(8));
    assert_eq!(one["value"]["child_tagged"], json!(0));
    // company 2: the edited line's company 2 value; the other line has no company 2 entry
    let two = json2(
        &h,
        "onchange",
        json!({
            "ids": [parent],
            "values": {
                "name": "Parent", "child_total": -1, "child_tagged": -1,
                "child_ids": [[1, lines[0], {"count": 10}]]
            },
            "field_names": ["child_ids"], "fields_spec": spec
        }),
        2,
    )
    .await;
    assert_eq!(two["value"]["child_total"], json!(10));
    // nothing was written
    let mut tx = h.harness().begin().await.unwrap();
    assert_eq!(
        stored_map(&mut tx, "property_computed", "count", lines[0]).await,
        json!({"1": 7, "2": 9})
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn drafts_take_static_defaults_and_admit_html_per_company() {
    let h = Harness::builder([
        &base_users::ADDON,
        &erp_addon_base::ADDON,
        &native_consumer::ADDON,
    ])
    .build()
    .await
    .unwrap();
    let mut tx = h.begin().await.unwrap();
    seed(&mut tx).await;
    let one = company(&tx.env(), 1);
    let two = company(&tx.env(), 2);
    let model = one.records("property.computed").unwrap();
    let input = "<p>kept</p><script>dropped()</script>";
    // the stored record's value for the same input is the reference
    let stored = model
        .create_values(&mut tx, &[vals(&[("body", Value::str(input))])])
        .await
        .unwrap();
    let expected = stored.value(&mut tx, "body").await.unwrap();
    assert_ne!(expected, Value::str(input));
    // the static default is the environment company's value; another company has no default entry
    let draft = tx
        .new_record(
            &model,
            raw_vals(&[("body", Cell::Str(input.into()))]),
            None,
            None,
        )
        .await
        .unwrap();
    assert_eq!(draft.value(&mut tx, "quota").await.unwrap(), Value::Int(4));
    assert_eq!(
        draft
            .with_env(two.clone())
            .value(&mut tx, "quota")
            .await
            .unwrap(),
        Value::Int(0)
    );
    // the Html value is admitted like a stored one, for the environment company only
    assert_eq!(draft.value(&mut tx, "body").await.unwrap(), expected);
    assert_eq!(
        draft
            .with_env(two.clone())
            .value(&mut tx, "body")
            .await
            .unwrap(),
        Value::False
    );
    // a write on the draft in company 2 is admitted the same way and leaves company 1's value
    draft
        .with_env(two.clone())
        .write_values(&mut tx, &vals(&[("body", Value::str(input))]))
        .await
        .unwrap();
    assert_eq!(
        draft.with_env(two).value(&mut tx, "body").await.unwrap(),
        expected
    );
    assert_eq!(draft.value(&mut tx, "body").await.unwrap(), expected);
    drop(tx);
    h.cleanup().await.unwrap();
}
