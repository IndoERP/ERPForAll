//! Fixtures for the default company check: one model
//! per rule, none of them naming `check_company` unless the rule is the opt-out itself. They use
//! their own `res.company` (a tree, so the parent matches work); load them without `test_rel`,
//! which has the M1 fixture of the same name.

use erp_sdk::prelude::*;

erp_sdk::addon!(name = "test_company");

/// `res.company` with a parent path (`parent_of` needs it).
#[model(
    name = "res.company",
    description = "Companies",
    addon = "test_company",
    parent_store = true,
    parent_name = "parent_id"
)]
pub struct Company {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(many2one, comodel = "res.company")]
    pub parent_id: Option<RecId>,
    #[field(char, index)]
    pub parent_path: Option<Arc<str>>,
}

#[extend(ResUsers, addon = "test_company", company_access = unrestricted(reason = "base user access currently has no company restriction; the reference ERP user rule has non-company escapes"))]
pub struct UsersCompanies {
    #[field(
        many2many,
        comodel = "res.company",
        relation = "res_company_users_rel",
        column1 = "user_id",
        column2 = "cid"
    )]
    pub company_ids: Vec<RecId>,
}

/// A catalogue whose records a company may use when they belong to it or to one of its parents.
#[model(
    name = "test_company.partner",
    company_access = unrestricted(reason = "company-consistency fixture; access is not under test"),
    description = "Company Partner",
    company_match = "parent_of",
    addon = "test_company"
)]
pub struct CompanyPartner {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(boolean, default = true)]
    pub active: bool,
    #[field(many2one, comodel = "res.company")]
    pub company_id: Option<RecId>,
}

/// A plain company-scoped model (`exact`).
#[model(
    name = "test_company.tag",
    company_access = unrestricted(reason = "company-consistency fixture; access is not under test"),
    description = "Company Tag",
    addon = "test_company"
)]
pub struct Tag {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(boolean, default = true)]
    pub active: bool,
    #[field(many2one, comodel = "res.company")]
    pub company_id: Option<RecId>,
}

/// A model shared between several companies (`parent_of_any`).
#[model(
    name = "test_company.shared",
    company_access = unrestricted(reason = "company-consistency fixture; access is not under test"),
    description = "Company Shared",
    company_match = "parent_of_any",
    addon = "test_company"
)]
pub struct Shared {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(many2many, comodel = "res.company")]
    pub company_ids: Vec<RecId>,
}

/// A model without company: links to it are never checked.
#[model(
    name = "test_company.plain",
    description = "Company Plain",
    addon = "test_company"
)]
pub struct Plain {
    #[field(char)]
    pub name: Option<Arc<str>>,
}

/// The default-checked document: every rule on one model.
#[model(
    name = "test_company.doc",
    company_access = unrestricted(reason = "company-consistency fixture; access is not under test"),
    description = "Company Document",
    addon = "test_company"
)]
pub struct Doc {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(many2one, comodel = "res.company")]
    pub company_id: Option<RecId>,
    /// Checked: `exact`.
    #[field(many2one, comodel = "test_company.tag", string = "Tag")]
    pub tag_id: Option<RecId>,
    /// Checked: a many2many, `exact`.
    #[field(many2many, comodel = "test_company.tag", string = "Tags")]
    pub tag_ids: Vec<RecId>,
    /// Checked: `parent_of`.
    #[field(many2one, comodel = "test_company.partner", string = "Partner")]
    pub partner_id: Option<RecId>,
    /// Checked: `any_of` against the user's companies.
    #[field(many2one, comodel = "res.users", string = "Owner")]
    pub owner_id: Option<RecId>,
    /// Checked: `parent_of_any`.
    #[field(many2many, comodel = "test_company.shared", string = "Shared")]
    pub shared_ids: Vec<RecId>,
    /// Checked: a self link.
    #[field(many2one, comodel = "test_company.doc", string = "Parent Document")]
    pub parent_doc_id: Option<RecId>,
    /// Opt-out of one field.
    #[field(
        many2one,
        comodel = "test_company.tag",
        check_company = false,
        string = "Free Tag"
    )]
    pub free_tag_id: Option<RecId>,
    /// Not checked: the target has no company.
    #[field(many2one, comodel = "test_company.plain", string = "Plain")]
    pub plain_id: Option<RecId>,
    /// Not checked: related.
    #[field(
        many2one,
        comodel = "test_company.tag",
        related = "tag_id",
        string = "Related Tag"
    )]
    pub related_tag_id: Option<RecId>,
    /// Not checked by default: the lines check their own inverse.
    #[field(one2many, comodel = "test_company.line", inverse_name = "doc_id")]
    pub line_ids: Vec<RecId>,
}

/// A line of a document: its `doc_id` is checked against the line's company.
#[model(
    name = "test_company.line",
    company_access = unrestricted(reason = "company-consistency fixture; access is not under test"),
    description = "Company Line",
    addon = "test_company"
)]
pub struct Line {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(many2one, comodel = "res.company")]
    pub company_id: Option<RecId>,
    #[field(many2one, comodel = "test_company.doc", string = "Document")]
    pub doc_id: Option<RecId>,
}

/// A company-dependent link: checked against the current company when it declares
/// `check_company` (not by default until per-company storage).
#[model(
    name = "test_company.dep",
    company_access = unrestricted(reason = "company-consistency fixture; access is not under test"),
    description = "Company Dependent",
    addon = "test_company"
)]
pub struct Dep {
    #[field(many2one, comodel = "res.company")]
    pub company_id: Option<RecId>,
    /// Checked against `env.company()`, not the record's company.
    #[field(
        many2one,
        comodel = "test_company.tag",
        company_dependent = true,
        check_company,
        string = "Preferred Tag"
    )]
    pub pref_tag_id: Option<RecId>,
}

/// A model that opts out as a whole.
#[model(
    name = "test_company.free",
    company_access = unrestricted(reason = "company-consistency fixture; access is not under test"),
    description = "Company Free",
    check_company_auto = false,
    addon = "test_company"
)]
pub struct Free {
    #[field(many2one, comodel = "res.company")]
    pub company_id: Option<RecId>,
    #[field(many2one, comodel = "test_company.tag")]
    pub tag_id: Option<RecId>,
}

/// Delegates to a document: the delegated fields are the parent's to check.
#[model(
    name = "test_company.child",
    company_access = unrestricted(reason = "company-consistency fixture; access is not under test"),
    description = "Company Child",
    inherits = [("test_company.doc", "doc_id")],
    addon = "test_company"
)]
pub struct Child {
    #[field(many2one, comodel = "test_company.doc", required, ondelete = "cascade")]
    pub doc_id: Option<RecId>,
}

/// `res.company` itself: a declared check on a field of a company (CRUD-061 form a).
#[extend(ResCompany, addon = "test_company", check_company_auto = true)]
pub struct CompanyChecked {
    /// Checked because it says so (a company's fields are not checked by default).
    #[field(
        many2one,
        comodel = "test_company.tag",
        check_company,
        string = "Default Tag"
    )]
    pub default_tag_id: Option<RecId>,
}

/// A document whose checked link is a stored compute (review D1): `tag_id` follows `source_id`,
/// and is declared checked against the document's company.
#[model(
    name = "test_company.calc",
    company_access = unrestricted(reason = "company-consistency fixture; access is not under test"),
    description = "Company Computed",
    addon = "test_company"
)]
pub struct Calc {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(many2one, comodel = "res.company")]
    pub company_id: Option<RecId>,
    /// The input of the compute; not checked itself.
    #[field(
        many2one,
        comodel = "test_company.tag",
        check_company = false,
        string = "Source"
    )]
    pub source_id: Option<RecId>,
    /// Checked because it says so (a computed field is not checked by default): a stored
    /// compute, copied from `source_id`.
    #[field(
        many2one,
        comodel = "test_company.tag",
        compute = "_compute_tag_id",
        store,
        check_company,
        string = "Computed Tag"
    )]
    pub tag_id: Option<RecId>,
}

#[methods(Calc, addon = "test_company")]
impl Calc {
    #[compute(depends = ["source_id"])]
    fn _compute_tag_id(batch: &mut Batch<'_, Calc>) -> Result<()> {
        for i in batch.indices() {
            let source = batch.get(i, Calc::SOURCE_ID)?;
            batch.set(i, Calc::TAG_ID, source)?;
        }
        Ok(())
    }
}
