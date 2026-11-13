// SPDX-License-Identifier: MIT OR Apache-2.0
//! Direct compiler contracts for prepared company values.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]
use erp_domain::{Domain, Operator};
use erp_fields::{Cell, Decimal, ErrorKind};
use erp_registry::{AddonDescriptor, FieldDecl, ModelDecl, Registry, RegistryBuilder};
use erp_sql::company::CompanyValues;
use erp_sql::filter::{
    Unaccent, WhereContext, compile_where, compile_where_with_company, plan_where_with_company,
    render_where_at,
};
use erp_sql::order::{compile_order, compile_order_with_company};

static ADDON: AddonDescriptor = AddonDescriptor {
    name: "property_sql",
    depends: &[],
    exact_only: false,
};
static MODEL: ModelDecl = ModelDecl::new("property_sql", "property.sql").fields(
    const {
        &[
            FieldDecl::char("name")
                .company_dependent(true)
                .index(Some(erp_fields::IndexKind::BtreeNotNull)),
            FieldDecl::float_digits("amount", erp_fields::Digits::Unrounded)
                .company_dependent(true),
            FieldDecl::integer("plain"),
            FieldDecl::datetime("moment").company_dependent(true),
            FieldDecl::many2one("parent_id", "property.sql").company_dependent(true),
        ]
    },
);
fn registry() -> Registry {
    RegistryBuilder::new([&ADDON])
        .linked(false)
        .with_decls([&MODEL])
        .build()
        .unwrap()
}
fn values(registry: &Registry, company: i64, name: &str) -> CompanyValues {
    let model = registry.model("property.sql").unwrap();
    let mut values = CompanyValues::new(company).unwrap();
    values
        .insert(model, model.field("name").unwrap(), &Cell::Str(name.into()))
        .unwrap();
    values
        .insert(
            model,
            model.field("amount").unwrap(),
            &Cell::Decimal(Decimal::from_str_exact("9007199254740993.125").unwrap()),
        )
        .unwrap();
    values
}
#[test]
fn direct_compilers_require_preparation_only_for_property_fields() {
    let registry = registry();
    let model = registry.model("property.sql").unwrap();
    let context = WhereContext::new(&registry, model, Unaccent::Off);
    let domain =
        Domain::condition("name", Operator::In, erp_domain::Value::list(["needle"])).unwrap();
    let error = compile_where(&domain, &context).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.message(),
        "property.sql.name: company-dependent SQL requires prepared company values"
    );
    let error = compile_order(&registry, model, "name", None).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.message(),
        "property.sql.name: company-dependent SQL requires prepared company values"
    );
    assert!(compile_order(&registry, model, "plain", None).is_ok());
    let empty = CompanyValues::new(1).unwrap();
    let error = compile_where_with_company(&domain, &context, &empty).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.message(),
        "property.sql.name: company-dependent SQL requires prepared company values"
    );
}
#[test]
fn company_values_are_bound_and_shapes_do_not_capture_values() {
    let registry = registry();
    let model = registry.model("property.sql").unwrap();
    let context = WhereContext::new(&registry, model, Unaccent::Off);
    let domain =
        Domain::condition("name", Operator::In, erp_domain::Value::list(["needle"])).unwrap();
    let one = values(&registry, 1, "secret one");
    let two = values(&registry, 2, "secret two");
    let a = plan_where_with_company(&domain, &context, &one, 8).unwrap();
    let b = plan_where_with_company(&domain, &context, &two, 8).unwrap();
    assert_eq!(a.key, b.key);
    let sql = render_where_at(&a.key, &registry, 8).sql;
    assert!(sql.contains("$8::text::jsonb"));
    assert!(sql.contains("$9::text[]"));
    assert!(!sql.contains("secret"));
    assert_ne!(one.param().unwrap(), two.param().unwrap());
    let order = compile_order_with_company(&registry, model, "amount desc", None, &one, 8).unwrap();
    assert!(order.sql.contains("::numeric"));
    assert!(order.sql.contains("$8::text::jsonb"));
    assert!(!order.sql.contains("9007199"));
}
#[test]
fn prepared_values_reject_invalid_company_and_scalar_types() {
    let error = CompanyValues::new(0).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "company-dependent SQL requires a positive company id"
    );
    let registry = registry();
    let model = registry.model("property.sql").unwrap();
    let mut values = CompanyValues::new(1).unwrap();
    let error = values
        .insert(model, model.field("name").unwrap(), &Cell::Int(2))
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.message(),
        "property.sql.name: invalid prepared company fallback type"
    );
}

#[test]
fn nonnull_prefilter_is_used_only_when_fallback_cannot_match() {
    let registry = registry();
    let model = registry.model("property.sql").unwrap();
    let context = WhereContext::new(&registry, model, Unaccent::Off);
    let yes = values(&registry, 1, "needle");
    let no = values(&registry, 1, "other");
    let domain =
        Domain::condition("name", Operator::In, erp_domain::Value::list(["needle"])).unwrap();
    let matching = compile_where_with_company(&domain, &context, &yes).unwrap();
    let excluded = compile_where_with_company(&domain, &context, &no).unwrap();
    let raw = "\"property_sql\".\"name\" IS NOT NULL";
    assert!(!matching.sql.contains(raw));
    assert!(excluded.sql.contains(raw));
    let a = plan_where_with_company(&domain, &context, &yes, 1).unwrap();
    let b = plan_where_with_company(&domain, &context, &no, 1).unwrap();
    assert_ne!(a.key, b.key);
    let unknown = Domain::condition("name", Operator::Like, "_%wild").unwrap();
    assert!(
        !compile_where_with_company(&unknown, &context, &no)
            .unwrap()
            .sql
            .contains(raw)
    );
}

#[test]
fn property_existence_subqueries_are_tracked_and_unused_context_is_not_bound() {
    let registry = registry();
    let model = registry.model("property.sql").unwrap();
    let context = WhereContext::new(&registry, model, Unaccent::Off);
    let mut company = values(&registry, 1, "name");
    company
        .insert(model, model.field("parent_id").unwrap(), &Cell::Null)
        .unwrap();
    let domain =
        Domain::condition("parent_id", Operator::In, erp_domain::Value::list([42])).unwrap();
    let plan = plan_where_with_company(&domain, &context, &company, 1).unwrap();
    assert!(plan.key.selects_from(model.id));
    let plain = compile_where_with_company(&Domain::TRUE, &context, &company).unwrap();
    assert!(plain.params.is_empty());
}

#[test]
fn prepared_context_preserves_microseconds_and_rejects_oversized_values_atomically() {
    use erp_sql::param::{Param, Scalar};
    let registry = registry();
    let model = registry.model("property.sql").unwrap();
    let mut context = values(&registry, 1, "safe");
    let base = erp_fields::DateTime::parse_odoo("2026-02-03 04:05:06").unwrap();
    context
        .insert(
            model,
            model.field("moment").unwrap(),
            &Cell::DateTime(erp_fields::DateTime::from_micros(base.micros() + 123456)),
        )
        .unwrap();
    let before = context.param().unwrap();
    assert!(
        matches!(&before, Param::Scalar(Scalar::Text(text)) if text.contains("2026-02-03 04:05:06.123456"))
    );
    let error = context
        .insert(
            model,
            model.field("name").unwrap(),
            &Cell::Str("x".repeat(16 * 1024 * 1024).into()),
        )
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "prepared company context exceeds the byte limit"
    );
    assert_eq!(context.param().unwrap(), before);
}

#[test]
fn independent_user_paths_do_not_share_another_models_property_expression() {
    use erp_domain::{PrincipalParam, UserPath, Value as D};
    static LEFT: ModelDecl = ModelDecl::new("property_sql", "property.left").fields(
        const { &[FieldDecl::many2one("prop", "property.target").company_dependent(true)] },
    );
    static RIGHT: ModelDecl = ModelDecl::new("property_sql", "property.right").fields(
        const { &[FieldDecl::many2one("prop", "property.target").company_dependent(true)] },
    );
    static TARGET: ModelDecl = ModelDecl::new("property_sql", "property.target")
        .fields(const { &[FieldDecl::char("name")] });
    static USERS: ModelDecl = ModelDecl::new("property_sql", "res.users")
        .inherit(&["res.users"])
        .fields(
            const {
                &[
                    FieldDecl::many2one("left", "property.left"),
                    FieldDecl::many2one("right", "property.right"),
                ]
            },
        );
    static DOC: ModelDecl = ModelDecl::new("property_sql", "property.doc")
        .fields(const { &[FieldDecl::many2one("link", "property.target")] });
    let registry = RegistryBuilder::new([&ADDON])
        .linked(false)
        .with_decls([&LEFT, &RIGHT, &TARGET, &USERS, &DOC])
        .build()
        .unwrap();
    let left = registry.model("property.left").unwrap();
    let right = registry.model("property.right").unwrap();
    let lf = left.field("prop").unwrap();
    let rf = right.field("prop").unwrap();
    assert_eq!(lf.id, rf.id);
    let mut values = CompanyValues::new(1).unwrap();
    values.insert(left, lf, &Cell::Id(41)).unwrap();
    values.insert(right, rf, &Cell::Id(51)).unwrap();
    let cond = |path| {
        Domain::condition(
            "link",
            Operator::In,
            D::Set(
                [D::Param(PrincipalParam::UserPath(
                    UserPath::parse(path).unwrap(),
                ))]
                .into_iter()
                .collect(),
            ),
        )
        .unwrap()
    };
    let compiled = compile_where_with_company(
        &(cond("left.prop") | cond("right.prop")),
        &WhereContext::new(
            &registry,
            registry.model("property.doc").unwrap(),
            Unaccent::Off,
        ),
        &values,
    )
    .unwrap();
    assert!(compiled.sql.contains(&format!(
        "->'values'->'{}.{}'",
        left.id.index(),
        lf.id.index()
    )));
    assert!(compiled.sql.contains(&format!(
        "->'values'->'{}.{}'",
        right.id.index(),
        rf.id.index()
    )));
}
