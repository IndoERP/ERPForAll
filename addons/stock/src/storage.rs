// SPDX-License-Identifier: MIT OR Apache-2.0
//! Storage categories, their capacities and package types: tables only. The
//! capacity checks at putaway belong to, package behaviour to.

use erp_sdk::prelude::*;

/// `stock.storage.category`: limits on what the locations of a category hold.
#[model(
    name = "stock.storage.category",
    description = "Storage Category",
    order = "name",
    company_access = direct(field = "company_id", branch = "exact", empty = "allow"),
    constraints = [(
        "_positive_max_weight",
        "CHECK (max_weight >= 0)",
        "Maximum weight cannot be negative."
    )],
)]
pub struct StockStorageCategory {
    /// Category name.
    #[field(char, required = true, string = "Storage Category")]
    pub name: Option<Arc<str>>,
    /// Maximum weight a location of the category holds; 0 keeps its specified meaning.
    #[field(float, digits = "Stock Weight", string = "Max Weight")]
    pub max_weight: Decimal,
    /// Capacity rows by product or package type.
    #[field(
        one2many,
        comodel = "stock.storage.category.capacity",
        inverse_name = "storage_category_id",
        copy = true
    )]
    pub capacity_ids: Vec<RecId>,
    /// When a new product may enter a location of the category.
    #[field(
        selection,
        selection = [
            ("empty", "Only when the location is empty"),
            ("same", "Only when all products are the same"),
            ("mixed", "Allow mixed products")
        ],
        default = "mixed",
        required = true
    )]
    pub allow_new_product: Option<Arc<str>>,
    /// Locations of the category.
    #[field(
        one2many,
        comodel = "stock.location",
        inverse_name = "storage_category_id"
    )]
    pub location_ids: Vec<RecId>,
    /// Owning company; empty for a shared category.
    #[field(many2one, comodel = "res.company", string = "Company")]
    pub company_id: Option<RecId>,
}

/// `stock.storage.category.capacity`: how much of one product or one package type a location of
/// the category holds.
#[model(
    name = "stock.storage.category.capacity",
    description = "Storage Category Capacity",
    order = "storage_category_id",
    company_access = unrestricted(reason = "the reference ERP has no company rule for capacity rows; the category carries the company"),
    constraints = [
        (
            "_positive_quantity",
            "CHECK (quantity > 0)",
            "A capacity quantity must be greater than zero."
        ),
        (
            "_unique_product",
            "UNIQUE (product_id, storage_category_id)",
            "A storage category takes one capacity rule per product."
        ),
        (
            "_unique_package_type",
            "UNIQUE (package_type_id, storage_category_id)",
            "A storage category takes one capacity rule per package type."
        ),
    ],
)]
pub struct StockStorageCategoryCapacity {
    /// The category; deleting it deletes its capacities.
    #[field(
        many2one,
        comodel = "stock.storage.category",
        required = true,
        ondelete = "cascade",
        index = true
    )]
    pub storage_category_id: Option<RecId>,
    /// The product limited by this row.
    #[field(
        many2one,
        comodel = "product.product",
        ondelete = "cascade",
        index = "btree_not_null",
        check_company = true,
        string = "Product"
    )]
    pub product_id: Option<RecId>,
    /// The package type limited by this row.
    #[field(
        many2one,
        comodel = "stock.package.type",
        ondelete = "cascade",
        index = "btree_not_null",
        check_company = true,
        string = "Package Type"
    )]
    pub package_type_id: Option<RecId>,
    /// Maximum quantity, strictly positive. Exact numeric without scale (`digits = 0`): an exact-only
    /// addon stores no binary floats.
    #[field(float, digits = 0, required = true)]
    pub quantity: Decimal,
    /// The category's company, stored so the product and package type links are company-checked
    /// (51,55`).
    #[field(
        many2one,
        comodel = "res.company",
        related = "storage_category_id.company_id",
        store = true,
        string = "Company"
    )]
    pub company_id: Option<RecId>,
}

/// `stock.package.type`: a kind of box, pallet or tote.
#[model(
    name = "stock.package.type",
    description = "Stock package type",
    order = "sequence, id",
    company_access = unrestricted(
        reason = "package types carry no company rule yet; the stock access rows decide the restriction"
    ),
    constraints = [
        (
            "_barcode_uniq",
            "UNIQUE (barcode)",
            "This barcode already belongs to another package type."
        ),
        ("_positive_height", "CHECK (height >= 0.0)", "Height must be positive"),
        ("_positive_width", "CHECK (width >= 0.0)", "Width must be positive"),
        (
            "_positive_length",
            "CHECK (packaging_length >= 0.0)",
            "Length must be positive"
        ),
        (
            "_positive_max_weight",
            "CHECK (max_weight >= 0.0)",
            "The maximum weight of a package type cannot be negative."
        ),
    ],
)]
pub struct StockPackageType {
    /// Package type name.
    #[field(char, required = true, string = "Package Type")]
    pub name: Option<Arc<str>>,
    /// Display order; the first is the default.
    #[field(integer, default = 1)]
    pub sequence: i32,
    /// Numbering of its packages.
    #[field(
        many2one,
        comodel = "ir.sequence",
        copy = false,
        string = "Reference Sequence"
    )]
    pub sequence_id: Option<RecId>,
    /// Height. The dimensions and weights are exact numerics without scale (`digits = 0`).
    #[field(float, digits = 0)]
    pub height: Decimal,
    /// Width.
    #[field(float, digits = 0)]
    pub width: Decimal,
    /// Length.
    #[field(float, digits = 0, string = "Length")]
    pub packaging_length: Decimal,
    /// Weight of the empty package.
    #[field(float, digits = 0, string = "Weight")]
    pub base_weight: Decimal,
    /// Maximum weight shippable in this package.
    #[field(float, digits = 0, string = "Max Weight")]
    pub max_weight: Decimal,
    /// Barcode, unique across package types.
    #[field(char, copy = false)]
    pub barcode: Option<Arc<str>>,
    /// Owning company; empty for a shared type.
    #[field(many2one, comodel = "res.company", index = true, string = "Company")]
    pub company_id: Option<RecId>,
    /// Whether the package is emptied and reused.
    #[field(
        selection,
        selection = [
            ("disposable", "Disposable Box"),
            ("reusable", "Reusable Box (totes)")
        ],
        default = "disposable",
        required = true,
        string = "Package Use"
    )]
    pub package_use: Option<Arc<str>>,
    /// Capacities that name this package type.
    #[field(
        one2many,
        comodel = "stock.storage.category.capacity",
        inverse_name = "package_type_id",
        copy = true,
        string = "Storage Category Capacity"
    )]
    pub storage_category_capacity_ids: Vec<RecId>,
    /// Routes packages of this type follow.
    #[field(
        many2many,
        comodel = "stock.route",
        relation = "stock_package_type_stock_route_rel",
        column1 = "stock_package_type_id",
        column2 = "stock_route_id",
        string = "Routes"
    )]
    pub route_ids: Vec<RecId>,
}
