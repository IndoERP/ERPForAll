//! They link records across companies on purpose, so the links opt out of the default company
//! check (`check_company = false`). Load them with `test_company`, whose `res.company` has a
//! parent path.

use erp_sdk::prelude::*;

erp_sdk::addon!(name = "test_company_related", depends = ["test_company"]);

/// The order of `res.company` (`sequence, name`), which the suggested company of a hint follows
/// (review RC9).
#[extend(ResCompany, addon = "test_company_related")]
pub struct CompanyOrder {
    #[field(integer, default = 10)]
    pub sequence: i32,
}

/// The end of the chain.
#[model(
    name = "test_company_related.leaf",
    company_access = unrestricted(reason = "company-bound related fixture; access is not under test"),
    description = "Related Leaf",
    addon = "test_company_related"
)]
pub struct Leaf {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(many2one, comodel = "res.company")]
    pub company_id: Option<RecId>,
}

/// The middle of the chain.
#[model(
    name = "test_company_related.mid",
    company_access = unrestricted(reason = "company-bound related fixture; access is not under test"),
    description = "Related Mid",
    addon = "test_company_related"
)]
pub struct Mid {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(many2one, comodel = "res.company")]
    pub company_id: Option<RecId>,
    #[field(many2one, comodel = "test_company_related.leaf", check_company = false)]
    pub leaf_id: Option<RecId>,
    /// Several records of a company-scoped model (a company-bound path can end on them).
    #[field(many2many, comodel = "test_company.tag", check_company = false)]
    pub tag_ids: Vec<RecId>,
}

/// A record without company scope whose own company-bound field reads through company-scoped
/// records: a field of another model reads it through this one (review SC1). Its record name is
/// that field, so labels of links to it go through the bound too.
#[model(
    name = "test_company_related.holder",
    description = "Related Holder",
    addon = "test_company_related",
    rec_name = "leaf_name"
)]
pub struct Holder {
    #[field(many2one, comodel = "test_company_related.mid", check_company = false)]
    pub mid_id: Option<RecId>,
    /// Company-bound: two hops, both company-scoped.
    #[field(char, related = "mid_id.leaf_id.name")]
    pub leaf_name: Option<Arc<str>>,
}

/// A record without company scope whose record name is a company-bound field of one hop: the
/// hop of a label read through a link is not the reader's to see by id (review SD4).
#[model(
    name = "test_company_related.holder1",
    description = "Related Holder One",
    addon = "test_company_related",
    rec_name = "mid_name"
)]
pub struct HolderOne {
    #[field(many2one, comodel = "test_company_related.mid", check_company = false)]
    pub mid_id: Option<RecId>,
    /// Company-bound: one hop.
    #[field(char, related = "mid_id.name")]
    pub mid_name: Option<Arc<str>>,
}

/// A record whose record name is a plain many2one to a holder: its label goes through the
/// holder's label, whose record name is company-bound (review SD3, two levels).
#[model(
    name = "test_company_related.link",
    description = "Related Link",
    addon = "test_company_related",
    rec_name = "holder_id"
)]
pub struct Link {
    #[field(
        many2one,
        comodel = "test_company_related.holder",
        check_company = false
    )]
    pub holder_id: Option<RecId>,
}

/// A document without company that reads through company-scoped records.
#[model(
    name = "test_company_related.doc",
    description = "Related Doc",
    addon = "test_company_related"
)]
pub struct Doc {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(many2one, comodel = "test_company_related.mid", check_company = false)]
    pub mid_id: Option<RecId>,
    #[field(many2one, comodel = "test_company.partner", check_company = false)]
    pub partner_id: Option<RecId>,
    #[field(many2one, comodel = "test_company.shared", check_company = false)]
    pub shared_id: Option<RecId>,
    /// Company-bound: two hops, both company-scoped (`exact`).
    #[field(char, related = "mid_id.leaf_id.name")]
    pub leaf_name: Option<Arc<str>>,
    #[field(char, related = "mid_id.leaf_id.name", compute_sudo = true)]
    pub leaf_name_sudo: Option<Arc<str>>,
    /// Read as the caller.
    #[field(char, related = "mid_id.leaf_id.name", compute_sudo = false)]
    pub leaf_name_user: Option<Arc<str>>,
    /// Company-bound: one hop.
    #[field(char, related = "mid_id.name")]
    pub mid_name: Option<Arc<str>>,
    /// Stored: filled at write time from its sources, not bound at read time.
    #[field(char, related = "mid_id.name", store)]
    pub mid_name_stored: Option<Arc<str>>,
    /// Company-bound and editable: written through its hop, read back through the same hop.
    #[field(char, related = "mid_id.name", readonly = false)]
    pub mid_name_edit: Option<Arc<str>>,
    /// Company-bound: a `parent_of` model.
    #[field(char, related = "partner_id.name")]
    pub partner_name: Option<Arc<str>>,
    /// Company-bound: a `parent_of_any` model.
    #[field(char, related = "shared_id.name")]
    pub shared_name: Option<Arc<str>>,
    #[field(char, related = "partner_id.name", compute_sudo = true)]
    pub partner_name_sudo: Option<Arc<str>>,
    #[field(char, related = "shared_id.name", compute_sudo = true)]
    pub shared_name_sudo: Option<Arc<str>>,
    #[field(
        many2one,
        comodel = "test_company_related.holder",
        check_company = false
    )]
    pub holder_id: Option<RecId>,
    /// Company-bound through a model without company scope: the bound comes from the company-bound
    /// field it reads (review SC1).
    #[field(char, related = "holder_id.leaf_name")]
    pub holder_leaf_name: Option<Arc<str>>,
    /// Company-bound many2one through two hops.
    #[field(
        many2one,
        comodel = "test_company_related.leaf",
        related = "mid_id.leaf_id"
    )]
    pub leaf_id: Option<RecId>,
    #[field(
        many2one,
        comodel = "test_company_related.leaf",
        related = "mid_id.leaf_id",
        compute_sudo = true,
        string = "Leaf Sudo"
    )]
    pub leaf_id_sudo: Option<RecId>,
    /// Company-bound one-segment alias of a link to a company-scoped model (review RD3).
    #[field(
        many2one,
        comodel = "test_company_related.mid",
        related = "mid_id",
        string = "Mid Alias"
    )]
    pub mid_alias_id: Option<RecId>,
    /// Company-bound and ending on several company-scoped records (review SD1).
    #[field(
        many2many,
        comodel = "test_company.tag",
        related = "mid_id.tag_ids",
        string = "Tag Rel"
    )]
    pub tag_rel_ids: Vec<RecId>,
    #[field(many2one, comodel = "test_company_related.link", check_company = false)]
    pub link_id: Option<RecId>,
    #[field(
        many2one,
        comodel = "test_company_related.holder1",
        check_company = false
    )]
    pub holder1_id: Option<RecId>,
}

/// Delegates to a document: a company-bound path through an inherited field (review RD1).
#[model(
    name = "test_company_related.kid",
    description = "Related Kid",
    inherits = [("test_company_related.doc", "doc_id")],
    addon = "test_company_related"
)]
pub struct Kid {
    #[field(
        many2one,
        comodel = "test_company_related.doc",
        required,
        ondelete = "cascade"
    )]
    pub doc_id: Option<RecId>,
    /// Company-bound: the first step (`mid_id`) is the parent's field.
    #[field(char, related = "mid_id.name", string = "Kid Mid Name")]
    pub kid_mid_name: Option<Arc<str>>,
    /// Company-bound: two hops behind an inherited step.
    #[field(char, related = "mid_id.leaf_id.name", string = "Kid Leaf Name")]
    pub kid_leaf_name: Option<Arc<str>>,
}
