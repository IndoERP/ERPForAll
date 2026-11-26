// SPDX-License-Identifier: MIT OR Apache-2.0

use super::*;

/// `active` of the menu `module.name`, and the external names of its children by sequence.
async fn menu(tx: &mut Transaction, module: &str, name: &str) -> (bool, Vec<String>) {
    let id = xmlid(tx, module, name).await;
    let active: bool = tx
        .query("SELECT active FROM ir_ui_menu WHERE id = $1", &[&id])
        .await
        .unwrap()[0]
        .get(0);
    let children = tx
        .query(
            "SELECT d.module || '.' || d.name FROM ir_ui_menu m \
               JOIN ir_model_data d ON d.model = 'ir.ui.menu' AND d.res_id = m.id \
              WHERE m.parent_id = $1 ORDER BY m.sequence, m.id",
            &[&id],
        )
        .await
        .unwrap()
        .iter()
        .map(|row| row.get(0))
        .collect();
    (active, children)
}

#[test]
fn the_sales_application_shows_the_sales_menu() {
    run(async {
        let (_db, system, orm) = cloned_sale_system().await;
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let (active, children) = menu(&mut tx, "sale", "sale_menu_root").await;
        assert!(!active, "sale alone keeps its root menu hidden");
        assert_eq!(
            children,
            [
                "sale.sale_order_menu",
                "sale.product_menu_catalog",
                "sale.menu_sale_report",
                "sale.menu_sale_config",
            ]
        );
        let (_, orders) = menu(&mut tx, "sale", "sale_order_menu").await;
        assert_eq!(
            orders,
            [
                "sale.menu_sale_quotations",
                "sale.menu_sale_order",
                "sale.menu_sale_teams_dashboard",
                "sale.res_partner_menu",
            ]
        );
        let (_, config) = menu(&mut tx, "sale", "menu_sale_config").await;
        assert_eq!(
            config,
            [
                "sale.menu_sale_settings",
                "sale.sales_team_config",
                "sale.menu_sale_order_template",
            ]
        );
        tx.rollback().await.unwrap();

        let application = [ModuleName::new("sale_management").unwrap()];
        let installed = Box::pin(
            system.button_immediate_install(&system.system_env(Context::new()), &application),
        )
        .await
        .unwrap();
        let mut tx = installed.orm.begin(SUPERUSER_ID).await.unwrap();
        assert!(menu(&mut tx, "sale", "sale_menu_root").await.0);
        tx.rollback().await.unwrap();

        let removed = Box::pin(
            system.button_immediate_uninstall(&system.system_env(Context::new()), &application),
        )
        .await
        .unwrap();
        assert!(removed.uninstalled.contains(&application[0]));
        let mut tx = removed.orm.begin(SUPERUSER_ID).await.unwrap();
        // no uninstall hook: the menu stays shown, and every entry still opens, while sale is
        // installed (a data-only application cannot undo its data record)
        assert!(menu(&mut tx, "sale", "sale_menu_root").await.0);
    });
}

#[test]
fn quotations_and_orders_open_without_a_fixed_state_domain() {
    run(async {
        let (_db, orm) = cloned_sale().await;
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        for name in ["action_quotations", "action_orders"] {
            let action = xmlid(&mut tx, "sale", name).await;
            let actions = tx
                .env()
                .records("ir.actions.act_window")
                .unwrap()
                .browse([action])
                .unwrap();
            for field in ["domain", "context"] {
                let value = actions.cell(&mut tx, field).await.unwrap();
                assert!(
                    match &value {
                        Cell::Null | Cell::Bool(false) => true,
                        Cell::Str(text) => matches!(text.trim(), "" | "{}" | "[]"),
                        _ => false,
                    },
                    "{name}.{field} is {value:?}"
                );
            }
        }
        // a new record of the actions is a quotation
        let world = world(&mut tx).await;
        let quotation = new_order(&mut tx, &world).await;
        assert_eq!(text(&mut tx, &quotation, "state").await, "draft");
    });
}
