//! The reference-query carrier (Amendment 1): the SQL shape of an `any!` condition on a
//! `many2one_reference` column, and the refusals that keep a pass which forgot the carrier from
//! letting rows through. The domain compiler is called directly, as in `filter.rs`.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

use erp_domain::{Domain, Operator, Value};
use erp_fields::ErrorKind;
use erp_registry::{AddonDescriptor, FieldDecl, ModelDecl, ModelKind, Registry, RegistryBuilder};
use erp_sql::filter::{Unaccent, WhereContext, compile_where};
use erp_sql::param::{Param, Scalar, ScalarKind};
use erp_sql::reference::ReferenceQuery;

static T: AddonDescriptor = AddonDescriptor {
    name: "t",
    depends: &["base"],
    exact_only: false,
};

/// The referencing side: `model` names the target, `res_id` points at one of its records.
static MSG: ModelDecl = ModelDecl::new("t", "t.msg").table("t_msg").fields(
    const {
        &[
            FieldDecl::char("model"),
            FieldDecl::many2one_reference("res_id", "model"),
            FieldDecl::many2one("partner_id", "t.partner"),
        ]
    },
);

static LEAD: ModelDecl = ModelDecl::new("t", "t.lead").table("t_lead").fields(
    const {
        &[
            FieldDecl::char("name"),
            FieldDecl::integer("number"),
            FieldDecl::many2one("partner_id", "t.partner"),
        ]
    },
);

static DEAL: ModelDecl = ModelDecl::new("t", "t.deal")
    .table("t_deal")
    .fields(const { &[FieldDecl::char("title")] });

static PARTNER: ModelDecl = ModelDecl::new("t", "t.partner")
    .table("t_partner")
    .fields(const { &[FieldDecl::char("name")] });

static MIXIN: ModelDecl = ModelDecl::new("t", "t.mixin")
    .kind(ModelKind::Abstract)
    .fields(const { &[FieldDecl::char("name")] });

fn registry() -> Registry {
    RegistryBuilder::new([&T])
        .with_decls([&MSG, &LEAD, &DEAL, &PARTNER, &MIXIN])
        .build()
        .expect("registry")
}

fn compile(registry: &Registry, domain: &Domain) -> erp_fields::Result<erp_sql::filter::Filter> {
    compile_where(
        domain,
        &WhereContext {
            registry,
            model: registry.model("t.msg").expect("model"),
            unaccent: Unaccent::Off,
            lang: None,
        },
    )
}

fn norm(sql: &str) -> String {
    sql.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn cond(field: &str, op: Operator, value: impl Into<Value>) -> Domain {
    Domain::condition(field, op, value).expect("condition")
}

fn text(s: &str) -> Param {
    Param::Array(ScalarKind::Text, vec![Scalar::Text(s.into())])
}

/// The branch the security expansion writes for one thread model: `model = M AND res_id any!
/// reference_query(M, sub)`, the carrier already optimized.
fn branch(registry: &Registry, target: &str, sub: Domain) -> Domain {
    let id = registry.model(target).expect("target").id;
    cond("model", Operator::In, Value::list([target]))
        & cond(
            "res_id",
            Operator::AnyBypass,
            ReferenceQuery::new(id, sub.clone())
                .optimized(sub)
                .into_value(),
        )
}

#[test]
fn a_reference_query_is_a_sub_query_on_the_target_model() {
    let registry = registry();
    let domain = branch(
        &registry,
        "t.lead",
        cond("name", Operator::In, Value::list(["x"])),
    );
    let filter = compile(&registry, &domain).unwrap();
    assert_eq!(
        norm(&filter.sql),
        "(\"t_msg\".\"model\" = ANY($1::text[]) AND \"t_msg\".\"res_id\" IN (SELECT \
         \"t_lead\".\"id\" FROM \"t_lead\" WHERE \"t_lead\".\"name\" = ANY($2::text[])))"
    );
    assert_eq!(filter.params, [text("t.lead"), text("x")]);
    // the column and what the sub-query reads are reported, so pending writes are flushed first
    let names: Vec<String> = filter
        .fields
        .iter()
        .map(|(m, f)| {
            let spec = registry.model_by_id(*m);
            format!("{}.{}", spec.name, spec.field_by_id(*f).name)
        })
        .collect();
    assert_eq!(names, ["t.msg.model", "t.msg.res_id", "t.lead.name"]);
}

/// A target whose predicate is `TRUE` still renders the sub-query: the column has no foreign key,
/// so a record that no longer exists must not match (an orphan message).
#[test]
fn a_true_target_predicate_still_selects_from_the_target_table() {
    let registry = registry();
    let domain = branch(&registry, "t.lead", Domain::TRUE);
    let filter = compile(&registry, &domain).unwrap();
    assert_eq!(
        norm(&filter.sql),
        "(\"t_msg\".\"model\" = ANY($1::text[]) AND \"t_msg\".\"res_id\" IN (SELECT \
         \"t_lead\".\"id\" FROM \"t_lead\"))"
    );
}

#[test]
fn branches_for_several_models_are_numbered_in_order() {
    let registry = registry();
    let domain = branch(
        &registry,
        "t.lead",
        cond("number", Operator::Gt, Value::Int(3)),
    ) | branch(
        &registry,
        "t.deal",
        cond("title", Operator::In, Value::list(["y"])),
    );
    let filter = compile(&registry, &domain).unwrap();
    assert_eq!(
        norm(&filter.sql),
        "((\"t_msg\".\"model\" = ANY($1::text[]) AND \"t_msg\".\"res_id\" IN (SELECT \
         \"t_lead\".\"id\" FROM \"t_lead\" WHERE \"t_lead\".\"number\" > $2::int8)) OR \
         (\"t_msg\".\"model\" = ANY($3::text[]) AND \"t_msg\".\"res_id\" IN (SELECT \
         \"t_deal\".\"id\" FROM \"t_deal\" WHERE \"t_deal\".\"title\" = ANY($4::text[]))))"
    );
}

/// The refusals are programming errors that name the amendment, with the full message.
#[test]
fn a_carrier_the_planner_cannot_trust_is_refused() {
    let registry = registry();
    let lead = registry.model("t.lead").unwrap().id;
    let mixin = registry.model("t.mixin").unwrap().id;
    let sub = cond("name", Operator::In, Value::list(["x"]));
    let ready = || ReferenceQuery::new(lead, sub.clone()).optimized(sub.clone());

    let refuse = |domain: Domain| {
        let err = compile(&registry, &domain).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Programming, "{err}");
        err.message().to_owned()
    };

    let (lead_n, mixin_n) = (lead.index(), mixin.index());
    let tail = "";

    // negated, in both spellings the optimizer can leave
    let negated = cond("res_id", Operator::NotAnyBypass, ready().into_value());
    assert_eq!(
        refuse(negated),
        format!(
            "reference query in condition ('res_id', 'not any!', reference_query[model \
             #{lead_n}, optimized: [('name', 'in', ['x'])]]) on t.msg.res_id: only the positive \
             'any!' form is compiled, not 'not any!': a negated reference query could let rows \
             through{tail}"
        )
    );
    let checked = cond("res_id", Operator::Any, ready().into_value());
    assert_eq!(
        refuse(checked),
        format!(
            "reference query in condition ('res_id', 'any', reference_query[model #{lead_n}, \
             optimized: [('name', 'in', ['x'])]]) on t.msg.res_id: only the positive 'any!' form \
             is compiled, not 'any': a negated reference query could let rows through{tail}"
        )
    );

    // not optimized: a pass over the predicate did not descend
    let raw = cond(
        "res_id",
        Operator::AnyBypass,
        ReferenceQuery::new(lead, sub.clone()).into_value(),
    );
    assert_eq!(
        refuse(raw),
        format!(
            "reference query in condition ('res_id', 'any!', reference_query[model #{lead_n}: \
             [('name', 'in', ['x'])]]) on t.msg.res_id: its sub-domain was not optimized: a pass \
             over the access predicate did not descend into the reference query{tail}"
        )
    );

    // another kind of field
    let wrong_kind = cond("partner_id", Operator::AnyBypass, ready().into_value());
    assert_eq!(
        refuse(wrong_kind),
        format!(
            "reference query in condition ('partner_id', 'any!', reference_query[model \
             #{lead_n}, optimized: [('name', 'in', ['x'])]]) on t.msg.partner_id: a reference \
             query applies to a many2one_reference column, not to a many2one field{tail}"
        )
    );

    // an abstract target has no table
    let abstract_target = cond(
        "res_id",
        Operator::AnyBypass,
        ReferenceQuery::new(mixin, Domain::TRUE)
            .optimized(Domain::TRUE)
            .into_value(),
    );
    assert_eq!(
        refuse(abstract_target),
        format!(
            "reference query in condition ('res_id', 'any!', reference_query[model #{mixin_n}, \
             optimized: [(1, '=', 1)]]) on t.msg.res_id: its target model t.mixin has no table \
             (abstract){tail}"
        )
    );
}
