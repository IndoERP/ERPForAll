//! Fixture "company access" (
//! A1-D2, A1-D3; design): one model per declared form, owner
//! chains, a transient, an unrestricted, a mixin and a company-less model. Model names follow the
//! spec's (`M1` is `test_company_access.m1`, `L2` is `test_company_access.l2`, …). It has its own
//! `res.company` with a hierarchy, so load it without `test_rel` and `test_company`, which declare
//! the same model.

use erp_sdk::prelude::*;

erp_sdk::addon!(name = "test_company_access");

/// `res.company` with its hierarchy (`ancestor` forms need `parent_path`).
#[model(
    name = "res.company",
    description = "Companies",
    addon = "test_company_access",
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

/// `M1`, form 1: single company, `exact`, `deny`.
#[model(
    name = "test_company_access.m1",
    description = "Form 1",
    addon = "test_company_access",
    company_access = direct(field = "company_id", branch = "exact", empty = "deny")
)]
pub struct M1 {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(many2one, comodel = "res.company")]
    pub company_id: Option<RecId>,
}

/// `M2`, form 2: single company, `ancestor`, `deny`.
#[model(
    name = "test_company_access.m2",
    description = "Form 2",
    addon = "test_company_access",
    company_access = direct(field = "company_id", branch = "ancestor", empty = "deny")
)]
pub struct M2 {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(many2one, comodel = "res.company")]
    pub company_id: Option<RecId>,
}

/// `M3`, form 3: single company, `ancestor`, `allow`.
#[model(
    name = "test_company_access.m3",
    description = "Form 3",
    addon = "test_company_access",
    company_access = direct(field = "company_id", branch = "ancestor", empty = "allow")
)]
pub struct M3 {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(many2one, comodel = "res.company")]
    pub company_id: Option<RecId>,
}

/// `M4`, form 4: single company, `exact`, `allow`.
#[model(
    name = "test_company_access.m4",
    description = "Form 4",
    addon = "test_company_access",
    company_access = direct(field = "company_id", branch = "exact", empty = "allow")
)]
pub struct M4 {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(many2one, comodel = "res.company")]
    pub company_id: Option<RecId>,
}

/// `M5`, form 5: many companies, `ancestor`, `deny`.
#[model(
    name = "test_company_access.m5",
    description = "Form 5",
    addon = "test_company_access",
    company_access = direct(field = "company_ids", branch = "ancestor", empty = "deny")
)]
pub struct M5 {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(many2many, comodel = "res.company")]
    pub company_ids: Vec<RecId>,
}

/// `M6`, form 6: many companies, `exact`, `deny`.
#[model(
    name = "test_company_access.m6",
    description = "Form 6",
    addon = "test_company_access",
    company_access = direct(field = "company_ids", branch = "exact", empty = "deny")
)]
pub struct M6 {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(many2many, comodel = "res.company")]
    pub company_ids: Vec<RecId>,
}

/// `M7`, form 7: many companies, `exact`, `allow`.
#[model(
    name = "test_company_access.m7",
    description = "Form 7",
    addon = "test_company_access",
    company_access = direct(field = "company_ids", branch = "exact", empty = "allow")
)]
pub struct M7 {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(many2many, comodel = "res.company")]
    pub company_ids: Vec<RecId>,
}

/// `M8`, form 8: many companies, `ancestor`, `allow`.
#[model(
    name = "test_company_access.m8",
    description = "Form 8",
    addon = "test_company_access",
    company_access = direct(field = "company_ids", branch = "ancestor", empty = "allow")
)]
pub struct M8 {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(many2many, comodel = "res.company")]
    pub company_ids: Vec<RecId>,
}

/// `L`, owned by `M1` through `doc_id` (one level).
#[model(
    name = "test_company_access.l",
    description = "Line",
    addon = "test_company_access",
    company_access = parent(edge = "doc_id")
)]
pub struct L {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(
        many2one,
        comodel = "test_company_access.m1",
        required,
        ondelete = "cascade"
    )]
    pub doc_id: Option<RecId>,
}

/// `L2`, owned by `L` through `line_id` (two levels).
#[model(
    name = "test_company_access.l2",
    description = "Sub-line",
    addon = "test_company_access",
    company_access = parent(edge = "line_id")
)]
pub struct L2 {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(
        many2one,
        comodel = "test_company_access.l",
        required,
        ondelete = "cascade"
    )]
    pub line_id: Option<RecId>,
}

/// `P`, form 3 on its own company and owned by `M2` through `owner_id`.
#[model(
    name = "test_company_access.p",
    description = "Repartition",
    addon = "test_company_access",
    company_access = direct(
        field = "company_id",
        branch = "ancestor",
        empty = "allow",
        parent = "owner_id"
    )
)]
pub struct P {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(many2one, comodel = "res.company")]
    pub company_id: Option<RecId>,
    #[field(
        many2one,
        comodel = "test_company_access.m2",
        required,
        ondelete = "cascade"
    )]
    pub owner_id: Option<RecId>,
}

/// `T`, a transient model with form 1.
#[model(
    name = "test_company_access.t",
    description = "Wizard",
    kind = "transient",
    addon = "test_company_access",
    company_access = direct(field = "company_id", branch = "exact", empty = "deny")
)]
pub struct T {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(many2one, comodel = "res.company")]
    pub company_id: Option<RecId>,
}

/// `U`, company-scoped and declared unrestricted.
#[model(
    name = "test_company_access.u",
    description = "Unrestricted",
    addon = "test_company_access",
    company_access = unrestricted(
        reason = "fixture of an explicit opt-out: visible to every company"
    )
)]
pub struct U {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(many2one, comodel = "res.company")]
    pub company_id: Option<RecId>,
}

/// `X`, an abstract mixin declaring form 1; validated on `XC`.
#[model(
    name = "test_company_access.x",
    description = "Company mixin",
    kind = "abstract",
    addon = "test_company_access",
    company_access = direct(field = "company_id", branch = "exact", empty = "deny")
)]
pub struct X {
    #[field(many2one, comodel = "res.company")]
    pub company_id: Option<RecId>,
}

/// `XC`, the concrete model of `X`.
#[model(
    name = "test_company_access.xc",
    description = "Mixin user",
    inherit = [X],
    addon = "test_company_access"
)]
pub struct XC {
    #[field(char)]
    pub name: Option<Arc<str>>,
}

/// `N`, without company field, explicitly opts out despite its required link to `M1`.
#[model(
    name = "test_company_access.n",
    description = "Note",
    company_access = unrestricted(reason = "fixture demonstrates an explicit opt-out from owner access"),
    addon = "test_company_access"
)]
pub struct N {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(
        many2one,
        comodel = "test_company_access.m1",
        required,
        ondelete = "cascade"
    )]
    pub m1_id: Option<RecId>,
}

/// `RC`, form 1 on a stored company that is related to a related company: writing `anchor_id`
/// moves `company_id` through `mid_company_id` (two hops of dependents, neither written).
#[model(
    name = "test_company_access.rc",
    description = "Related company chain",
    addon = "test_company_access",
    company_access = direct(field = "company_id", branch = "exact", empty = "deny")
)]
pub struct RC {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(many2one, comodel = "test_company_access.m1", check_company = false)]
    pub anchor_id: Option<RecId>,
    #[field(
        many2one,
        comodel = "res.company",
        related = "anchor_id.company_id",
        store
    )]
    pub mid_company_id: Option<RecId>,
    #[field(many2one, comodel = "res.company", related = "mid_company_id", store)]
    pub company_id: Option<RecId>,
}
