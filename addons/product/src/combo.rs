//! Product Combo models (`product.combo`, `product.combo.item`).

use erp_sdk::prelude::*;

/// `product.combo`: Configuration of selectable choices for a combo product.
#[model(
    name = "product.combo",
    description = "Product Combo",
    order = "sequence, id",
    company_access = direct(field = "company_id", branch = "ancestor", empty = "allow"),
)]
pub struct ProductCombo {
    /// Combo choice group name (e.g. 'Sides', 'Drinks').
    #[field(char, required = true, translate = true)]
    pub name: Option<Arc<str>>,

    /// Sequence order.
    #[field(integer, default = 1)]
    pub sequence: i32,

    /// Permissible product choices.
    #[field(one2many, comodel = "product.combo.item", inverse_name = "combo_id")]
    pub combo_item_ids: Vec<RecId>,

    /// Restricting company.
    #[field(many2one, comodel = "res.company", index = true)]
    pub company_id: Option<RecId>,
}

/// `product.combo.item`: A single product option in a combo choice group.
#[model(
    name = "product.combo.item",
    description = "Product Combo Item",
    order = "id",
    company_access = parent(edge = "combo_id"),
)]
pub struct ProductComboItem {
    /// Parent combo group.
    #[field(
        many2one,
        comodel = "product.combo",
        required = true,
        ondelete = "cascade",
        index = true
    )]
    pub combo_id: Option<RecId>,

    /// Product option.
    #[field(
        many2one,
        comodel = "product.product",
        required = true,
        ondelete = "restrict",
        index = true
    )]
    pub product_id: Option<RecId>,

    /// Extra fee when this option is selected.
    #[field(float, digits = "Product Price")]
    pub extra_price: Decimal,
}
