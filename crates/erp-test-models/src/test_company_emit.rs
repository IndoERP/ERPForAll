//! Fixture "company emit" (design): two models on the base
//! addon's own `res.company`, one restricted directly and one through its owner, for the tests of
//! the module operations that emit generated restrictions. (`test_company_access` defines its own
//! `res.company`, so it cannot be loaded next to the base addon.)

use erp_sdk::prelude::*;

erp_sdk::addon!(name = "test_company_emit", depends = ["base"]);

/// Restricted through its own company field.
#[model(
    name = "test_company_emit.doc",
    description = "Document",
    addon = "test_company_emit",
    company_access = direct(field = "company_id", branch = "exact", empty = "deny")
)]
pub struct Doc {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(many2one, comodel = "res.company")]
    pub company_id: Option<RecId>,
}

/// Restricted through its owner `doc_id`.
#[model(
    name = "test_company_emit.line",
    description = "Line",
    addon = "test_company_emit",
    company_access = parent(edge = "doc_id")
)]
pub struct Line {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(
        many2one,
        comodel = "test_company_emit.doc",
        required,
        ondelete = "cascade"
    )]
    pub doc_id: Option<RecId>,
}
