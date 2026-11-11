//! Company access declarations (A1-D2, A1-D3; design,
//! §5): the declared forms, their composition through the MRO (an extension may only narrow) and
//! the registry validation of the resolved declaration. Pure: no database.
//!
//! Every message is asserted whole, with the error kind and the typed variant behind it.
#![allow(clippy::unwrap_used, clippy::panic, reason = "tests")]

use std::error::Error as _;

use erp_fields::{ErrorKind, OnDelete};
use erp_registry::{
    AddonDescriptor, CompanyAccessDecl, CompanyAccessDef, CompanyAccessError, CompanyBranch,
    CompanyCardinality, EmptyCompany, FieldDecl, FieldDef, KindDef, MAX_OWNER_DEPTH,
    MAX_UNRESTRICTED_REASON_BYTES, ModelDecl, ModelDef, ModelKind, Overlay, Registry,
    RegistryBuilder, Restriction,
};
use erp_registry::{CompanyAccessState, NotApplicable};

use CompanyBranch::{Ancestor, Exact};
use EmptyCompany::{Allow, Deny};

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
static CO_MORE: AddonDescriptor = AddonDescriptor {
    name: "co_more",
    depends: &["co_ext"],
    exact_only: false,
};

fn leak<T>(items: Vec<T>) -> &'static [T] {
    Vec::leak(items)
}

fn leak_str(s: String) -> &'static str {
    Box::leak(s.into_boxed_str())
}

fn build(decls: Vec<ModelDecl>) -> erp_fields::Result<Registry> {
    let decls: Vec<&'static ModelDecl> = decls
        .into_iter()
        .map(|d| &*Box::leak(Box::new(d)))
        .collect();
    RegistryBuilder::new([&CO, &CO_EXT, &CO_MORE])
        .linked(false)
        .with_decls(decls)
        .build()
}

/// The declared access of a model; a test that expects a declaration fails on any other state.
trait DeclaredExt {
    fn declared(&self) -> erp_registry::CompanyAccess;
}

impl DeclaredExt for CompanyAccessState {
    fn declared(&self) -> erp_registry::CompanyAccess {
        match self {
            CompanyAccessState::Declared(a) => *a,
            other => panic!("expected a declaration, got {other:?}"),
        }
    }
}

/// The typed error behind a registry error.
fn typed(err: &erp_fields::Error) -> CompanyAccessError {
    err.source()
        .and_then(|s| s.downcast_ref::<CompanyAccessError>())
        .cloned()
        .unwrap_or_else(|| panic!("not a company access error: {err:?}"))
}

/// `(kind, message, typed error)` of a refused registry.
fn refusal(decls: Vec<ModelDecl>) -> (ErrorKind, String, CompanyAccessError) {
    let err = build(decls).unwrap_err();
    let t = typed(&err);
    assert_eq!(
        t.to_string(),
        err.message(),
        "the typed error carries the message"
    );
    (err.kind(), err.message().to_owned(), t)
}

/// Assert a refusal: Programming, the full message, and the variant.
fn refused(decls: Vec<ModelDecl>, message: &str, variant: fn(&CompanyAccessError) -> bool) {
    let (kind, got, t) = refusal(decls);
    assert_eq!(kind, ErrorKind::Programming, "{got}");
    assert_eq!(got, message);
    assert!(variant(&t), "unexpected variant {t:?}");
    // the typed parts name what the message names
    assert!(
        got.starts_with(&format!("{}: ", t.model())),
        "model() {} for {got}",
        t.model()
    );
    assert!(
        got.contains(&format!("declared by {}", t.declared_by())),
        "declared_by() {} for {got}",
        t.declared_by()
    );
}

fn m(name: &'static str, fields: Vec<FieldDecl>) -> ModelDecl {
    ModelDecl::new("co", name).fields(leak(fields))
}

/// An in-place extension of `name` by `addon`.
fn ext(addon: &'static str, name: &'static str) -> ModelDecl {
    ModelDecl::extension(addon, name, leak(vec![name]))
}

/// `res.company` with its hierarchy (`parent_id` and `parent_path`).
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

/// `res.company` with a parent field but no `parent_store`.
fn company_flat() -> ModelDecl {
    m(
        "res.company",
        vec![
            FieldDecl::char("name"),
            FieldDecl::many2one("parent_id", "res.company"),
        ],
    )
}

/// `res.company` without a parent field.
fn company_bare() -> ModelDecl {
    m("res.company", vec![FieldDecl::char("name")])
}

/// A model with `company_id` (scope single).
fn doc(name: &'static str, mut fields: Vec<FieldDecl>) -> ModelDecl {
    fields.insert(0, FieldDecl::many2one("company_id", "res.company"));
    m(name, fields)
}

/// A model with `company_ids` (scope many).
fn many(name: &'static str, mut fields: Vec<FieldDecl>) -> ModelDecl {
    fields.insert(0, FieldDecl::many2many("company_ids", "res.company"));
    m(name, fields)
}

/// A required many2one `name` to `comodel` (an owner edge).
fn edge(name: &'static str, comodel: &'static str) -> FieldDecl {
    FieldDecl::many2one(name, comodel).required(true)
}

fn direct(field: &'static str, branch: CompanyBranch, empty: EmptyCompany) -> CompanyAccessDecl {
    CompanyAccessDecl::direct(field, branch, empty)
}

fn parent(edge: &'static str) -> CompanyAccessDecl {
    CompanyAccessDecl::parent(edge)
}

fn both(
    field: &'static str,
    branch: CompanyBranch,
    empty: EmptyCompany,
    edge: &'static str,
) -> CompanyAccessDecl {
    CompanyAccessDecl::direct_and_parent(field, branch, empty, edge)
}

fn open(reason: &'static str) -> CompanyAccessDecl {
    CompanyAccessDecl::unrestricted(reason)
}

/// `co.doc`, restricted to its company (form 1): the owner of most owner tests.
fn owner_doc() -> ModelDecl {
    doc("co.doc", vec![]).company_access(direct("company_id", Exact, Deny))
}

const DOC_DECL: &str = "model definition 'co.doc' of addon 'co'";

// ------------------------------------------------------------------------------------------------
// forms and spellings

/// Every declared form resolves to field ids, cardinality, owner and source.
#[test]
fn each_declaration_form_resolves() {
    let r = build(vec![
        company_tree(),
        doc("co.doc", vec![])
            .company_access(direct("company_id", Exact, Deny))
            .at("src/doc.rs", 10),
        many("co.shared", vec![]).company_access(direct("company_ids", Ancestor, Allow)),
        m("co.line", vec![edge("doc_id", "co.doc")]).company_access(parent("doc_id")),
        doc("co.rep", vec![edge("doc_id", "co.doc")]).company_access(both(
            "company_id",
            Ancestor,
            Allow,
            "doc_id",
        )),
        doc("co.open", vec![]).company_access(open("visible to every company")),
    ])
    .unwrap();

    let doc_spec = r.model("co.doc").unwrap();
    let access = doc_spec.company_access.declared();
    let Restriction::Direct(d) = access.restriction() else {
        panic!("{:?}", access.restriction());
    };
    assert_eq!(d.field(), doc_spec.field_id("company_id").unwrap());
    assert_eq!(d.cardinality(), CompanyCardinality::Single);
    assert_eq!((d.branch(), d.empty()), (Exact, Deny));
    assert!(access.generates());
    assert_eq!(
        access.anchors().collect::<Vec<_>>(),
        vec![doc_spec.field_id("company_id").unwrap()]
    );
    assert_eq!(
        access.declaration(),
        direct("company_id", Exact, Deny),
        "the declaration as written"
    );
    let source = access.source();
    assert_eq!(
        (
            source.addon(),
            source.definition(),
            source.file(),
            source.line()
        ),
        ("co", "co.doc", "src/doc.rs", 10)
    );
    assert_eq!(
        source.describe(),
        doc_spec.mro.first().unwrap().describe(),
        "the source is described like the declaring definition"
    );

    let shared = r.model("co.shared").unwrap();
    let Restriction::Direct(d) = shared.company_access.declared().restriction() else {
        panic!()
    };
    assert_eq!(
        (d.field(), d.cardinality(), d.branch(), d.empty()),
        (
            shared.field_id("company_ids").unwrap(),
            CompanyCardinality::Many,
            Ancestor,
            Allow
        )
    );

    let line = r.model("co.line").unwrap();
    let access = line.company_access.declared();
    let Restriction::Owner(o) = access.restriction() else {
        panic!("{:?}", access.restriction())
    };
    assert_eq!(o.edge(), line.field_id("doc_id").unwrap());
    assert_eq!(o.owner(), r.model_id("co.doc").unwrap());
    assert_eq!(
        access.anchors().collect::<Vec<_>>(),
        vec![line.field_id("doc_id").unwrap()]
    );

    let rep = r.model("co.rep").unwrap();
    let access = rep.company_access.declared();
    let Restriction::DirectAndOwner(d, o) = access.restriction() else {
        panic!("{:?}", access.restriction())
    };
    assert_eq!(
        (d.field(), d.cardinality(), d.branch(), d.empty()),
        (
            rep.field_id("company_id").unwrap(),
            CompanyCardinality::Single,
            Ancestor,
            Allow
        )
    );
    assert_eq!(
        (o.edge(), o.owner()),
        (
            rep.field_id("doc_id").unwrap(),
            r.model_id("co.doc").unwrap()
        )
    );
    assert_eq!(
        access.anchors().collect::<Vec<_>>(),
        vec![
            rep.field_id("company_id").unwrap(),
            rep.field_id("doc_id").unwrap()
        ]
    );

    let open_spec = r.model("co.open").unwrap();
    let access = open_spec.company_access.declared();
    assert!(matches!(
        access.restriction(),
        Restriction::Unrestricted {
            reason: "visible to every company",
            ..
        }
    ));
    assert!(!access.generates());
    assert_eq!(access.anchors().count(), 0);

    assert_eq!(
        r.model("res.company").unwrap().company_access,
        CompanyAccessState::NotApplicable(NotApplicable::CompanyModel)
    );
}

/// The spelling every message uses is the macro's syntax; reasons are quoted and escaped.
#[test]
fn spellings_are_the_macro_syntax() {
    assert_eq!(
        direct("company_id", Exact, Deny).spelling(),
        r#"direct(field = "company_id", branch = "exact", empty = "deny")"#
    );
    assert_eq!(
        direct("company_ids", Ancestor, Allow).spelling(),
        r#"direct(field = "company_ids", branch = "ancestor", empty = "allow")"#
    );
    assert_eq!(parent("doc_id").spelling(), r#"parent(edge = "doc_id")"#);
    assert_eq!(
        both("company_id", Ancestor, Allow, "tax_id").spelling(),
        r#"direct(field = "company_id", branch = "ancestor", empty = "allow", parent = "tax_id")"#
    );
    assert_eq!(
        open("say \"hi\"\r\nSociété").spelling(),
        r#"unrestricted(reason = "say \"hi\"\r\nSociété")"#
    );
}

/// `branch` and `empty` parse their two spellings each and nothing else.
#[test]
fn branch_and_empty_parse_their_spellings_only() {
    for name in CompanyBranch::NAMES {
        assert_eq!(CompanyBranch::parse(name).unwrap().as_str(), name);
    }
    for name in EmptyCompany::NAMES {
        assert_eq!(EmptyCompany::parse(name).unwrap().as_str(), name);
    }
    let e = CompanyBranch::parse("Exact").unwrap_err();
    assert_eq!(
        (e.kind(), e.message()),
        (
            ErrorKind::Programming,
            r#"branch must be "exact" or "ancestor" (got "Exact")"#
        )
    );
    let e = EmptyCompany::parse("").unwrap_err();
    assert_eq!(
        (e.kind(), e.message()),
        (
            ErrorKind::Programming,
            r#"empty must be "allow" or "deny" (got "")"#
        )
    );
}

/// `Programming` and its full message, here and in the per-variant tests of this file; the mixin
/// and runtime-overlay routes for a sample of variants; `Undeclared` is MIGRATE's, the failing
/// installation INSTALL's)
/// chains not ending in a direct scope, two owners and an ancestor branch without the company
/// hierarchy fail the registry build with their full messages; the missing-declaration variant is
/// MIGRATE's, the installation failing as one unit INSTALL's; SQL-view models do not exist yet)
#[test]
fn spec_accsec_089_invalid_declarations_fail_the_build() {
    type Case = (
        Vec<ModelDecl>,
        &'static str,
        fn(&CompanyAccessError) -> bool,
    );
    let cases: Vec<Case> = vec![
        (
            vec![
                company_tree(),
                owner_doc(),
                m("co.line", vec![FieldDecl::many2one("doc_id", "co.doc")])
                    .company_access(parent("doc_id")),
            ],
            "co.line: company_access parent(edge = \"doc_id\") names the owner 'doc_id', but \
             co.line.doc_id is optional: an owner edge is required (required = true), so every \
             record has an owner (declared by model definition 'co.line' of addon 'co')",
            |e| matches!(e, CompanyAccessError::EdgeOptional { .. }),
        ),
        (
            vec![
                company_tree(),
                owner_doc(),
                m(
                    "co.line",
                    vec![edge("doc_id", "co.doc").store(false).compute("_doc")],
                )
                .company_access(parent("doc_id")),
            ],
            "co.line: company_access parent(edge = \"doc_id\") names the owner 'doc_id', but \
             co.line.doc_id is not stored (declared by model definition 'co.line' of addon 'co')",
            |e| matches!(e, CompanyAccessError::EdgeNotStored { .. }),
        ),
        (
            vec![
                company_tree(),
                owner_doc(),
                m("co.line", vec![FieldDecl::many2many("doc_id", "co.doc")])
                    .company_access(parent("doc_id")),
            ],
            "co.line: company_access parent(edge = \"doc_id\") names the owner 'doc_id', but \
             co.line.doc_id is a many2many, not a many2one (declared by model definition \
             'co.line' of addon 'co')",
            |e| matches!(e, CompanyAccessError::EdgeNotMany2one { .. }),
        ),
        (
            vec![
                company_tree(),
                m("co.a", vec![edge("b_id", "co.b")]).company_access(parent("b_id")),
                m("co.b", vec![edge("a_id", "co.a")]).company_access(parent("a_id")),
            ],
            "co.a: the owners of company_access form a cycle: co.a.b_id -> co.b.a_id -> co.a \
             (declared by model definition 'co.a' of addon 'co')",
            |e| matches!(e, CompanyAccessError::OwnerCycle { .. }),
        ),
        (
            vec![
                company_tree(),
                doc("co.doc", vec![]).company_access(open("shared catalogue")),
                m("co.line", vec![edge("doc_id", "co.doc")]).company_access(parent("doc_id")),
            ],
            "co.line: company_access parent(edge = \"doc_id\") inherits the company restriction \
             of co.doc through 'doc_id', but co.doc is declared unrestricted: an owner chain must \
             end in a direct company restriction (declared by model definition 'co.line' of addon \
             'co')",
            |e| matches!(e, CompanyAccessError::OwnerNotRestricted { .. }),
        ),
        (
            vec![
                company_tree(),
                owner_doc(),
                m(
                    "co.line",
                    vec![edge("doc_id", "co.doc"), edge("other_id", "co.doc")],
                )
                .company_access(parent("doc_id")),
                ext("co_ext", "co.line").company_access(parent("other_id")),
            ],
            "co.line: company_access names the owner 'other_id' (declared by model definition \
             'co.line' of addon 'co_ext'), but model definition 'co.line' of addon 'co' names the \
             owner 'doc_id': a record has one owner",
            |e| matches!(e, CompanyAccessError::TwoOwners { .. }),
        ),
        (
            vec![
                company_flat(),
                doc("co.doc", vec![]).company_access(direct("company_id", Ancestor, Deny)),
            ],
            "co.doc: company_access direct(field = \"company_id\", branch = \"ancestor\", empty = \
             \"deny\") uses branch \"ancestor\", which needs res.company to keep its hierarchy (a \
             parent field and _parent_store); use branch \"exact\" or declare parent_store on \
             res.company (declared by model definition 'co.doc' of addon 'co')",
            |e| matches!(e, CompanyAccessError::AncestorWithoutHierarchy { .. }),
        ),
    ];
    for (decls, message, variant) in cases {
        refused(decls, message, variant);
    }
}

// ------------------------------------------------------------------------------------------------
// one test per variant

#[test]
fn direct_needs_a_company_field() {
    refused(
        vec![
            company_tree(),
            m("co.plain", vec![FieldDecl::char("name")]).company_access(direct(
                "company_id",
                Exact,
                Deny,
            )),
        ],
        "co.plain: company_access direct(field = \"company_id\", branch = \"exact\", empty = \
         \"deny\") names the company field 'company_id', but co.plain has no company_id or \
         company_ids field linked to res.company (declared by model definition 'co.plain' of \
         addon 'co')",
        |e| matches!(e, CompanyAccessError::NotCompanyScoped { .. }),
    );
    // a company_id that links elsewhere does not scope the model
    refused(
        vec![
            company_tree(),
            m("co.plain", vec![FieldDecl::char("name")]),
            m(
                "co.other",
                vec![FieldDecl::many2one("company_id", "co.plain")],
            )
            .company_access(direct("company_id", Exact, Allow)),
        ],
        "co.other: company_access direct(field = \"company_id\", branch = \"exact\", empty = \
         \"allow\") names the company field 'company_id', but co.other has no company_id or \
         company_ids field linked to res.company (declared by model definition 'co.other' of \
         addon 'co')",
        |e| matches!(e, CompanyAccessError::NotCompanyScoped { .. }),
    );
}

#[test]
fn direct_names_the_company_field() {
    refused(
        vec![
            company_tree(),
            doc(
                "co.doc",
                vec![FieldDecl::many2many("company_ids", "res.company")],
            )
            .company_access(direct("company_ids", Exact, Deny)),
        ],
        "co.doc: company_access direct(field = \"company_ids\", branch = \"exact\", empty = \
         \"deny\") names the company field 'company_ids', but co.doc belongs to companies \
         through 'company_id' (declared by model definition 'co.doc' of addon 'co')",
        |e| matches!(e, CompanyAccessError::WrongCompanyField { .. }),
    );
    refused(
        vec![
            company_tree(),
            many("co.shared", vec![]).company_access(direct("company_id", Exact, Deny)),
        ],
        "co.shared: company_access direct(field = \"company_id\", branch = \"exact\", empty = \
         \"deny\") names the company field 'company_id', but co.shared belongs to companies \
         through 'company_ids' (declared by model definition 'co.shared' of addon 'co')",
        |e| matches!(e, CompanyAccessError::WrongCompanyField { .. }),
    );
}

#[test]
fn res_company_declares_nothing() {
    refused(
        vec![
            company_tree()
                .company_access(open("the company model"))
                .at("src/company.rs", 4),
        ],
        "res.company: company_access cannot be declared on the company model, which keeps its \
         base access policy (declared by model definition 'res.company' of addon 'co' \
         (src/company.rs:4))",
        |e| matches!(e, CompanyAccessError::OnCompanyModel { .. }),
    );
    refused(
        vec![
            company_tree(),
            ext("co_ext", "res.company").company_access(direct("company_id", Exact, Deny)),
        ],
        "res.company: company_access cannot be declared on the company model, which keeps its \
         base access policy (declared by model definition 'res.company' of addon 'co_ext')",
        |e| matches!(e, CompanyAccessError::OnCompanyModel { .. }),
    );
    // several declarations: refused as the company model before they are composed (a direct form
    // then `unrestricted` would otherwise be reported as widening)
    refused(
        vec![
            company_tree().company_access(direct("company_id", Exact, Deny)),
            ext("co_ext", "res.company").company_access(open("then opened")),
        ],
        "res.company: company_access cannot be declared on the company model, which keeps its \
         base access policy (declared by model definition 'res.company' of addon 'co_ext')",
        |e| matches!(e, CompanyAccessError::OnCompanyModel { .. }),
    );
}

#[test]
fn ancestor_needs_the_company_hierarchy() {
    let message = "co.doc: company_access direct(field = \"company_id\", branch = \"ancestor\", \
                   empty = \"allow\") uses branch \"ancestor\", which needs res.company to keep \
                   its hierarchy (a parent field and _parent_store); use branch \"exact\" or \
                   declare parent_store on res.company (declared by model definition 'co.doc' of \
                   addon 'co')";
    for company in [company_flat(), company_bare()] {
        refused(
            vec![
                company,
                doc("co.doc", vec![]).company_access(direct("company_id", Ancestor, Allow)),
            ],
            message,
            |e| matches!(e, CompanyAccessError::AncestorWithoutHierarchy { .. }),
        );
    }
    // the direct half of a direct-and-parent declaration too
    refused(
        vec![
            company_flat(),
            owner_doc(),
            many("co.rep", vec![edge("doc_id", "co.doc")]).company_access(both(
                "company_ids",
                Ancestor,
                Deny,
                "doc_id",
            )),
        ],
        "co.rep: company_access direct(field = \"company_ids\", branch = \"ancestor\", empty = \
         \"deny\", parent = \"doc_id\") uses branch \"ancestor\", which needs res.company to keep \
         its hierarchy (a parent field and _parent_store); use branch \"exact\" or declare \
         parent_store on res.company (declared by model definition 'co.rep' of addon 'co')",
        |e| matches!(e, CompanyAccessError::AncestorWithoutHierarchy { .. }),
    );
    // `exact` needs no hierarchy; `ancestor` with it resolves
    build(vec![company_bare(), owner_doc()]).unwrap();
    build(vec![
        company_tree(),
        doc("co.doc", vec![]).company_access(direct("company_id", Ancestor, Allow)),
    ])
    .unwrap();
}

#[test]
fn parent_alone_on_a_company_scoped_model_is_refused() {
    refused(
        vec![
            company_tree(),
            owner_doc(),
            doc("co.line", vec![edge("doc_id", "co.doc")]).company_access(parent("doc_id")),
        ],
        "co.line: company_access parent(edge = \"doc_id\") names only the owner 'doc_id', but \
         co.line has its own company field 'company_id': declare its direct restriction too, \
         direct(field = \"company_id\", branch = …, empty = …, parent = \"doc_id\") (declared by \
         model definition 'co.line' of addon 'co')",
        |e| matches!(e, CompanyAccessError::OwnerWithOwnCompany { .. }),
    );
    refused(
        vec![
            company_tree(),
            owner_doc(),
            many("co.line", vec![edge("doc_id", "co.doc")]).company_access(parent("doc_id")),
        ],
        "co.line: company_access parent(edge = \"doc_id\") names only the owner 'doc_id', but \
         co.line has its own company field 'company_ids': declare its direct restriction too, \
         direct(field = \"company_ids\", branch = …, empty = …, parent = \"doc_id\") (declared by \
         model definition 'co.line' of addon 'co')",
        |e| matches!(e, CompanyAccessError::OwnerWithOwnCompany { .. }),
    );
}

#[test]
fn owner_edge_must_exist() {
    refused(
        vec![
            company_tree(),
            owner_doc(),
            m("co.line", vec![FieldDecl::char("name")]).company_access(parent("doc_id")),
        ],
        "co.line: company_access parent(edge = \"doc_id\") names the owner 'doc_id', which is \
         not a field of co.line (declared by model definition 'co.line' of addon 'co')",
        |e| matches!(e, CompanyAccessError::UnknownEdge { .. }),
    );
    // the edge of a direct-and-parent declaration too
    refused(
        vec![
            company_tree(),
            owner_doc(),
            doc("co.rep", vec![]).company_access(both("company_id", Exact, Deny, "tax_id")),
        ],
        "co.rep: company_access direct(field = \"company_id\", branch = \"exact\", empty = \
         \"deny\", parent = \"tax_id\") names the owner 'tax_id', which is not a field of co.rep \
         (declared by model definition 'co.rep' of addon 'co')",
        |e| matches!(e, CompanyAccessError::UnknownEdge { .. }),
    );
}

#[test]
fn owner_edge_must_be_a_many2one() {
    let cases: [(FieldDecl, &str); 4] = [
        (
            FieldDecl::one2many("doc_id", "co.kid", "line_id"),
            "one2many",
        ),
        (FieldDecl::many2many("doc_id", "co.doc"), "many2many"),
        (FieldDecl::char("doc_id").required(true), "char"),
        (
            FieldDecl::reference("doc_id", &[("co.doc", "Document")]),
            "reference",
        ),
    ];
    for (field, ty) in cases {
        refused(
            vec![
                company_tree(),
                owner_doc(),
                m("co.line", vec![field]).company_access(parent("doc_id")),
                m("co.kid", vec![FieldDecl::many2one("line_id", "co.line")]),
            ],
            &format!(
                "co.line: company_access parent(edge = \"doc_id\") names the owner 'doc_id', but \
                 co.line.doc_id is a {ty}, not a many2one (declared by model definition 'co.line' \
                 of addon 'co')"
            ),
            |e| matches!(e, CompanyAccessError::EdgeNotMany2one { .. }),
        );
    }
}

#[test]
fn owner_edge_must_be_stored() {
    let line = |field: FieldDecl| {
        m("co.line", vec![edge("hop_id", "co.hop"), field]).company_access(parent("doc_id"))
    };
    let hop = || m("co.hop", vec![edge("doc_id", "co.doc")]);
    let not_stored = "co.line: company_access parent(edge = \"doc_id\") names the owner \
                      'doc_id', but co.line.doc_id is not stored (declared by model definition \
                      'co.line' of addon 'co')";
    // computed
    refused(
        vec![
            company_tree(),
            owner_doc(),
            hop(),
            line(edge("doc_id", "co.doc").compute("_compute_doc")),
        ],
        not_stored,
        |e| matches!(e, CompanyAccessError::EdgeNotStored { .. }),
    );
    // related
    refused(
        vec![
            company_tree(),
            owner_doc(),
            hop(),
            line(FieldDecl::many2one("doc_id", "co.doc").related("hop_id.doc_id")),
        ],
        not_stored,
        |e| matches!(e, CompanyAccessError::EdgeNotStored { .. }),
    );
    // company-dependent
    refused(
        vec![
            company_tree(),
            owner_doc(),
            hop(),
            line(FieldDecl::many2one("doc_id", "co.doc").company_dependent(true)),
        ],
        "co.line: company_access parent(edge = \"doc_id\") names the owner 'doc_id', but \
         co.line.doc_id is company-dependent (declared by model definition 'co.line' of addon \
         'co')",
        |e| matches!(e, CompanyAccessError::EdgeNotStored { .. }),
    );
    // an `_inherits` copy
    refused(
        vec![
            company_tree(),
            owner_doc(),
            m("co.base", vec![edge("doc_id", "co.doc")]),
            m(
                "co.child",
                vec![edge("base_id", "co.base").ondelete(OnDelete::Cascade)],
            )
            .inherits(leak(vec![("co.base", "base_id")]))
            .company_access(parent("doc_id")),
        ],
        "co.child: company_access parent(edge = \"doc_id\") names the owner 'doc_id', but \
         co.child.doc_id is inherited from co.base through _inherits; declare the restriction on \
         co.base (declared by model definition 'co.child' of addon 'co')",
        |e| matches!(e, CompanyAccessError::EdgeNotStored { .. }),
    );
    // the delegate many2one itself is an ordinary stored, required edge
    let r = build(vec![
        company_tree(),
        owner_doc(),
        m("co.base", vec![edge("doc_id", "co.doc")]).company_access(parent("doc_id")),
        m(
            "co.child",
            vec![edge("base_id", "co.base").ondelete(OnDelete::Cascade)],
        )
        .inherits(leak(vec![("co.base", "base_id")]))
        .company_access(parent("base_id")),
    ])
    .unwrap();
    assert!(matches!(
        r.model("co.child").unwrap().company_access,
        CompanyAccessState::Declared(_)
    ));
}

#[test]
fn owner_edge_must_be_required() {
    refused(
        vec![
            company_tree(),
            owner_doc(),
            m("co.line", vec![FieldDecl::many2one("doc_id", "co.doc")])
                .company_access(parent("doc_id")),
        ],
        "co.line: company_access parent(edge = \"doc_id\") names the owner 'doc_id', but \
         co.line.doc_id is optional: an owner edge is required (required = true), so every \
         record has an owner (declared by model definition 'co.line' of addon 'co')",
        |e| matches!(e, CompanyAccessError::EdgeOptional { .. }),
    );
}

#[test]
fn owner_edge_to_res_company_is_refused() {
    let message = "co.line: company_access parent(edge = \"owner_company_id\") names the owner \
                   'owner_company_id', which links to res.company: restrict by the company field \
                   instead, direct(field = \"company_id\" or \"company_ids\", …) (declared by \
                   model definition 'co.line' of addon 'co')";
    refused(
        vec![
            company_tree(),
            m("co.line", vec![edge("owner_company_id", "res.company")])
                .company_access(parent("owner_company_id")),
        ],
        message,
        |e| matches!(e, CompanyAccessError::EdgeToCompany { .. }),
    );
    // reported before the edge's other faults
    refused(
        vec![
            company_tree(),
            m(
                "co.line",
                vec![FieldDecl::many2one("owner_company_id", "res.company")],
            )
            .company_access(parent("owner_company_id")),
        ],
        message,
        |e| matches!(e, CompanyAccessError::EdgeToCompany { .. }),
    );
}

#[test]
fn owner_chain_must_end_in_a_direct_scope() {
    // an unrestricted owner
    refused(
        vec![
            company_tree(),
            doc("co.doc", vec![]).company_access(open("shared")),
            m("co.line", vec![edge("doc_id", "co.doc")]).company_access(parent("doc_id")),
        ],
        "co.line: company_access parent(edge = \"doc_id\") inherits the company restriction of \
         co.doc through 'doc_id', but co.doc is declared unrestricted: an owner chain must end in \
         a direct company restriction (declared by model definition 'co.line' of addon 'co')",
        |e| matches!(e, CompanyAccessError::OwnerNotRestricted { .. }),
    );
    // a company-scoped owner that declares nothing
    refused(
        vec![
            company_tree(),
            doc("co.doc", vec![]),
            m("co.line", vec![edge("doc_id", "co.doc")]).company_access(parent("doc_id")),
        ],
        "co.line: company_access parent(edge = \"doc_id\") inherits the company restriction of \
         co.doc through 'doc_id', but co.doc declares no company_access: an owner chain must end \
         in a direct company restriction (declared by model definition 'co.line' of addon 'co')",
        |e| matches!(e, CompanyAccessError::OwnerNotRestricted { .. }),
    );
    // a company-less owner without an owner of its own
    refused(
        vec![
            company_tree(),
            m("co.plain", vec![FieldDecl::char("name")]),
            m("co.line", vec![edge("plain_id", "co.plain")]).company_access(parent("plain_id")),
        ],
        "co.line: company_access parent(edge = \"plain_id\") inherits the company restriction of \
         co.plain through 'plain_id', but co.plain has no company field and no owner: an owner \
         chain must end in a direct company restriction (declared by model definition 'co.line' \
         of addon 'co')",
        |e| matches!(e, CompanyAccessError::OwnerNotRestricted { .. }),
    );
    // deeper in the chain: the hop that is wrong is reported, whatever the model order
    refused(
        vec![
            company_tree(),
            m("co.sub", vec![edge("line_id", "co.line")]).company_access(parent("line_id")),
            m("co.line", vec![edge("doc_id", "co.doc")]).company_access(parent("doc_id")),
            doc("co.doc", vec![]).company_access(open("shared")),
        ],
        "co.line: company_access parent(edge = \"doc_id\") inherits the company restriction of \
         co.doc through 'doc_id', but co.doc is declared unrestricted: an owner chain must end in \
         a direct company restriction (declared by model definition 'co.line' of addon 'co')",
        |e| matches!(e, CompanyAccessError::OwnerNotRestricted { .. }),
    );
    // a chain through a direct-and-parent owner still has to end in a direct scope
    refused(
        vec![
            company_tree(),
            doc("co.doc", vec![]).company_access(open("shared")),
            doc("co.rep", vec![edge("doc_id", "co.doc")]).company_access(both(
                "company_id",
                Exact,
                Deny,
                "doc_id",
            )),
            m("co.line", vec![edge("rep_id", "co.rep")]).company_access(parent("rep_id")),
        ],
        "co.rep: company_access direct(field = \"company_id\", branch = \"exact\", empty = \
         \"deny\", parent = \"doc_id\") inherits the company restriction of co.doc through \
         'doc_id', but co.doc is declared unrestricted: an owner chain must end in a direct \
         company restriction (declared by model definition 'co.rep' of addon 'co')",
        |e| matches!(e, CompanyAccessError::OwnerNotRestricted { .. }),
    );
    // an abstract owner: no records, never "declares no company_access" (review F3)
    refused(
        vec![
            company_tree(),
            doc("co.mixin", vec![])
                .kind(ModelKind::Abstract)
                .company_access(direct("company_id", Exact, Deny)),
            m("co.line", vec![edge("mixin_id", "co.mixin")]).company_access(parent("mixin_id")),
        ],
        "co.line: company_access parent(edge = \"mixin_id\") inherits the company restriction of \
         co.mixin through 'mixin_id', but co.mixin is abstract and has no records: an owner chain \
         must end in a direct company restriction (declared by model definition 'co.line' of \
         addon 'co')",
        |e| {
            matches!(
                e,
                CompanyAccessError::OwnerNotRestricted {
                    why: erp_registry::OwnerGap::Abstract,
                    ..
                }
            )
        },
    );
}

#[test]
fn owner_cycles_are_refused() {
    // self
    refused(
        vec![
            company_tree(),
            m("co.a", vec![edge("a_id", "co.a")]).company_access(parent("a_id")),
        ],
        "co.a: the owners of company_access form a cycle: co.a.a_id -> co.a (declared by model \
         definition 'co.a' of addon 'co')",
        |e| matches!(e, CompanyAccessError::OwnerCycle { .. }),
    );
    // two models
    refused(
        vec![
            company_tree(),
            m("co.a", vec![edge("b_id", "co.b")]).company_access(parent("b_id")),
            m("co.b", vec![edge("a_id", "co.a")]).company_access(parent("a_id")),
        ],
        "co.a: the owners of company_access form a cycle: co.a.b_id -> co.b.a_id -> co.a \
         (declared by model definition 'co.a' of addon 'co')",
        |e| matches!(e, CompanyAccessError::OwnerCycle { .. }),
    );
    // three with a tail: the cycle is named from its first model, without the tail
    refused(
        vec![
            company_tree(),
            m("co.t", vec![edge("a_id", "co.a")]).company_access(parent("a_id")),
            m("co.a", vec![edge("b_id", "co.b")]).company_access(parent("b_id")),
            m("co.b", vec![edge("c_id", "co.c")]).company_access(parent("c_id")),
            m("co.c", vec![edge("a_id", "co.a")]).company_access(parent("a_id")),
        ],
        "co.a: the owners of company_access form a cycle: co.a.b_id -> co.b.c_id -> co.c.a_id \
         -> co.a (declared by model definition 'co.a' of addon 'co')",
        |e| matches!(e, CompanyAccessError::OwnerCycle { .. }),
    );
    // through direct-and-parent models: each has a direct half, the owners still loop
    refused(
        vec![
            company_tree(),
            doc("co.x", vec![edge("y_id", "co.y")]).company_access(both(
                "company_id",
                Exact,
                Deny,
                "y_id",
            )),
            doc("co.y", vec![edge("x_id", "co.x")]).company_access(both(
                "company_id",
                Exact,
                Deny,
                "x_id",
            )),
        ],
        "co.x: the owners of company_access form a cycle: co.x.y_id -> co.y.x_id -> co.x \
         (declared by model definition 'co.x' of addon 'co')",
        |e| matches!(e, CompanyAccessError::OwnerCycle { .. }),
    );
}

/// A chain of `edges` owner edges: `<prefix>0 -> <prefix>1 -> … -> <prefix><edges>`, the last
/// model restricted directly.
fn chain(prefix: &str, edges: usize) -> Vec<ModelDecl> {
    let name = |i: usize| leak_str(format!("co.{prefix}{i}"));
    let mut decls = Vec::new();
    for i in 0..edges {
        decls
            .push(m(name(i), vec![edge("next_id", name(i + 1))]).company_access(parent("next_id")));
    }
    decls.push(doc(name(edges), vec![]).company_access(direct("company_id", Exact, Deny)));
    decls
}

#[test]
fn owner_chains_are_bounded() {
    assert_eq!(MAX_OWNER_DEPTH, 8);
    let mut decls = vec![company_tree()];
    decls.extend(chain("ok", MAX_OWNER_DEPTH));
    let r = build(decls).unwrap();
    assert!(matches!(
        r.model("co.ok0").unwrap().company_access,
        CompanyAccessState::Declared(_)
    ));

    let mut decls = vec![company_tree()];
    decls.extend(chain("deep", MAX_OWNER_DEPTH + 1));
    let path = (0..=MAX_OWNER_DEPTH)
        .map(|i| format!("co.deep{i}.next_id"))
        .chain([format!("co.deep{}", MAX_OWNER_DEPTH + 1)])
        .collect::<Vec<_>>()
        .join(" -> ");
    refused(
        decls,
        &format!(
            "co.deep0: the owners of company_access are more than 8 edges deep: {path} (declared \
             by model definition 'co.deep0' of addon 'co')"
        ),
        |e| matches!(e, CompanyAccessError::OwnerChainTooDeep { .. }),
    );
}

#[test]
fn unrestricted_needs_a_company_connection_and_a_reason() {
    refused(
        vec![
            company_tree(),
            m("co.plain", vec![FieldDecl::char("name")]).company_access(open("not needed")),
        ],
        "co.plain: company_access unrestricted(reason = \"not needed\") is declared, but co.plain \
         has neither a company field nor a required stored link to a company-scoped model; no \
         declaration is needed (declared by model definition 'co.plain' of addon 'co')",
        |e| matches!(e, CompanyAccessError::UnrestrictedWithoutCompany { .. }),
    );
    assert_eq!(MAX_UNRESTRICTED_REASON_BYTES, 512);
    let reason_refused = |reason: &'static str, fault: &str| {
        let (kind, got, t) = refusal(vec![
            company_tree(),
            doc("co.doc", vec![]).company_access(open(reason)),
        ]);
        assert_eq!(kind, ErrorKind::Programming, "{got}");
        assert_eq!(
            got,
            format!(
                "co.doc: company_access unrestricted(…) needs a reason that is not blank and at \
                 most 512 bytes long, but it is {fault} (declared by {DOC_DECL})"
            ),
            "{reason:?}"
        );
        assert!(
            matches!(
                t,
                CompanyAccessError::UnrestrictedReason { bytes, .. } if bytes == reason.len()
            ),
            "{t:?}"
        );
    };
    reason_refused("", "blank");
    // blank: only whitespace, Unicode whitespace included (review M1)
    for reason in [" ", "   ", "\t\r\n", "\u{a0}\u{3000}"] {
        reason_refused(reason, "blank");
    }
    reason_refused(leak_str("r".repeat(513)), "513 bytes long");
    // bytes, not characters: 511 ASCII bytes and a two-byte letter
    reason_refused(leak_str(format!("{}é", "r".repeat(511))), "513 bytes long");
    // too long and blank: the length is reported
    reason_refused(leak_str(" ".repeat(513)), "513 bytes long");
    for reason in [
        leak_str("r".repeat(512)),
        "x",
        // text with whitespace around it is not blank
        " x ",
        "Société d’Ébène: visible à toutes les sociétés",
        "line one\r\nline two",
    ] {
        let r = build(vec![
            company_tree(),
            doc("co.doc", vec![]).company_access(open(reason)),
        ])
        .unwrap();
        let restriction = r
            .model("co.doc")
            .unwrap()
            .company_access
            .declared()
            .restriction();
        assert!(
            matches!(restriction, Restriction::Unrestricted { reason: r, .. } if r == reason),
            "{restriction:?}"
        );
    }
    // every declaration of the MRO is checked alone, not only the effective one (review F1): an
    // empty reason narrowed by a later direct form is still refused, naming its own definition
    refused(
        vec![
            company_tree(),
            doc("co.doc", vec![])
                .company_access(open(""))
                .at("src/doc.rs", 2),
            ext("co_ext", "co.doc").company_access(direct("company_id", Exact, Deny)),
        ],
        "co.doc: company_access unrestricted(…) needs a reason that is not blank and at most 512 \
         bytes long, but it is blank (declared by model definition 'co.doc' of addon 'co' \
         (src/doc.rs:2))",
        |e| matches!(e, CompanyAccessError::UnrestrictedReason { .. }),
    );
    // also on a mixin reached through a concrete model: the mixin's own declaration is checked
    // alone on the mixin itself, which comes first in model order (review L1)
    refused(
        vec![
            company_tree(),
            doc("co.mixin", vec![])
                .kind(ModelKind::Abstract)
                .company_access(open(""))
                .at("src/mixin.rs", 1),
            m("co.thing", vec![])
                .inherit(leak(vec!["co.mixin"]))
                .company_access(direct("company_id", Exact, Deny))
                .at("src/thing.rs", 1),
        ],
        "co.mixin: company_access unrestricted(…) needs a reason that is not blank and at most \
         512 bytes long, but it is blank (declared by model definition 'co.mixin' of addon 'co' \
         (src/mixin.rs:1))",
        |e| matches!(e, CompanyAccessError::UnrestrictedReason { .. }),
    );
}

/// The outcome of declaring `old` on `co.doc` and `new` in an extension.
#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    /// Composition accepted it (the effective declaration may still fail its own checks).
    Accepted,
    Widened,
    TwoOwners,
}

fn compose_pair(old: CompanyAccessDecl, new: CompanyAccessDecl) -> Outcome {
    match build(vec![
        company_tree(),
        owner_doc(),
        doc("co.item", vec![edge("doc_id", "co.doc")]).company_access(old),
        ext("co_ext", "co.item").company_access(new),
    ]) {
        Ok(r) => {
            let access = r.model("co.item").unwrap().company_access.declared();
            assert_eq!(
                access.declaration(),
                new,
                "the newest declaration is effective"
            );
            assert_eq!(access.source().addon(), "co_ext");
            Outcome::Accepted
        }
        Err(e) => match typed(&e) {
            CompanyAccessError::Widened { .. } => Outcome::Widened,
            CompanyAccessError::TwoOwners { .. } => Outcome::TwoOwners,
            // a parent alone on this company-scoped model: composition accepted it, the
            // effective declaration's own check refused it
            CompanyAccessError::OwnerWithOwnCompany { .. } => Outcome::Accepted,
            other => panic!("{old:?} then {new:?}: {other}"),
        },
    }
}

/// Q9: a later declaration may only narrow (⊑), the full table of §3.4.
#[test]
fn extensions_only_narrow() {
    let forms: [(&str, CompanyAccessDecl); 10] = [
        ("D(e,d)", direct("company_id", Exact, Deny)),
        ("D(e,a)", direct("company_id", Exact, Allow)),
        ("D(a,d)", direct("company_id", Ancestor, Deny)),
        ("D(a,a)", direct("company_id", Ancestor, Allow)),
        ("P", parent("doc_id")),
        ("DP(e,d)", both("company_id", Exact, Deny, "doc_id")),
        ("DP(e,a)", both("company_id", Exact, Allow, "doc_id")),
        ("DP(a,d)", both("company_id", Ancestor, Deny, "doc_id")),
        ("DP(a,a)", both("company_id", Ancestor, Allow, "doc_id")),
        ("U", open("open")),
    ];
    // (newer, older) pairs composition accepts; every other pair is widening
    let accepted: &[(&str, &str)] = &[
        // anything narrows unrestricted
        ("D(e,d)", "U"),
        ("D(e,a)", "U"),
        ("D(a,d)", "U"),
        ("D(a,a)", "U"),
        ("P", "U"),
        ("DP(e,d)", "U"),
        ("DP(e,a)", "U"),
        ("DP(a,d)", "U"),
        ("DP(a,a)", "U"),
        ("U", "U"),
        // direct: exact ⊑ ancestor, deny ⊑ allow
        ("D(e,d)", "D(e,d)"),
        ("D(e,d)", "D(e,a)"),
        ("D(e,d)", "D(a,d)"),
        ("D(e,d)", "D(a,a)"),
        ("D(e,a)", "D(e,a)"),
        ("D(e,a)", "D(a,a)"),
        ("D(a,d)", "D(a,d)"),
        ("D(a,d)", "D(a,a)"),
        ("D(a,a)", "D(a,a)"),
        // the same owner
        ("P", "P"),
        // adding the other half narrows
        ("DP(e,d)", "D(e,d)"),
        ("DP(e,d)", "D(e,a)"),
        ("DP(e,d)", "D(a,d)"),
        ("DP(e,d)", "D(a,a)"),
        ("DP(e,a)", "D(e,a)"),
        ("DP(e,a)", "D(a,a)"),
        ("DP(a,d)", "D(a,d)"),
        ("DP(a,d)", "D(a,a)"),
        ("DP(a,a)", "D(a,a)"),
        ("DP(e,d)", "P"),
        ("DP(e,a)", "P"),
        ("DP(a,d)", "P"),
        ("DP(a,a)", "P"),
        ("DP(e,d)", "DP(e,d)"),
        ("DP(e,d)", "DP(e,a)"),
        ("DP(e,d)", "DP(a,d)"),
        ("DP(e,d)", "DP(a,a)"),
        ("DP(e,a)", "DP(e,a)"),
        ("DP(e,a)", "DP(a,a)"),
        ("DP(a,d)", "DP(a,d)"),
        ("DP(a,d)", "DP(a,a)"),
        ("DP(a,a)", "DP(a,a)"),
    ];
    for (new_name, new) in &forms {
        for (old_name, old) in &forms {
            let expected = if accepted.contains(&(*new_name, *old_name)) {
                Outcome::Accepted
            } else {
                Outcome::Widened
            };
            assert_eq!(
                compose_pair(*old, *new),
                expected,
                "{new_name} declared after {old_name}"
            );
        }
    }
    // the full message, once
    refused(
        vec![
            company_tree(),
            doc("co.item", vec![])
                .company_access(direct("company_id", Exact, Deny))
                .at("src/item.rs", 3),
            ext("co_ext", "co.item")
                .company_access(direct("company_id", Exact, Allow))
                .at("src/item_ext.rs", 7),
        ],
        "co.item: company_access direct(field = \"company_id\", branch = \"exact\", empty = \
         \"allow\") (declared by model definition 'co.item' of addon 'co_ext' \
         (src/item_ext.rs:7)) is wider than direct(field = \"company_id\", branch = \"exact\", \
         empty = \"deny\") (declared by model definition 'co.item' of addon 'co' \
         (src/item.rs:3)): a later declaration may only narrow a model's company restriction",
        |e| matches!(e, CompanyAccessError::Widened { .. }),
    );
    // three declarations: each is compared with the one before it
    refused(
        vec![
            company_tree(),
            doc("co.item", vec![]).company_access(direct("company_id", Ancestor, Allow)),
            ext("co_ext", "co.item").company_access(direct("company_id", Exact, Deny)),
            ext("co_more", "co.item").company_access(direct("company_id", Ancestor, Deny)),
        ],
        "co.item: company_access direct(field = \"company_id\", branch = \"ancestor\", empty = \
         \"deny\") (declared by model definition 'co.item' of addon 'co_more') is wider than \
         direct(field = \"company_id\", branch = \"exact\", empty = \"deny\") (declared by model \
         definition 'co.item' of addon 'co_ext'): a later declaration may only narrow a model's \
         company restriction",
        |e| matches!(e, CompanyAccessError::Widened { .. }),
    );
    // an extension that says nothing keeps the declaration
    let r = build(vec![
        company_tree(),
        doc("co.item", vec![]).company_access(direct("company_id", Ancestor, Allow)),
        ext("co_ext", "co.item"),
    ])
    .unwrap();
    let access = r.model("co.item").unwrap().company_access.declared();
    assert_eq!(access.source().addon(), "co");
    // a parent alone after a parent or after `unrestricted`, on a company-less model, where the
    // effective declaration also passes its own checks (review F6)
    for old in [parent("doc_id"), open("open")] {
        let r = build(vec![
            company_tree(),
            owner_doc(),
            m("co.item2", vec![edge("doc_id", "co.doc")]).company_access(old),
            ext("co_ext", "co.item2")
                .company_access(parent("doc_id"))
                .at("src/item2_ext.rs", 4),
        ]);
        let access = r
            .unwrap()
            .model("co.item2")
            .unwrap()
            .company_access
            .declared();
        assert_eq!(access.declaration().spelling(), "parent(edge = \"doc_id\")");
        assert_eq!(
            access.source().describe(),
            "model definition 'co.item2' of addon 'co_ext' (src/item2_ext.rs:4)"
        );
    }
}

/// The declarations compose in MRO order, whatever order the builder is given them in (review
/// L3): the addons' load order and, inside an addon, the source location decide.
#[test]
fn composition_ignores_the_input_order() {
    let narrowing = || {
        vec![
            company_tree(),
            doc("co.item", vec![]).company_access(direct("company_id", Ancestor, Allow)),
            ext("co_ext", "co.item").company_access(direct("company_id", Exact, Allow)),
            ext("co_more", "co.item").company_access(direct("company_id", Exact, Deny)),
        ]
    };
    for reverse in [false, true] {
        let mut decls = narrowing();
        if reverse {
            decls.reverse();
        }
        let r = build(decls).unwrap();
        let access = r.model("co.item").unwrap().company_access.declared();
        assert_eq!(
            access.declaration(),
            direct("company_id", Exact, Deny),
            "reversed: {reverse}"
        );
        assert_eq!(access.source().addon(), "co_more", "reversed: {reverse}");
    }
    // a widening found in load order is reported the same way from a reversed input
    let widening = || {
        vec![
            company_tree(),
            doc("co.item", vec![]).company_access(direct("company_id", Exact, Deny)),
            ext("co_ext", "co.item").company_access(direct("company_id", Exact, Allow)),
        ]
    };
    let message = "co.item: company_access direct(field = \"company_id\", branch = \"exact\", \
                   empty = \"allow\") (declared by model definition 'co.item' of addon 'co_ext') \
                   is wider than direct(field = \"company_id\", branch = \"exact\", empty = \
                   \"deny\") (declared by model definition 'co.item' of addon 'co'): a later \
                   declaration may only narrow a model's company restriction";
    refused(widening(), message, |e| {
        matches!(e, CompanyAccessError::Widened { .. })
    });
    let mut reversed = widening();
    reversed.reverse();
    refused(reversed, message, |e| {
        matches!(e, CompanyAccessError::Widened { .. })
    });
}

/// `co.item` declared by `co`, narrowed by `co_ext`, then given the abstract `co.mixin` by
/// `co_more`, which loads after the extender. `mixin_first` is the order of the extension's
/// `inherit`: `["co.mixin", "co.item"]` or `["co.item", "co.mixin"]`.
fn item_with_late_mixin(mixin_access: CompanyAccessDecl, mixin_first: bool) -> Vec<ModelDecl> {
    let inherit = if mixin_first {
        vec!["co.mixin", "co.item"]
    } else {
        vec!["co.item", "co.mixin"]
    };
    vec![
        company_tree(),
        doc("co.item", vec![])
            .company_access(direct("company_id", Ancestor, Allow))
            .at("src/item.rs", 1),
        ext("co_ext", "co.item")
            .company_access(direct("company_id", Exact, Allow))
            .at("src/item_ext.rs", 1),
        // inside `co_more`, the mixin's file sorts before the extension's
        ModelDecl::new("co_more", "co.mixin")
            .kind(ModelKind::Abstract)
            .fields(leak(vec![FieldDecl::many2one("company_id", "res.company")]))
            .company_access(mixin_access)
            .at("src/a_mixin.rs", 1),
        ModelDecl::extension("co_more", "co.item", leak(inherit)).at("src/b_item.rs", 1),
    ]
}

/// A mixin listed before the model in a later `inherit` is the newest declaration and must
/// narrow the extender's; a mixin listed after it composes below the model's own definitions, so
/// it must be at least as wide as the oldest of them and the extender's declaration stays
/// effective.
#[test]
fn a_mixin_from_a_later_addon_composes_at_its_mro_position() {
    let effective = |r: &Registry| {
        let access = r.model("co.item").unwrap().company_access.declared();
        (
            access.declaration(),
            access.source().addon(),
            access.source().definition(),
        )
    };
    // mixin first: it is the newest declaration and wins
    let r = build(item_with_late_mixin(
        direct("company_id", Exact, Deny),
        true,
    ))
    .unwrap();
    assert_eq!(
        effective(&r),
        (direct("company_id", Exact, Deny), "co_more", "co.mixin")
    );
    refused(
        item_with_late_mixin(direct("company_id", Ancestor, Allow), true),
        "co.item: company_access direct(field = \"company_id\", branch = \"ancestor\", empty = \
         \"allow\") (declared by model definition 'co.mixin' of addon 'co_more' (src/a_mixin.rs:1)) \
         is wider than direct(field = \"company_id\", branch = \"exact\", empty = \"allow\") \
         (declared by model definition 'co.item' of addon 'co_ext' (src/item_ext.rs:1)): a later \
         declaration may only narrow a model's company restriction",
        |e| matches!(e, CompanyAccessError::Widened { .. }),
    );
    // mixin last: it composes as the oldest declaration, the extender's stays effective
    for wide in [open("open"), direct("company_id", Ancestor, Allow)] {
        let r = build(item_with_late_mixin(wide, false)).unwrap();
        assert_eq!(
            effective(&r),
            (direct("company_id", Exact, Allow), "co_ext", "co.item"),
            "{wide:?}"
        );
    }
    refused(
        item_with_late_mixin(direct("company_id", Exact, Deny), false),
        "co.item: company_access direct(field = \"company_id\", branch = \"ancestor\", empty = \
         \"allow\") (declared by model definition 'co.item' of addon 'co' (src/item.rs:1)) is \
         wider than direct(field = \"company_id\", branch = \"exact\", empty = \"deny\") \
         (declared by model definition 'co.mixin' of addon 'co_more' (src/a_mixin.rs:1)): a later \
         declaration may only narrow a model's company restriction",
        |e| matches!(e, CompanyAccessError::Widened { .. }),
    );
}

#[test]
fn two_owners_are_refused() {
    let two = |old: CompanyAccessDecl, new: CompanyAccessDecl| {
        vec![
            company_tree(),
            owner_doc(),
            doc(
                "co.item",
                vec![edge("doc_id", "co.doc"), edge("other_id", "co.doc")],
            )
            .company_access(old),
            ext("co_ext", "co.item").company_access(new),
        ]
    };
    let message = "co.item: company_access names the owner 'other_id' (declared by model \
                   definition 'co.item' of addon 'co_ext'), but model definition 'co.item' of \
                   addon 'co' names the owner 'doc_id': a record has one owner";
    for (old, new) in [
        (parent("doc_id"), parent("other_id")),
        (
            both("company_id", Exact, Deny, "doc_id"),
            both("company_id", Exact, Deny, "other_id"),
        ),
        (
            parent("doc_id"),
            both("company_id", Exact, Deny, "other_id"),
        ),
    ] {
        refused(two(old, new), message, |e| {
            matches!(e, CompanyAccessError::TwoOwners { .. })
        });
    }
    assert_eq!(
        compose_pair(parent("doc_id"), parent("doc_id")),
        Outcome::Accepted
    );
}

// ------------------------------------------------------------------------------------------------
// where declarations come from and what acquires one

/// The concrete model of the mixin (declared after it: within an addon, declarations follow their
/// source location).
fn thing() -> ModelDecl {
    m("co.thing", vec![])
        .inherit(leak(vec!["co.mixin"]))
        .at("src/thing.rs", 1)
}

/// the error naming the concrete model and the mixin's definition; only the hierarchy variant
/// and widening are exercised through the mixin)
#[test]
fn abstract_declarations_are_checked_when_concrete() {
    let mixin = || {
        doc("co.mixin", vec![])
            .kind(ModelKind::Abstract)
            .company_access(direct("company_id", Ancestor, Deny))
            .at("src/mixin.rs", 5)
    };
    // valid once concrete: the source is the mixin's definition
    let r = build(vec![company_tree(), mixin(), thing()]).unwrap();
    assert_eq!(
        r.model("co.mixin").unwrap().company_access,
        CompanyAccessState::NotApplicable(NotApplicable::Abstract),
        "abstract models resolve nothing"
    );
    let access = r.model("co.thing").unwrap().company_access.declared();
    assert_eq!(access.source().definition(), "co.mixin");
    assert!(matches!(access.restriction(), Restriction::Direct(_)));
    // an abstract declaration alone is not validated ...
    build(vec![company_flat(), mixin()]).unwrap();
    // ... until a concrete model inherits it: the error names the concrete model and the mixin
    refused(
        vec![company_flat(), mixin(), thing()],
        "co.thing: company_access direct(field = \"company_id\", branch = \"ancestor\", empty = \
         \"deny\") uses branch \"ancestor\", which needs res.company to keep its hierarchy (a \
         parent field and _parent_store); use branch \"exact\" or declare parent_store on \
         res.company (declared by model definition 'co.mixin' of addon 'co' (src/mixin.rs:5))",
        |e| matches!(e, CompanyAccessError::AncestorWithoutHierarchy { .. }),
    );
    // and the concrete model may narrow the mixin's declaration, never widen it
    let r = build(vec![
        company_tree(),
        mixin(),
        thing().company_access(direct("company_id", Exact, Deny)),
    ])
    .unwrap();
    assert_eq!(
        r.model("co.thing")
            .unwrap()
            .company_access
            .declared()
            .source()
            .definition(),
        "co.thing"
    );
    let (_, _, t) = refusal(vec![
        company_tree(),
        mixin(),
        thing().company_access(open("wider")),
    ]);
    assert!(matches!(t, CompanyAccessError::Widened { .. }), "{t:?}");
}

/// An abstract model nothing makes concrete still has every declaration of its MRO checked alone
/// (review L1): a reason the macro would refuse, built by hand or by a model file that skipped
/// `check_limits`, fails the build, naming the abstract definition.
#[test]
fn abstract_declarations_are_checked_alone() {
    for (reason, fault) in [("", "blank"), ("  ", "blank")] {
        refused(
            vec![
                company_tree(),
                doc("co.mixin", vec![])
                    .kind(ModelKind::Abstract)
                    .company_access(open(reason))
                    .at("src/mixin.rs", 5),
            ],
            &format!(
                "co.mixin: company_access unrestricted(…) needs a reason that is not blank and at \
                 most 512 bytes long, but it is {fault} (declared by model definition 'co.mixin' \
                 of addon 'co' (src/mixin.rs:5))"
            ),
            |e| matches!(e, CompanyAccessError::UnrestrictedReason { .. }),
        );
    }
    // also when the bad declaration is an older one of the abstract model's MRO
    refused(
        vec![
            company_tree(),
            doc("co.mixin", vec![])
                .kind(ModelKind::Abstract)
                .company_access(open(leak_str("r".repeat(513))))
                .at("src/mixin.rs", 5),
            ext("co_ext", "co.mixin")
                .kind(ModelKind::Abstract)
                .company_access(direct("company_id", Exact, Deny)),
        ],
        "co.mixin: company_access unrestricted(…) needs a reason that is not blank and at most \
         512 bytes long, but it is 513 bytes long (declared by model definition 'co.mixin' of \
         addon 'co' (src/mixin.rs:5))",
        |e| matches!(e, CompanyAccessError::UnrestrictedReason { .. }),
    );
    // the other checks still wait for a concrete model (A1-D3)
    build(vec![
        company_flat(),
        m("co.mixin", vec![FieldDecl::char("name")])
            .kind(ModelKind::Abstract)
            .company_access(open("checked when concrete")),
    ])
    .unwrap();
}

#[test]
fn transient_models_are_validated() {
    let r = build(vec![
        company_tree(),
        doc("co.wizard", vec![])
            .kind(ModelKind::Transient)
            .company_access(direct("company_id", Exact, Deny)),
    ])
    .unwrap();
    assert!(matches!(
        r.model("co.wizard").unwrap().company_access,
        CompanyAccessState::Declared(_)
    ));
    refused(
        vec![
            company_tree(),
            doc("co.wizard", vec![])
                .kind(ModelKind::Transient)
                .company_access(open("")),
        ],
        "co.wizard: company_access unrestricted(…) needs a reason that is not blank and at most \
         512 bytes long, but it is blank (declared by model definition 'co.wizard' of addon 'co')",
        |e| matches!(e, CompanyAccessError::UnrestrictedReason { .. }),
    );
}

#[test]
fn company_less_models_acquire_nothing() {
    let r = build(vec![
        company_tree(),
        owner_doc(),
        m("co.note", vec![FieldDecl::many2one("doc_id", "co.doc")]),
        m("co.plain", vec![FieldDecl::char("name")]),
    ])
    .unwrap();
    for name in ["co.note", "co.plain"] {
        assert_eq!(
            r.model(name).unwrap().company_access,
            CompanyAccessState::NotApplicable(NotApplicable::CompanyLess)
        );
    }
    assert_eq!(
        r.model("res.company").unwrap().company_access,
        CompanyAccessState::NotApplicable(NotApplicable::CompanyModel)
    );
}

/// SQL-view models do not exist yet)
#[test]
fn every_concrete_company_scoped_model_declares() {
    for kind in [ModelKind::Regular, ModelKind::Transient] {
        let declaration = doc("co.missing", vec![]).kind(kind);
        let source = declaration.describe();
        let err = build(vec![company_tree(), declaration]).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Programming);
        assert_eq!(
            err.message(),
            format!(
                "co.missing: belongs to companies through 'company_id' but declares no company_access; declare direct(field = \"company_id\", branch = \"exact\" or \"ancestor\", empty = \"allow\" or \"deny\"), add parent = \"<owner>\" when another record owns it, or unrestricted(reason = \"…\") (model created by {source})"
            )
        );
        assert!(
            err.source()
                .and_then(|s| s.downcast_ref::<CompanyAccessError>())
                .is_some()
        );
    }
}

#[test]
fn company_less_child_requires_an_explicit_access_decision() {
    let child = m("co.child", vec![edge("doc_id", "co.doc")]);
    let source = child.describe();
    let err = build(vec![company_tree(), owner_doc(), child]).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        format!(
            "co.child: required stored link 'doc_id' points to company-scoped co.doc but declares no company_access; declare parent(edge = \"doc_id\") or unrestricted(reason = \"…\") (model created by {source})"
        )
    );
    assert!(
        err.source()
            .and_then(|s| s.downcast_ref::<CompanyAccessError>())
            .is_some()
    );
}

#[test]
fn company_less_child_can_explicitly_be_unrestricted() {
    let child = m("co.child", vec![edge("doc_id", "co.doc")])
        .company_access(open("fixture is intentionally visible across companies"));
    let r = build(vec![company_tree(), owner_doc(), child]).unwrap();
    assert!(
        matches!(r.model("co.child").unwrap().company_access, CompanyAccessState::Declared(a) if matches!(a.restriction(), Restriction::Unrestricted { .. }))
    );
}

#[test]
fn abstract_company_less_and_explicitly_open_states() {
    let r = build(vec![
        company_tree(),
        doc("co.abstract", vec![]).kind(ModelKind::Abstract),
        m("co.plain", vec![FieldDecl::char("name")]),
        doc("co.open", vec![]).company_access(open("visible to every company")),
    ])
    .unwrap();
    assert_eq!(
        r.model("co.abstract").unwrap().company_access,
        CompanyAccessState::NotApplicable(NotApplicable::Abstract)
    );
    assert_eq!(
        r.model("co.plain").unwrap().company_access,
        CompanyAccessState::NotApplicable(NotApplicable::CompanyLess)
    );
    assert!(matches!(
        r.model("co.open").unwrap().company_access,
        CompanyAccessState::Declared(_)
    ));
}

/// wildcard arm, so a new variant breaks every consumer; review R2 MEDIUM-1)
#[test]
fn company_access_state_is_matched_exhaustively_outside_the_crate() {
    fn label(state: CompanyAccessState) -> &'static str {
        match state {
            CompanyAccessState::Declared(_) => "declared",
            CompanyAccessState::NotApplicable(reason) => match reason {
                NotApplicable::Abstract => "abstract",
                NotApplicable::CompanyModel => "company model",
                NotApplicable::CompanyLess => "company-less",
            },
            CompanyAccessState::Undeclared => "undeclared",
        }
    }
    let r = build(vec![
        company_tree(),
        doc("co.undeclared", vec![]).company_access(open("fixture")),
    ])
    .unwrap();
    assert_eq!(
        label(r.model("co.undeclared").unwrap().company_access),
        "declared"
    );
    assert_eq!(
        label(r.model("res.company").unwrap().company_access),
        "company model"
    );
}

#[test]
fn undeclared_concrete_models_are_refused() {
    for declaration in [
        doc("co.single", vec![]),
        many("co.multiple", vec![]),
        doc("co.wizard", vec![]).kind(ModelKind::Transient),
    ] {
        let name = declaration.name;
        let err = build(vec![company_tree(), declaration]).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Programming);
        assert!(
            matches!(typed(&err), CompanyAccessError::Undeclared { model, .. } if model == name)
        );
    }
}

#[test]
fn bridge_accepts_an_explicit_opt_out_on_a_scoped_child() {
    let child = m("co.child", vec![edge("doc_id", "co.doc")])
        .company_access(open("the child is intentionally visible across companies"));
    let registry = build(vec![company_tree(), owner_doc(), child]).unwrap();
    assert!(matches!(
        registry.model("co.child").unwrap().company_access,
        CompanyAccessState::Declared(access)
            if matches!(access.restriction(), Restriction::Unrestricted { .. })
    ));
}

/// messages naming its file and line; sampled variants: hierarchy, reason, owner on a scoped model)
#[test]
fn runtime_overlays_use_the_same_validation() {
    let def = |access: CompanyAccessDef| {
        let mut d = ModelDef::new("co_ext", "co.rt")
            .field(FieldDef::new(
                "company_id",
                KindDef::Many2one {
                    comodel: "res.company".to_owned(),
                    ondelete: None,
                },
            ))
            .field(
                FieldDef::new(
                    "doc_id",
                    KindDef::Many2one {
                        comodel: "co.doc".to_owned(),
                        ondelete: None,
                    },
                )
                .required(true),
            )
            .company_access(access);
        d.file = "models/rt.toml".to_owned();
        d.line = 3;
        d
    };
    let build_with = |access: CompanyAccessDef| {
        let mut overlay = Overlay::new();
        overlay.add_addon_defs([def(access)])?;
        let decls: Vec<&'static ModelDecl> = vec![
            Box::leak(Box::new(company_flat())),
            Box::leak(Box::new(owner_doc())),
        ];
        RegistryBuilder::new([&CO, &CO_EXT])
            .linked(false)
            .with_decls(decls)
            .overlay(&overlay)
            .build()
    };
    // valid
    let r = build_with(CompanyAccessDef::direct_and_parent(
        "company_id",
        Exact,
        Allow,
        "doc_id",
    ))
    .unwrap();
    let rt = r.model("co.rt").unwrap();
    let access = rt.company_access.declared();
    assert_eq!(
        access.declaration(),
        both("company_id", Exact, Allow, "doc_id")
    );
    assert_eq!(
        (access.source().file(), access.source().line()),
        ("models/rt.toml", 3)
    );
    // the same refusals, the definition named with its file and line
    let err = build_with(CompanyAccessDef::direct("company_id", Ancestor, Deny)).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "co.rt: company_access direct(field = \"company_id\", branch = \"ancestor\", empty = \
         \"deny\") uses branch \"ancestor\", which needs res.company to keep its hierarchy (a \
         parent field and _parent_store); use branch \"exact\" or declare parent_store on \
         res.company (declared by model definition 'co.rt' of addon 'co_ext' \
         (models/rt.toml:3))"
    );
    assert!(matches!(
        typed(&err),
        CompanyAccessError::AncestorWithoutHierarchy { .. }
    ));
    // a definition alone is refused before the build, with the macro's words
    let err = build_with(CompanyAccessDef::unrestricted("")).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.message(),
        "model 'co.rt': company_access: unrestricted(…) needs a reason that is not blank and at \
         most 512 bytes long, but it is blank"
    );
    let err = build_with(CompanyAccessDef::parent("doc_id")).unwrap_err();
    assert!(matches!(
        typed(&err),
        CompanyAccessError::OwnerWithOwnCompany { .. }
    ));
}

/// Owned declarations are bounded like every runtime definition.
///
/// whitespace-only and 513-byte reasons refused, 512 bytes and a non-ASCII reason with CRLF
/// accepted, checked before the registry is built; plus the name limits of model files)
#[test]
fn runtime_declarations_are_bounded() {
    let long = "x".repeat(erp_registry::MAX_NAME_BYTES + 1);
    let err = ModelDef::new("co", "co.rt")
        .company_access(CompanyAccessDef::parent(long.clone()))
        .check_limits()
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.message(),
        "model 'co.rt': company_access edge name is 129 bytes long, more than the limit of 128"
    );
    let err = ModelDef::new("co", "co.rt")
        .company_access(CompanyAccessDef::direct(long, Exact, Deny))
        .check_limits()
        .unwrap_err();
    assert_eq!(
        err.message(),
        "model 'co.rt': company_access field name is 129 bytes long, more than the limit of 128"
    );
    let refused_def = |access: CompanyAccessDef| {
        let err = ModelDef::new("co", "co.rt")
            .company_access(access)
            .check_limits()
            .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Validation);
        err.message().to_owned()
    };
    assert_eq!(
        refused_def(CompanyAccessDef::unrestricted("r".repeat(513))),
        "model 'co.rt': company_access: unrestricted(…) needs a reason that is not blank and at \
         most 512 bytes long, but it is 513 bytes long"
    );
    for blank in ["", " ", "\t\r\n", "\u{a0}\u{3000}"] {
        assert_eq!(
            refused_def(CompanyAccessDef::unrestricted(blank)),
            "model 'co.rt': company_access: unrestricted(…) needs a reason that is not blank \
             and at most 512 bytes long, but it is blank",
            "{blank:?}"
        );
    }
    assert_eq!(
        refused_def(CompanyAccessDef::direct("", Exact, Deny)),
        "model 'co.rt': company_access: the field name is empty"
    );
    assert_eq!(
        refused_def(CompanyAccessDef::parent("")),
        "model 'co.rt': company_access: the edge name is empty"
    );
    assert_eq!(
        refused_def(CompanyAccessDef::direct_and_parent(
            "company_id",
            Exact,
            Deny,
            ""
        )),
        "model 'co.rt': company_access: the edge name is empty"
    );
    // 512 bytes, a non-ASCII reason with CRLF and text with whitespace around it are accepted
    for reason in [
        "r".repeat(512),
        "Société\r\nligne".to_owned(),
        " x ".to_owned(),
    ] {
        ModelDef::new("co", "co.rt")
            .company_access(CompanyAccessDef::unrestricted(reason))
            .check_limits()
            .unwrap();
    }
    // equal definitions intern to one declaration
    let def = ModelDef::new("co", "co.rt").company_access(CompanyAccessDef::unrestricted("same"));
    assert!(std::ptr::eq(
        def.to_decl().unwrap(),
        def.clone().to_decl().unwrap()
    ));
    assert_eq!(def.to_decl().unwrap().company_access, Some(open("same")));
}

/// Every successfully built registry has no company access gaps.
#[test]
fn company_access_gaps_are_empty_after_validation() {
    let r = build(vec![
        company_tree(),
        owner_doc(),
        doc("co.open", vec![]).company_access(open("reason")),
        m("co.plain", vec![FieldDecl::char("name")]),
        m("co.line", vec![edge("doc_id", "co.doc")]).company_access(parent("doc_id")),
    ])
    .unwrap();
    assert_eq!(r.company_access_gaps().count(), 0);
    assert_eq!(build(vec![]).unwrap().company_access_gaps().count(), 0);
}

/// `company_access` is a separate contract: scope, match and checks do not change with it.
#[test]
fn declarations_leave_company_match_and_checks_unchanged() {
    let models = |declare: bool| {
        let with = |d: ModelDecl, access: CompanyAccessDecl| {
            if declare {
                d.company_access(access)
            } else {
                d.company_access(open("comparison fixture"))
            }
        };
        vec![
            company_tree(),
            with(
                doc("co.doc", vec![FieldDecl::many2one("tag_id", "co.tag")])
                    .company_match(erp_registry::CompanyMatch::ParentOf),
                direct("company_id", Ancestor, Allow),
            ),
            with(
                doc("co.tag", vec![]).check_company_auto(false),
                direct("company_id", Exact, Deny),
            ),
            with(
                many(
                    "co.shared",
                    vec![FieldDecl::many2one("doc_id", "co.doc").required(true)],
                ),
                both("company_ids", Exact, Allow, "doc_id"),
            ),
        ]
    };
    let plain = build(models(false)).unwrap();
    let declared = build(models(true)).unwrap();
    for (a, b) in plain.models().iter().zip(declared.models()) {
        assert_eq!(a.name, b.name);
        assert_eq!(a.company_scope, b.company_scope, "{}", a.name);
        assert_eq!(a.check_company_auto, b.check_company_auto, "{}", a.name);
        let checks = |m: &erp_registry::ModelSpec| {
            m.fields
                .iter()
                .map(|f| (f.name, f.check_company))
                .collect::<Vec<_>>()
        };
        assert_eq!(checks(a), checks(b), "{}", a.name);
    }
    assert!(
        declared
            .models()
            .iter()
            .filter(|m| m.name.starts_with("co."))
            .all(|m| matches!(m.company_access, CompanyAccessState::Declared(_)))
    );
}

// ------------------------------------------------------------------------------------------------
// escape: `direct(…, escape = "<stored boolean>")`

fn escaping(
    field: &'static str,
    branch: CompanyBranch,
    empty: EmptyCompany,
    escape: &'static str,
) -> CompanyAccessDecl {
    CompanyAccessDecl::direct_escaping(field, branch, empty, escape)
}

/// A company-scoped model with a stored required `share` boolean (what `res.users` has).
fn escapable(extra: Vec<FieldDecl>) -> ModelDecl {
    let mut fields = vec![FieldDecl::boolean("share").required(true).store(true)];
    fields.extend(extra);
    doc("co.doc", fields)
}

const ESCAPE_SPELLING: &str =
    r#"direct(field = "company_id", branch = "exact", empty = "deny", escape = "share")"#;

/// install check belong to the generator tests)
#[test]
fn escape_resolves_to_a_stored_required_boolean() {
    let r = build(vec![
        company_flat(),
        escapable(vec![]).company_access(escaping("company_id", Exact, Deny, "share")),
    ])
    .unwrap();
    let spec = r.model("co.doc").unwrap();
    let access = spec.company_access.declared();
    let Restriction::Direct(d) = access.restriction() else {
        panic!("{:?}", access.restriction());
    };
    assert_eq!(d.escape(), Some(spec.field_id("share").unwrap()));
    assert_eq!(access.declaration().spelling(), ESCAPE_SPELLING);
    // the escape is a read input of the rule, so it is an anchor next to the company field
    assert_eq!(
        access.anchors().collect::<Vec<_>>(),
        vec![
            spec.field_id("company_id").unwrap(),
            spec.field_id("share").unwrap()
        ]
    );
    // a plain direct declaration has no escape
    let plain = build(vec![company_flat(), owner_doc()]).unwrap();
    let Restriction::Direct(p) = plain
        .model("co.doc")
        .unwrap()
        .company_access
        .declared()
        .restriction()
    else {
        panic!()
    };
    assert_eq!(p.escape(), None);
}

#[test]
fn escape_accepts_a_stored_compute() {
    build(vec![
        company_flat(),
        doc(
            "co.doc",
            vec![
                FieldDecl::boolean("share")
                    .required(true)
                    .store(true)
                    .compute("compute_share"),
            ],
        )
        .company_access(escaping("company_id", Exact, Deny, "share")),
    ])
    .unwrap();
}

#[test]
fn escape_field_refusals() {
    let bad_escape = |e: &CompanyAccessError| matches!(e, CompanyAccessError::BadEscape { .. });
    let cases: Vec<(Vec<FieldDecl>, &str)> = vec![
        (
            vec![],
            "co.doc: company_access direct(field = \"company_id\", branch = \"exact\", empty = \
             \"deny\", escape = \"share\") names the escape 'share', but co.doc has no field \
             'share' (declared by model definition 'co.doc' of addon 'co')",
        ),
        (
            vec![FieldDecl::char("share").required(true).store(true)],
            "co.doc: company_access direct(field = \"company_id\", branch = \"exact\", empty = \
             \"deny\", escape = \"share\") names the escape 'share', but co.doc.share is a char \
             field: an escape must be a boolean (declared by model definition 'co.doc' of addon \
             'co')",
        ),
        (
            vec![
                FieldDecl::boolean("share")
                    .required(true)
                    .compute("compute_share"),
            ],
            "co.doc: company_access direct(field = \"company_id\", branch = \"exact\", empty = \
             \"deny\", escape = \"share\") names the escape 'share', but co.doc.share has no \
             column of its own (computed or related without store, company_dependent, or \
             inherited): the rule runs in SQL against the column (declared by model definition \
             'co.doc' of addon 'co')",
        ),
        (
            vec![FieldDecl::boolean("share").store(true)],
            "co.doc: company_access direct(field = \"company_id\", branch = \"exact\", empty = \
             \"deny\", escape = \"share\") names the escape 'share', but co.doc.share is \
             optional: an escape must be required (required = true), because a negative \
             boolean test also matches a NULL column and would admit the row (declared by model \
             definition 'co.doc' of addon 'co')",
        ),
    ];
    for (fields, message) in cases {
        refused(
            vec![
                company_flat(),
                doc("co.doc", fields).company_access(escaping("company_id", Exact, Deny, "share")),
            ],
            message,
            bad_escape,
        );
    }
}

/// but never add or change it)
#[test]
fn escape_composes_by_narrowing_only() {
    let base = |access: CompanyAccessDecl| {
        escapable(vec![FieldDecl::boolean("other").required(true).store(true)])
            .company_access(access)
    };
    // dropping the escape narrows
    let r = build(vec![
        company_flat(),
        base(escaping("company_id", Exact, Deny, "share")),
        ext("co_ext", "co.doc").company_access(direct("company_id", Exact, Deny)),
    ])
    .unwrap();
    let access = r.model("co.doc").unwrap().company_access.declared();
    assert_eq!(access.declaration(), direct("company_id", Exact, Deny));
    // keeping the same escape narrows the other modes
    build(vec![
        company_flat(),
        base(escaping("company_id", Exact, Allow, "share")),
        ext("co_ext", "co.doc").company_access(escaping("company_id", Exact, Deny, "share")),
    ])
    .unwrap();
    let widened = |e: &CompanyAccessError| matches!(e, CompanyAccessError::Widened { .. });
    // adding an escape widens
    refused(
        vec![
            company_flat(),
            base(direct("company_id", Exact, Deny)),
            ext("co_ext", "co.doc").company_access(escaping("company_id", Exact, Deny, "share")),
        ],
        "co.doc: company_access direct(field = \"company_id\", branch = \"exact\", empty = \
         \"deny\", escape = \"share\") (declared by model definition 'co.doc' of addon 'co_ext') \
         is wider than direct(field = \"company_id\", branch = \"exact\", empty = \"deny\") \
         (declared by model definition 'co.doc' of addon 'co'): a later declaration may only \
         narrow a model's company restriction",
        widened,
    );
    // changing the escape widens
    refused(
        vec![
            company_flat(),
            base(escaping("company_id", Exact, Deny, "share")),
            ext("co_ext", "co.doc").company_access(escaping("company_id", Exact, Deny, "other")),
        ],
        "co.doc: company_access direct(field = \"company_id\", branch = \"exact\", empty = \
         \"deny\", escape = \"other\") (declared by model definition 'co.doc' of addon 'co_ext') \
         is wider than direct(field = \"company_id\", branch = \"exact\", empty = \"deny\", \
         escape = \"share\") (declared by model definition 'co.doc' of addon 'co'): a later \
         declaration may only narrow a model's company restriction",
        widened,
    );
}
