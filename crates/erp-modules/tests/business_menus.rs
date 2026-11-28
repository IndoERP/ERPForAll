// SPDX-License-Identifier: MIT OR Apache-2.0
//! Resolve shipped menus in installation order, including records contributed by other addons.
use std::{collections::BTreeMap, fs, path::Path};

use erp_data::{DataOperation, FieldValue, MenuItemOp, parse_xml_str};
use erp_modules::{AddonIndex, LoadMode, ModuleGraph, ModuleState};

#[derive(Default)]
struct Menu {
    parent: Option<String>,
    action: Option<String>,
    icon: Option<String>,
    sequence: i32,
    active: bool,
}

fn declare(menus: &mut BTreeMap<String, Menu>, module: &str, item: &MenuItemOp) {
    let menu = menus.entry(item.id().complete_name(module)).or_default();
    menu.parent = item.parent().map(|id| id.complete_name(module));
    menu.active = item.active().unwrap_or(true);
    menu.sequence = item.sequence().unwrap_or(10);
    if let Some(action) = item.action() {
        menu.action = Some(action.complete_name(module));
    }
    if let Some(icon) = item.web_icon() {
        menu.icon = Some(icon.to_owned());
    }
    for child in item.children() {
        declare(menus, module, child);
    }
}

#[test]
fn shipped_business_apps_have_operational_homes_and_active_menus_have_icons() {
    let addons = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../addons");
    let mut index = AddonIndex::new(true);
    index.add_dev_path(&addons);
    let manifests = index.discover_all().unwrap();
    let states = manifests
        .keys()
        .map(|name| (name.clone(), ModuleState::Installed))
        .collect::<BTreeMap<_, _>>();
    let (graph, dropped) = ModuleGraph::build(&manifests, &states, LoadMode::Load).unwrap();
    assert!(dropped.is_empty(), "{dropped:?}");

    let mut menus = BTreeMap::new();
    for node in graph.plain_load_order() {
        let module = node.name().as_str();
        for path in node
            .manifest()
            .data()
            .iter()
            .filter(|path| path.ends_with(".xml"))
        {
            let xml = fs::read_to_string(addons.join(module).join(path)).unwrap();
            for op in parse_xml_str(&xml, path, module, false).unwrap() {
                match op {
                    DataOperation::MenuItem(item) => declare(&mut menus, module, &item),
                    DataOperation::Record(record) if record.model() == "ir.ui.menu" => {
                        let key = record.id().unwrap().complete_name(module);
                        let menu = menus
                            .get_mut(&key)
                            .unwrap_or_else(|| panic!("Undeclared menu: {key}"));
                        for field in record.fields() {
                            match (field.name(), field.value()) {
                                ("web_icon", FieldValue::Text(icon)) => {
                                    menu.icon = Some(icon.clone())
                                }
                                ("active", FieldValue::Eval { expr, .. }) => {
                                    menu.active = match expr.as_str() {
                                        "True" => true,
                                        "False" => false,
                                        expr => {
                                            panic!("Unhandled active expression for {key}: {expr}")
                                        }
                                    }
                                }
                                (
                                    name @ ("action" | "active" | "parent_id" | "web_icon"
                                    | "sequence"),
                                    _,
                                ) => panic!("Unhandled menu update: {key}.{name}"),
                                _ => {}
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    let missing: Vec<_> = menus
        .iter()
        .filter(|(_, m)| m.active && m.icon.as_deref().is_none_or(str::is_empty))
        .map(|(id, _)| id)
        .collect();
    assert!(
        missing.is_empty(),
        "Missing first-party menu icons: {missing:?}"
    );
    assert!(!menus["crm.menu_submit_lead"].active);
    for (root, action) in [
        ("crm.menu_crm", "crm.overview_action"),
        (
            "purchase.menu_purchase_root",
            "purchase.purchase_overview_action",
        ),
        (
            "contacts.menu_contacts_root",
            "contacts.contacts_overview_action",
        ),
        (
            "product.menu_product_root",
            "product.catalog_overview_action",
        ),
        ("hr.hr_menu_root", "hr.employee_overview_action"),
        ("approvals.approvals_menu", "approvals.overview_window"),
        (
            "account.menu_finance",
            "account.open_account_journal_dashboard_kanban",
        ),
        ("stock.menu_stock_root", "stock.stock_picking_type_action"),
        ("sale.sale_menu_root", "sale.action_sale_teams_dashboard"),
        (
            "mail.discuss_workspace_menu",
            "mail.discuss_workspace_action",
        ),
        ("ai_base.workspace_menu", "ai_base.workspace_action"),
        ("base.settings_menu", "base.settings_action"),
    ] {
        let menu = &menus[root];
        assert!(
            menu.active && menu.parent.is_none(),
            "{root} must be an active app"
        );
        let first_action = menu.action.as_ref().or_else(|| {
            menus
                .values()
                .filter(|child| child.active && child.parent.as_deref() == Some(root))
                .min_by_key(|child| child.sequence)?
                .action
                .as_ref()
        });
        assert_eq!(first_action.map(String::as_str), Some(action), "{root}");
    }
}
