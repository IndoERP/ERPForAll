//! SQL lowering of principal parameters (design note §3.1): the planned text
//! is principal-free (one text serves every principal with the same rules), parameters are
//! statement parameters bound by the ORM, user paths are sub-selects bound by the subject, "now"
//! is the transaction's start time.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

use std::sync::Arc;

use erp_domain::{Domain, NowKind, Operation, Operator, PrincipalParam, UserPath, Value};
use erp_fields::ErrorKind;
use erp_registry::{AddonDescriptor, FieldDecl, ModelDecl, Registry, RegistryBuilder};
use erp_sql::filter::{Unaccent, WhereContext, compile_where, plan_where};
use erp_sql::param::{Param, Scalar, ScalarKind};

static T: AddonDescriptor = AddonDescriptor {
    name: "t",
    depends: &["base"],
    exact_only: false,
};

static DOC: ModelDecl = ModelDecl::new("t", "t.doc").table("t_doc").fields(
    const {
        &[
            FieldDecl::many2one("user_id", "res.users"),
            FieldDecl::many2one("company_id", "t.company"),
            FieldDecl::many2one("owner_id", "t.partner").required(true),
            FieldDecl::many2one("partner_id", "t.partner"),
            FieldDecl::char("name"),
            FieldDecl::date("deadline"),
            FieldDecl::datetime("moment"),
            FieldDecl::integer("number"),
        ]
    },
);

static USERS: ModelDecl = ModelDecl::new("t", "res.users")
    .inherit(&["res.users"])
    .fields(
        const {
            &[
                FieldDecl::many2one("partner_id", "t.partner").required(true),
                FieldDecl::many2one("home_company_id", "t.company"),
                FieldDecl::many2one("cd_company_id", "t.company").company_dependent(true),
                FieldDecl::many2many("group_ids", "t.group").relation(
                    "t_group_user_rel",
                    "uid",
                    "gid",
                ),
            ]
        },
    );

static PARTNER: ModelDecl = ModelDecl::new("t", "t.partner").table("t_partner").fields(
    const {
        &[
            FieldDecl::char("name"),
            FieldDecl::many2one("commercial_partner_id", "t.partner"),
            FieldDecl::one2many("child_ids", "t.partner", "parent_id"),
            FieldDecl::many2one("parent_id", "t.partner"),
        ]
    },
);

static COMPANY: ModelDecl = ModelDecl::new("t", "t.company")
    .table("t_company")
    .fields(const { &[FieldDecl::char("name")] });

static GROUP: ModelDecl = ModelDecl::new("t", "t.group")
    .table("t_group")
    .fields(const { &[FieldDecl::char("name")] });

fn registry() -> Registry {
    RegistryBuilder::new([&T])
        .with_decls([&DOC, &USERS, &PARTNER, &COMPANY, &GROUP])
        .build()
        .expect("registry")
}

fn sql(registry: &Registry, domain: &Domain) -> (String, Vec<Param>) {
    let model = registry.model("t.doc").expect("model");
    let filter = compile_where(
        domain,
        &WhereContext {
            registry,
            model,
            unaccent: Unaccent::Off,
            lang: None,
        },
    )
    .expect("compile");
    (filter.sql, filter.params)
}

fn sql_err(registry: &Registry, domain: &Domain) -> erp_fields::Error {
    let model = registry.model("t.doc").expect("model");
    compile_where(
        domain,
        &WhereContext {
            registry,
            model,
            unaccent: Unaccent::Off,
            lang: None,
        },
    )
    .expect_err("refused")
}

fn param(p: PrincipalParam) -> Value {
    Value::Param(p)
}

fn set(values: impl IntoIterator<Item = Value>) -> Value {
    Value::Set(values.into_iter().collect())
}

fn cond(field: &str, op: Operator, value: Value) -> Domain {
    Domain::condition(field, op, value).expect("condition")
}

fn slot(p: PrincipalParam, array: bool) -> Param {
    Param::Principal {
        param: p,
        kind: ScalarKind::Int8,
        array,
    }
}

fn path(dotted: &str) -> PrincipalParam {
    PrincipalParam::UserPath(UserPath::parse(dotted).unwrap())
}

/// The users-path sub-select of `partner_id` (a many2one on `res.users`) with placeholder `n`.
fn users_partner(n: usize) -> String {
    format!(
        "SELECT \"__up0\".\"partner_id\" FROM \"res_users\" AS \"__up0\" WHERE \"__up0\".\"id\" = \
         ${n}::int8"
    )
}

#[test]
fn scalar_and_collection_parameters_bind_as_statement_parameters() {
    let r = registry();
    let (s, p) = sql(
        &r,
        &cond("user_id", Operator::In, set([param(PrincipalParam::Uid)])),
    );
    assert_eq!(s, "\"t_doc\".\"user_id\" = $1::int8");
    assert_eq!(p, [slot(PrincipalParam::Uid, false)]);

    let (s, p) = sql(
        &r,
        &cond(
            "company_id",
            Operator::In,
            set([Value::FALSE, param(PrincipalParam::CompanyIds)]),
        ),
    );
    assert_eq!(
        s,
        "(\"t_doc\".\"company_id\" IS NULL OR \"t_doc\".\"company_id\" = ANY($1::int8[]))"
    );
    assert_eq!(p, [slot(PrincipalParam::CompanyIds, true)]);

    let (s, p) = sql(
        &r,
        &cond(
            "company_id",
            Operator::NotIn,
            set([param(PrincipalParam::CompanyIds)]),
        ),
    );
    assert_eq!(
        s,
        "(\"t_doc\".\"company_id\" <> ALL($1::int8[]) OR \"t_doc\".\"company_id\" IS NULL)"
    );
    assert_eq!(p, [slot(PrincipalParam::CompanyIds, true)]);

    // literals first, then the parameters, in placeholder order; NOT NULL column
    let (s, p) = sql(
        &r,
        &cond(
            "owner_id",
            Operator::NotIn,
            set([
                Value::Int(7),
                param(PrincipalParam::CompanyId),
                Value::FALSE,
            ]),
        ),
    );
    assert_eq!(
        s,
        "(\"t_doc\".\"owner_id\" <> ALL($1::int8[]) AND \"t_doc\".\"owner_id\" <> $2::int8)"
    );
    assert_eq!(
        p,
        [
            Param::Array(ScalarKind::Int8, vec![Scalar::Int8(7)]),
            slot(PrincipalParam::CompanyId, false)
        ]
    );

    let grant = PrincipalParam::GrantIds {
        model: Arc::from("t.doc"),
        op: Operation::Read,
    };
    let (s, p) = sql(&r, &cond("id", Operator::In, set([param(grant.clone())])));
    assert_eq!(s, "\"t_doc\".\"id\" = ANY($1::int8[])");
    assert_eq!(p, [slot(grant, true)]);
}

#[test]
fn user_paths_are_sub_selects_bound_by_the_subject() {
    let r = registry();
    // a path of required many2ones never yields NULL: a plain comparison
    let (s, p) = sql(
        &r,
        &cond("partner_id", Operator::In, set([param(path("partner_id"))])),
    );
    let sub = users_partner(1);
    assert_eq!(s, format!("\"t_doc\".\"partner_id\" = ({sub})"));
    assert_eq!(p, [slot(PrincipalParam::Uid, false)]);
    let (s, _) = sql(
        &r,
        &cond(
            "partner_id",
            Operator::In,
            set([param(path("partner_id.id"))]),
        ),
    );
    assert!(s.starts_with("\"t_doc\".\"partner_id\" = (SELECT"), "{s}");
    assert!(!s.contains("IS NULL"), "{s}");

    let (s, p) = sql(
        &r,
        &cond(
            "company_id",
            Operator::In,
            set([param(path("home_company_id"))]),
        ),
    );
    let sub = "SELECT \"__up0\".\"home_company_id\" FROM \"res_users\" AS \"__up0\" WHERE \
               \"__up0\".\"id\" = $1::int8";
    assert_eq!(
        s,
        format!(
            "(\"t_doc\".\"company_id\" = ({sub}) OR (\"t_doc\".\"company_id\" IS NULL AND \
             ({sub}) IS NULL))"
        )
    );
    assert_eq!(p, [slot(PrincipalParam::Uid, false)]);
    // a nullable many2one reached through a required one
    let (s, _) = sql(
        &r,
        &cond(
            "partner_id",
            Operator::In,
            set([param(path("partner_id.commercial_partner_id"))]),
        ),
    );
    assert!(s.contains("IS NULL AND"), "{s}");

    // a trailing `.id` and a deeper path join through the many2ones
    let (s, _) = sql(
        &r,
        &cond(
            "partner_id",
            Operator::NotIn,
            set([param(path("partner_id.commercial_partner_id.id"))]),
        ),
    );
    assert_eq!(
        s,
        "\"t_doc\".\"partner_id\" IS DISTINCT FROM (SELECT \"__up2\".\"id\" FROM \"res_users\" AS \
         \"__up0\" JOIN \"t_partner\" AS \"__up1\" ON (\"__up0\".\"partner_id\" = \"__up1\".\"id\") \
         JOIN \"t_partner\" AS \"__up2\" ON (\"__up1\".\"commercial_partner_id\" = \"__up2\".\"id\") \
         WHERE \"__up0\".\"id\" = $1::int8)"
    );

    // an x2many on the path makes a set
    let (s, p) = sql(
        &r,
        &cond(
            "partner_id",
            Operator::In,
            set([Value::Int(3), param(path("partner_id.child_ids"))]),
        ),
    );
    assert_eq!(
        s,
        "(\"t_doc\".\"partner_id\" = ANY($1::int8[]) OR \"t_doc\".\"partner_id\" IN (SELECT \
         \"__up2\".\"id\" FROM \"res_users\" AS \"__up0\" JOIN \"t_partner\" AS \"__up1\" ON \
         (\"__up0\".\"partner_id\" = \"__up1\".\"id\") JOIN \"t_partner\" AS \"__up2\" ON \
         (\"__up2\".\"parent_id\" = \"__up1\".\"id\") WHERE \"__up0\".\"id\" = $2::int8))"
    );
    assert_eq!(
        p,
        [
            Param::Array(ScalarKind::Int8, vec![Scalar::Int8(3)]),
            slot(PrincipalParam::Uid, false)
        ]
    );
    let (s, _) = sql(
        &r,
        &cond("id", Operator::NotIn, set([param(path("group_ids"))])),
    );
    assert_eq!(
        s,
        "\"t_doc\".\"id\" NOT IN (SELECT \"__up1\".\"id\" FROM \"res_users\" AS \"__up0\" JOIN \
         \"t_group_user_rel\" AS \"__upr0\" ON (\"__upr0\".\"uid\" = \"__up0\".\"id\") JOIN \
         \"t_group\" AS \"__up1\" ON (\"__up1\".\"id\" = \"__upr0\".\"gid\") WHERE \
         \"__up0\".\"id\" = $1::int8)"
    );
}

#[test]
fn a_terminal_ids_is_a_set_of_the_records_reached() {
    let r = registry();
    let (s, p) = sql(
        &r,
        &cond(
            "partner_id",
            Operator::In,
            set([param(path("partner_id.commercial_partner_id.ids"))]),
        ),
    );
    assert_eq!(
        s,
        "\"t_doc\".\"partner_id\" IN (SELECT \"__up2\".\"id\" FROM \"res_users\" AS \"__up0\" \
         JOIN \"t_partner\" AS \"__up1\" ON (\"__up0\".\"partner_id\" = \"__up1\".\"id\") JOIN \
         \"t_partner\" AS \"__up2\" ON (\"__up1\".\"commercial_partner_id\" = \"__up2\".\"id\") \
         WHERE \"__up0\".\"id\" = $1::int8)"
    );
    assert_eq!(p, [slot(PrincipalParam::Uid, false)]);
}

/// User paths read the current company's scalar relation, with one shared context binding.
#[test]
fn user_paths_require_company_preparation_and_use_effective_ids() {
    let r = registry();
    let users = r.model("res.users").unwrap();
    let mut values = erp_sql::company::CompanyValues::new(1).unwrap();
    values
        .insert(
            users,
            users.field("cd_company_id").unwrap(),
            &erp_fields::Cell::Id(77),
        )
        .unwrap();
    for p in ["cd_company_id", "cd_company_id.id"] {
        let domain = cond("company_id", Operator::In, set([param(path(p))]));
        let err = sql_err(&r, &domain);
        assert_eq!(err.kind(), ErrorKind::Programming);
        assert_eq!(
            err.message(),
            "res.users.cd_company_id: company-dependent SQL requires prepared company values"
        );
        let compiled = erp_sql::filter::compile_where_with_company(
            &domain,
            &WhereContext::new(&r, r.model("t.doc").unwrap(), Unaccent::Off),
            &values,
        )
        .unwrap();
        assert!(compiled.sql.contains("$1::text::jsonb"));
        assert!(compiled.sql.contains("$2::int8"));
        assert_eq!(compiled.params.len(), 2);
        assert!(!compiled.sql.contains("77"));
    }
}

#[test]
fn user_paths_record_the_fields_they_read() {
    let r = registry();
    let model = r.model("t.doc").unwrap();
    let plan = plan_where(
        &cond(
            "partner_id",
            Operator::In,
            set([param(path("partner_id.commercial_partner_id"))]),
        ),
        &WhereContext {
            registry: &r,
            model,
            unaccent: Unaccent::Off,
            lang: None,
        },
    )
    .unwrap();
    let users = r.model("res.users").unwrap();
    let partner = r.model("t.partner").unwrap();
    let read = |m: &erp_registry::ModelSpec, f: &str| (m.id, m.field(f).unwrap().id);
    for expected in [
        read(model, "partner_id"),
        read(users, "partner_id"),
        read(partner, "commercial_partner_id"),
    ] {
        assert!(plan.fields.contains(&expected), "{expected:?} is flushed");
    }
}

#[test]
fn now_is_the_transaction_start() {
    let r = registry();
    let (s, p) = sql(
        &r,
        &cond(
            "deadline",
            Operator::Ge,
            param(PrincipalParam::Now(NowKind::Date)),
        ),
    );
    assert_eq!(
        s,
        "\"t_doc\".\"deadline\" >= (now() AT TIME ZONE 'UTC')::date"
    );
    assert!(p.is_empty());
    let (s, _) = sql(
        &r,
        &cond(
            "moment",
            Operator::In,
            set([param(PrincipalParam::Now(NowKind::Timestamp))]),
        ),
    );
    assert_eq!(s, "\"t_doc\".\"moment\" = (now() AT TIME ZONE 'UTC')");
}

#[test]
fn the_text_is_the_same_for_every_principal() {
    // two plans of one rule have equal shape keys: the memoized text serves every principal
    let r = registry();
    let model = r.model("t.doc").unwrap();
    let ctx = WhereContext {
        registry: &r,
        model,
        unaccent: Unaccent::Off,
        lang: None,
    };
    let rule = Domain::or([
        cond("user_id", Operator::In, set([param(PrincipalParam::Uid)])),
        cond(
            "company_id",
            Operator::In,
            set([param(PrincipalParam::CompanyIds)]),
        ),
    ]);
    let a = plan_where(&rule, &ctx).unwrap();
    let b = plan_where(&rule, &ctx).unwrap();
    assert_eq!(a.key, b.key);
    assert_eq!(a.params, b.params);
    // and a literal in the same place has another shape (another token)
    let literal = Domain::or([
        cond("user_id", Operator::In, set([Value::Int(5)])),
        cond(
            "company_id",
            Operator::In,
            set([param(PrincipalParam::CompanyIds)]),
        ),
    ]);
    assert_ne!(plan_where(&literal, &ctx).unwrap().key, a.key);
}

#[test]
fn parameters_are_refused_where_they_cannot_compare() {
    let r = registry();
    let cases = [
        (
            cond("name", Operator::In, set([param(PrincipalParam::Uid)])),
            "The principal parameter user.id in ('name', 'in', OrderedSet([user.id])) compares \
             with t.doc.name, a char field: it is a record id",
        ),
        (
            cond("number", Operator::Lt, param(PrincipalParam::Uid)),
            "Invalid principal parameter in the inequality ('number', '<', user.id): only \
             context_today() and datetime.datetime.now() compare with '<', '>', '<=', '>='",
        ),
        (
            cond(
                "partner_id",
                Operator::In,
                set([param(path("partner_id.name"))]),
            ),
            "The user path user.partner_id.name in ('partner_id', 'in', \
             OrderedSet([user.partner_id.name])) cannot be compiled: t.partner.name is not a \
             stored relational field (a user path compares record ids)",
        ),
        (
            cond("partner_id", Operator::In, set([param(path("nope_id"))])),
            "The user path user.nope_id in ('partner_id', 'in', OrderedSet([user.nope_id])) \
             cannot be compiled: res.users.nope_id does not exist",
        ),
        (
            cond(
                "deadline",
                Operator::Gt,
                param(PrincipalParam::Now(NowKind::Timestamp)),
            ),
            "The principal parameter datetime.datetime.now() in ('deadline', '>', \
             datetime.datetime.now()) compares with t.doc.deadline, a date field",
        ),
    ];
    for (domain, message) in cases {
        let error = sql_err(&r, &domain);
        assert_eq!(error.kind(), ErrorKind::Programming, "{message}");
        assert_eq!(error.message(), message);
    }
}

#[test]
fn optimization_keeps_parameters_as_set_members() {
    // `=` becomes `in` of a one-member set; an OR of `in` sets on one field merges into a union
    let r = registry();
    let schema = erp_domain_schema(&r);
    let ctx = erp_domain::ModelContext::new(&schema, "t.doc");
    let rule = Domain::or([
        cond("company_id", Operator::Eq, Value::FALSE),
        cond(
            "company_id",
            Operator::In,
            param(PrincipalParam::CompanyIds),
        ),
    ]);
    let optimized = rule.optimize(&ctx).unwrap();
    assert_eq!(
        format!("{optimized:?}"),
        "[('company_id', 'in', [False, company_ids])]"
    );
    // an AND of `in` sets would intersect values only SQL knows: it stays unmerged
    let both = Domain::and([
        cond("company_id", Operator::In, set([Value::Int(1)])),
        cond(
            "company_id",
            Operator::In,
            param(PrincipalParam::CompanyIds),
        ),
    ]);
    assert_eq!(
        format!("{:?}", both.optimize(&ctx).unwrap()),
        "['&', ('company_id', 'in', [1]), ('company_id', 'in', [company_ids])]"
    );
}

/// A schema of the registry for BASIC optimization (the ORM's `RegistrySchema` is crate-private).
fn erp_domain_schema(registry: &Registry) -> TestSchema<'_> {
    TestSchema(registry)
}

struct TestSchema<'a>(&'a Registry);

impl erp_domain::Schema for TestSchema<'_> {
    fn field(&self, model: &str, field: &str) -> Option<erp_domain::FieldInfo<'_>> {
        let spec = self.0.model(model)?.field(field)?;
        let (ty, comodel) = match &spec.ty {
            erp_fields::FieldType::Many2one { comodel, .. } => {
                (erp_domain::FieldType::Many2one, Some(comodel.as_str()))
            }
            erp_fields::FieldType::Id => (erp_domain::FieldType::Id, None),
            erp_fields::FieldType::Integer => (erp_domain::FieldType::Integer, None),
            erp_fields::FieldType::Date => (erp_domain::FieldType::Date, None),
            erp_fields::FieldType::Datetime => (erp_domain::FieldType::Datetime, None),
            _ => (erp_domain::FieldType::Char, None),
        };
        let mut info = erp_domain::FieldInfo::new(spec.name, ty);
        if let Some(comodel) = comodel {
            info = info.with_comodel(comodel);
        }
        Some(info)
    }

    fn has_model(&self, model: &str) -> bool {
        self.0.contains(model)
    }

    fn version(&self) -> u64 {
        0
    }
}
