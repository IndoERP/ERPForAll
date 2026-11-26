// SPDX-License-Identifier: MIT OR Apache-2.0
//! The Inventory master-data views, actions and menus are declared in the addon's data files.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

use std::fs;
use std::path::Path;

fn read(rel: &str) -> String {
    fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join(rel)).expect(rel)
}

/// (file, model, action id)
const SCREENS: [(&str, &str, &str); 11] = [
    (
        "stock_warehouse_views.xml",
        "stock.warehouse",
        "action_stock_warehouse",
    ),
    (
        "stock_location_views.xml",
        "stock.location",
        "action_stock_location",
    ),
    (
        "stock_operation_type_views.xml",
        "stock.picking.type",
        "action_stock_picking_type_config",
    ),
    ("stock_route_views.xml", "stock.route", "action_stock_route"),
    ("stock_route_views.xml", "stock.rule", "action_stock_rule"),
    ("stock_lot_views.xml", "stock.lot", "action_stock_lot"),
    (
        "stock_package_views.xml",
        "stock.package.type",
        "action_stock_package_type",
    ),
    (
        "stock_storage_views.xml",
        "stock.storage.category",
        "action_stock_storage_category",
    ),
    (
        "stock_storage_views.xml",
        "stock.putaway.rule",
        "action_stock_putaway_rule",
    ),
    (
        "stock_orderpoint_views.xml",
        "stock.warehouse.orderpoint",
        "action_stock_orderpoint",
    ),
    ("stock_quant_views.xml", "stock.quant", "action_stock_quant"),
];

/// Every master model has a list view, an action on its model, and the file is loaded by the manifest.
#[test]
fn master_models_declare_list_view_and_action() {
    let manifest = read("manifest.toml");
    for (file, model, action) in SCREENS {
        let xml = read(&format!("views/{file}"));
        assert!(
            manifest.contains(&format!("\"views/{file}\"")),
            "{file} not in manifest"
        );
        assert!(
            xml.contains(&format!("<field name=\"model\">{model}</field>")),
            "{model} view"
        );
        assert!(xml.contains(&format!("id=\"{action}\"")), "{action}");
        assert!(
            xml.contains(&format!("<field name=\"res_model\">{model}</field>")),
            "{action} res_model"
        );
        assert!(xml.contains("<list"), "{model} list");
    }
}

/// Models that are created by hand also have a form; the quant list and form are read-only.
#[test]
fn forms_exist_and_quant_list_is_read_only() {
    for (file, model) in [
        ("stock_warehouse_views.xml", "stock.warehouse"),
        ("stock_location_views.xml", "stock.location"),
        ("stock_operation_type_views.xml", "stock.picking.type"),
        ("stock_route_views.xml", "stock.rule"),
        ("stock_lot_views.xml", "stock.lot"),
        ("stock_package_views.xml", "stock.package.type"),
        ("stock_storage_views.xml", "stock.putaway.rule"),
        ("stock_orderpoint_views.xml", "stock.warehouse.orderpoint"),
    ] {
        let xml = read(&format!("views/{file}"));
        assert!(xml.contains(&format!("<field name=\"model\">{model}</field>")));
        assert!(xml.contains("<form"), "{model} form");
    }
    let quant = read("views/stock_quant_views.xml");
    assert!(quant.contains(r#"<list create="false" delete="false">"#));
    assert!(quant.contains(r#"<form string="Stock" create="false" delete="false">"#));
    assert!(quant.contains(r#"<field name="product_id" readonly="1"/>"#));
    assert!(quant.contains(r#"<field name="location_id" readonly="1"/>"#));
    assert!(quant.contains(r#"<field name="quantity" string="Quantity On Hand" readonly="1"/>"#));
    assert!(quant.contains(r#"<field name="view_mode">list,form</field>"#));
    assert!(quant.contains(r#"context="{'group_by': 'product_id'}""#));
    assert!(quant.contains(r#"context="{'group_by': 'location_id'}""#));
}

/// Each master action has a menu. Parent menus and actions load before their consumers.
#[test]
fn master_menus_point_at_every_action_and_load_last() {
    let manifest: toml::Table = read("manifest.toml").parse().unwrap();
    let data = manifest["data"].as_array().expect("manifest data list");
    let load_index = |file: &str| {
        data.iter()
            .position(|entry| entry.as_str() == Some(file))
            .expect(file)
    };
    let master_menu_file = "views/stock_menus_master.xml";
    let master_menu_index = load_index(master_menu_file);
    let menus = read(master_menu_file);
    for id in ["menu_stock_products", "menu_stock_reporting"] {
        assert!(menus.contains(&format!("id=\"{id}\"")), "{id}");
    }
    for (file, _, action) in SCREENS {
        assert!(
            menus.contains(&format!("action=\"{action}\"")),
            "menu for {action}"
        );
        let action_file = format!("views/{file}");
        assert!(
            load_index(&action_file) < master_menu_index,
            "{action_file} must load before {master_menu_file}"
        );
    }
    assert!(menus.contains(r#"action="product.product_template_action""#));
    assert!(menus.contains(r#"parent="menu_stock_config_settings""#));
    assert!(
        load_index("views/settings_views.xml") < master_menu_index,
        "Inventory and Configuration parents must load before the master menus"
    );
    let report_menu_index = load_index("views/stock_report_menus.xml");
    assert!(
        master_menu_index < report_menu_index,
        "the Reporting parent must load before the report menus"
    );
    assert!(
        load_index("data/stock_report_data.xml") < report_menu_index,
        "report definitions must load before their menu actions"
    );
}
