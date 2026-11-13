//! Company scope, match and the default company check at registry build (2, 7.6;
//! design). Pure: no database.
#![allow(clippy::unwrap_used, clippy::panic, reason = "tests")]

use erp_fields::ErrorKind;
use erp_registry::{
    AddonDescriptor, CheckCompany, CompanyAccessDecl, CompanyDelegation, CompanyMatch,
    CompanyScope, FieldDecl, ManyMatch, ModelDecl, ModelDef, ModelKind, Registry, RegistryBuilder,
    SingleMatch,
};

static CO: AddonDescriptor = AddonDescriptor {
    name: "co",
    depends: &["base"],
    exact_only: false,
};
static CO_EXT: AddonDescriptor = AddonDescriptor {
    name: "co_ext",
    depends: &["co"],
    exact_only: false,
};

fn leak<T>(items: Vec<T>) -> &'static [T] {
    Vec::leak(items)
}

fn build(decls: Vec<ModelDecl>) -> erp_fields::Result<Registry> {
    let decls: Vec<&'static ModelDecl> = decls
        .into_iter()
        .map(|d| &*Box::leak(Box::new(d)))
        .collect();
    RegistryBuilder::new([&CO, &CO_EXT])
        .with_decls(decls)
        .build()
}

/// `(kind, message)` of the registry error.
fn refusal(decls: Vec<ModelDecl>) -> (ErrorKind, String) {
    let err = build(decls).unwrap_err();
    (err.kind(), err.message().to_owned())
}

fn m(name: &'static str, fields: Vec<FieldDecl>) -> ModelDecl {
    ModelDecl::new("co", name).fields(leak(fields))
}

/// `res.company` without a parent path.
fn company() -> ModelDecl {
    m(
        "res.company",
        vec![
            FieldDecl::char("name"),
            FieldDecl::many2one("parent_id", "res.company"),
        ],
    )
}

/// `res.company` with a parent path.
fn company_tree() -> ModelDecl {
    m(
        "res.company",
        vec![
            FieldDecl::char("name"),
            FieldDecl::many2one("parent_id", "res.company"),
            FieldDecl::char("parent_path").index(Some(erp_fields::IndexKind::Btree)),
        ],
    )
    .parent_store(true)
}

/// A company-scoped model (`company_id`).
fn doc(name: &'static str, mut fields: Vec<FieldDecl>) -> ModelDecl {
    fields.insert(0, FieldDecl::many2one("company_id", "res.company"));
    m(name, fields).company_access(CompanyAccessDecl::unrestricted(
        "company consistency fixture; access is not under test",
    ))
}

fn field_state(r: &Registry, model: &str, field: &str) -> CheckCompany {
    r.model(model).unwrap().field(field).unwrap().check_company
}

/// Scope of every model by shape.
#[test]
fn scope_is_found_by_field_shape() {
    let r = build(vec![
        company(),
        doc("co.single", vec![]),
        m(
            "co.many",
            vec![FieldDecl::many2many("company_ids", "res.company")],
        )
        .company_access(CompanyAccessDecl::unrestricted(
            "company consistency fixture; access is not under test",
        )),
        m("co.plain", vec![FieldDecl::char("name")]),
        // company_id of another shape: not scoped
        m(
            "co.dependent",
            vec![FieldDecl::many2one("company_id", "res.company").company_dependent(true)],
        ),
        m(
            "co.computed",
            vec![
                FieldDecl::many2one("company_id", "res.company")
                    .store(false)
                    .compute("compute_company"),
            ],
        ),
        m(
            "co.other",
            vec![FieldDecl::many2one("company_id", "co.plain")],
        ),
        // company_id wins over company_ids
        doc(
            "co.both",
            vec![FieldDecl::many2many("company_ids", "res.company")],
        ),
        doc("co.mixin", vec![]).kind(ModelKind::Abstract),
    ])
    .unwrap();
    let scope = |n: &str| r.model(n).unwrap().company_scope;
    assert_eq!(scope("res.company"), Some(CompanyScope::Itself));
    let single = r.model("co.single").unwrap();
    assert_eq!(
        single.company_scope,
        Some(CompanyScope::Single {
            field: single.field_id("company_id").unwrap(),
            matching: SingleMatch::Exact
        })
    );
    let many = r.model("co.many").unwrap();
    assert_eq!(
        many.company_scope,
        Some(CompanyScope::Many {
            field: many.field_id("company_ids").unwrap(),
            matching: ManyMatch::AnyOf
        })
    );
    assert_eq!(scope("co.plain"), None);
    assert_eq!(scope("co.dependent"), None);
    assert_eq!(scope("co.computed"), None);
    assert_eq!(scope("co.other"), None);
    assert!(matches!(
        scope("co.both"),
        Some(CompanyScope::Single { .. })
    ));
    assert_eq!(scope("co.mixin"), None, "abstract models are never scoped");
}

/// The core `res.users` extended with `company_id` and `company_ids`, as the base addon has them.
fn users_with_companies() -> ModelDecl {
    ModelDecl::new("co", "res.users")
        .inherit(&["res.users"])
        .company_access(CompanyAccessDecl::unrestricted(
            "company consistency fixture; access is not under test",
        ))
        .fields(leak(vec![
            FieldDecl::many2one("company_id", "res.company"),
            FieldDecl::many2many("company_ids", "res.company"),
        ]))
}

#[test]
fn res_users_is_scoped_by_company_ids_not_company_id() {
    let r = build(vec![
        company(),
        users_with_companies(),
        doc("co.task", vec![FieldDecl::many2one("user_id", "res.users")]),
    ])
    .unwrap();
    let users = r.model("res.users").unwrap();
    assert_eq!(
        users.company_scope,
        Some(CompanyScope::Many {
            field: users.field_id("company_ids").unwrap(),
            matching: ManyMatch::AnyOf
        })
    );
    assert_eq!(field_state(&r, "co.task", "user_id"), CheckCompany::Default);
    // without a usable `company_ids` the exception does not apply: `company_id` scopes it
    let r = build(vec![
        company(),
        ModelDecl::new("co", "res.users")
            .inherit(&["res.users"])
            .company_access(CompanyAccessDecl::unrestricted(
                "company consistency fixture; access is not under test",
            ))
            .fields(leak(vec![FieldDecl::many2one("company_id", "res.company")])),
    ])
    .unwrap();
    assert!(matches!(
        r.model("res.users").unwrap().company_scope,
        Some(CompanyScope::Single { .. })
    ));
    // only a many form may be declared on it
    assert_eq!(
        refusal(vec![
            company(),
            users_with_companies().company_match(CompanyMatch::Exact),
        ]),
        (
            ErrorKind::Programming,
            "res.users: company_match = \"exact\" needs a company_id field; res.users links \
             companies through company_ids (use \"any_of\" or \"parent_of_any\") (declared in \
             addon 'co')"
                .to_owned()
        )
    );
}

/// The field default: what is checked and what is not (rule 2).
#[test]
fn defaults_check_links_to_company_scoped_models() {
    let r = build(vec![
        company(),
        doc("co.target", vec![]),
        doc("co.base", vec![FieldDecl::char("name")]),
        m("co.plain", vec![]),
        doc(
            "co.line",
            vec![
                FieldDecl::many2one("target_id", "co.target"),
                FieldDecl::many2many("target_ids", "co.target"),
                FieldDecl::many2one("plain_id", "co.plain"),
                FieldDecl::many2one("partner_id", "co.target").check_company(false),
                FieldDecl::many2one("forced_id", "co.target").check_company(true),
                FieldDecl::many2one("dep_forced_id", "co.target")
                    .company_dependent(true)
                    .check_company(true),
                FieldDecl::many2one("calc_id", "co.target")
                    .store(false)
                    .compute("compute_calc"),
                FieldDecl::many2one("rel_id", "co.target").related("target_id"),
                FieldDecl::one2many("kids", "co.line", "parent_line_id"),
                FieldDecl::many2one("parent_line_id", "co.line"),
                FieldDecl::many2one("dep_id", "co.target").company_dependent(true),
                FieldDecl::many2one("base_id", "co.base")
                    .delegate(true)
                    .required(true)
                    .ondelete(erp_fields::OnDelete::Cascade),
                FieldDecl::integer("qty"),
            ],
        )
        .company_access(CompanyAccessDecl::unrestricted(
            "company consistency fixture; access is not under test",
        ))
        .inherits(&[("co.base", "base_id")]),
    ])
    .unwrap();
    let st = |f: &str| field_state(&r, "co.line", f);
    assert_eq!(st("target_id"), CheckCompany::Default);
    assert_eq!(st("target_ids"), CheckCompany::Default);
    assert_eq!(st("parent_line_id"), CheckCompany::Default, "self link");
    assert_eq!(
        st("dep_id"),
        CheckCompany::Off,
        "company-dependent: declared only until a later release"
    );
    assert_eq!(st("dep_forced_id"), CheckCompany::Declared);
    assert_eq!(st("plain_id"), CheckCompany::Off, "target has no company");
    assert_eq!(st("partner_id"), CheckCompany::Off, "declared opt-out");
    assert_eq!(st("forced_id"), CheckCompany::Declared, "declared");
    assert_eq!(st("calc_id"), CheckCompany::Off, "computed");
    assert_eq!(st("rel_id"), CheckCompany::Off, "related");
    assert_eq!(st("kids"), CheckCompany::Off, "one2many");
    assert_eq!(st("base_id"), CheckCompany::Off, "delegate");
    assert_eq!(st("qty"), CheckCompany::Off, "not relational");
    assert_eq!(st("company_id"), CheckCompany::Off, "link to res.company");
    assert_eq!(st("create_uid"), CheckCompany::Off, "audit field");
    // the inherited copy of the parent's company field is never checked
    assert_eq!(st("name"), CheckCompany::Off);
    assert!(r.model("co.line").unwrap().check_company_auto);
}

/// Model default and its opt-out (rule 2).
#[test]
fn model_default_and_opt_outs() {
    let r = build(vec![
        company(),
        doc("co.target", vec![]),
        // a scoped model with a checked field: on
        doc("co.on", vec![FieldDecl::many2one("t_id", "co.target")]),
        // a scoped model with nothing to check: off
        doc("co.none", vec![FieldDecl::char("name")]),
        // model opt-out
        doc("co.off", vec![FieldDecl::many2one("t_id", "co.target")]).check_company_auto(false),
        // a model without company
        m("co.plain", vec![FieldDecl::many2one("t_id", "co.target")]),
        doc("co.forced", vec![FieldDecl::char("name")]).check_company_auto(true),
    ])
    .unwrap();
    let auto = |n: &str| r.model(n).unwrap().check_company_auto;
    assert!(auto("co.on"));
    assert!(!auto("co.none"));
    assert!(!auto("co.off"));
    assert!(!auto("co.plain"));
    assert!(auto("co.forced"));
    assert!(
        !auto("res.company"),
        "res.company keeps the reference ERP's default"
    );
    assert!(!auto("co.target"), "nothing to check on the target");
    // the field of an opted-out model keeps its own state
    assert_eq!(field_state(&r, "co.off", "t_id"), CheckCompany::Default);
    assert_eq!(field_state(&r, "co.plain", "t_id"), CheckCompany::Off);
}

/// An extension's newest declaration decides (MRO), for model and field.
#[test]
fn newest_declaration_wins() {
    let r = build(vec![
        company(),
        doc("co.target", vec![]),
        doc("co.on", vec![FieldDecl::many2one("t_id", "co.target")]),
        ModelDecl::extension("co_ext", "co.on", &["co.on"])
            .check_company_auto(false)
            .fields(leak(vec![
                FieldDecl::many2one("t_id", "co.target").check_company(false),
            ])),
    ])
    .unwrap();
    assert!(!r.model("co.on").unwrap().check_company_auto);
    assert_eq!(field_state(&r, "co.on", "t_id"), CheckCompany::Off);
}

/// `company_match` defaults and the accepted forms (rule 1).
#[test]
fn company_match_forms() {
    let r = build(vec![
        company_tree(),
        doc("co.catalog", vec![]).company_match(CompanyMatch::ParentOf),
        m(
            "co.many",
            vec![FieldDecl::many2many("company_ids", "res.company")],
        )
        .company_access(CompanyAccessDecl::unrestricted(
            "company consistency fixture; access is not under test",
        ))
        .company_match(CompanyMatch::ParentOfAny),
    ])
    .unwrap();
    assert_eq!(
        r.model("co.catalog")
            .unwrap()
            .company_scope
            .unwrap()
            .matching(),
        Some(CompanyMatch::ParentOf)
    );
    assert_eq!(
        r.model("co.many")
            .unwrap()
            .company_scope
            .unwrap()
            .matching(),
        Some(CompanyMatch::ParentOfAny)
    );
    // an extension that sets a match later wins
    let r = build(vec![
        company_tree(),
        doc("co.catalog", vec![]),
        ModelDecl::extension("co_ext", "co.catalog", &["co.catalog"])
            .company_match(CompanyMatch::ParentOf),
    ])
    .unwrap();
    assert_eq!(
        r.model("co.catalog")
            .unwrap()
            .company_scope
            .unwrap()
            .matching(),
        Some(CompanyMatch::ParentOf)
    );
}

#[test]
fn company_match_on_a_model_without_company_is_refused() {
    assert_eq!(
        refusal(vec![
            company_tree(),
            m("co.plain", vec![]).company_match(CompanyMatch::Exact)
        ]),
        (
            ErrorKind::Programming,
            "co.plain: company_match = \"exact\" needs a company_id or company_ids field linked \
             to res.company (declared in addon 'co')"
                .to_owned()
        )
    );
    // res.company itself has no match
    assert_eq!(
        refusal(vec![company_tree().company_match(CompanyMatch::Exact)]),
        (
            ErrorKind::Programming,
            "res.company: company_match = \"exact\" needs a company_id or company_ids field \
             linked to res.company (declared in addon 'co')"
                .to_owned()
        )
    );
}

#[test]
fn company_match_of_the_wrong_form_is_refused() {
    assert_eq!(
        refusal(vec![
            company_tree(),
            doc("co.x", vec![]).company_match(CompanyMatch::AnyOf)
        ]),
        (
            ErrorKind::Programming,
            "co.x: company_match = \"any_of\" needs a company_ids field; co.x links one company \
             through company_id (use \"exact\" or \"parent_of\") (declared in addon 'co')"
                .to_owned()
        )
    );
    assert_eq!(
        refusal(vec![
            company_tree(),
            m(
                "co.y",
                vec![FieldDecl::many2many("company_ids", "res.company")]
            )
            .company_match(CompanyMatch::ParentOf)
        ]),
        (
            ErrorKind::Programming,
            "co.y: company_match = \"parent_of\" needs a company_id field; co.y links companies \
             through company_ids (use \"any_of\" or \"parent_of_any\") (declared in addon 'co')"
                .to_owned()
        )
    );
}

#[test]
fn parent_forms_need_a_parent_path_on_res_company() {
    assert_eq!(
        refusal(vec![
            company(),
            doc("co.x", vec![]).company_match(CompanyMatch::ParentOf)
        ]),
        (
            ErrorKind::Programming,
            "co.x: company_match = \"parent_of\" needs res.company to keep a parent path \
             (_parent_store) (declared in addon 'co')"
                .to_owned()
        )
    );
    assert_eq!(
        refusal(vec![
            company(),
            m(
                "co.y",
                vec![FieldDecl::many2many("company_ids", "res.company")]
            )
            .company_match(CompanyMatch::ParentOfAny)
        ]),
        (
            ErrorKind::Programming,
            "co.y: company_match = \"parent_of_any\" needs res.company to keep a parent path \
             (_parent_store) (declared in addon 'co')"
                .to_owned()
        )
    );
}

#[test]
fn check_company_on_a_model_without_company_is_refused() {
    assert_eq!(
        refusal(vec![
            company(),
            doc("co.target", vec![]),
            m(
                "co.plain",
                vec![FieldDecl::many2one("t_id", "co.target").check_company(true)]
            ),
        ]),
        (
            ErrorKind::Programming,
            "co.plain.t_id declares check_company, but co.plain has no company_id or company_ids \
             field linked to res.company (declared in addon 'co')"
                .to_owned()
        )
    );
    assert_eq!(
        refusal(vec![
            company(),
            m("co.plain", vec![FieldDecl::char("name")]).check_company_auto(true),
        ]),
        (
            ErrorKind::Programming,
            "co.plain declares check_company_auto, but has no company_id or company_ids field \
             linked to res.company (declared in addon 'co')"
                .to_owned()
        )
    );
    // company_id of the wrong shape does not make a model scoped
    assert_eq!(
        refusal(vec![
            company(),
            m(
                "co.dep",
                vec![FieldDecl::many2one("company_id", "res.company").company_dependent(true)]
            )
            .check_company_auto(true),
        ]),
        (
            ErrorKind::Programming,
            "co.dep declares check_company_auto, but has no company_id or company_ids field \
             linked to res.company (declared in addon 'co')"
                .to_owned()
        )
    );
}

/// Review F3, F4: a declared check that could never compare anything is refused when the registry
/// is built: a target without a company scope, a link to `res.company` itself, a field that is not
/// relational.
#[test]
fn a_declared_check_that_cannot_compare_is_refused() {
    assert_eq!(
        refusal(vec![
            company(),
            m("co.plain", vec![FieldDecl::char("name")]),
            doc(
                "co.doc",
                vec![FieldDecl::many2one("p_id", "co.plain").check_company(true)]
            ),
        ]),
        (
            ErrorKind::Programming,
            "co.doc.p_id declares check_company, but its target co.plain has no company_id or \
             company_ids field linked to res.company, so there is nothing to compare (declared \
             in addon 'co')"
                .to_owned()
        )
    );
    assert_eq!(
        refusal(vec![
            company(),
            doc(
                "co.doc",
                vec![FieldDecl::many2one("c_id", "res.company").check_company(true)]
            ),
        ]),
        (
            ErrorKind::Programming,
            "co.doc.c_id declares check_company, but it links to res.company itself, which has \
             no company to compare with (declared in addon 'co')"
                .to_owned()
        )
    );
    assert_eq!(
        refusal(vec![
            company(),
            doc("co.doc", vec![FieldDecl::char("note").check_company(true)]),
        ]),
        (
            ErrorKind::Programming,
            "co.doc.note declares check_company, but it is not a relational field (declared in \
             addon 'co')"
                .to_owned()
        )
    );
    // a scoped target (Single or Many) is fine
    build(vec![
        company(),
        doc("co.target", vec![]),
        doc(
            "co.doc",
            vec![FieldDecl::many2one("t_id", "co.target").check_company(true)],
        ),
    ])
    .unwrap();
}

/// A declared check on an abstract mixin is composed into its concrete models, which are checked
/// there; the mixin itself is not an error.
#[test]
fn abstract_mixins_may_declare_checks() {
    let r = build(vec![
        company(),
        doc("co.target", vec![]),
        m(
            "co.mixin",
            vec![FieldDecl::many2one("t_id", "co.target").check_company(true)],
        )
        .kind(ModelKind::Abstract),
        doc("co.user", vec![])
            .inherit(&["co.mixin"])
            .check_company_auto(true),
    ])
    .unwrap();
    assert_eq!(field_state(&r, "co.user", "t_id"), CheckCompany::Declared);
    assert_eq!(field_state(&r, "co.mixin", "t_id"), CheckCompany::Declared);
}

/// Review R5: an abstract mixin may declare `company_match`; the concrete models composing it get
/// the match, the mixin itself stays unscoped and is no error.
#[test]
fn abstract_mixins_may_declare_a_company_match() {
    let r = build(vec![
        company_tree(),
        m("co.mixin", vec![])
            .kind(ModelKind::Abstract)
            .company_match(CompanyMatch::ParentOf),
        doc("co.user", vec![]).inherit(&["co.mixin"]),
    ])
    .unwrap();
    assert_eq!(r.model("co.mixin").unwrap().company_scope, None);
    let user = r.model("co.user").unwrap();
    assert_eq!(
        user.company_scope,
        Some(CompanyScope::Single {
            field: user.field_id("company_id").unwrap(),
            matching: SingleMatch::ParentOf
        })
    );
}

/// `res.company` fields are checked only where declared; `Itself` accepts `check_company_auto`.
#[test]
fn res_company_checks_only_declared_fields() {
    let r = build(vec![
        doc("co.target", vec![]),
        m(
            "res.company",
            vec![
                FieldDecl::many2one("parent_id", "res.company"),
                FieldDecl::many2one("t_id", "co.target").check_company(true),
                FieldDecl::many2one("u_id", "co.target"),
            ],
        )
        .check_company_auto(true),
    ])
    .unwrap();
    let c = r.model("res.company").unwrap();
    assert!(c.check_company_auto);
    assert_eq!(
        field_state(&r, "res.company", "t_id"),
        CheckCompany::Declared
    );
    assert_eq!(field_state(&r, "res.company", "u_id"), CheckCompany::Off);
    assert_eq!(
        field_state(&r, "res.company", "parent_id"),
        CheckCompany::Off
    );
}

/// Runtime declarations get the same opt-outs: a `ModelDef` opts out of the model check.
#[test]
fn runtime_model_defs_carry_the_opt_outs() {
    let def = ModelDef::new("co_ext", "co.rt")
        .check_company_auto(false)
        .company_match(CompanyMatch::Exact);
    assert_eq!(def.check_company_auto, Some(false));
    assert_eq!(def.company_match, Some(CompanyMatch::Exact));
}

/// A non-stored related field that leaves `compute_sudo` unset reads company-bound (rule 6); an
/// explicit value, or a stored field, does not. The read mode is one value (review T6), so the
/// superuser read and the bound cannot disagree.
#[test]
fn related_fields_read_company_bound_unless_compute_sudo_is_declared() {
    let r = build(vec![
        company(),
        doc("co.target", vec![FieldDecl::char("name")]),
        doc(
            "co.line",
            vec![
                FieldDecl::many2one("t_id", "co.target"),
                FieldDecl::char("a").related("t_id.name"),
                FieldDecl::char("b").related("t_id.name").compute_sudo(true),
                FieldDecl::char("c")
                    .related("t_id.name")
                    .compute_sudo(false),
                FieldDecl::char("d"),
                FieldDecl::char("e").related("t_id.name").store(true),
            ],
        ),
    ])
    .unwrap();
    let line = r.model("co.line").unwrap();
    // (read as superuser, company-bound) per field: the read mode is not a public field
    let mode = |f: &str| {
        let field = line.field(f).unwrap();
        (field.compute_sudo(), field.company_bound())
    };
    assert_eq!(mode("a"), (true, true));
    assert_eq!(mode("b"), (true, false));
    assert_eq!(mode("c"), (false, false));
    assert_eq!(mode("d"), (false, false));
    assert_eq!(mode("e"), (true, false), "stored: covered at write time");
    let a = line.field("a").unwrap();
    assert!(
        a.company_bound() && a.compute_sudo(),
        "bound implies superuser"
    );
    assert!(!line.field("b").unwrap().company_bound());
    assert!(line.field("b").unwrap().compute_sudo());
    assert!(!line.field("c").unwrap().compute_sudo());
}

/// Review TD2: the copy of a company-bound related field that a child inherits through `_inherits`
/// has no related path of its own (it is read on the parent): it is read as the caller, not
/// "company-bound" and not as superuser. Before the fix it kept the parent's mode.
#[test]
fn an_inherited_copy_of_a_company_bound_field_is_read_as_the_caller() {
    let r = build(vec![
        company(),
        doc("co.target", vec![FieldDecl::char("name")]),
        doc(
            "co.base",
            vec![
                FieldDecl::many2one("t_id", "co.target"),
                FieldDecl::char("a").related("t_id.name"),
                FieldDecl::char("b").related("t_id.name").compute_sudo(true),
            ],
        ),
        m(
            "co.child",
            vec![
                FieldDecl::many2one("base_id", "co.base")
                    .delegate(true)
                    .required(true)
                    .ondelete(erp_fields::OnDelete::Cascade),
            ],
        )
        .company_access(CompanyAccessDecl::unrestricted(
            "company consistency fixture; access is not under test",
        ))
        .inherits(&[("co.base", "base_id")]),
    ])
    .unwrap();
    let mode = |model: &str, f: &str| {
        let field = r.model(model).unwrap().field(f).unwrap();
        (field.compute_sudo(), field.company_bound())
    };
    assert_eq!(mode("co.base", "a"), (true, true));
    assert_eq!(mode("co.child", "a"), (false, false));
    assert_eq!(mode("co.child", "b"), (false, false));
}

/// The delegate many2one of a model built by `_inherits`.
fn delegate(name: &'static str, parent: &'static str) -> FieldDecl {
    FieldDecl::many2one(name, parent)
        .delegate(true)
        .required(true)
        .ondelete(erp_fields::OnDelete::Cascade)
}

/// `co.variant`: delegates to `co.template` through `template_id` (A2), so its
/// `company_id` (when the template has one) is the template's; `extra` are its own fields.
fn variant(extra: Vec<FieldDecl>) -> ModelDecl {
    let mut fields = vec![delegate("template_id", "co.template")];
    fields.extend(extra);
    m("co.variant", fields)
        .company_access(CompanyAccessDecl::unrestricted(
            "company consistency fixture; access is not under test",
        ))
        .inherits(&[("co.template", "template_id")])
}

/// `co.lot`: a scoped model with a declared check toward the delegating `co.variant` and a link
/// to it that declares nothing.
fn lot() -> ModelDecl {
    doc(
        "co.lot",
        vec![
            FieldDecl::many2one("variant_id", "co.variant").check_company(true),
            FieldDecl::many2one("plain_id", "co.variant"),
        ],
    )
    .check_company_auto(true)
}

/// A declared check toward a model whose `company_id` comes by `_inherits` delegation builds; the
/// delegating model keeps no company scope of its own but records the delegation, also over two
/// levels of delegation.
#[test]
fn a_declared_check_toward_a_delegated_company_builds() {
    let r = build(vec![
        company_tree(),
        doc("co.template", vec![FieldDecl::char("name")]).company_match(CompanyMatch::ParentOf),
        variant(vec![]),
        // its required link reaches no company-scoped model, so it declares no company access
        m("co.pallet", vec![delegate("variant_id", "co.variant")])
            .inherits(&[("co.variant", "variant_id")]),
        lot(),
    ])
    .unwrap();
    let template = r.model("co.template").unwrap();
    let template_company = template.field_id("company_id").unwrap();
    let v = r.model("co.variant").unwrap();
    assert_eq!(v.company_scope, None, "the scope itself does not change");
    assert_eq!(
        v.company_delegation,
        Some(CompanyDelegation {
            field: v.field_id("company_id").unwrap(),
            delegate: v.field_id("template_id").unwrap(),
            storing_model: template.id,
            matching: SingleMatch::ParentOf,
        })
    );
    assert_eq!(
        template.company_scope,
        Some(CompanyScope::Single {
            field: template_company,
            matching: SingleMatch::ParentOf
        })
    );
    assert_eq!(template.company_delegation, None, "scoped by its own field");
    let pallet = r.model("co.pallet").unwrap();
    assert_eq!(pallet.company_scope, None);
    assert_eq!(
        pallet.company_delegation,
        Some(CompanyDelegation {
            field: pallet.field_id("company_id").unwrap(),
            delegate: pallet.field_id("variant_id").unwrap(),
            storing_model: template.id,
            matching: SingleMatch::ParentOf,
        })
    );
    assert_eq!(
        field_state(&r, "co.lot", "variant_id"),
        CheckCompany::Declared
    );
    assert!(r.model("co.lot").unwrap().check_company_auto);
    assert_eq!(r.model("co.lot").unwrap().company_delegation, None);
}

/// A2-D2: the rule 2 default does not reach a link to a delegating model; only a
/// declared check does.
#[test]
fn a_link_to_a_delegated_company_is_not_checked_by_default() {
    let r = build(vec![
        company(),
        doc("co.template", vec![FieldDecl::char("name")]),
        variant(vec![]),
        lot(),
        // nothing declared on the model either: no checked field, so no model default
        doc(
            "co.loose",
            vec![FieldDecl::many2one("variant_id", "co.variant")],
        ),
    ])
    .unwrap();
    assert_eq!(field_state(&r, "co.lot", "plain_id"), CheckCompany::Off);
    assert_eq!(field_state(&r, "co.loose", "variant_id"), CheckCompany::Off);
    assert!(!r.model("co.loose").unwrap().check_company_auto);
    // a model scoped by its own company_id records no delegation
    assert_eq!(r.model("co.lot").unwrap().company_delegation, None);
}

/// A2-D3: `company_ids` that arrive by delegation are refused as a check target, and the
/// message names the delegation.
#[test]
fn a_declared_check_toward_delegated_company_ids_is_refused() {
    assert_eq!(
        refusal(vec![
            company(),
            m(
                "co.template",
                vec![
                    FieldDecl::char("name"),
                    FieldDecl::many2many("company_ids", "res.company"),
                ],
            )
            .company_access(CompanyAccessDecl::unrestricted(
                "company consistency fixture; access is not under test",
            )),
            variant(vec![]),
            lot(),
        ]),
        (
            ErrorKind::Programming,
            "co.lot.variant_id declares check_company, but its target co.variant has company_ids \
             only through the delegation co.variant.template_id to co.template, which the company \
             check does not support (declared in addon 'co')"
                .to_owned()
        )
    );
}

/// A2-D3: a check on a field of the delegating model itself is refused, naming the
/// delegation.
#[test]
fn a_declared_check_on_a_delegating_model_is_refused() {
    assert_eq!(
        refusal(vec![
            company(),
            doc("co.target", vec![]),
            doc("co.template", vec![FieldDecl::char("name")]),
            variant(vec![
                FieldDecl::many2one("t_id", "co.target").check_company(true)
            ]),
        ]),
        (
            ErrorKind::Programming,
            "co.variant.t_id declares check_company, but co.variant has its company only through \
             the delegation co.variant.template_id to co.template; checks on a delegating model's \
             own fields are not supported (declared in addon 'co')"
                .to_owned()
        )
    );
}

/// A2-D3: `check_company_auto` on the delegating model itself is refused, naming the
/// delegation.
#[test]
fn check_company_auto_on_a_delegating_model_is_refused() {
    assert_eq!(
        refusal(vec![
            company(),
            doc("co.template", vec![FieldDecl::char("name")]),
            variant(vec![]).check_company_auto(true),
        ]),
        (
            ErrorKind::Programming,
            "co.variant declares check_company_auto, but has its company only through the \
             delegation co.variant.template_id to co.template; checks on a delegating model's own \
             fields are not supported (declared in addon 'co')"
                .to_owned()
        )
    );
}

#[test]
fn company_match_parses_the_four_spellings_only() {
    for (text, expected) in [
        ("exact", CompanyMatch::Exact),
        ("parent_of", CompanyMatch::ParentOf),
        ("any_of", CompanyMatch::AnyOf),
        ("parent_of_any", CompanyMatch::ParentOfAny),
    ] {
        let parsed = CompanyMatch::parse(text).unwrap();
        assert_eq!(parsed, expected);
        assert_eq!(parsed.as_str(), text);
    }
    let err = CompanyMatch::parse("Exact").unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "company_match must be one of \"exact\", \"parent_of\", \"any_of\", \"parent_of_any\" \
         (got \"Exact\")"
    );
}

/// Review T1: a `Single` scope holds only the `company_id` forms and a `Many` scope only the
/// `company_ids` forms; the declaration spelling maps to exactly one of them.
#[test]
fn the_two_scope_forms_are_disjoint() {
    for name in CompanyMatch::NAMES {
        let m = CompanyMatch::parse(name).unwrap();
        assert!(
            m.single().is_some() != m.many().is_some(),
            "{name} belongs to exactly one scope form"
        );
        let back: CompanyMatch = match (m.single(), m.many()) {
            (Some(s), None) => s.into(),
            (None, Some(many)) => many.into(),
            other => panic!("{name}: {other:?}"),
        };
        assert_eq!(back, m);
    }
    assert_eq!(CompanyMatch::Exact.single(), Some(SingleMatch::Exact));
    assert_eq!(
        CompanyMatch::ParentOfAny.many(),
        Some(ManyMatch::ParentOfAny)
    );
}
