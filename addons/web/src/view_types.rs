//! The builtin H18 view-type declarations: `list`, `form`, `search` and `pivot`.
//!
//! Each gets its closed builtin schema, the arch-reference metadata field set and an empty
//! data-method list. The validator and builder callbacks are typed against the checked host input.
//! Native serving obtains that input through the real checked host bridge and
//! invokes these callbacks from the exact generation's published declarations.
use erp_sdk::views::{
    CheckedViewInput, DescriptionBudget, DescriptionValue, DescriptionValueBuilder, FieldPredicate,
    MetadataFieldSet, ViewResult, ViewSchema, ViewTypeDecl, form_schema, list_schema, pivot_schema,
    search_schema,
};

macro_rules! builtin_callbacks {
    ($validate:ident, $build:ident, $key:literal) => {
        fn $validate(input: &CheckedViewInput<'_>, budget: &DescriptionBudget) -> ViewResult<()> {
            crate::h18_lower::validate(input, $key, budget)
        }
        fn $build(
            input: &CheckedViewInput<'_>,
            output: DescriptionValueBuilder<'_>,
        ) -> ViewResult<DescriptionValue> {
            crate::h18_lower::build(input, $key, output)
        }
    };
}
builtin_callbacks!(validate_list, build_list, "list");
builtin_callbacks!(validate_form, build_form, "form");
builtin_callbacks!(validate_search, build_search, "search");
builtin_callbacks!(validate_pivot, build_pivot, "pivot");

fn declare(
    key: &'static str,
    schema: &ViewSchema,
    validate: erp_sdk::views::ValidateView,
    build: erp_sdk::views::BuildView,
    fields: MetadataFieldSet,
) -> ViewResult<ViewTypeDecl> {
    ViewTypeDecl::builder("web", key)
        .wire_major(1)
        .renderer(match key {
            "list" => "web.list_sections_v1",
            "form" => "web.form",
            "search" => "web.search_sections_v1",
            "pivot" => "web.pivot",
            _ => {
                return Err(erp_sdk::views::ViewError::InvalidDescription(
                    "Unknown builtin view type.".to_owned(),
                ));
            }
        })
        .schema(schema)
        .validator(validate)
        .builder(build)
        .metadata_fields(fields)
        .methods(&[])
        .finish()
}

/// `list` (H18): metadata for the fields its arch references.
pub(crate) fn list_view_type() -> ViewResult<ViewTypeDecl> {
    let fields = MetadataFieldSet::builder().arch_references().finish()?;
    declare("list", &list_schema()?, validate_list, build_list, fields)
}

/// `form` (H18): metadata for the fields its arch references.
pub(crate) fn form_view_type() -> ViewResult<ViewTypeDecl> {
    let fields = MetadataFieldSet::builder().arch_references().finish()?;
    declare("form", &form_schema()?, validate_form, build_form, fields)
}

/// `pivot` (H18): metadata for the fields its arch references.
pub(crate) fn pivot_view_type() -> ViewResult<ViewTypeDecl> {
    let fields = MetadataFieldSet::builder().arch_references().finish()?;
    declare(
        "pivot",
        &pivot_schema()?,
        validate_pivot,
        build_pivot,
        fields,
    )
}

/// `search` (H18): the arch references plus every readable field of the root model, for
/// the custom filter/domain editor.
pub(crate) fn search_view_type() -> ViewResult<ViewTypeDecl> {
    let fields = MetadataFieldSet::builder()
        .arch_references()
        .predicate(FieldPredicate::AllReadable)
        .finish()?;
    declare(
        "search",
        &search_schema()?,
        validate_search,
        build_search,
        fields,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use erp_sdk::views::{FieldPredicate, validate_view_types};

    fn all() -> Vec<erp_sdk::views::ViewTypeDecl> {
        [
            list_view_type,
            form_view_type,
            search_view_type,
            pivot_view_type,
        ]
        .into_iter()
        .map(|declare| declare().unwrap())
        .collect()
    }

    /// values and publication are WP3b)
    #[test]
    fn web_declares_list_form_search_and_pivot_as_the_owner() {
        let decls = all();
        let refs: Vec<_> = decls.iter().collect();
        validate_view_types(&refs).unwrap();
        let summary: Vec<_> = decls
            .iter()
            .map(|d| {
                (
                    d.addon(),
                    d.key(),
                    d.wire_major(),
                    d.renderer(),
                    d.schema().revision(),
                )
            })
            .collect();
        assert_eq!(
            summary,
            vec![
                ("web", "list", 1, "web.list_sections_v1", 1),
                ("web", "form", 1, "web.form", 1),
                ("web", "search", 1, "web.search_sections_v1", 1),
                ("web", "pivot", 1, "web.pivot", 1),
            ]
        );
    }

    #[test]
    fn metadata_fields_and_methods_follow_adr_0060_d1() {
        for decl in all() {
            assert!(
                decl.metadata_fields().includes_arch_references(),
                "{}",
                decl.key()
            );
            assert!(decl.methods().is_empty(), "{}", decl.key());
            assert_eq!(decl.metadata_fields().explicit().count(), 0);
        }
        // The search request also offers every readable field of the root model (custom filters).
        let search = search_view_type().unwrap();
        assert_eq!(
            search.metadata_fields().predicates(),
            &[FieldPredicate::AllReadable]
        );
        assert_eq!(
            list_view_type().unwrap().metadata_fields().predicates(),
            &[] as &[FieldPredicate]
        );
    }

    #[test]
    fn linkme_slice_carries_the_four_declarations() {
        let mut keys: Vec<String> = erp_sdk::views::VIEW_TYPES
            .iter()
            .map(|declare| declare().unwrap().key().to_owned())
            .collect();
        keys.sort();
        assert_eq!(keys, ["form", "list", "pivot", "search"]);
    }
}
