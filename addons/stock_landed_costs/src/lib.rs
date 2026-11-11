//! Addon `stock_landed_costs` for ERPForAll.

erp_sdk::addon!(
    name = "stock_landed_costs",
    depends = ["stock_account", "purchase_stock"]
);
