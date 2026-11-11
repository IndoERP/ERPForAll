//! Addon `stock_delivery` for ERPForAll.

erp_sdk::addon!(
    name = "stock_delivery",
    depends = ["sale_stock", "delivery"]
);
