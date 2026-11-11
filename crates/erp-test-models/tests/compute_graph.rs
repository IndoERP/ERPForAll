//! Dependency graph errors at ORM assembly (COMP §4.3).
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

use erp_registry::{AddonDescriptor, FieldDecl, ModelDecl, RegistryBuilder};
use erp_sdk::__private::{BatchData, ComputeEntry, ComputeImpl, ComputeSuper, ImplKind};
use erp_sdk::{ErrorKind, Orm, StaticPolicy};
use erp_testkit::Harness;

static T: AddonDescriptor = AddonDescriptor {
    name: "test_graph",
    depends: &["base"],
    exact_only: false,
};

fn noop(_: &mut BatchData, _: ComputeSuper) -> erp_sdk::Result<()> {
    Ok(())
}

fn imp(method: &'static str, depends: &'static [&'static str]) -> &'static ComputeImpl {
    Box::leak(Box::new(ComputeImpl {
        addon: "test_graph",
        model: "x.m",
        method,
        kind: ImplKind::Define,
        depends,
        reads: &[],
        depends_context: &[],
        entry: ComputeEntry::Sync(noop),
        file: file!(),
        line: line!(),
    }))
}

async fn build(
    h: &Harness,
    fields: Vec<FieldDecl>,
    impls: Vec<&'static ComputeImpl>,
) -> Result<(), (ErrorKind, String)> {
    let fields: &'static [FieldDecl] = Box::leak(fields.into_boxed_slice());
    let decl: &'static ModelDecl =
        Box::leak(Box::new(ModelDecl::new("test_graph", "x.m").fields(fields)));
    let registry = RegistryBuilder::new([&T])
        .with_decls([decl])
        .build()
        .map_err(|e| (e.kind(), e.message().to_owned()))?;
    Orm::builder(std::sync::Arc::new(registry), h.db().pool().clone())
        .policy(StaticPolicy::AllowAll)
        .computes(impls)
        .build()
        .map(|_| ())
        .map_err(|e| (e.kind(), e.message().to_owned()))
}

#[tokio::test]
async fn comp_001_and_dependency_graph_errors() {
    let h = Harness::builder([&T])
        .without_schema()
        .build()
        .await
        .unwrap();
    let x = || FieldDecl::char("x").compute("_compute_x");
    let (kind, msg) = build(&h, vec![x()], vec![imp("_compute_x", &["id"])])
        .await
        .unwrap_err();
    assert_eq!(kind, ErrorKind::Programming);
    assert_eq!(msg, "Compute method cannot depend on field 'id'.");
    // a relational path is accepted (S4b); a path through a non-relational field is not
    build(
        &h,
        vec![FieldDecl::many2one("user_id", "res.users"), x()],
        vec![imp("_compute_x", &["user_id.name"])],
    )
    .await
    .unwrap();
    let (_, msg) = build(
        &h,
        vec![FieldDecl::char("y"), x()],
        vec![imp("_compute_x", &["y.name"])],
    )
    .await
    .unwrap_err();
    assert!(
        msg.contains("goes through x.m.y, which is not a many2one, one2many or many2many field"),
        "{msg}"
    );
    let (_, msg) = build(
        &h,
        vec![FieldDecl::many2one("user_id", "res.users"), x()],
        vec![imp("_compute_x", &["user_id.nope"])],
    )
    .await
    .unwrap_err();
    assert_eq!(
        msg,
        "Wrong @depends on '_compute_x' (compute method of field x.m.x). Dependency field \
         'nope' not found in model res.users."
    );
    let (_, msg) = build(&h, vec![x()], vec![imp("_compute_x", &["nope"])])
        .await
        .unwrap_err();
    assert_eq!(
        msg,
        "Wrong @depends on '_compute_x' (compute method of field x.m.x). Dependency field \
         'nope' not found in model x.m."
    );
    // a cycle
    let (_, msg) = build(
        &h,
        vec![
            FieldDecl::char("a").compute("_compute_a"),
            FieldDecl::char("b").compute("_compute_b"),
        ],
        vec![imp("_compute_a", &["b"]), imp("_compute_b", &["a"])],
    )
    .await
    .unwrap_err();
    assert!(msg.contains("depend on each other in a cycle"), "{msg}");
    // review item 16: the message names an actual cycle
    assert!(
        msg.ends_with("a -> b -> a") || msg.ends_with("b -> a -> b"),
        "{msg}"
    );
    // review item 7: a stored field cannot depend on the context
    let (_, msg) = build(
        &h,
        vec![
            FieldDecl::char("x")
                .compute("_compute_x")
                .store(true)
                .depends_context(&["lang"]),
        ],
        vec![imp("_compute_x", &[])],
    )
    .await
    .unwrap_err();
    assert!(
        msg.contains("is stored but depends on the context"),
        "{msg}"
    );
    // no implementation of the compute method
    let (_, msg) = build(&h, vec![x()], vec![]).await.unwrap_err();
    assert!(
        msg.contains("compute method '_compute_x' not found"),
        "{msg}"
    );
    // an implementation no field uses
    let (_, msg) = build(&h, vec![FieldDecl::char("y")], vec![imp("_compute_x", &[])])
        .await
        .unwrap_err();
    assert!(
        msg.contains("is not the compute method of any field"),
        "{msg}"
    );
    // a related field through a relation is accepted; through a scalar field it is not
    build(
        &h,
        vec![
            FieldDecl::many2one("user_id", "res.users"),
            FieldDecl::char("user_name").related("user_id.name"),
        ],
        vec![],
    )
    .await
    .unwrap();
    let (_, msg) = build(
        &h,
        vec![
            FieldDecl::char("y"),
            FieldDecl::char("user_name").related("y.name"),
        ],
        vec![],
    )
    .await
    .unwrap_err();
    // the registry refuses the path before the ORM builds its graph
    assert_eq!(
        msg,
        "related field x.m.user_name (related='y.name'): the path goes through x.m.y, which is \
         not a relation field"
    );
    build(&h, vec![x()], vec![imp("_compute_x", &["x"])])
        .await
        .unwrap();
    h.cleanup().await.unwrap();
}

/// Review items 1 and 12: a dependency path must let the ORM find the records to recompute:
/// through a non-stored related many2one (searched) yes; through a non-stored computed field
/// that is not related, or a many2one with `index=False`, no (refused at assembly).
#[tokio::test]
async fn review_untraceable_dependency_paths_are_refused() {
    let h = Harness::builder([&T])
        .without_schema()
        .build()
        .await
        .unwrap();
    let x = || FieldDecl::char("x").compute("_compute_x");
    build(
        &h,
        vec![
            FieldDecl::many2one("user_id", "res.users"),
            FieldDecl::many2one("rel_user_id", "res.users").related("user_id"),
            x(),
        ],
        vec![imp("_compute_x", &["rel_user_id.name"])],
    )
    .await
    .unwrap();
    let (kind, msg) = build(
        &h,
        vec![
            FieldDecl::many2one("calc_user_id", "res.users").compute("_compute_calc"),
            x(),
        ],
        vec![
            imp("_compute_calc", &[]),
            imp("_compute_x", &["calc_user_id.name"]),
        ],
    )
    .await
    .unwrap_err();
    assert_eq!(kind, ErrorKind::Programming);
    assert!(msg.contains("neither stored nor related"), "{msg}");
    let (kind, msg) = build(
        &h,
        vec![FieldDecl::many2one("user_id", "res.users").index(None), x()],
        vec![imp("_compute_x", &["user_id.name"])],
    )
    .await
    .unwrap_err();
    assert_eq!(kind, ErrorKind::Programming);
    assert!(msg.contains("index = false"), "{msg}");
    h.cleanup().await.unwrap();
}

/// computes after the INSERT is refused at ORM assembly, naming the field and the first blocking
/// input; through a many2one the declaration builds. Fails on the code before PC-1, where every
/// one of these declarations was dropped silently and the ORM built.
#[tokio::test]
async fn spec_comp_049_declared_precompute_the_graph_cannot_honour_is_refused() {
    let h = Harness::builder([&T])
        .without_schema()
        .build()
        .await
        .unwrap();
    let declared = |name: &'static str, method: &'static str| {
        FieldDecl::char(name)
            .compute(method)
            .store(true)
            .precompute(true)
    };
    let refused = |field: &str, reason: &str| {
        format!(
            "Field x.m.{field} declares precompute = true, but it cannot be computed before the \
             INSERT: {reason}"
        )
    };
    // a direct input opted out with `precompute = false`
    let (kind, msg) = build(
        &h,
        vec![
            FieldDecl::char("a"),
            FieldDecl::char("b")
                .compute("_compute_b")
                .store(true)
                .precompute(false),
            declared("c", "_compute_c"),
        ],
        vec![imp("_compute_b", &["a"]), imp("_compute_c", &["b"])],
    )
    .await
    .unwrap_err();
    assert_eq!(kind, ErrorKind::Programming);
    assert_eq!(
        msg,
        refused(
            "c",
            "it reads x.m.b, which is computed after the INSERT; x.m.b is declared \
             precompute = false"
        )
    );
    // the opt-out two levels down, through an undeclared stored compute
    let (_, msg) = build(
        &h,
        vec![
            FieldDecl::char("a")
                .compute("_compute_a")
                .store(true)
                .precompute(false),
            FieldDecl::char("b").compute("_compute_b").store(true),
            declared("c", "_compute_c"),
        ],
        vec![
            imp("_compute_a", &[]),
            imp("_compute_b", &["a"]),
            imp("_compute_c", &["b"]),
        ],
    )
    .await
    .unwrap_err();
    assert_eq!(
        msg,
        refused(
            "c",
            "it reads x.m.b, which is computed after the INSERT; x.m.b reads x.m.a, which is \
             computed after the INSERT; x.m.a is declared precompute = false"
        )
    );
    // an input through a one2many: the lines are created after the INSERT
    let (kind, msg) = build(
        &h,
        vec![
            FieldDecl::char("name"),
            FieldDecl::many2one("parent_id", "x.m"),
            FieldDecl::one2many("child_ids", "x.m", "parent_id"),
            declared("c", "_compute_c"),
        ],
        vec![imp("_compute_c", &["child_ids.name"])],
    )
    .await
    .unwrap_err();
    assert_eq!(kind, ErrorKind::Programming);
    assert_eq!(
        msg,
        refused(
            "c",
            "it reads child_ids.name through the one2many x.m.child_ids, whose lines are \
             created after the INSERT; precompute over one2many lines is not supported until \
             PERF.PRECOMPUTE-X2M"
        )
    );
    // an input through a many2many
    let (_, msg) = build(
        &h,
        vec![
            FieldDecl::many2many("user_ids", "res.users"),
            declared("c", "_compute_c"),
        ],
        vec![imp("_compute_c", &["user_ids.name"])],
    )
    .await
    .unwrap_err();
    assert_eq!(
        msg,
        refused(
            "c",
            "it reads user_ids.name through the many2many x.m.user_ids; only many2one steps \
             have their targets before the INSERT"
        )
    );
    // `display_name` is computed on every read
    let (_, msg) = build(
        &h,
        vec![declared("c", "_compute_c")],
        vec![imp("_compute_c", &["display_name"])],
    )
    .await
    .unwrap_err();
    assert_eq!(
        msg,
        refused(
            "c",
            "it reads x.m.display_name, which is computed after the INSERT; \
             x.m.display_name is computed from the record name on every read"
        )
    );
    // a field of the same compute method opts out: the group decides once
    let (_, msg) = build(
        &h,
        vec![
            declared("g1", "_compute_g"),
            FieldDecl::char("g2")
                .compute("_compute_g")
                .store(true)
                .precompute(false),
        ],
        vec![imp("_compute_g", &[])],
    )
    .await
    .unwrap_err();
    assert_eq!(
        msg,
        refused(
            "g1",
            "it shares its compute method with x.m.g2, which is declared precompute = false"
        )
    );
    // a stored related field through a one2many keeps its declaration too
    let (_, msg) = build(
        &h,
        vec![
            FieldDecl::char("name"),
            FieldDecl::many2one("parent_id", "x.m"),
            FieldDecl::one2many("child_ids", "x.m", "parent_id"),
            FieldDecl::char("child_name")
                .related("child_ids.name")
                .store(true)
                .precompute(true),
        ],
        vec![],
    )
    .await
    .unwrap_err();
    assert_eq!(
        msg,
        refused(
            "child_name",
            "it reads child_ids.name through the one2many x.m.child_ids, whose lines are \
             created after the INSERT; precompute over one2many lines is not supported until \
             PERF.PRECOMPUTE-X2M"
        )
    );
    // through a many2one the targets exist before the INSERT: both declarations build
    build(
        &h,
        vec![
            FieldDecl::many2one("user_id", "res.users"),
            declared("c", "_compute_c"),
            FieldDecl::char("user_name")
                .related("user_id.name")
                .store(true)
                .precompute(true),
        ],
        vec![imp("_compute_c", &["user_id.name"])],
    )
    .await
    .unwrap();
    h.cleanup().await.unwrap();
}

/// /D7: a compute method runs once before the INSERT, at the position of the first
/// field of its group in the dependency order. When another field of the group depends on a field
/// that sits between the two, the method would run before that input has a value, so the group is
/// computed after the INSERT (conservative on purpose) and a declared `precompute = true` on it is
/// refused.
#[tokio::test]
async fn a_group_whose_member_reads_a_field_between_its_members_is_late() {
    let h = Harness::builder([&T])
        .without_schema()
        .build()
        .await
        .unwrap();
    let fields = |declared: bool| {
        vec![
            FieldDecl::char("a"),
            FieldDecl::char("g1")
                .compute("_compute_g")
                .store(true)
                .precompute(declared),
            FieldDecl::char("mid").compute("_compute_mid").store(true),
            FieldDecl::char("g2")
                .compute("_compute_g")
                .store(true)
                .depends(&["mid"]),
        ]
    };
    let impls = || vec![imp("_compute_g", &[]), imp("_compute_mid", &["a"])];
    let (kind, msg) = build(&h, fields(true), impls()).await.unwrap_err();
    assert_eq!(kind, ErrorKind::Programming);
    assert_eq!(
        msg,
        "Field x.m.g1 declares precompute = true, but it cannot be computed before the INSERT: \
         its compute method reads x.m.mid, which comes later in the dependency order, so the \
         method runs before it has a value"
    );
    // without the declaration the same graph builds (the group is computed after the INSERT)
    build(&h, fields(false), impls()).await.unwrap();
    h.cleanup().await.unwrap();
}

/// `precompute = true` on a field that is not a stored computed or related field is a
/// registry error, never dropped; `precompute = false` there has nothing to opt out of.
#[tokio::test]
async fn precompute_on_a_field_that_is_not_stored_and_computed_is_refused() {
    let h = Harness::builder([&T])
        .without_schema()
        .build()
        .await
        .unwrap();
    let (kind, msg) = build(
        &h,
        vec![FieldDecl::char("c").compute("_compute_c").precompute(true)],
        vec![imp("_compute_c", &[])],
    )
    .await
    .unwrap_err();
    assert_eq!(kind, ErrorKind::Programming);
    assert_eq!(
        msg,
        "field 'x.m.c': precompute = true is for stored computed or related fields; this field \
         is not stored"
    );
    let (kind, msg) = build(&h, vec![FieldDecl::char("p").precompute(true)], vec![])
        .await
        .unwrap_err();
    assert_eq!(kind, ErrorKind::Programming);
    assert_eq!(
        msg,
        "field 'x.m.p': precompute = true is for stored computed or related fields; this field \
         is not computed"
    );
    build(
        &h,
        vec![
            FieldDecl::char("c").compute("_compute_c").precompute(false),
            FieldDecl::char("p").precompute(false),
        ],
        vec![imp("_compute_c", &[])],
    )
    .await
    .unwrap();
    h.cleanup().await.unwrap();
}
