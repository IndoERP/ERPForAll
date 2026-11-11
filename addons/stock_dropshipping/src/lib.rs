//! Addon `stock_dropshipping` for ERPForAll.

erp_sdk::addon!(
    name = "stock_dropshipping",
    depends = ["purchase_stock", "sale_stock"]
);
