//! Fixture "company escape": a model whose `company_access`
//! carries an `escape` over a stored, required boolean that is itself a stored compute (the shape
//! of `res.users.share`), and one whose escape is a plain stored boolean. Has its own
//! `res.company` with a hierarchy, so load it without `test_rel`, `test_company` and
//! `test_company_access`.

use erp_sdk::prelude::*;

erp_sdk::addon!(name = "test_company_escape");

/// `res.company` with its hierarchy.
#[model(
    name = "res.company",
    description = "Companies",
    addon = "test_company_escape",
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

/// Escape over a stored compute: `share` is true when `kind` is `portal`.
#[model(
    name = "test_company_escape.doc",
    description = "Escape over a stored compute",
    addon = "test_company_escape",
    keys = [key(name = "_name_key", fields = ["name"], message = "A document name must be unique.")],
    company_access = direct(field = "company_id", branch = "exact", empty = "deny", escape = "share")
)]
pub struct Doc {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(char, default = "user")]
    pub kind: Option<Arc<str>>,
    #[field(boolean, compute = "_compute_share", store, required, default = false)]
    pub share: bool,
    #[field(many2one, comodel = "res.company")]
    pub company_id: Option<RecId>,
}

#[methods(Doc, addon = "test_company_escape")]
impl Doc {
    #[compute(depends = ["kind"])]
    fn _compute_share(batch: &mut Batch<'_, Doc>) -> Result<()> {
        for i in batch.indices() {
            let kind = batch.get(i, Doc::KIND)?;
            batch.set(i, Doc::SHARE, kind.as_deref() == Some("portal"))?;
        }
        Ok(())
    }
}

/// Escape over a plain stored boolean with a default.
#[model(
    name = "test_company_escape.plain",
    description = "Escape over a plain boolean",
    addon = "test_company_escape",
    company_access = direct(field = "company_id", branch = "exact", empty = "allow", escape = "flag")
)]
pub struct Plain {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(boolean, default = false, required)]
    pub flag: bool,
    #[field(many2one, comodel = "res.company")]
    pub company_id: Option<RecId>,
}
