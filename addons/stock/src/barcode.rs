// SPDX-License-Identifier: MIT OR Apache-2.0
//! Inventory-owned terminal barcode types, independent of inventory transaction models.
use erp_addon_barcodes::models::BarcodeRule;
use erp_sdk::prelude::*;
#[extend(BarcodeRule, addon = "stock")]
struct StockBarcodeRule {
    #[field(selection, selection_add = [("weight","Weight"),("location","Location"),("lot","Lot"),("package","Package")], ondelete = [("weight","set default"),("location","set default"),("lot","set default"),("package","set default")])]
    pub r#type: Option<Arc<str>>,
}
