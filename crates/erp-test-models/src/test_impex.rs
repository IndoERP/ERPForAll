use erp_sdk::prelude::*;

erp_sdk::addon!(name = "test_import_export");

/// `export.boolean`.
#[model(
    name = "export.boolean",
    description = "Export: boolean",
    addon = "test_import_export",
    rec_name = "value"
)]
pub struct ExportBoolean {
    #[field(integer, default = 4)]
    pub r#const: i32,
    #[field(boolean)]
    pub value: bool,
}

/// `export.integer`.
#[model(
    name = "export.integer",
    description = "Export: integer",
    addon = "test_import_export",
    rec_name = "value"
)]
pub struct ExportInteger {
    #[field(integer, default = 4)]
    pub r#const: i32,
    #[field(integer, default = 4)]
    pub value: i32,
}

/// `export.string.required`.
#[model(
    name = "export.string.required",
    description = "Export: string.required",
    addon = "test_import_export",
    rec_name = "value"
)]
pub struct ExportStringRequired {
    #[field(integer, default = 4)]
    pub r#const: i32,
    #[field(char, required)]
    pub value: Option<Arc<str>>,
}

/// `export.many2one`.
#[model(
    name = "export.many2one",
    description = "Export: many2one",
    addon = "test_import_export",
    rec_name = "value"
)]
pub struct ExportMany2one {
    #[field(integer, default = 4)]
    pub r#const: i32,
    #[field(many2one, comodel = "export.integer")]
    pub value: Option<RecId>,
}

/// `export.one2many`.
#[model(
    name = "export.one2many",
    description = "Export: one2many",
    addon = "test_import_export",
    rec_name = "const"
)]
pub struct ExportOne2many {
    #[field(integer, default = 4)]
    pub r#const: i32,
    #[field(
        one2many,
        comodel = "export.one2many.child",
        inverse_name = "parent_id"
    )]
    pub value: Vec<RecId>,
}

/// `export.one2many.child`.
#[model(
    name = "export.one2many.child",
    description = "Export One to Many Child",
    addon = "test_import_export",
    rec_name = "value"
)]
pub struct ExportOne2manyChild {
    #[field(many2one, comodel = "export.one2many")]
    pub parent_id: Option<RecId>,
    #[field(char)]
    pub str: Option<Arc<str>>,
    #[field(many2one, comodel = "export.integer")]
    pub m2o: Option<RecId>,
    #[field(integer)]
    pub value: i32,
    #[field(
        one2many,
        comodel = "export.one2many.grandchild",
        inverse_name = "parent_id"
    )]
    pub grandchild_ids: Vec<RecId>,
}

/// `export.one2many.grandchild`.
#[model(
    name = "export.one2many.grandchild",
    description = "Export One to Many Grandchild",
    addon = "test_import_export",
    rec_name = "value"
)]
pub struct ExportOne2manyGrandchild {
    #[field(many2one, comodel = "export.one2many.child")]
    pub parent_id: Option<RecId>,
    #[field(integer)]
    pub value: i32,
}

/// `export.one2many.multiple`.
#[model(
    name = "export.one2many.multiple",
    description = "Export One To Many Multiple",
    addon = "test_import_export"
)]
pub struct ExportOne2manyMultiple {
    #[field(integer, default = 36)]
    pub r#const: i32,
    #[field(
        one2many,
        comodel = "export.one2many.child.1",
        inverse_name = "parent_id"
    )]
    pub child1: Vec<RecId>,
    #[field(
        one2many,
        comodel = "export.one2many.child.2",
        inverse_name = "parent_id"
    )]
    pub child2: Vec<RecId>,
}

/// `export.one2many.child.1`.
#[model(
    name = "export.one2many.child.1",
    description = "Export One to Many Child 1",
    addon = "test_import_export",
    rec_name = "value"
)]
pub struct ExportOne2manyChild1 {
    #[field(many2one, comodel = "export.one2many.multiple")]
    pub parent_id: Option<RecId>,
    #[field(char)]
    pub str: Option<Arc<str>>,
    #[field(integer)]
    pub value: i32,
}

/// `export.one2many.child.2`.
#[model(
    name = "export.one2many.child.2",
    description = "Export One To Many Child 2",
    addon = "test_import_export",
    rec_name = "value"
)]
pub struct ExportOne2manyChild2 {
    #[field(many2one, comodel = "export.one2many.multiple")]
    pub parent_id: Option<RecId>,
    #[field(char)]
    pub str: Option<Arc<str>>,
    #[field(integer)]
    pub value: i32,
}

/// `export.many2many.other`.
#[model(
    name = "export.many2many.other",
    description = "Export Many to Many Other",
    addon = "test_import_export",
    rec_name = "value"
)]
pub struct ExportMany2manyOther {
    #[field(char)]
    pub str: Option<Arc<str>>,
    #[field(integer)]
    pub value: i32,
}

#[model(
    name = "export.mixed",
    description = "Export: mixed",
    addon = "test_import_export"
)]
pub struct ExportMixed {
    #[field(integer)]
    pub value: i32,
    #[field(boolean)]
    pub flag: bool,
    #[field(many2one, comodel = "export.integer")]
    pub m2o: Option<RecId>,
    #[field(many2many, comodel = "export.many2many.other")]
    pub m2m: Vec<RecId>,
}

/// A model referencing itself, for imports whose rows reference earlier rows of the same file.
#[model(
    name = "export.self",
    description = "Export: self reference",
    addon = "test_import_export",
    rec_name = "value"
)]
pub struct ExportSelf {
    #[field(integer)]
    pub value: i32,
    #[field(many2one, comodel = "export.self")]
    pub parent_id: Option<RecId>,
}

/// A self-referencing model with a required `name`: a record without a name fails at write time
/// (a flush error) while rows can still reference earlier rows by external id, which splits the
/// import into several flush groups (PERF.LOAD-STREAM batch tests).
#[model(
    name = "export.self.required",
    description = "Export: self.required",
    addon = "test_import_export",
    rec_name = "name"
)]
pub struct ExportSelfRequired {
    #[field(char, required)]
    pub name: Option<Arc<str>>,
    #[field(many2one, comodel = "export.self.required")]
    pub parent_id: Option<RecId>,
}

#[model(
    name = "export.typed",
    description = "Export: typed",
    addon = "test_import_export"
)]
pub struct ExportTyped {
    #[field(selection, selection = [("1", "Foo"), ("2", "Bar"), ("3", "Qux")])]
    pub selection: Option<Arc<str>>,
    #[field(date)]
    pub date: Option<Date>,
    #[field(datetime)]
    pub moment: Option<DateTime>,
    #[field(float)]
    pub float: f64,
    #[field(float, digits = (16, 3))]
    pub decimal: Decimal,
    #[field(monetary, currency_field = "currency_id")]
    pub amount: Decimal,
    #[field(many2one, comodel = "res.currency")]
    pub currency_id: Option<RecId>,
    #[field(json)]
    pub json: Option<erp_sdk::serde_json::Value>,
    #[field(reference, selection = [("export.integer", "integer")])]
    pub reference: Option<Arc<str>>,
}

/// `export.inherits.parent`.
#[model(
    name = "export.inherits.parent",
    description = "export.inherits.parent",
    addon = "test_import_export"
)]
pub struct ExportInheritsParent {
    #[field(integer)]
    pub value_parent: i32,
}

/// `export.inherits.child`.
#[model(
    name = "export.inherits.child",
    description = "export.inherits.child",
    inherits = [("export.inherits.parent", "parent_id")],
    addon = "test_import_export"
)]
pub struct ExportInheritsChild {
    #[field(
        many2one,
        comodel = "export.inherits.parent",
        required,
        ondelete = "cascade"
    )]
    pub parent_id: Option<RecId>,
    #[field(integer)]
    pub value: i32,
}
