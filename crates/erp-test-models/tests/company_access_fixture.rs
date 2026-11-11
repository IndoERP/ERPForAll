//! The fixture "company access" composes and resolves
//! every declared form (A1-D2; design). Pure: no
//! database. The later sub-slices generate and enforce restrictions on these models.
#![allow(clippy::unwrap_used, clippy::panic, reason = "tests")]

use erp_registry::{
    CompanyAccessState, CompanyBranch, CompanyCardinality, EmptyCompany, NotApplicable, Registry,
    RegistryBuilder, Restriction,
};
use erp_test_models::test_company_access;

use CompanyBranch::{Ancestor, Exact};
use CompanyCardinality::{Many, Single};
use EmptyCompany::{Allow, Deny};

const PREFIX: &str = "test_company_access.";

fn registry() -> Registry {
    RegistryBuilder::new([&test_company_access::ADDON])
        .build()
        .unwrap()
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

fn model(r: &Registry, short: &str) -> &'static str {
    r.model(&format!("{PREFIX}{short}")).unwrap().name
}

/// `(field name, cardinality, branch, empty)` of a direct restriction.
fn direct_of(
    r: &Registry,
    d: erp_registry::DirectRestriction,
    model: &str,
) -> (
    &'static str,
    CompanyCardinality,
    CompanyBranch,
    EmptyCompany,
) {
    let spec = r.model(model).unwrap();
    (
        spec.field_by_id(d.field()).name,
        d.cardinality(),
        d.branch(),
        d.empty(),
    )
}

/// `(edge name, owner model)` of an owner restriction.
fn owner_of(
    r: &Registry,
    o: erp_registry::OwnerRestriction,
    model: &str,
) -> (&'static str, &'static str) {
    let spec = r.model(model).unwrap();
    (
        spec.field_by_id(o.edge()).name,
        r.model_by_id(o.owner()).name,
    )
}

/// two levels, direct-and-owner, transient, unrestricted and mixin build; `N` and `res.company`
/// acquire nothing)
#[test]
fn the_fixture_resolves_every_form() {
    let r = registry();
    let forms = [
        ("m1", ("company_id", Single, Exact, Deny)),
        ("m2", ("company_id", Single, Ancestor, Deny)),
        ("m3", ("company_id", Single, Ancestor, Allow)),
        ("m4", ("company_id", Single, Exact, Allow)),
        ("m5", ("company_ids", Many, Ancestor, Deny)),
        ("m6", ("company_ids", Many, Exact, Deny)),
        ("m7", ("company_ids", Many, Exact, Allow)),
        ("m8", ("company_ids", Many, Ancestor, Allow)),
        ("t", ("company_id", Single, Exact, Deny)),
        ("xc", ("company_id", Single, Exact, Deny)),
    ];
    for (short, expected) in forms {
        let name = model(&r, short);
        let access = r.model(name).unwrap().company_access.declared();
        let Restriction::Direct(d) = access.restriction() else {
            panic!("{name}: {:?}", access.restriction());
        };
        assert_eq!(direct_of(&r, d, name), expected, "{name}");
        assert_eq!(access.source().addon(), "test_company_access");
    }
    // XC takes the mixin's declaration
    let xc = r.model(model(&r, "xc")).unwrap().company_access.declared();
    assert_eq!(xc.source().definition(), "test_company_access.x");
    assert!(xc.source().file().ends_with("test_company_access.rs"));

    let owners = [
        ("l", ("doc_id", "test_company_access.m1")),
        ("l2", ("line_id", "test_company_access.l")),
    ];
    for (short, expected) in owners {
        let name = model(&r, short);
        let Restriction::Owner(o) = r
            .model(name)
            .unwrap()
            .company_access
            .declared()
            .restriction()
        else {
            panic!("{name}");
        };
        assert_eq!(owner_of(&r, o, name), expected, "{name}");
    }
    let p = model(&r, "p");
    let Restriction::DirectAndOwner(d, o) =
        r.model(p).unwrap().company_access.declared().restriction()
    else {
        panic!("{p}");
    };
    assert_eq!(direct_of(&r, d, p), ("company_id", Single, Ancestor, Allow));
    assert_eq!(owner_of(&r, o, p), ("owner_id", "test_company_access.m2"));

    let u = r.model(model(&r, "u")).unwrap().company_access.declared();
    assert!(matches!(u.restriction(), Restriction::Unrestricted { .. }));
    assert!(!u.generates());

    // `n` explicitly opts out; `x` is abstract and `res.company` is the company model.
    assert!(
        matches!(r.model("test_company_access.n").unwrap().company_access,
        CompanyAccessState::Declared(a) if matches!(a.restriction(), Restriction::Unrestricted { .. }))
    );
    for (name, reason) in [
        ("test_company_access.x", NotApplicable::Abstract),
        ("res.company", NotApplicable::CompanyModel),
    ] {
        assert_eq!(
            r.model(name).unwrap().company_access,
            CompanyAccessState::NotApplicable(reason),
            "{name}"
        );
    }
    assert_eq!(
        r.company_access_gaps().count(),
        0,
        "every scoped fixture model declares"
    );
}
