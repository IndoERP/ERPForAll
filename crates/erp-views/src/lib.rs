//! `erp-views`: the view arch inheritance engine (layer L7), pure and database-free.
//!
//! Callers build a [`ViewCatalogue`] from their view records and ask for combined archs.
//!
//! Arch strings are untrusted input: parsing rejects DTDs, unknown entities and namespaces, and
//! every stage is bounded by [`ViewLimits`] (input size, elements and XML nodes, depth, XPath size
//! and nesting, evaluation steps per combination, nodes and bytes built while combining). No
//! traversal recurses over the tree.
//!
//! [`record`] and [`validate`] are the pure half of the `ir.ui.view` records (
//! ): the `mode` rules, rows to descriptors, the views a write must
//! check, write-time validation of a combined arch, and the record layer's typed errors.

mod apply;
mod builtin_schemas;
pub use builtin_schemas::{form_schema, list_schema, pivot_schema, search_schema};
mod checked_input;
pub use checked_input::{CheckedContent, CheckedNode, CheckedViewInput};
mod arch;
mod attributes;
mod combine;
mod description_budget;
mod description_builder;
pub use description_builder::{
    AdmittedBuiltinView, DescriptionBuilder, DescriptionObjectBuilder, DescriptionPayload,
    DescriptionValue, DescriptionValueBuilder,
};
mod description_encoding;
pub use description_encoding::{DescriptionBuffer, DescriptionEncoder};
mod description_schema;
pub use description_schema::{
    FieldReference, ReferenceScope, ReferenceVisitor, SchemaNode, SchemaProperty, SchemaVariant,
    ViewSchema,
};
mod reference_index;
pub use reference_index::SchemaReferences;
mod expression_decode;
pub use expression_decode::{
    ExpressionDecodePreflight, expression_decode_preflight, panel_domain_references,
};
mod error;
mod exprs;
pub mod guard;
#[cfg(feature = "host-access")]
pub mod host_access;
pub use description_budget::{DescriptionBudget, DescriptionReservation};
mod expression_preflight;
pub use expression_preflight::{ExpressionPreflight, expression_preflight};
mod expression_references;
mod limits;
pub use expression_references::{ExpressionKind, expression_references, validate_panel_domain};
mod locate;
mod metadata_fields;
mod parse;
pub mod record;
pub mod registry;
pub mod validate;
pub use metadata_fields::{FieldPredicate, MetadataFieldSet, MetadataFieldSetBuilder};
mod view_type_decl;
pub use view_type_decl::{
    BuildView, ValidateView, ViewTypeDecl, ViewTypeDeclBuilder, ViewTypeDeclFn,
};
mod view_type_owner;
pub mod wire;
mod xpath;
pub use view_type_owner::{OwnerBoundViewTypes, ViewTypeContribution};
mod view_type_publication;
pub use view_type_publication::{InstalledClosure, PublishedViewTypes};

#[cfg(test)]
mod tests;

pub use arch::{Arch, NodeIdx, NodeOrigin, NodeRef};
pub use combine::{CombinedView, Inheritance, ViewCatalogue, ViewDescriptor};
pub use error::{LimitKind, ViewError, ViewResult, XpathErrorKind};
pub use guard::{GuardLookup, GuardedArch};
pub use limits::ViewLimits;
pub use parse::{check_xml_nesting, root_tag};
pub use registry::{find_view_type, is_valid_view_type_key, validate_view_types};
