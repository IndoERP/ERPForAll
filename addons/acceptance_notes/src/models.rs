//! Ordinary company-scoped records for exercising native addon installation and generic views.

use erp_sdk::ValueMap;
use erp_sdk::prelude::*;

/// A note belongs to a company and an author; access is declared in the addon's security data.
#[model(
    name = "acceptance_notes.note",
    description = "Note",
    company_access = direct(field = "company_id", branch = "exact", empty = "deny"),
    order = "id desc"
)]
pub struct Note {
    /// Human-readable title.
    #[field(char, required = true)]
    pub name: Option<Arc<str>>,
    /// Plain text content; never interpreted as HTML.
    #[field(text)]
    pub body: Option<Arc<str>>,
    /// Current company, validated again by ORM access and relation consistency.
    #[field(
        many2one,
        comodel = "res.company",
        required = true,
        ondelete = "restrict"
    )]
    pub company_id: Option<RecId>,
    /// Record owner. Internal users can access their own notes; administrators can manage notes.
    #[field(
        many2one,
        comodel = "res.users",
        required = true,
        ondelete = "restrict"
    )]
    pub owner_id: Option<RecId>,
    /// Priority displayed by the generic selection widget.
    #[field(selection, selection = [("low", "Low"), ("normal", "Normal"), ("high", "High")], default = "normal")]
    pub priority: Option<Arc<str>>,
    /// Archived notes are hidden by the ordinary ORM active filter.
    #[field(boolean, default = true)]
    pub active: bool,
    /// Tests field-level access on both descriptors and direct RPC projections.
    #[field(text, groups = "base.group_system")]
    pub internal_memo: Option<Arc<str>>,
}

#[methods(Note, addon = "acceptance_notes")]
impl Note {
    /// Supply company and owner from the bound caller without replacing explicit context defaults.
    ///
    /// # Errors
    /// Propagates framework default resolution and unauthorized company selection errors.
    #[method(override)]
    async fn default_get(
        this: &RecordSet<Note>,
        tx: &mut Transaction,
        names: Vec<String>,
        sup: Super,
    ) -> Result<ValueMap> {
        let wants_owner = names.iter().any(|name| name == "owner_id");
        let wants_company = names.iter().any(|name| name == "company_id");
        let mut values = sup.call(tx, this.as_records(), (names,)).await?;
        if wants_owner && !values.contains_key("owner_id") {
            values.insert("owner_id".to_owned(), Value::Int(this.env().uid()));
        }
        if wants_company
            && !values.contains_key("company_id")
            && let Some(company) = this.env().company_id()?
        {
            values.insert("company_id".to_owned(), Value::Int(company));
        }
        Ok(values)
    }
}
