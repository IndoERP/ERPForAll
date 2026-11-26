// SPDX-License-Identifier: MIT OR Apache-2.0
//! Whole-case tests of the company-dependent field spec cases, one test per case.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

use erp_fields::{Cell, Error, ErrorKind, Value};
use erp_orm::{Env, Records, SearchOptions, Transaction};
use erp_sdk::{Domain, Operator};
use erp_test_models::base_users;
use erp_testkit::Harness;
use serde_json::json;

fn company(env: &Env, id: i64) -> Env {
    env.with_context([("allowed_company_ids", Value::list([Value::Int(id)]))])
}
fn vals(items: &[(&str, Value)]) -> std::collections::BTreeMap<String, Value> {
    items
        .iter()
        .map(|(k, v)| ((*k).to_owned(), v.clone()))
        .collect()
}
async fn seed(tx: &mut Transaction) -> i64 {
    let currency = erp_testkit::fixtures::seed_currency_in(tx).await.unwrap();
    tx.execute("INSERT INTO res_company (tz, id, name, currency_id) VALUES ('UTC', 1, 'One', $1), ('UTC', 2, 'Two', $1), ('UTC', 3, 'Three', $1) ON CONFLICT DO NOTHING", &[&currency]).await.unwrap();
    currency
}
fn error(error: Error, kind: ErrorKind, message: &str) {
    assert_eq!(error.kind(), kind);
    assert_eq!(error.message(), message);
}

mod inherit_base {
    use erp_sdk::prelude::*;
    erp_sdk::addon!(name = "spec_inherit_property", depends = ["base"]);
    #[model(name = "spec.inherit.property", addon = "spec_inherit_property")]
    pub struct InheritProperty {
        #[field(char)]
        pub name: Option<Arc<str>>,
        #[field(integer, company_dependent = true)]
        pub property_foo: i32,
        #[field(integer, company_dependent = true)]
        pub property_bar: i32,
    }
}
mod inherit_override {
    use super::inherit_base::InheritProperty;
    use erp_sdk::prelude::*;
    /// What the compute of the redefined `property_bar` assigns.
    pub const BAR: i32 = 42;
    erp_sdk::addon!(
        name = "spec_inherit_property_override",
        depends = ["spec_inherit_property"]
    );
    #[extend(InheritProperty, addon = "spec_inherit_property_override")]
    pub struct PlainProperty {
        #[field(integer, company_dependent = false)]
        pub property_foo: i32,
        #[field(integer, compute = "_compute_bar", company_dependent = false)]
        pub property_bar: i32,
    }
    #[methods(InheritProperty, addon = "spec_inherit_property_override")]
    impl PlainProperty {
        #[compute(depends = [])]
        fn _compute_bar(batch: &mut Batch<'_, InheritProperty>) -> Result<()> {
            for i in batch.indices() {
                batch.set(i, PlainProperty::PROPERTY_BAR, BAR)?;
            }
            Ok(())
        }
    }
}

/// The `data_type` of each column of `table` named in `columns` (absent columns are left out).
async fn column_types(
    tx: &mut Transaction,
    table: &str,
    columns: &[&str],
) -> std::collections::BTreeMap<String, String> {
    let names: Vec<String> = columns.iter().map(|c| (*c).to_owned()).collect();
    tx.query(
        "SELECT column_name::text, data_type::text FROM information_schema.columns \
         WHERE table_schema = current_schema() AND table_name = $1 AND column_name = ANY($2)",
        &[&table, &names],
    )
    .await
    .unwrap()
    .iter()
    .map(|row| (row.get::<_, String>(0), row.get::<_, String>(1)))
    .collect()
}

#[tokio::test]
async fn spec_field_081_extension_redefines_company_dependent_fields_as_plain_and_computed() {
    // The base alone keeps both fields company-dependent in a jsonb column: the Given of the case.
    let base = Harness::builder([
        &base_users::ADDON,
        &erp_addon_base::ADDON,
        &inherit_base::ADDON,
    ])
    .build()
    .await
    .unwrap();
    let model = base.registry().model("spec.inherit.property").unwrap();
    for name in ["property_foo", "property_bar"] {
        assert!(model.field(name).unwrap().company_dependent, "{name}");
    }
    let mut tx = base.begin().await.unwrap();
    let types = column_types(
        &mut tx,
        "spec_inherit_property",
        &["property_foo", "property_bar"],
    )
    .await;
    assert_eq!(
        types,
        [("property_bar", "jsonb"), ("property_foo", "jsonb")]
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect()
    );
    drop(tx);
    base.cleanup().await.unwrap();

    let h = Harness::builder([
        &base_users::ADDON,
        &erp_addon_base::ADDON,
        &inherit_base::ADDON,
        &inherit_override::ADDON,
    ])
    .build()
    .await
    .unwrap();
    let model = h.registry().model("spec.inherit.property").unwrap();
    let foo = model.field("property_foo").unwrap();
    let bar = model.field("property_bar").unwrap();
    assert!(!foo.company_dependent);
    assert!(foo.store);
    assert!(foo.has_column());
    assert!(foo.compute.is_none());
    assert!(!bar.company_dependent);
    assert!(bar.compute.is_some());
    assert!(!bar.store);
    assert!(!bar.has_column());

    let mut tx = h.begin().await.unwrap();
    seed(&mut tx).await;
    // No current company: neither field needs one once it is no longer company-dependent.
    let env = tx.env();
    let record = env
        .records("spec.inherit.property")
        .unwrap()
        .create_values(&mut tx, &[vals(&[("name", Value::str("Stuff"))])])
        .await
        .unwrap();
    assert_eq!(
        record.value(&mut tx, "name").await.unwrap(),
        Value::str("Stuff")
    );
    assert_eq!(
        record.value(&mut tx, "property_foo").await.unwrap(),
        Value::Int(0)
    );
    assert_eq!(
        record.value(&mut tx, "property_bar").await.unwrap(),
        Value::Int(i64::from(inherit_override::BAR))
    );
    // The same reads in a company: one value for every company.
    let one = record.with_env(company(&env, 1));
    assert_eq!(
        one.value(&mut tx, "property_foo").await.unwrap(),
        Value::Int(0)
    );
    assert_eq!(
        one.value(&mut tx, "property_bar").await.unwrap(),
        Value::Int(i64::from(inherit_override::BAR))
    );
    tx.flush().await.unwrap();
    let types = column_types(
        &mut tx,
        "spec_inherit_property",
        &["property_foo", "property_bar"],
    )
    .await;
    // `property_foo` is an integer column; the computed `property_bar` has no column; no jsonb.
    assert_eq!(
        types,
        [("property_foo".to_owned(), "integer".to_owned())]
            .into_iter()
            .collect()
    );
    assert!(types.values().all(|kind| kind != "jsonb"));
    let stored = tx
        .query(
            "SELECT property_foo FROM spec_inherit_property WHERE id = $1",
            &[&record.ids()[0]],
        )
        .await
        .unwrap();
    // Stored as SQL NULL or 0; either way the plain integer reads 0 and no company map exists.
    assert!(matches!(stored[0].get::<_, Option<i32>>(0), None | Some(0)));
    drop(tx);
    h.cleanup().await.unwrap();
}

mod fixture {
    use erp_sdk::prelude::*;
    erp_sdk::addon!(name = "spec_property", depends = ["base"]);
    #[model(name = "spec.html", addon = "spec_property")]
    pub struct HtmlProperty {
        #[field(html, company_dependent = true, sanitize = false)]
        pub html1: Option<Arc<str>>,
        #[field(html, company_dependent = true, sanitize = true)]
        pub html2: Option<Arc<str>>,
    }
    #[model(name = "spec.big", addon = "spec_property")]
    pub struct BigProperty {
        #[field(char)]
        pub name: Option<Arc<str>>,
        #[field(bigint, company_dependent = true)]
        pub big: BigInt,
    }
    #[model(
        name = "spec.ctag",
        company_access = unrestricted(reason = "fixture; company access is not under test"),
        description = "Spec Company Tag",
        addon = "spec_property"
    )]
    pub struct CompanyTag {
        #[field(char)]
        pub name: Option<Arc<str>>,
        #[field(many2one, comodel = "res.company")]
        pub company_id: Option<RecId>,
    }
    #[model(
        name = "spec.nocompany",
        company_access = unrestricted(reason = "fixture; company access is not under test"),
        description = "Spec No Company",
        addon = "spec_property"
    )]
    pub struct NoCompany {
        #[field(char)]
        pub name: Option<Arc<str>>,
        #[field(many2one, comodel = "res.company")]
        pub company_id: Option<RecId>,
        /// An ordinary link, checked against the record's company.
        #[field(many2one, comodel = "spec.ctag", string = "Tag")]
        pub tag_id: Option<RecId>,
        #[field(char, company_dependent = true)]
        pub note: Option<Arc<str>>,
        #[field(boolean, company_dependent = true)]
        pub truth: bool,
        #[field(integer, company_dependent = true)]
        pub count: i32,
        #[field(float, company_dependent = true)]
        pub ratio: f64,
        /// A company-dependent link, checked against the current company.
        #[field(
            many2one,
            comodel = "spec.ctag",
            company_dependent = true,
            check_company,
            string = "Preferred Tag"
        )]
        pub pref_tag_id: Option<RecId>,
    }
    #[model(name = "spec.required", addon = "spec_property")]
    pub struct RequiredProperty {
        #[field(char)]
        pub name: Option<Arc<str>>,
        #[field(char, company_dependent = true, required)]
        pub note: Option<Arc<str>>,
    }
    #[model(name = "spec.draft", addon = "spec_property")]
    pub struct Draft {
        #[field(char)]
        pub name: Option<Arc<str>>,
        #[field(char, company_dependent = true)]
        pub note: Option<Arc<str>>,
        #[field(integer, company_dependent = true)]
        pub count: i32,
        #[field(many2one, comodel = "spec.tag", company_dependent = true)]
        pub tag_id: Option<RecId>,
        #[field(char, compute = "_compute_tag_state")]
        pub tag_state: Option<Arc<str>>,
        #[field(many2one, comodel = "spec.draft")]
        pub parent_id: Option<RecId>,
        #[field(one2many, comodel = "spec.draft", inverse_name = "parent_id")]
        pub line_ids: Vec<RecId>,
    }
    #[methods(Draft, addon = "spec_property")]
    impl Draft {
        #[compute(depends = ["tag_id"], depends_context = ["company"])]
        fn _compute_tag_state(batch: &mut Batch<'_, Draft>) -> Result<()> {
            for i in batch.indices() {
                let state = if batch.get(i, Draft::TAG_ID)?.is_some() {
                    "tagged"
                } else {
                    "untagged"
                };
                batch.set(i, Draft::TAG_STATE, Some(state.into()))?;
            }
            Ok(())
        }
    }
    #[model(name = "spec.holder", addon = "spec_property")]
    pub struct Holder {
        #[field(char)]
        pub name: Option<Arc<str>>,
        #[field(
            many2one,
            comodel = "spec.tag",
            company_dependent = true,
            ondelete = "restrict"
        )]
        pub tag_id: Option<RecId>,
        /// The default referential action: set null.
        #[field(many2one, comodel = "spec.tag", company_dependent = true)]
        pub other_id: Option<RecId>,
    }
    #[model(name = "spec.tag", addon = "spec_property")]
    pub struct Tag {
        #[field(char)]
        pub name: Option<Arc<str>>,
    }
    #[model(name = "spec.computed.links", addon = "spec_property")]
    pub struct ComputedLinks {
        #[field(many2one, comodel = "spec.tag")]
        pub source_id: Option<RecId>,
        #[field(many2many, comodel = "spec.tag", compute = "_compute_tags", store)]
        pub tag_ids: Vec<RecId>,
    }
    #[methods(ComputedLinks, addon = "spec_property")]
    impl ComputedLinks {
        #[compute(depends = ["source_id"])]
        fn _compute_tags(batch: &mut Batch<'_, ComputedLinks>) -> Result<()> {
            for i in batch.indices() {
                let target = batch.get(i, ComputedLinks::SOURCE_ID)?;
                batch.set(
                    i,
                    ComputedLinks::TAG_IDS,
                    target.into_iter().collect::<Vec<_>>(),
                )?;
            }
            Ok(())
        }
    }
    #[model(name = "spec.search", addon = "spec_property")]
    pub struct SearchProperty {
        #[field(boolean, company_dependent = true, index = "btree_not_null")]
        pub truth: bool,
        #[field(integer, company_dependent = true, index = "btree_not_null")]
        pub count: i32,
        #[field(float, company_dependent = true, index = "btree_not_null")]
        pub phi: f64,
        #[field(char, company_dependent = true, index = "btree_not_null")]
        pub foo: Option<Arc<str>>,
        #[field(date, company_dependent = true, index = "btree_not_null")]
        pub day: Option<Date>,
        #[field(datetime, company_dependent = true, index = "btree_not_null")]
        pub moment: Option<DateTime>,
        #[field(many2one, comodel = "spec.tag", company_dependent = true)]
        pub tag_id: Option<RecId>,
        #[field(many2one, comodel = "res.company", company_dependent = true)]
        pub company_ref: Option<RecId>,
        #[field(many2one, comodel = "res.partner", company_dependent = true)]
        pub partner_ref: Option<RecId>,
        #[field(text, company_dependent = true, index = "btree_not_null")]
        pub body: Option<Arc<str>>,
        #[field(html, company_dependent = true)]
        pub page: Option<Arc<str>>,
    }
}

async fn harness() -> Harness {
    Harness::builder([&base_users::ADDON, &erp_addon_base::ADDON, &fixture::ADDON])
        .build()
        .await
        .unwrap()
}

/// A transaction of user `uid`, who may act in companies 1 to 3 and whose current company is
/// `current`.
async fn begin_in_company(h: &Harness, uid: i64, current: i64) -> Transaction {
    let all = [1, 2, 3].map(|id| erp_orm::CompanyId::new(id).unwrap());
    let resolution = erp_orm::Resolution::in_id_order(
        erp_orm::GroupSet::default(),
        all,
        std::num::NonZeroU64::MIN,
    )
    .unwrap();
    let principal = erp_orm::PrincipalBuilder::user(erp_orm::UserId::new(uid).unwrap())
        .resolution(resolution)
        .companies(vec![erp_orm::CompanyId::new(current).unwrap()])
        .build()
        .unwrap();
    let tx = h
        .orm()
        .begin_opts(erp_orm::BeginOptions::for_principal(principal))
        .await
        .unwrap();
    assert_eq!(
        tx.env().company().unwrap(),
        Some(erp_orm::CompanyId::new(current).unwrap())
    );
    tx
}

/// The sanitised form keeps the text and closes every tag the input left open.
fn assert_sanitised(value: &Value, closed: &[&str], text: &[&str]) {
    assert!(matches!(value, Value::Str(_)), "not a string: {value:?}");
    let Value::Str(html) = value else { return };
    for tag in closed {
        assert!(
            html.contains(&format!("</{tag}>")),
            "{tag} not closed: {html}"
        );
    }
    for word in text {
        assert!(html.contains(word), "{word} lost: {html}");
    }
}

/// The model, declared natively: `html2` is sanitised with class and style
/// stripping, a policy the field macro has no option for.
fn html_case_model() -> erp_registry::ModelDecl {
    use erp_fields::html::{DefaultPolicy, FieldPolicy, SanitizePolicy};
    use erp_registry::FieldDecl;
    const FIELDS: &[FieldDecl] = &[
        FieldDecl::html("html1")
            .company_dependent(true)
            .sanitize(false),
        FieldDecl::html("html2")
            .company_dependent(true)
            .html_policy(FieldPolicy::Sanitize(SanitizePolicy::Default(
                DefaultPolicy::new().with_strip_classes().with_strip_style(),
            ))),
    ];
    erp_registry::ModelDecl::new("spec_property", "spec.htmlcase").fields(FIELDS)
}

#[tokio::test]
async fn spec_field_086_company_dependent_html_per_user_company() {
    // Malformed on purpose: `div`, `p`, `b` and `td` are never closed.
    const FIRST: &str = "<div class=\"spec-note\" style=\"margin: 0px\"><p>Reorder <b>pallets<table><tr class=\"spec-row\"><td style=\"border: 1px solid black\">north dock";
    const SECOND: &str = "<div class=\"spec-other\" style=\"padding: 3px\"><p>Count <b>crates<table><tr class=\"spec-line\"><td style=\"border: 2px solid black\">south dock";
    const OPEN: [&str; 6] = ["div", "p", "b", "table", "tr", "td"];
    // What the sanitiser must strip is in both inputs.
    for input in [FIRST, SECOND] {
        assert!(input.contains("class=") && input.contains("style="));
    }
    let no_class_or_style = |value: &Value| {
        assert!(matches!(value, Value::Str(_)), "not a string: {value:?}");
        if let Value::Str(html) = value {
            assert!(!html.contains("class="), "{html}");
            assert!(!html.contains("style="), "{html}");
        }
    };
    let h = Harness::builder([&base_users::ADDON, &erp_addon_base::ADDON, &fixture::ADDON])
        .with_decls([html_case_model()])
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    seed(&mut tx).await;
    tx.execute("INSERT INTO res_users (id, login, name, active, share) VALUES (20, 'spec-zero', 'Zero', true, false), (21, 'spec-one', 'One', true, false), (22, 'spec-two', 'Two', true, false) ON CONFLICT DO NOTHING", &[]).await.unwrap();
    tx.commit().await.unwrap();
    // users 20, 21 and 22 act in companies 1, 2 and 3: the case's companies 0, 1 and 2
    let users = [(20, 1), (21, 2), (22, 3)];

    let mut zero = begin_in_company(&h, users[0].0, users[0].1).await;
    let record = zero
        .env()
        .records("spec.htmlcase")
        .unwrap()
        .create_values(
            &mut zero,
            &[vals(&[
                ("html1", Value::str(FIRST)),
                ("html2", Value::str(FIRST)),
            ])],
        )
        .await
        .unwrap();
    let id = record.ids()[0];
    zero.commit().await.unwrap();

    let read = async |h: &Harness, user: (i64, i64), field: &'static str| -> Value {
        let mut tx = begin_in_company(h, user.0, user.1).await;
        let record = tx
            .env()
            .records("spec.htmlcase")
            .unwrap()
            .browse([id])
            .unwrap();
        record.value(&mut tx, field).await.unwrap()
    };
    assert_eq!(read(&h, users[0], "html1").await, Value::str(FIRST));
    assert_eq!(read(&h, users[1], "html1").await, Value::False);
    assert_eq!(read(&h, users[2], "html1").await, Value::False);
    let sanitised_zero = read(&h, users[0], "html2").await;
    no_class_or_style(&sanitised_zero);
    assert_sanitised(
        &sanitised_zero,
        &OPEN,
        &["Reorder", "pallets", "north dock"],
    );
    assert_ne!(sanitised_zero, Value::str(FIRST));
    assert_eq!(read(&h, users[1], "html2").await, Value::False);
    assert_eq!(read(&h, users[2], "html2").await, Value::False);

    let mut one = begin_in_company(&h, users[1].0, users[1].1).await;
    one.env()
        .records("spec.htmlcase")
        .unwrap()
        .browse([id])
        .unwrap()
        .write_values(
            &mut one,
            &vals(&[("html1", Value::str(SECOND)), ("html2", Value::str(SECOND))]),
        )
        .await
        .unwrap();
    one.commit().await.unwrap();

    assert_eq!(read(&h, users[0], "html1").await, Value::str(FIRST));
    assert_eq!(read(&h, users[1], "html1").await, Value::str(SECOND));
    assert_eq!(read(&h, users[2], "html1").await, Value::False);
    assert_eq!(read(&h, users[0], "html2").await, sanitised_zero);
    let sanitised_one = read(&h, users[1], "html2").await;
    assert_sanitised(&sanitised_one, &OPEN, &["Count", "crates", "south dock"]);
    no_class_or_style(&sanitised_one);
    assert_ne!(sanitised_one, sanitised_zero);
    assert_eq!(read(&h, users[2], "html2").await, Value::False);
    h.cleanup().await.unwrap();
}

async fn set_default(
    tx: &mut Transaction,
    env: &Env,
    model: &str,
    field: &str,
    value: &Value,
    company: Option<i64>,
) {
    erp_orm::ir_default::set(
        tx,
        env,
        model,
        field,
        value,
        None,
        company.map(|id| erp_fields::RecId::from_real(id).unwrap()),
        None,
    )
    .await
    .unwrap();
}
fn cond(field: &str, op: Operator, value: impl Into<erp_domain::Value>) -> Domain {
    Domain::condition(field, op, value).unwrap()
}
/// Search, count and complement of `domain` over `records` against `filtered_domain` on them.
async fn check(tx: &mut Transaction, records: &Records, domain: Domain) -> Vec<i64> {
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
        got.len() as u64,
        "{domain:?}"
    );
    let complement = records
        .search(tx, &(scope & !domain.clone()))
        .await
        .unwrap();
    assert_eq!(got.len() + complement.len(), records.len(), "{domain:?}");
    assert!(
        complement.ids().iter().all(|id| !got.ids().contains(id)),
        "{domain:?}"
    );
    got_ids
}

#[tokio::test]
async fn bigint_company_values_fallback_search_and_order() {
    // All beyond 32 bits, and beyond the 53 bits a float keeps exact.
    const HIGH: i64 = 9_007_199_254_740_993;
    const MID: i64 = 5_000_000_000;
    const LOW: i64 = -9_007_199_254_740_995;
    const FALLBACK: i64 = 6_000_000_001;
    let big = |n: i64| Value::BigInt(erp_fields::BigInt::from_i64(n));
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    seed(&mut tx).await;
    let one = company(&tx.env(), 1);
    let two = company(&tx.env(), 2);
    let three = company(&tx.env(), 3);
    let model = one.records("spec.big").unwrap();
    let records = model
        .create_values(
            &mut tx,
            &[
                vals(&[("name", Value::str("a")), ("big", big(HIGH))]),
                vals(&[("name", Value::str("b")), ("big", big(LOW))]),
                vals(&[("name", Value::str("c"))]),
            ],
        )
        .await
        .unwrap();
    let ids = records.ids().to_vec();
    let (a, b, c) = (ids[0], ids[1], ids[2]);
    // Company 1 reads its values; companies 2 and 3 have none and no fallback exists yet.
    assert_eq!(read_big(&mut tx, &one, a).await, big(HIGH));
    assert_eq!(read_big(&mut tx, &one, b).await, big(LOW));
    assert_eq!(read_big(&mut tx, &one, c).await, big(0));
    assert_eq!(read_big(&mut tx, &two, a).await, big(0));
    // Company 2 writes its own value on `a`; company 1 keeps its own.
    model
        .browse([a])
        .unwrap()
        .with_env(two.clone())
        .write_values(&mut tx, &vals(&[("big", big(MID))]))
        .await
        .unwrap();
    assert_eq!(read_big(&mut tx, &two, a).await, big(MID));
    assert_eq!(read_big(&mut tx, &one, a).await, big(HIGH));
    assert_eq!(read_big(&mut tx, &three, a).await, big(0));
    // The stored map holds the exact integers under each company.
    tx.flush().await.unwrap();
    let stored = tx
        .query("SELECT big::text FROM spec_big WHERE id = $1", &[&a])
        .await
        .unwrap();
    let map: serde_json::Value = serde_json::from_str(&stored[0].get::<_, String>(0)).unwrap();
    // each entry is the exact integer (a JSON number or its decimal text), under its company
    let entries: std::collections::BTreeMap<String, String> = map
        .as_object()
        .unwrap()
        .iter()
        .map(|(company, value)| {
            let text = value
                .as_i64()
                .map(|n| n.to_string())
                .or_else(|| value.as_str().map(str::to_owned))
                .unwrap_or_else(|| panic!("not an integer entry: {value}"));
            (company.clone(), text)
        })
        .collect();
    assert_eq!(
        entries,
        [("1", HIGH), ("2", MID)]
            .into_iter()
            .map(|(company, n)| (company.to_owned(), n.to_string()))
            .collect()
    );
    // A fallback for company 3 only, then a shared one: the more specific one wins.
    set_default(&mut tx, &one, "spec.big", "big", &big(FALLBACK), Some(3)).await;
    assert_eq!(read_big(&mut tx, &three, a).await, big(FALLBACK));
    assert_eq!(read_big(&mut tx, &three, c).await, big(FALLBACK));
    assert_eq!(read_big(&mut tx, &two, c).await, big(0));
    assert_eq!(read_big(&mut tx, &one, c).await, big(0));
    set_default(&mut tx, &one, "spec.big", "big", &big(MID), None).await;
    assert_eq!(read_big(&mut tx, &three, c).await, big(FALLBACK));
    assert_eq!(read_big(&mut tx, &two, c).await, big(MID));
    assert_eq!(read_big(&mut tx, &one, c).await, big(MID));
    assert_eq!(read_big(&mut tx, &one, a).await, big(HIGH));

    // Search: effective values per company. Company 1: a=HIGH, b=LOW, c=MID (fallback).
    // Company 2: a=MID, b=MID (fallback), c=MID (fallback). Company 3: all FALLBACK.
    let effective = |company: i64| -> [(i64, i64); 3] {
        match company {
            1 => [(a, HIGH), (b, LOW), (c, MID)],
            2 => [(a, MID), (b, MID), (c, MID)],
            _ => [(a, FALLBACK), (b, FALLBACK), (c, FALLBACK)],
        }
    };
    for (number, env) in [(1, &one), (2, &two), (3, &three)] {
        let scoped = records.with_env(env.clone());
        let values = effective(number);
        for operand in [HIGH, MID, LOW, FALLBACK, 0, HIGH - 1, i64::from(i32::MAX)] {
            for (op, keep) in [
                (Operator::Eq, (|v, o| v == o) as fn(i64, i64) -> bool),
                (Operator::Ne, |v, o| v != o),
                (Operator::Lt, |v, o| v < o),
                (Operator::Le, |v, o| v <= o),
                (Operator::Gt, |v, o| v > o),
                (Operator::Ge, |v, o| v >= o),
            ] {
                let mut expected: Vec<i64> = values
                    .iter()
                    .filter(|(_, value)| keep(*value, operand))
                    .map(|(id, _)| *id)
                    .collect();
                expected.sort_unstable();
                let got = check(&mut tx, &scoped, cond("big", op, operand.to_string())).await;
                assert_eq!(got, expected, "company {number}: big {op:?} {operand}");
            }
        }
        for op in [Operator::In, Operator::NotIn] {
            let list = [HIGH, MID];
            let mut expected: Vec<i64> = values
                .iter()
                .filter(|(_, value)| list.contains(value) == (op == Operator::In))
                .map(|(id, _)| *id)
                .collect();
            expected.sort_unstable();
            let got = check(
                &mut tx,
                &scoped,
                cond(
                    "big",
                    op,
                    erp_domain::Value::list(list.map(|n| n.to_string())),
                ),
            )
            .await;
            assert_eq!(got, expected, "company {number}: big {op:?} {list:?}");
        }
        // Order by the effective value, id as the tie-break, both directions.
        let scope = cond(
            "id",
            Operator::In,
            erp_domain::Value::list(ids.iter().copied()),
        );
        let mut ascending = values;
        ascending.sort_by_key(|(id, value)| (*value, *id));
        let got = scoped
            .search_with(&mut tx, &scope, &SearchOptions::new().order("big, id"))
            .await
            .unwrap();
        assert_eq!(
            got.ids().to_vec(),
            ascending.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
            "company {number}"
        );
        let mut descending = values;
        descending.sort_by_key(|(id, value)| (std::cmp::Reverse(*value), *id));
        let got = scoped
            .search_with(&mut tx, &scope, &SearchOptions::new().order("big desc, id"))
            .await
            .unwrap();
        assert_eq!(
            got.ids().to_vec(),
            descending.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
            "company {number}"
        );
        let options = SearchOptions::new().order("big desc, id").limit(1);
        let mut cursor = None;
        for (id, value) in descending {
            let page = scoped
                .search_after(&mut tx, &scope, &options, cursor.as_ref())
                .await
                .unwrap();
            assert_eq!(page.records.ids(), &[id]);
            let next = page.next.unwrap();
            assert_eq!(next.values()[0], big(value));
            cursor = Some(erp_orm::Cursor::from_json(&next.to_json().unwrap()).unwrap());
        }
    }
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Run explicitly to compare PostgreSQL bulk mutation and company pagination costs.
#[tokio::test]
#[ignore = "PostgreSQL profiling fixture"]
async fn profile_company_bigint_bulk_and_paging() {
    use erp_data::ImpexExt;
    use std::time::Instant;

    const SIZE: usize = 50_000;
    const DEEP: usize = 40_000;
    const WIDTH: u64 = 100;
    const FIRST: i64 = 9_007_199_254_740_993;
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    seed(&mut tx).await;
    let env = company(&tx.env(), 1);
    let model = env.records("spec.big").unwrap();
    let create: Vec<_> = (0..SIZE)
        .map(|index| {
            vals(&[
                ("name", Value::str("created")),
                (
                    "big",
                    Value::BigInt(erp_fields::BigInt::from_i64(FIRST + index as i64)),
                ),
            ])
        })
        .collect();
    let load: Vec<_> = (0..SIZE)
        .map(|index| vec!["loaded".to_owned(), (FIRST + index as i64).to_string()])
        .collect();
    let start = Instant::now();
    let created = model.create_values(&mut tx, &create).await.unwrap();
    tx.flush().await.unwrap();
    let create_time = start.elapsed();
    assert_eq!(created.len(), SIZE);
    let start = Instant::now();
    let loaded = model.load(&mut tx, &["name", "big"], &load).await.unwrap();
    tx.flush().await.unwrap();
    let load_time = start.elapsed();
    assert_eq!(
        loaded.ids.as_ref().map(Vec::len),
        Some(SIZE),
        "{:?}",
        loaded.messages
    );
    let created_ids = created.ids();
    let scope = cond("name", Operator::Eq, "created");
    let order = SearchOptions::new().order("big, id").limit(WIDTH);
    let cursor = erp_orm::Cursor::new(
        "big, id",
        vec![
            Value::BigInt(erp_fields::BigInt::from_i64(FIRST + DEEP as i64 - 1)),
            Value::Int(created_ids[DEEP - 1]),
        ],
    );
    let offset_order = order.clone().offset(DEEP as u64);
    let mut offsets = Vec::new();
    let mut cursors = Vec::new();
    for _ in 0..7 {
        let start = Instant::now();
        let page = model
            .search_with(&mut tx, &scope, &offset_order)
            .await
            .unwrap();
        offsets.push(start.elapsed());
        let start = Instant::now();
        let resumed = model
            .search_after(&mut tx, &scope, &order, Some(&cursor))
            .await
            .unwrap();
        cursors.push(start.elapsed());
        assert_eq!(page.ids(), resumed.records.ids());
        assert_eq!(page.ids(), &created_ids[DEEP..DEEP + WIDTH as usize]);
    }
    offsets.sort_unstable();
    cursors.sort_unstable();
    println!(
        "{SIZE} rows each: create+flush {create_time:?}, load+flush {load_time:?}; median page {DEEP}/{WIDTH}: offset {:?}, keyset {:?}",
        offsets[3], cursors[3]
    );
    drop(tx);
    h.cleanup().await.unwrap();
}
async fn read_big(tx: &mut Transaction, env: &Env, id: i64) -> Value {
    env.records("spec.big")
        .unwrap()
        .browse([id])
        .unwrap()
        .value(tx, "big")
        .await
        .unwrap()
}

/// The statement of a search of `domain` on `records`, as sent to the database.
async fn search_sql(tx: &mut Transaction, records: &Records, domain: &Domain) -> String {
    let table = records.spec().table.to_string();
    tx.flush().await.unwrap();
    tx.capture_statements(true);
    records.search(tx, domain).await.unwrap();
    let sql = tx
        .captured()
        .iter()
        .rev()
        .find(|sql| sql.contains(&format!("FROM \"{table}\"")))
        .cloned();
    tx.capture_statements(false);
    assert!(
        sql.is_some(),
        "the search of {domain:?} sent no statement on {table}"
    );
    sql.unwrap_or_default()
}
/// The effective-value expression of `field` in `sql`: the entry of the current company in the
/// stored map, coalesced with the company's fallback (both come from one bound parameter, so no
/// value is in the text), as text cast to `cast`.
fn assert_effective_expression(sql: &str, table: &str, field: &str, cast: &str) {
    let head = format!("COALESCE(\"{table}\".\"{field}\"->((");
    let at = sql.find(&head);
    assert!(at.is_some(), "no company entry of {field} in: {sql}");
    let rest = &sql[at.unwrap_or_default() + head.len()..];
    let split = rest.split_once("::text::jsonb)->>'company'), (");
    assert!(split.is_some(), "no company key of {field} in: {sql}");
    let (slot, rest) = split.unwrap_or_default();
    assert!(
        slot.strip_prefix('$')
            .is_some_and(|n| n.parse::<usize>().is_ok_and(|n| n > 0)),
        "{field}: the company context is not a bound parameter: {sql}"
    );
    let rest = rest.strip_prefix(&format!("{slot}::text::jsonb)->'values'->"));
    assert!(rest.is_some(), "no fallback of {field} in: {sql}");
    let split = rest.unwrap_or_default().split_once(") END)#>>'{}')::");
    assert!(split.is_some(), "no cast of {field} in: {sql}");
    let (key, rest) = split.unwrap_or_default();
    assert!(
        key.len() > 2 && key.starts_with('\'') && key.ends_with('\'') && !key.contains(' '),
        "{field}: fallback key {key} in: {sql}"
    );
    let after = rest.strip_prefix(cast);
    assert!(
        after.is_some_and(
            |after| !after.starts_with(|c: char| c.is_ascii_alphanumeric() || c == '_')
        ),
        "{field} is not cast to {cast} in: {sql}"
    );
}

/// One field of the grid.
struct GridField {
    name: &'static str,
    /// The two truthy values, as written and as `ir.default`.
    truthy: [Value; 2],
    /// Operator and operands.
    grid: Vec<(Operator, Vec<erp_domain::Value>)>,
    /// The SQL type the effective value is cast to.
    cast: &'static str,
    /// Whether the field has a `btree_not_null` index.
    indexed: bool,
}

#[tokio::test]
async fn spec_field_085_search_grid_on_company_dependent_fields() {
    use erp_domain::Value as D;
    const TABLE: &str = "spec_search";
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    seed(&mut tx).await;
    let env = company(&tx.env(), 1);
    let model = env.records("spec.search").unwrap();
    // Four records for every case (two truthy values, an explicit falsy one, no value) and one
    // that never gets a value: it reads the fallback, whatever the fallback is.
    let records = model
        .create_values(&mut tx, &[vals(&[]), vals(&[]), vals(&[]), vals(&[])])
        .await
        .unwrap();
    let fallback = model.create_values(&mut tx, &[vals(&[])]).await.unwrap();
    let tags = env
        .records("spec.tag")
        .unwrap()
        .create_values(
            &mut tx,
            &[
                vals(&[("name", Value::str("oak"))]),
                vals(&[("name", Value::str("elm"))]),
            ],
        )
        .await
        .unwrap();
    let (oak, elm) = (tags.ids()[0], tags.ids()[1]);
    // Companies 1 > 2 > 3 and three partners in the same shape.
    tx.execute("UPDATE res_company SET parent_id = CASE id WHEN 2 THEN 1 WHEN 3 THEN 2 ELSE NULL END, parent_path = CASE id WHEN 1 THEN '1/' WHEN 2 THEN '1/2/' WHEN 3 THEN '1/2/3/' END WHERE id IN (1,2,3)", &[]).await.unwrap();
    let partners = env.records("res.partner").unwrap();
    let mut partner_ids: Vec<i64> = Vec::new();
    for name in ["Root", "Branch", "Leaf"] {
        let mut values = vec![("name", Value::str(name))];
        if let Some(parent) = partner_ids.last() {
            values.push(("parent_id", Value::Int(*parent)));
        }
        let partner = partners
            .create_values(&mut tx, &[vals(&values)])
            .await
            .unwrap();
        partner_ids.push(partner.ids()[0]);
    }
    let comparisons = [Operator::Lt, Operator::Ge, Operator::Le, Operator::Gt];
    let likes = [
        Operator::Like,
        Operator::ILike,
        Operator::NotLike,
        Operator::NotILike,
    ];
    let equalities = [Operator::Eq, Operator::Ne];
    let memberships = [Operator::In, Operator::NotIn];
    // `=`/`!=` with the two values, `extra` and false; the comparisons with the values and `extra`.
    let ordered = |values: [D; 2], extra: Option<D>| -> Vec<(Operator, Vec<D>)> {
        let mut out = Vec::new();
        for op in equalities {
            let mut operands = values.to_vec();
            operands.extend(extra.clone());
            operands.push(D::FALSE);
            out.push((op, operands));
        }
        for op in comparisons {
            let mut operands = values.to_vec();
            operands.extend(extra.clone());
            out.push((op, operands));
        }
        out
    };
    // The like family, `=`/`!=` and `in`/`not in` of a text field.
    let textual = |a: &str, b: &str, like_a: &str, like_b: &str| -> Vec<(Operator, Vec<D>)> {
        let mut out = Vec::new();
        for op in likes {
            out.push((op, vec![D::from(like_a), D::from(like_b)]));
        }
        for op in equalities {
            out.push((op, vec![D::from(a), D::from(b), D::FALSE]));
        }
        for op in memberships {
            out.push((
                op,
                vec![
                    D::list([D::from(a), D::from(b)]),
                    D::list([D::from(a), D::FALSE]),
                    D::list([D::FALSE]),
                    D::list(Vec::<D>::new()),
                ],
            ));
        }
        out
    };
    let hierarchy = |ids: [i64; 3]| -> Vec<(Operator, Vec<D>)> {
        [Operator::ChildOf, Operator::ParentOf]
            .into_iter()
            .map(|op| (op, ids.iter().map(|id| D::Int(*id)).collect()))
            .collect()
    };
    let mut relation = Vec::new();
    for op in likes {
        relation.push((op, vec![D::from("oak"), D::from("elm")]));
    }
    for op in equalities {
        relation.push((op, vec![D::Int(oak), D::Int(elm), D::FALSE]));
    }
    for op in memberships {
        relation.push((
            op,
            vec![
                D::list([D::Int(oak), D::Int(elm)]),
                D::list([D::Int(elm), D::FALSE]),
                D::list([D::FALSE]),
                D::list(Vec::<D>::new()),
            ],
        ));
    }
    for op in [Operator::Any, Operator::NotAny] {
        relation.push((
            op,
            vec![
                D::Domain(cond("name", Operator::Eq, "oak")),
                D::Domain(cond("name", Operator::Eq, false)),
                D::Domain(Domain::TRUE),
            ],
        ));
    }
    let (day_a, day_b) = ("2026-03-09", "2026-03-10");
    let (moment_a, moment_b) = ("2026-03-09 07:08:09", "2026-03-10 07:08:09");
    let (page_a, page_b) = ("<p>cedar plank</p>", "<p>birch plank</p>");
    let fields = vec![
        GridField {
            name: "truth",
            truthy: [Value::Bool(true), Value::Bool(true)],
            grid: equalities
                .into_iter()
                .map(|op| (op, vec![D::from(true), D::FALSE]))
                .collect(),
            cast: "boolean",
            indexed: true,
        },
        GridField {
            name: "count",
            truthy: [Value::Int(17), Value::Int(-3)],
            grid: ordered([D::Int(17), D::Int(-3)], Some(D::Int(0))),
            cast: "int4",
            indexed: true,
        },
        GridField {
            name: "phi",
            truthy: [Value::Float(2.75), Value::Float(-1.5)],
            grid: ordered([D::from(2.75), D::from(-1.5)], Some(D::from(0.0))),
            cast: "float8",
            indexed: true,
        },
        GridField {
            name: "foo",
            truthy: [Value::str("cedar"), Value::str("birch")],
            grid: textual("cedar", "birch", "cedar", "birch"),
            cast: "text",
            indexed: true,
        },
        GridField {
            name: "day",
            truthy: [Value::str(day_a), Value::str(day_b)],
            grid: ordered([D::from(day_a), D::from(day_b)], None),
            cast: "date",
            indexed: true,
        },
        GridField {
            name: "moment",
            truthy: [Value::str(moment_a), Value::str(moment_b)],
            grid: ordered([D::from(moment_a), D::from(moment_b)], None),
            cast: "timestamp",
            indexed: true,
        },
        GridField {
            name: "tag_id",
            truthy: [Value::Int(oak), Value::Int(elm)],
            grid: relation,
            cast: "int8",
            indexed: false,
        },
        GridField {
            name: "company_ref",
            truthy: [Value::Int(2), Value::Int(3)],
            grid: hierarchy([1, 2, 3]),
            cast: "int8",
            indexed: false,
        },
        GridField {
            name: "partner_ref",
            truthy: [Value::Int(partner_ids[1]), Value::Int(partner_ids[2])],
            grid: hierarchy([partner_ids[0], partner_ids[1], partner_ids[2]]),
            cast: "int8",
            indexed: false,
        },
        GridField {
            name: "body",
            truthy: [Value::str("cedar plank"), Value::str("birch plank")],
            grid: textual("cedar plank", "birch plank", "cedar", "birch"),
            cast: "text",
            indexed: true,
        },
        GridField {
            name: "page",
            truthy: [Value::str(page_a), Value::str(page_b)],
            grid: textual(page_a, page_b, "cedar", "birch"),
            cast: "text",
            indexed: false,
        },
    ];
    let scope = cond(
        "id",
        Operator::In,
        erp_domain::Value::list(records.ids().iter().copied()),
    );
    let mut searches = 0_usize;
    let mut prefiltered = 0_usize;
    // Conditions on a `btree_not_null` field whose fallback does not satisfy them, and those of
    // them whose statement has no `IS NOT NULL` prefilter.
    let mut unsatisfied = 0_usize;
    let mut without_prefilter: Vec<String> = Vec::new();
    for field in &fields {
        // no default, default false, default = the first truthy value
        for default in [None, Some(Value::False), Some(field.truthy[0].clone())] {
            if let Some(value) = &default {
                set_default(&mut tx, &env, "spec.search", field.name, value, None).await;
            }
            for (id, value) in records.ids()[..3].iter().zip([
                field.truthy[0].clone(),
                field.truthy[1].clone(),
                Value::False,
            ]) {
                model
                    .browse([*id])
                    .unwrap()
                    .write_values(&mut tx, &vals(&[(field.name, value)]))
                    .await
                    .unwrap();
            }
            // The first two records read what was written (Html: its admitted form), the unset
            // record and the fallback record read the default of this phase.
            let unset = model.browse([records.ids()[3]]).unwrap();
            assert_eq!(
                unset.value(&mut tx, field.name).await.unwrap(),
                fallback.value(&mut tx, field.name).await.unwrap(),
                "{} with default {default:?}",
                field.name
            );
            for (operator, operands) in &field.grid {
                for operand in operands {
                    let domain = cond(field.name, *operator, operand.clone());
                    check(&mut tx, &records, domain.clone()).await;
                    searches += 1;
                    // A membership test of an empty list is decided without the field (`in []`
                    // matches nothing, `not in []` everything): no effective value to look for.
                    if matches!(operand, D::List(items) if items.is_empty()) {
                        continue;
                    }
                    let sql = search_sql(&mut tx, &model, &(scope.clone() & domain.clone())).await;
                    assert_effective_expression(&sql, TABLE, field.name, field.cast);
                    let satisfied = !fallback
                        .filtered_domain(&mut tx, &domain)
                        .await
                        .unwrap()
                        .is_empty();
                    // Not asserted, clause not met: the `IS NOT NULL` prefilter of a condition
                    // with `=`, `!=`, `in` or `not in` whose fallback does not satisfy it. The
                    // statement of such a condition has no prefilter; the comparisons and the
                    // like family have it and are asserted here.
                    let asserted = comparisons.contains(operator) || likes.contains(operator);
                    if field.indexed && !satisfied {
                        let prefilter = format!("\"{TABLE}\".\"{}\" IS NOT NULL", field.name);
                        unsatisfied += 1;
                        if asserted {
                            assert!(
                                sql.contains(&prefilter),
                                "{domain:?} with default {default:?}: the fallback does not \
                                 satisfy the condition, yet no `{prefilter}` in: {sql}"
                            );
                            prefiltered += 1;
                        } else if !sql.contains(&prefilter) {
                            // reported at the end, in grid order; not asserted (see above)
                            without_prefilter.push(format!("{domain:?} with default {default:?}"));
                        }
                    }
                }
            }
        }
    }
    // The grid ran: every operand of every field in three default states.
    let expected: usize = fields
        .iter()
        .map(|field| {
            field
                .grid
                .iter()
                .map(|(_, operands)| operands.len())
                .sum::<usize>()
                * 3
        })
        .sum();
    assert_eq!(searches, expected);
    assert!(prefiltered > 0);
    // The finding of the partial tag, printed from a passing run (nextest shows it with
    // `--success-output final`).
    println!(
        " prefilter: {} of {unsatisfied} conditions whose fallback does not satisfy them have no IS NOT NULL prefilter",
        without_prefilter.len()
    );
    for condition in &without_prefilter {
        println!("{condition}");
    }
    drop(tx);
    h.cleanup().await.unwrap();
}

const NO_COMPANY: &str = "company-dependent fields require a current company";
/// The company-consistency refusal of one link.
fn inconsistent(record: &str, company: &str, label: &str, field: &str, target: &str) -> String {
    format!(
        "These values link records of different companies:\n- “{record}” is in company “{company}”, and “{label}” ({field}: '{target}') points to a record of another company.\nChoose records of the same company, or records shared by all companies."
    )
}
async fn count_rows(tx: &mut Transaction, table: &str) -> i64 {
    tx.flush().await.unwrap();
    let sql = format!("SELECT count(*) FROM {table}");
    tx.query(&sql, &[]).await.unwrap()[0].get(0)
}

#[tokio::test]
async fn spec_field_106_create_without_a_current_company() {
    const PROPERTIES: &str = "note IS NULL AND truth IS NULL AND count IS NULL AND ratio IS NULL AND pref_tag_id IS NULL";
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    seed(&mut tx).await;
    // The System environment: no current company.
    let env = tx.env();
    assert_eq!(env.company().unwrap(), None);
    let one = company(&env, 1);
    let two = company(&env, 2);
    let three = company(&env, 3);
    let tags = env
        .records("spec.ctag")
        .unwrap()
        .create_values(
            &mut tx,
            &[
                vals(&[
                    ("name", Value::str("TagOne")),
                    ("company_id", Value::Int(1)),
                ]),
                vals(&[
                    ("name", Value::str("TagTwo")),
                    ("company_id", Value::Int(2)),
                ]),
            ],
        )
        .await
        .unwrap();
    let (tag_one, tag_two) = (tags.ids()[0], tags.ids()[1]);
    let model = env.records("spec.nocompany").unwrap();

    // Omitted properties need no company and store SQL NULL: dynamic and typed.
    let records = model
        .create_values(
            &mut tx,
            &[
                vals(&[
                    ("name", Value::str("first")),
                    ("company_id", Value::Int(1)),
                    ("tag_id", Value::Int(tag_one)),
                ]),
                vals(&[("name", Value::str("second"))]),
            ],
        )
        .await
        .unwrap();
    let typed = env
        .model::<fixture::NoCompany>()
        .unwrap()
        .create(
            &mut tx,
            vec![erp_orm::Vals::new().set(
                fixture::NoCompany::NAME,
                Some(std::sync::Arc::<str>::from("typed")),
            )],
        )
        .await
        .unwrap();
    let mut expected_rows = 3_i64;
    assert_eq!(count_rows(&mut tx, "spec_nocompany").await, expected_rows);
    let null_rows = async |tx: &mut Transaction| -> i64 {
        tx.flush().await.unwrap();
        let sql = format!("SELECT count(*) FROM spec_nocompany WHERE {PROPERTIES}");
        tx.query(&sql, &[]).await.unwrap()[0].get(0)
    };
    assert_eq!(null_rows(&mut tx).await, expected_rows);
    // Ordinary fields keep working without a company.
    let first = model.browse([records.ids()[0]]).unwrap();
    let second = model.browse([records.ids()[1]]).unwrap();
    assert_eq!(
        first.value(&mut tx, "name").await.unwrap(),
        Value::str("first")
    );
    assert_eq!(
        first.value(&mut tx, "company_id").await.unwrap(),
        Value::Int(1)
    );
    assert_eq!(
        first.value(&mut tx, "tag_id").await.unwrap(),
        Value::Int(tag_one)
    );
    assert_eq!(
        typed.get(&mut tx, fixture::NoCompany::NAME).await.unwrap(),
        Some("typed".into())
    );
    second
        .write_values(&mut tx, &vals(&[("name", Value::str("second renamed"))]))
        .await
        .unwrap();
    assert_eq!(
        second.value(&mut tx, "name").await.unwrap(),
        Value::str("second renamed")
    );
    // Automatic company consistency of an ordinary link still refuses, on create and on write.
    let refused = tx
        .savepoint(async |tx| {
            model
                .create_values(
                    tx,
                    &[vals(&[
                        ("name", Value::str("mismatch")),
                        ("company_id", Value::Int(1)),
                        ("tag_id", Value::Int(tag_two)),
                    ])],
                )
                .await
                .map(|_| ())
        })
        .await;
    error(
        refused.unwrap_err(),
        ErrorKind::UserError,
        &inconsistent("mismatch", "One", "Tag", "tag_id", "TagTwo"),
    );
    let refused = tx
        .savepoint(async |tx| first.set_value(tx, "tag_id", Value::Int(tag_two)).await)
        .await;
    error(
        refused.unwrap_err(),
        ErrorKind::UserError,
        &inconsistent("first", "One", "Tag", "tag_id", "TagTwo"),
    );
    assert_eq!(count_rows(&mut tx, "spec_nocompany").await, expected_rows);

    // A supplied property refuses, also null, false and zero, and the batch creates nothing.
    for (field, value) in [
        ("note", Value::str("value")),
        ("note", Value::False),
        ("truth", Value::Bool(true)),
        ("truth", Value::False),
        ("count", Value::Int(5)),
        ("count", Value::Int(0)),
        ("ratio", Value::Float(0.0)),
        ("pref_tag_id", Value::Int(tag_one)),
        ("pref_tag_id", Value::False),
    ] {
        let result = tx
            .savepoint(async |tx| {
                model
                    .create_values(
                        tx,
                        &[
                            vals(&[("name", Value::str("must roll back"))]),
                            vals(&[("name", Value::str("supplied")), (field, value.clone())]),
                        ],
                    )
                    .await
                    .map(|_| ())
            })
            .await;
        error(result.unwrap_err(), ErrorKind::Validation, NO_COMPANY);
        assert_eq!(
            count_rows(&mut tx, "spec_nocompany").await,
            expected_rows,
            "{field} = {value:?}"
        );
    }
    // The typed surface: a supplied null, zero and false.
    for supplied in [
        erp_orm::Vals::new().set(fixture::NoCompany::NOTE, None::<std::sync::Arc<str>>),
        erp_orm::Vals::new().set(fixture::NoCompany::COUNT, 0),
        erp_orm::Vals::new().set(fixture::NoCompany::TRUTH, false),
    ] {
        let typed_model = env.model::<fixture::NoCompany>().unwrap();
        let result = tx
            .savepoint(async |tx| {
                typed_model
                    .create(
                        tx,
                        vec![
                            erp_orm::Vals::new().set(
                                fixture::NoCompany::NAME,
                                Some(std::sync::Arc::<str>::from("must roll back")),
                            ),
                            supplied.clone(),
                        ],
                    )
                    .await
                    .map(|_| ())
            })
            .await;
        error(result.unwrap_err(), ErrorKind::Validation, NO_COMPANY);
        assert_eq!(count_rows(&mut tx, "spec_nocompany").await, expected_rows);
    }
    // Supplied through creation defaults.
    for (key, value) in [
        ("default_note", Value::str("default")),
        ("default_count", Value::Int(0)),
    ] {
        let defaults = model.with_env(env.with_context([(key, value)]));
        let result = tx
            .savepoint(async |tx| {
                defaults
                    .create_values(tx, &[vals(&[("name", Value::str("defaulted"))])])
                    .await
                    .map(|_| ())
            })
            .await;
        error(result.unwrap_err(), ErrorKind::Validation, NO_COMPANY);
        assert_eq!(
            count_rows(&mut tx, "spec_nocompany").await,
            expected_rows,
            "{key}"
        );
    }

    // A shared `ir.default` fallback is not a supplied value: not offered, stored as NULL, and
    // read by every company as its fallback.
    set_default(
        &mut tx,
        &one,
        "spec.nocompany",
        "note",
        &Value::str("shared"),
        None,
    )
    .await;
    assert!(
        model
            .default_get(&mut tx, &["name", "note"])
            .await
            .unwrap()
            .get("note")
            .is_none()
    );
    let inherited = model
        .create_values(&mut tx, &[vals(&[("name", Value::str("inherits"))])])
        .await
        .unwrap();
    expected_rows += 1;
    assert_eq!(count_rows(&mut tx, "spec_nocompany").await, expected_rows);
    assert_eq!(null_rows(&mut tx).await, expected_rows);
    for env in [&one, &two, &three] {
        assert_eq!(
            inherited
                .with_env(env.clone())
                .value(&mut tx, "note")
                .await
                .unwrap(),
            Value::str("shared")
        );
    }

    // A company-dependent link is matched against the current company only.
    let dep = one
        .records("spec.nocompany")
        .unwrap()
        .create_values(
            &mut tx,
            &[vals(&[
                ("name", Value::str("dep")),
                ("company_id", Value::Int(1)),
                ("pref_tag_id", Value::Int(tag_one)),
            ])],
        )
        .await
        .unwrap();
    expected_rows += 1;
    let refused = tx
        .savepoint(async |tx| dep.set_value(tx, "pref_tag_id", Value::Int(tag_two)).await)
        .await;
    error(
        refused.unwrap_err(),
        ErrorKind::UserError,
        &inconsistent("dep", "One", "Preferred Tag", "pref_tag_id", "TagTwo"),
    );
    tx.flush().await.unwrap();
    tx.invalidate_all().await.unwrap();
    // Writing the record's own company field needs no current company and does not re-check the
    // link (it now points to a tag of company 1 from a record of company 2).
    let dep_without = dep.with_env(env.clone());
    dep_without
        .set_value(&mut tx, "company_id", Value::Int(2))
        .await
        .unwrap();
    tx.flush().await.unwrap();
    assert_eq!(
        dep_without.value(&mut tx, "company_id").await.unwrap(),
        Value::Int(2)
    );
    assert_eq!(
        dep.value(&mut tx, "pref_tag_id").await.unwrap(),
        Value::Int(tag_one)
    );
    // Writing the property itself still needs one; so does reading it.
    for (field, value) in [
        ("pref_tag_id", Value::Int(tag_two)),
        ("pref_tag_id", Value::False),
        ("note", Value::str("value")),
        ("note", Value::False),
        ("count", Value::Int(0)),
    ] {
        let result = tx
            .savepoint(async |tx| dep_without.set_value(tx, field, value.clone()).await)
            .await;
        error(result.unwrap_err(), ErrorKind::Validation, NO_COMPANY);
    }
    for field in ["note", "truth", "count", "ratio", "pref_tag_id"] {
        error(
            dep_without.value(&mut tx, field).await.unwrap_err(),
            ErrorKind::Validation,
            NO_COMPANY,
        );
    }
    // An explicitly empty company list is no company either, never an implicit one.
    let empty = dep.with_env(env.with_context([("allowed_company_ids", Value::list([]))]));
    error(
        empty.value(&mut tx, "note").await.unwrap_err(),
        ErrorKind::Validation,
        NO_COMPANY,
    );
    let result = tx
        .savepoint(async |tx| empty.set_value(tx, "note", Value::str("value")).await)
        .await;
    error(result.unwrap_err(), ErrorKind::Validation, NO_COMPANY);

    // Later reads and writes in valid companies keep independent values.
    let in_one = inherited.with_env(one.clone());
    let in_two = inherited.with_env(two.clone());
    let in_three = inherited.with_env(three.clone());
    in_one
        .write_values(
            &mut tx,
            &vals(&[("note", Value::str("one")), ("count", Value::Int(11))]),
        )
        .await
        .unwrap();
    tx.flush().await.unwrap();
    assert_eq!(
        in_two.value(&mut tx, "note").await.unwrap(),
        Value::str("shared")
    );
    assert_eq!(in_two.value(&mut tx, "count").await.unwrap(), Value::Int(0));
    in_two
        .write_values(
            &mut tx,
            &vals(&[("note", Value::str("two")), ("count", Value::Int(22))]),
        )
        .await
        .unwrap();
    tx.flush().await.unwrap();
    tx.invalidate_all().await.unwrap();
    assert_eq!(
        in_one.value(&mut tx, "note").await.unwrap(),
        Value::str("one")
    );
    assert_eq!(
        in_one.value(&mut tx, "count").await.unwrap(),
        Value::Int(11)
    );
    assert_eq!(
        in_two.value(&mut tx, "note").await.unwrap(),
        Value::str("two")
    );
    assert_eq!(
        in_two.value(&mut tx, "count").await.unwrap(),
        Value::Int(22)
    );
    assert_eq!(
        in_three.value(&mut tx, "note").await.unwrap(),
        Value::str("shared")
    );
    assert_eq!(
        in_three.value(&mut tx, "count").await.unwrap(),
        Value::Int(0)
    );
    assert_eq!(count_rows(&mut tx, "spec_nocompany").await, expected_rows);

    // A required company-dependent value is still required: omitted, it refuses with or without
    // a company; supplied without a company, it refuses for the missing company.
    let required = env.records("spec.required").unwrap();
    for model in [&required, &required.with_env(one.clone())] {
        let result = tx
            .savepoint(async |tx| {
                model
                    .create_values(tx, &[vals(&[("name", Value::str("needs a note"))])])
                    .await
                    .map(|_| ())
            })
            .await;
        error(
            result.unwrap_err(),
            ErrorKind::Validation,
            "spec.required.note: required company-dependent value is missing",
        );
    }
    let result = tx
        .savepoint(async |tx| {
            required
                .create_values(tx, &[vals(&[("note", Value::str("present"))])])
                .await
                .map(|_| ())
        })
        .await;
    error(result.unwrap_err(), ErrorKind::Validation, NO_COMPANY);
    assert_eq!(count_rows(&mut tx, "spec_required").await, 0);
    let kept = required
        .with_env(one.clone())
        .create_values(&mut tx, &[vals(&[("note", Value::str("present"))])])
        .await
        .unwrap();
    assert_eq!(
        kept.value(&mut tx, "note").await.unwrap(),
        Value::str("present")
    );
    assert_eq!(count_rows(&mut tx, "spec_required").await, 1);

    // A company the principal may not use is no validated company: a direct property read and
    // a direct property write refuse, and nothing is stored. User 30 may act in companies 1 and
    // 2 only.
    const UNAUTHORIZED: &str =
        "One of the selected companies does not exist or is not one of yours.";
    tx.execute("INSERT INTO res_users (id, login, name, active, share) VALUES (30, 'spec-limited', 'Limited', true, false)", &[]).await.unwrap();
    let id = inherited.ids()[0];
    tx.commit().await.unwrap();
    let may_use = [1, 2].map(|id| erp_orm::CompanyId::new(id).unwrap());
    let resolution = erp_orm::Resolution::in_id_order(
        erp_orm::GroupSet::default(),
        may_use,
        std::num::NonZeroU64::MIN,
    )
    .unwrap();
    let principal = erp_orm::PrincipalBuilder::user(erp_orm::UserId::new(30).unwrap())
        .resolution(resolution)
        .companies(vec![may_use[0]])
        .build()
        .unwrap();
    let mut tx = h
        .orm()
        .begin_opts(erp_orm::BeginOptions::for_principal(principal))
        .await
        .unwrap();
    let limited = tx.env();
    let own = limited
        .records("spec.nocompany")
        .unwrap()
        .browse([id])
        .unwrap();
    // In its own companies the user reads the values written above.
    assert_eq!(own.value(&mut tx, "note").await.unwrap(), Value::str("one"));
    assert_eq!(
        own.with_env(company(&limited, 2))
            .value(&mut tx, "note")
            .await
            .unwrap(),
        Value::str("two")
    );
    let before = stored(&mut tx, "spec_nocompany", "note", id).await;
    assert_eq!(before, Some(json!({"1": "one", "2": "two"})));
    // Company 3 exists and is not the user's: alone, and behind a company the user may use.
    for selected in [vec![3], vec![1, 3]] {
        let foreign = own.with_env(limited.with_context([(
            "allowed_company_ids",
            Value::list(selected.iter().map(|id| Value::Int(*id))),
        )]));
        for field in ["note", "truth", "count", "ratio", "pref_tag_id"] {
            error(
                foreign.value(&mut tx, field).await.unwrap_err(),
                ErrorKind::Access,
                UNAUTHORIZED,
            );
        }
        for (field, value) in [
            ("note", Value::str("foreign")),
            ("note", Value::False),
            ("count", Value::Int(0)),
            ("pref_tag_id", Value::False),
        ] {
            let result = tx
                .savepoint(async |tx| foreign.set_value(tx, field, value.clone()).await)
                .await;
            error(result.unwrap_err(), ErrorKind::Access, UNAUTHORIZED);
        }
        assert_eq!(stored(&mut tx, "spec_nocompany", "note", id).await, before);
    }
    drop(tx);
    h.cleanup().await.unwrap();
}

/// The stored value of `column` in row `id` of `table`: `None` for SQL NULL.
async fn stored(
    tx: &mut Transaction,
    table: &str,
    column: &str,
    id: i64,
) -> Option<serde_json::Value> {
    tx.flush().await.unwrap();
    let sql = format!("SELECT {column} FROM {table} WHERE id = $1");
    tx.query(&sql, &[&id]).await.unwrap()[0].get(0)
}

#[tokio::test]
async fn spec_field_110_saving_drafts_and_draft_many2one_values() {
    const FALLBACK_ONE: i32 = 5;
    const FALLBACK_TWO: i32 = 9;
    const LINE_COUNT: i32 = 3;
    // Record rules hide the rows named `hidden` from readers without elevation.
    let visible = cond("name", Operator::Ne, "hidden");
    let policy = erp_testkit::DomainPolicy::builder(erp_testkit::Otherwise::Allow)
        .rule("spec.draft", erp_orm::Operation::Read, visible)
        .build()
        .unwrap();
    let h = Harness::builder([&base_users::ADDON, &erp_addon_base::ADDON, &fixture::ADDON])
        .policy(policy)
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    seed(&mut tx).await;
    tx.execute(
        "INSERT INTO res_users (id, name, login, active) VALUES (7, 'plain', 'plain', true) ON CONFLICT DO NOTHING",
        &[],
    )
    .await
    .unwrap();
    let one = company(&tx.env(), 1);
    let two = company(&tx.env(), 2);
    set_default(
        &mut tx,
        &one,
        "spec.draft",
        "count",
        &Value::Int(i64::from(FALLBACK_ONE)),
        Some(1),
    )
    .await;
    set_default(
        &mut tx,
        &one,
        "spec.draft",
        "count",
        &Value::Int(i64::from(FALLBACK_TWO)),
        Some(2),
    )
    .await;
    set_default(
        &mut tx,
        &one,
        "spec.draft",
        "note",
        &Value::str("note two"),
        Some(2),
    )
    .await;
    let model = one.records("spec.draft").unwrap();
    let target = one
        .records("spec.tag")
        .unwrap()
        .create_values(&mut tx, &[vals(&[("name", Value::str("target"))])])
        .await
        .unwrap()
        .ids()[0];

    // Saving a draft through `create`: the caller's company value only, and a value equal to
    // that company's fallback stores no override.
    let draft = tx
        .new_record(
            &model,
            erp_orm::RawVals::new()
                .set_cell("name", Cell::Str("Saved".into()))
                .set_cell("note", Cell::Str("note one".into()))
                .set_cell("count", Cell::Int(FALLBACK_ONE))
                .set_cell("tag_id", Cell::Id(target)),
            None,
            None,
        )
        .await
        .unwrap();
    let mut save = Vec::new();
    for field in ["name", "note", "count", "tag_id"] {
        save.push((field, draft.value(&mut tx, field).await.unwrap()));
    }
    assert_eq!(
        save,
        vec![
            ("name", Value::str("Saved")),
            ("note", Value::str("note one")),
            ("count", Value::Int(i64::from(FALLBACK_ONE))),
            ("tag_id", Value::Int(target)),
        ]
    );
    let saved = model.create_values(&mut tx, &[vals(&save)]).await.unwrap();
    let id = saved.ids()[0];
    assert_eq!(
        stored(&mut tx, "spec_draft", "note", id).await,
        Some(json!({"1": "note one"}))
    );
    assert_eq!(
        stored(&mut tx, "spec_draft", "tag_id", id).await,
        Some(json!({"1": target}))
    );
    assert_eq!(stored(&mut tx, "spec_draft", "count", id).await, None);
    tx.invalidate_all().await.unwrap();
    assert_eq!(
        saved.value(&mut tx, "note").await.unwrap(),
        Value::str("note one")
    );
    assert_eq!(
        saved.value(&mut tx, "count").await.unwrap(),
        Value::Int(i64::from(FALLBACK_ONE))
    );
    assert_eq!(
        saved.value(&mut tx, "tag_id").await.unwrap(),
        Value::Int(target)
    );
    // another company reads its own fallback
    let other = saved.with_env(two.clone());
    assert_eq!(
        other.value(&mut tx, "note").await.unwrap(),
        Value::str("note two")
    );
    assert_eq!(
        other.value(&mut tx, "count").await.unwrap(),
        Value::Int(i64::from(FALLBACK_TWO))
    );
    assert_eq!(other.value(&mut tx, "tag_id").await.unwrap(), Value::False);

    // Write commands of draft lines carry the environment's company scalar, never a map.
    let mut values = erp_orm::RawVals::new();
    values.insert_commands(
        "line_ids".into(),
        vec![erp_orm::Command::Create(
            erp_orm::RawVals::new()
                .set_cell("name", Cell::Str("Line".into()))
                .set_cell("count", Cell::Int(LINE_COUNT))
                .set_cell("note", Cell::Str("line one".into())),
        )],
    );
    let parent = tx.new_record(&model, values, None, None).await.unwrap();
    // The line's values were given in company 1; company 2 has none and carries its fallbacks.
    for (env, count, note) in [
        (one.clone(), LINE_COUNT, "line one"),
        (two.clone(), FALLBACK_TWO, "note two"),
    ] {
        let commands = parent
            .with_env(env)
            .x2many_commands(&mut tx, "line_ids")
            .await
            .unwrap();
        match commands.as_slice() {
            [erp_orm::Command::Set(ids), erp_orm::Command::Create(line)] => {
                assert!(ids.is_empty());
                assert_eq!(line.get("count"), Some(&Cell::Int(count)));
                assert_eq!(line.get("note"), Some(&Cell::Str(note.into())));
            }
            other => panic!("unexpected commands {other:?}"),
        }
    }

    // Origins for a reader without elevation: one readable, one hidden by the record rule.
    let mut origins = Vec::new();
    for name in ["shown", "hidden"] {
        let origin = model
            .create_values(
                &mut tx,
                &[vals(&[
                    ("name", Value::str(name)),
                    ("tag_id", Value::Int(target)),
                ])],
            )
            .await
            .unwrap()
            .ids()[0];
        assert_eq!(
            stored(&mut tx, "spec_draft", "tag_id", origin).await,
            Some(json!({"1": target}))
        );
        origins.push(origin);
    }
    let (shown, hidden) = (origins[0], origins[1]);
    tx.commit().await.unwrap();

    let mut tx = h.begin_as(7).await.unwrap();
    let user = company(&tx.env(), 1);
    assert!(!user.su());
    let drafts = user.records("spec.draft").unwrap();
    let origin = |id: i64| Some(erp_fields::RecId::from_real(id).unwrap());
    // A readable origin: the draft reads the reference and the compute over it.
    let readable = tx
        .new_record(&drafts, erp_orm::RawVals::new(), origin(shown), None)
        .await
        .unwrap();
    assert_eq!(
        readable.cell(&mut tx, "tag_id").await.unwrap(),
        Cell::Id(target)
    );
    assert_eq!(
        readable.value(&mut tx, "tag_state").await.unwrap(),
        Value::str("tagged")
    );
    // Changing it on the draft changes the draft and its compute, and no stored map.
    readable
        .write_values(&mut tx, &vals(&[("tag_id", Value::False)]))
        .await
        .unwrap();
    assert_eq!(readable.cell(&mut tx, "tag_id").await.unwrap(), Cell::Null);
    assert_eq!(
        readable.value(&mut tx, "tag_state").await.unwrap(),
        Value::str("untagged")
    );
    assert_eq!(
        stored(&mut tx, "spec_draft", "tag_id", shown).await,
        Some(json!({"1": target}))
    );
    assert_eq!(
        drafts
            .browse([shown])
            .unwrap()
            .cell(&mut tx, "tag_id")
            .await
            .unwrap(),
        Cell::Id(target)
    );
    // A hidden origin: the draft's read refuses with the stored record's Access error, every time.
    let secret = tx
        .new_record(&drafts, erp_orm::RawVals::new(), origin(hidden), None)
        .await
        .unwrap();
    let of_stored = drafts
        .browse([hidden])
        .unwrap()
        .cell(&mut tx, "tag_id")
        .await
        .unwrap_err();
    assert_eq!(of_stored.kind(), ErrorKind::Access);
    for _ in 0..3 {
        let refused = secret.cell(&mut tx, "tag_id").await.unwrap_err();
        assert_eq!(refused.kind(), of_stored.kind());
        assert_eq!(refused.message(), of_stored.message());
    }
    // An unsaved target cannot be written into a persisted company map.
    let unsaved = tx
        .new_record(
            &user.records("spec.tag").unwrap(),
            erp_orm::RawVals::new(),
            None,
            None,
        )
        .await
        .unwrap();
    let link = erp_orm::RawVals::new().set_cell("tag_id", Cell::Id(unsaved.rec_ids()[0].get()));
    let persisted = drafts.browse([shown]).unwrap();
    let result = tx
        .savepoint(async |tx| persisted.write(tx, link.clone()).await)
        .await;
    error(
        result.unwrap_err(),
        ErrorKind::Validation,
        "spec.draft.tag_id: save the related record before writing a company-dependent many2one",
    );
    // Nothing of the drafts reached the table.
    assert_eq!(
        stored(&mut tx, "spec_draft", "tag_id", shown).await,
        Some(json!({"1": target}))
    );
    assert_eq!(
        stored(&mut tx, "spec_draft", "tag_id", hidden).await,
        Some(json!({"1": target}))
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn company_property_drafts_keep_local_targets_and_export_only_saved_origins() {
    use erp_orm::{Command, RawVals};

    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    seed(&mut tx).await;
    let one = company(&tx.env(), 1);
    let two = company(&tx.env(), 2);
    let tags = one.records("spec.tag").unwrap();
    let saved_tag = tags
        .create_values(&mut tx, &[vals(&[("name", Value::str("saved"))])])
        .await
        .unwrap()
        .rec_ids()[0];
    let edit_tag = tx
        .new_record(&tags, RawVals::new(), Some(saved_tag), None)
        .await
        .unwrap()
        .rec_ids()[0];
    let new_tag = tx
        .new_record(&tags, RawVals::new(), None, None)
        .await
        .unwrap()
        .rec_ids()[0];
    let drafts = one.records("spec.draft").unwrap();
    let mut input = RawVals::new();
    input.insert_commands(
        "line_ids".into(),
        vec![Command::Create(
            RawVals::new()
                .set_cell("tag_id", Cell::Id(edit_tag.get()))
                .set_cell("note", Cell::Str("line input".into())),
        )],
    );
    let parent = tx.new_record(&drafts, input, None, None).await.unwrap();
    let lines = parent.related(&mut tx, "line_ids").await.unwrap();
    assert_eq!(
        lines.cell(&mut tx, "tag_id").await.unwrap(),
        Cell::Id(edit_tag.get())
    );
    let other = lines.with_env(two.clone());
    assert_eq!(other.cell(&mut tx, "tag_id").await.unwrap(), Cell::Null);
    other
        .write(
            &mut tx,
            RawVals::new().set_cell("tag_id", Cell::Id(new_tag.get())),
        )
        .await
        .unwrap();
    assert_eq!(
        other.cell(&mut tx, "tag_id").await.unwrap(),
        Cell::Id(new_tag.get())
    );
    assert_eq!(
        lines.cell(&mut tx, "tag_id").await.unwrap(),
        Cell::Id(edit_tag.get())
    );
    let commands = parent.x2many_commands(&mut tx, "line_ids").await.unwrap();
    match commands.as_slice() {
        [Command::Set(_), Command::Create(line)] => {
            assert_eq!(line.get("tag_id"), Some(&Cell::Id(saved_tag.get())))
        }
        other => panic!("unexpected commands {other:?}"),
    }
    let error = parent
        .with_env(two)
        .x2many_commands(&mut tx, "line_ids")
        .await
        .unwrap_err();
    assert!(
        error
            .message()
            .contains("points at a new record without origin"),
        "{error}"
    );
    assert_eq!(count_rows(&mut tx, "spec_draft").await, 0);
    let mut vals = RawVals::new().set_cell("note", Cell::Str("parent input".into()));
    vals.insert_commands("line_ids".into(), commands);
    let persisted = drafts.create(&mut tx, vec![vals]).await.unwrap();
    let saved_line = persisted.related(&mut tx, "line_ids").await.unwrap();
    assert_eq!(
        persisted.value(&mut tx, "note").await.unwrap(),
        Value::str("parent input")
    );
    assert_eq!(
        saved_line.value(&mut tx, "note").await.unwrap(),
        Value::str("line input")
    );
    assert_eq!(
        saved_line.cell(&mut tx, "tag_id").await.unwrap(),
        Cell::Id(saved_tag.get())
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn stored_computed_many2many_survives_reload_and_replaces_relation_rows() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let env = tx.env();
    let tags = env
        .records("spec.tag")
        .unwrap()
        .create_values(&mut tx, &[vals(&[]), vals(&[])])
        .await
        .unwrap();
    let (first, second) = (tags.ids()[0], tags.ids()[1]);
    let linked = env
        .records("spec.computed.links")
        .unwrap()
        .create_values(&mut tx, &[vals(&[("source_id", Value::Int(first))])])
        .await
        .unwrap();
    let id = linked.ids()[0];
    tx.commit().await.unwrap();

    let mut tx = h.begin().await.unwrap();
    let linked = tx
        .env()
        .records("spec.computed.links")
        .unwrap()
        .browse([id])
        .unwrap();
    assert_eq!(
        linked.related(&mut tx, "tag_ids").await.unwrap().ids(),
        &[first]
    );
    linked
        .write_values(&mut tx, &vals(&[("source_id", Value::Int(second))]))
        .await
        .unwrap();
    tx.flush().await.unwrap();
    tx.invalidate_all().await.unwrap();
    assert_eq!(
        linked.related(&mut tx, "tag_ids").await.unwrap().ids(),
        &[second]
    );
    linked
        .write_values(&mut tx, &vals(&[("source_id", Value::False)]))
        .await
        .unwrap();
    tx.flush().await.unwrap();
    tx.invalidate_all().await.unwrap();
    assert!(linked.related(&mut tx, "tag_ids").await.unwrap().is_empty());
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_rel_015_company_dependent_many2one_deletion_and_dangling_values() {
    const DANGLING: i64 = 1000;
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    seed(&mut tx).await;
    let one = company(&tx.env(), 1);
    let two = company(&tx.env(), 2);
    let tags = one.records("spec.tag").unwrap();
    let created = tags
        .create_values(
            &mut tx,
            &[
                vals(&[("name", Value::str("t"))]),
                vals(&[("name", Value::str("u"))]),
            ],
        )
        .await
        .unwrap();
    let (t, u) = (created.ids()[0], created.ids()[1]);
    let holders = one.records("spec.holder").unwrap();
    let a = holders
        .create_values(
            &mut tx,
            &[vals(&[
                ("name", Value::str("a")),
                ("tag_id", Value::Int(t)),
                ("other_id", Value::Int(u)),
            ])],
        )
        .await
        .unwrap();
    // company 2 points the set-null field at the same target
    a.with_env(two.clone())
        .write_values(&mut tx, &vals(&[("other_id", Value::Int(u))]))
        .await
        .unwrap();
    let a_id = a.ids()[0];
    assert_eq!(
        stored(&mut tx, "spec_holder", "tag_id", a_id).await,
        Some(json!({"1": t}))
    );
    assert_eq!(
        stored(&mut tx, "spec_holder", "other_id", a_id).await,
        Some(json!({"1": u, "2": u}))
    );
    // A record whose `tag_id` holds an id no tag has, written straight to its table.
    assert!(
        tx.query("SELECT 1 FROM spec_tag WHERE id = $1", &[&DANGLING])
            .await
            .unwrap()
            .is_empty()
    );
    let dangling: i64 = tx
        .query(
            "INSERT INTO spec_holder (name, tag_id) VALUES ('dangling', $1) RETURNING id::int8",
            &[&json!({"1": DANGLING})],
        )
        .await
        .unwrap()[0]
        .get(0);

    // Deleting t is refused: the restrict field of `a` uses it.
    let refused = tx
        .savepoint(async |tx| tags.browse([t]).unwrap().unlink(tx).await)
        .await;
    error(
        refused.unwrap_err(),
        ErrorKind::UserError,
        "You cannot delete a record used by a company-dependent field",
    );
    assert_eq!(
        tx.query("SELECT 1 FROM spec_tag WHERE id = $1", &[&t])
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        stored(&mut tx, "spec_holder", "tag_id", a_id).await,
        Some(json!({"1": t}))
    );
    assert_eq!(a.value(&mut tx, "tag_id").await.unwrap(), Value::Int(t));

    // Deleting the target of the set-null field sets its entries to null.
    tags.browse([u]).unwrap().unlink(&mut tx).await.unwrap();
    assert!(
        tx.query("SELECT 1 FROM spec_tag WHERE id = $1", &[&u])
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        stored(&mut tx, "spec_holder", "other_id", a_id).await,
        Some(json!({"1": null, "2": null}))
    );
    tx.invalidate_all().await.unwrap();
    for env in [&one, &two] {
        assert_eq!(
            a.with_env(env.clone())
                .value(&mut tx, "other_id")
                .await
                .unwrap(),
            Value::False
        );
    }
    assert_eq!(
        stored(&mut tx, "spec_holder", "tag_id", a_id).await,
        Some(json!({"1": t}))
    );

    // The dangling record reads an empty value and is found by the false search.
    let dangling_record = holders.browse([dangling]).unwrap();
    assert_eq!(
        dangling_record.value(&mut tx, "tag_id").await.unwrap(),
        Value::False
    );
    assert_eq!(
        dangling_record.cell(&mut tx, "tag_id").await.unwrap(),
        Cell::Null
    );
    let both = holders.browse([a_id, dangling]).unwrap();
    let found = check(&mut tx, &both, cond("tag_id", Operator::Eq, false)).await;
    assert_eq!(found, vec![dangling]);
    let found = holders
        .search(&mut tx, &cond("tag_id", Operator::Eq, false))
        .await
        .unwrap();
    assert_eq!(found.ids().to_vec(), vec![dangling]);
    let found = check(&mut tx, &both, cond("tag_id", Operator::Ne, false)).await;
    assert_eq!(found, vec![a_id]);
    // the stored entry is still there: the read, not the row, treats it as empty
    assert_eq!(
        stored(&mut tx, "spec_holder", "tag_id", dangling).await,
        Some(json!({"1": DANGLING}))
    );
    drop(tx);
    h.cleanup().await.unwrap();
}
