//! Addon `stock_account` for ERPForAll.
//!
//! Following boundary §4.1 and Decision N8, `stock_account` is the bridge addon
//! hosting stock valuation (engine erp-stockval, value journal, cost state, FIFO layers,
//! and move/lot/quant/location valuation fields).

erp_sdk::addon!(
    name = "stock_account",
    exact_only = true,
    depends = ["stock"]
);
