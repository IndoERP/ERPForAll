//! `ir.default`: user-defined default values for fields (BASE §4.1).

use erp_sdk::prelude::*;

/// `ir.default`.
#[model(name = "ir.default", description = "Default Values", company_access = direct(field = "company_id", branch = "exact", empty = "allow"))]
pub struct IrDefault {
    /// Target field.
    #[field(
        many2one,
        comodel = "ir.model.fields",
        required = true,
        ondelete = "cascade"
    )]
    pub field_id: Option<RecId>,
    /// User scope (null for global).
    #[field(many2one, comodel = "res.users", ondelete = "cascade")]
    pub user_id: Option<RecId>,
    /// Company scope (null for global).
    #[field(many2one, comodel = "res.company", ondelete = "cascade")]
    pub company_id: Option<RecId>,
    /// Optional condition string (e.g. "field=value").
    #[field(char)]
    pub condition: Option<Arc<str>>,
    /// JSON-serialized value.
    #[field(char, required = true)]
    pub json_value: Option<Arc<str>>,
}
