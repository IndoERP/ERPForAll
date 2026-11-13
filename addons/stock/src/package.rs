// SPDX-License-Identifier: MIT OR Apache-2.0
//! Packages (`stock.package`): table only. The stored computes over contained
//! quants and child packages (`complete_name`, `location_id`, `company_id`) and the name sequence
//! default are's.

use erp_sdk::prelude::*;

/// `stock.package`: a physical or logical container of quants and other packages.
#[model(
    name = "stock.package",
    description = "Package",
    order = "name, id",
    parent_store = true,
    parent_name = "parent_package_id"
)]
pub struct StockPackage {
    /// Package reference.
    #[field(
        char,
        required = true,
        copy = false,
        index = "trigram",
        string = "Package Reference"
    )]
    pub name: Option<Arc<str>>,
    /// Package type (dimensions, weights, sequence).
    #[field(
        many2one,
        comodel = "stock.package.type",
        index = true,
        string = "Package Type"
    )]
    pub package_type_id: Option<RecId>,
    /// Container this package is in.
    #[field(
        many2one,
        comodel = "stock.package",
        index = "btree_not_null",
        string = "Container"
    )]
    pub parent_package_id: Option<RecId>,
    /// Packages contained in this one.
    #[field(
        one2many,
        comodel = "stock.package",
        inverse_name = "parent_package_id",
        string = "Contained Packages"
    )]
    pub child_package_ids: Vec<RecId>,
    /// Container the package goes into at its destination.
    #[field(
        many2one,
        comodel = "stock.package",
        index = "btree_not_null",
        string = "Destination Container"
    )]
    pub package_dest_id: Option<RecId>,
    /// Materialized ancestry path maintained by the ORM parent store.
    #[field(char, index = true)]
    pub parent_path: Option<Arc<str>>,
    /// Total weight of the package, when set by hand.
    #[field(float, digits = "Stock Weight", string = "Shipping Weight")]
    pub shipping_weight: Decimal,
    /// Date the package was packed.
    #[field(date, string = "Pack Date")]
    pub pack_date: Option<Date>,
}
