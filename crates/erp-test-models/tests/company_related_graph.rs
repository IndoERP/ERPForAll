//! The trigger edges of company-bound related fields at ORM assembly (reviews
//! RC3 and RD2): the company field of every company-scoped hop is a dependency of the value, and
//! the edge that leads to it is checked for what invalidation needs, not for the performance
//! rules of ordinary dependency paths. Own behaviour, so no SPEC tags.
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

fn imp(model: &'static str, method: &'static str) -> &'static ComputeImpl {
    Box::leak(Box::new(ComputeImpl {
        addon: "test_graph",
        model,
        method,
        kind: ImplKind::Define,
        depends: &[],
        reads: &[],
        depends_context: &[],
        entry: ComputeEntry::Sync(noop),
        file: file!(),
        line: line!(),
    }))
}

fn model(name: &'static str, fields: Vec<FieldDecl>) -> ModelDecl {
    let fields: &'static [FieldDecl] = Box::leak(fields.into_boxed_slice());
    ModelDecl::new("test_graph", name).fields(fields)
}

/// Companies, a company-scoped leaf, a mid with `leaf_id` as given and a document with the
/// related field `ref_leaf_id` (company-bound: `compute_sudo` unset) through `mid_id`.
async fn build(
    leaf_link: FieldDecl,
    impls: Vec<&'static ComputeImpl>,
) -> Result<(), (ErrorKind, String)> {
    let h = Harness::builder([&T])
        .without_schema()
        .build()
        .await
        .unwrap();
    let decls: Vec<&'static ModelDecl> = [
        model("res.company", vec![FieldDecl::char("name")]),
        model(
            "x.leaf",
            vec![
                FieldDecl::char("name"),
                FieldDecl::many2one("company_id", "res.company"),
            ],
        )
        .company_access(erp_registry::CompanyAccessDecl::unrestricted(
            "related graph fixture; company access is not under test",
        )),
        model("x.mid", vec![leaf_link]),
        model(
            "x.doc",
            vec![
                FieldDecl::many2one("mid_id", "x.mid"),
                FieldDecl::many2one("ref_leaf_id", "x.leaf").related("mid_id.leaf_id"),
            ],
        ),
    ]
    .into_iter()
    .map(|d| &*Box::leak(Box::new(d)))
    .collect();
    let registry = RegistryBuilder::new([&T])
        .with_decls(decls)
        .build()
        .map_err(|e| (e.kind(), e.message().to_owned()))?;
    let built = Orm::builder(std::sync::Arc::new(registry), h.db().pool().clone())
        .policy(StaticPolicy::AllowAll)
        .computes(impls)
        .build()
        .map(|_| ())
        .map_err(|e| (e.kind(), e.message().to_owned()));
    h.cleanup().await.unwrap();
    built
}

/// Review RD2: the edge that leads to the company of the record a company-bound path ends on is
/// there for invalidation. A many2one with `index = false` is traced by its column all the same
/// (only slower), so the index rule of ordinary dependency paths does not apply to it; before
/// the fix the registry refused this field, which built without the edge.
#[tokio::test]
async fn an_unindexed_last_link_is_traced_for_the_company_of_its_target() {
    build(
        FieldDecl::many2one("leaf_id", "x.leaf").index(None),
        Vec::new(),
    )
    .await
    .unwrap();
}

/// Review RD2: a last link that cannot be traced (computed, neither stored nor related) cannot
/// tell which documents to invalidate when its target changes company. The registry refuses it
/// with a message that names the company-bound field and the step (before the fix: the text of
/// the dependency-path rule, which talks about `depends`).
#[tokio::test]
async fn an_untraceable_last_link_is_refused_naming_the_bound_field() {
    let (kind, message) = build(
        FieldDecl::many2one("leaf_id", "x.leaf").compute("_compute_leaf"),
        vec![imp("x.mid", "_compute_leaf")],
    )
    .await
    .unwrap_err();
    assert_eq!(kind, ErrorKind::Programming);
    assert_eq!(
        message,
        "Field x.doc.ref_leaf_id is company-bound related to 'mid_id.leaf_id' (compute_sudo is \
         not declared): the company of the record it reaches through x.mid.leaf_id cannot be \
         watched, because x.mid.leaf_id is a computed field that is neither stored nor related; \
         make that field stored or related, or declare compute_sudo on the related field"
    );
}
