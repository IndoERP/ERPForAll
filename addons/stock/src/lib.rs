//! Addon `stock` for ERPForAll.
//!
//! Following boundary §4.1 and Decision N8, `stock` manages warehouse topology,
//! quants, moves, pickings, and locations, but carries no valuation tables or fields.
//! Stock valuation belongs to the `stock_account` bridge addon.

erp_sdk::addon!(
    name = "stock",
    exact_only = true,
    depends = ["product", "barcodes_gs1_nomenclature"]
);
