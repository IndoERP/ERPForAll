//! `ir.model`, `ir.model.fields`, and `ir.model.relation` reflection models (MODEL §4.6).

use erp_sdk::prelude::*;

/// `ir.model`.
#[model(name = "ir.model", description = "Models")]
pub struct IrModel {
    /// Model description.
    #[field(char, required = true)]
    pub name: Option<Arc<str>>,
    /// Model name (e.g. `res.partner`).
    #[field(char, required = true, index = true)]
    pub model: Option<Arc<str>>,
    /// Docstring or information.
    #[field(text)]
    pub info: Option<Arc<str>>,
    /// Model state (`base` or `manual`).
    #[field(selection, selection = [("manual", "Custom"), ("base", "Base")], default = "base", required = true)]
    pub state: Option<Arc<str>>,
    /// Whether this is a transient model.
    #[field(boolean, default = false)]
    pub transient: bool,
    /// Registered fields.
    #[field(one2many, comodel = "ir.model.fields", inverse_name = "model_id")]
    pub field_id: Vec<RecId>,
    /// The domain text of the generated company restriction the last module operation emitted for
    /// this model (A1; design `m3-company-a1.md` §6.3): the tamper ledger. Empty when
    /// the operation emitted none. An ERPForAll addition, written by module operations only.
    #[field(text, readonly = true, copy = false)]
    pub company_restriction: Option<Arc<str>>,
}

/// `ir.model.fields`.
#[model(name = "ir.model.fields", description = "Fields")]
pub struct IrModelFields {
    /// Field technical name.
    #[field(char, required = true)]
    pub name: Option<Arc<str>>,
    /// Model reference.
    #[field(many2one, comodel = "ir.model", required = true, ondelete = "cascade")]
    pub model_id: Option<RecId>,
    /// Model name string.
    #[field(char, required = true, index = true)]
    pub model: Option<Arc<str>>,
    /// Human-readable label.
    #[field(char)]
    pub field_description: Option<Arc<str>>,
    /// Help tooltip.
    #[field(text)]
    pub help: Option<Arc<str>>,
    /// Type name (`char`, `integer`, etc.).
    #[field(char, required = true)]
    pub ttype: Option<Arc<str>>,
    /// Relational target model.
    #[field(char)]
    pub relation: Option<Arc<str>>,
    /// Inverse field name (one2many).
    #[field(char)]
    pub relation_field: Option<Arc<str>>,
    /// Many2many intermediate table.
    #[field(char)]
    pub relation_table: Option<Arc<str>>,
    /// Many2many column 1.
    #[field(char)]
    pub column1: Option<Arc<str>>,
    /// Many2many column 2.
    #[field(char)]
    pub column2: Option<Arc<str>>,
    /// Field state (`base` or `manual`).
    #[field(selection, selection = [("manual", "Custom"), ("base", "Base")], default = "base", required = true)]
    pub state: Option<Arc<str>>,
    /// Index kind (e.g. `btree`).
    #[field(char)]
    pub index: Option<Arc<str>>,
    /// Stored in database column.
    #[field(boolean, default = true)]
    pub store: bool,
    /// Readonly flag.
    #[field(boolean, default = false)]
    pub readonly: bool,
    /// Required flag.
    #[field(boolean, default = false)]
    pub required: bool,
    /// Copied on duplicate.
    #[field(boolean, default = true)]
    pub copied: bool,
    /// Related field path.
    #[field(char)]
    pub related: Option<Arc<str>>,
    /// Selectable in views.
    #[field(boolean, default = true)]
    pub selectable: bool,
    /// Translatable flag.
    #[field(boolean, default = false)]
    pub translate: bool,
    /// Selection values for selection fields.
    #[field(
        one2many,
        comodel = "ir.model.fields.selection",
        inverse_name = "field_id"
    )]
    pub selection_ids: Vec<RecId>,
}

/// `ir.model.relation`.
#[model(name = "ir.model.relation", description = "Model Relations")]
pub struct IrModelRelation {
    /// Relational table name.
    #[field(char, required = true)]
    pub name: Option<Arc<str>>,
    /// Owning model.
    #[field(many2one, comodel = "ir.model", required = true, ondelete = "cascade")]
    pub model_id: Option<RecId>,
    /// Module name.
    #[field(char)]
    pub module: Option<Arc<str>>,
}

/// `ir.model.fields.selection`.
#[model(
    name = "ir.model.fields.selection",
    description = "Fields Selection",
    order = "sequence, id",
    constraints = [
        (
            "_selection_field_uniq",
            "UNIQUE (field_id, value)",
            "Selections values must be unique per field"
        )
    ]
)]
pub struct IrModelFieldsSelection {
    /// Field reference.
    #[field(
        many2one,
        comodel = "ir.model.fields",
        required = true,
        ondelete = "cascade",
        index = true
    )]
    pub field_id: Option<RecId>,
    /// Stored selection value key.
    #[field(char, required = true)]
    pub value: Option<Arc<str>>,
    /// Selection display label (translated).
    #[field(char, translate = true, required = true)]
    pub name: Option<Arc<str>>,
    /// Display sequence order.
    #[field(integer, default = 1000)]
    pub sequence: i32,
}
