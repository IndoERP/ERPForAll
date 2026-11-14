// SPDX-License-Identifier: MIT OR Apache-2.0
//! Explicit boundaries for unfinished inventory features and addon setup.
use erp_sdk::prelude::*;
use erp_sdk::settings::{PanelItemDecl, PanelItemValue, ScopeKind, SettingsScope};

const GROUPS: &str = "Unavailable until a later release supplies feature-group changes.";
const TRACKING: &str =
    "Unavailable until a later release and a later release supply tracking controls.";
const ROUTES: &str = "Unavailable until a later release supplies route activation.";
const FEATURES: &[(&str, &str, &str, &str)] = &[
    (
        "stock.group_stock_multi_locations",
        "Multiple locations",
        "Warehouse",
        GROUPS,
    ),
    (
        "stock.group_stock_adv_location",
        "Advanced routes",
        "Warehouse",
        GROUPS,
    ),
    (
        "stock.group_stock_production_lot",
        "Lot and serial tracking",
        "Traceability",
        TRACKING,
    ),
    (
        "stock.group_stock_lot_print_gs1",
        "GS1 tracking labels",
        "Traceability",
        TRACKING,
    ),
    (
        "stock.group_lot_on_delivery_slip",
        "Tracking details on delivery documents",
        "Traceability",
        TRACKING,
    ),
    (
        "stock.group_stock_tracking_lot",
        "Packages",
        "Operations",
        GROUPS,
    ),
    (
        "stock.group_stock_tracking_owner",
        "Consignment",
        "Operations",
        GROUPS,
    ),
    (
        "stock.group_stock_picking_batch",
        "Batch transfers",
        "Operations",
        GROUPS,
    ),
    (
        "stock.group_warning_stock",
        "Partner stock warnings",
        "Operations",
        GROUPS,
    ),
    (
        "stock.group_stock_sign_delivery",
        "Delivery signatures",
        "Shipping",
        GROUPS,
    ),
    (
        "stock.replenish_on_order",
        "Replenish on order",
        "Warehouse",
        ROUTES,
    ),
];
const MODULES: &[(&str, &str, &str)] = &[
    (
        "stock.module_product_expiry",
        "Set up expiry dates in Addons",
        "Traceability",
    ),
    (
        "stock.module_stock_sms",
        "Set up text delivery in Addons",
        "Shipping",
    ),
    (
        "stock.module_delivery",
        "Set up carriers in Addons",
        "Shipping",
    ),
    (
        "stock.module_stock_fleet",
        "Set up transport in Addons",
        "Operations",
    ),
    (
        "stock.module_stock_dropshipping",
        "Set up direct supplier delivery",
        "Warehouse",
    ),
    (
        "stock.module_stock_barcode",
        "Set up scanning in Addons",
        "Barcode",
    ),
    (
        "stock.module_stock_barcode_barcodelookup",
        "Set up barcode lookup in Addons",
        "Barcode",
    ),
    (
        "stock.module_quality_control",
        "Set up quality checks in Addons",
        "Operations",
    ),
    (
        "stock.module_quality_control_worksheet",
        "Set up quality worksheets in Addons",
        "Operations",
    ),
];
fn items() -> Result<Vec<PanelItemDecl>> {
    let mut items = Vec::new();
    for (key, label, block, _) in FEATURES {
        items.push(
            PanelItemDecl::info(key, label)?
                .block(block)?
                .sequence(100)
                .scope(ScopeKind::Company)
                .access(&["stock.group_stock_manager", "base.group_system"])?,
        );
    }
    for (key, label, block) in MODULES {
        items.push(
            PanelItemDecl::action_link(key, label, "stock.inventory_addons_action", false)?
                .block(block)?
                .sequence(200)
                .access(&["base.group_system"])?,
        );
    }
    Ok(items)
}
fn resolve<'a>(
    _env: &'a Env,
    _tx: &'a mut Transaction,
    _scope: &'a SettingsScope,
    key: &'a str,
) -> erp_sdk::BoxFuture<'a, Result<PanelItemValue>> {
    Box::pin(async move {
        let (_, label, _, detail) = FEATURES
            .iter()
            .find(|(candidate, _, _, _)| *candidate == key)
            .ok_or_else(|| {
                Error::programming(format!("Inventory has no information row {key}."))
            })?;
        PanelItemValue::info(Value::str(format!("{label} (unavailable)")))?.detail(detail)
    })
}
erp_sdk::settings_panel_items!(items, resolve);

pub(super) fn require_linked() -> Result<()> {
    let expected: std::collections::BTreeSet<_> = FEATURES
        .iter()
        .map(|(key, _, _, _)| *key)
        .chain(MODULES.iter().map(|(key, _, _)| *key))
        .map(str::to_owned)
        .collect();
    let mut keys = std::collections::BTreeSet::new();
    for entry in erp_sdk::settings::PANEL_CONTRIBUTIONS
        .iter()
        .filter(|entry| entry.addon.name == "stock")
    {
        for item in (entry.declarations)()? {
            keys.insert(item.key().to_owned());
        }
    }
    if keys != expected {
        return Err(Error::programming(
            "Inventory settings panel registration is missing from erp-addon-stock.",
        ));
    }
    Ok(())
}
