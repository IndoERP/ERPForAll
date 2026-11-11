//! Explicit standalone identity target for fixtures that do not link the base addon.
//! This contributes metadata only: no rounding data or native provider is fabricated.

use erp_registry::{FieldDecl, MODEL_DECLS, ModelDecl};

static CURRENCY: ModelDecl = ModelDecl::new("base", "res.currency")
    .description("Currency")
    .fields(&[FieldDecl::char("name")]);

/// Whether the actual declaration set contains the base addon's real currency root.
pub fn has_currency_root<'a>(decls: impl IntoIterator<Item = &'a ModelDecl>) -> bool {
    decls.into_iter().any(|decl| {
        decl.addon == "base" && decl.name == "res.currency" && !decl.inherit.contains(&decl.name)
    })
}

/// Ordinary linked builders always select base; an existing root must never be duplicated.
pub fn decls() -> impl Iterator<Item = &'static ModelDecl> {
    for_selected(has_currency_root(MODEL_DECLS.iter()))
}

/// Filtered/linked(false) builders decide from their actual selected declarations.
pub fn for_selected(has_root: bool) -> impl Iterator<Item = &'static ModelDecl> {
    (!has_root).then_some(&CURRENCY).into_iter()
}
