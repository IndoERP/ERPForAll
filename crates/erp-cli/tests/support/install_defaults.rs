// SPDX-License-Identifier: MIT OR Apache-2.0
//! The install-time default records of `account`, `stock`, `sales_team` and `sale`, as (query, expected rows): one
//! set of checks for the direct install tests and the served-node install test.
#![allow(
    dead_code,
    reason = "each test binary uses the checks of its own addons"
)]

/// The master account tags, the Accounting menus and the accounting groups.
pub const ACCOUNT: &[(&str, &[&str])] = &[
    (
        "SELECT d.name || '=' || t.name || '/' || t.applicability FROM ir_model_data d \
         JOIN account_account_tag t ON t.id = d.res_id \
         WHERE d.module = 'account' AND d.model = 'account.account.tag' ORDER BY d.name",
        &[
            "account_tag_financing=Financing Activities/accounts",
            "account_tag_investing=Investing & Extraordinary Activities/accounts",
            "account_tag_operating=Operating Activities/accounts",
        ],
    ),
    (
        "SELECT name FROM ir_model_data WHERE module = 'account' AND model = 'ir.ui.menu' ORDER BY name",
        &[
            "menu_account_asset",
            "menu_account_asset_category",
            "menu_account_asset_dep_run",
            "menu_account_asset_settlement",
            "menu_account_budget",
            "menu_account_budget_amendment",
            "menu_account_budget_commitment",
            "menu_account_budget_profile",
            "menu_account_cash_rounding",
            "menu_account_fiscal_period",
            "menu_account_followup_level",
            "menu_account_followup_report",
            "menu_account_incoterms",
            "menu_account_recurring_entry",
            "menu_account_settings",
            "menu_account_tag",
            "menu_action_account_fiscal_position_form",
            "menu_action_account_form",
            "menu_action_account_journal_form",
            "menu_action_account_journal_group_list",
            "menu_action_account_moves_all",
            "menu_action_account_payments_payable",
            "menu_action_account_payments_receivable",
            "menu_action_move_in_invoice_type",
            "menu_action_move_in_refund_type",
            "menu_action_move_journal_line",
            "menu_action_move_out_invoice_type",
            "menu_action_move_out_refund_type",
            "menu_action_payment_term",
            "menu_analytic_accounts",
            "menu_analytic_distribution_models",
            "menu_analytic_items",
            "menu_analytic_plans",
            "menu_analytic_skf",
            "menu_bank_reconciliation",
            "menu_bank_statements",
            "menu_batch_payment_purchases",
            "menu_batch_payment_sales",
            "menu_board_journal_1",
            "menu_consolidation_groups",
            "menu_consolidation_root",
            "menu_consolidation_runs",
            "menu_cost_allocation_cycles",
            "menu_cost_allocation_runs",
            "menu_finance",
            "menu_finance_configuration",
            "menu_finance_entries",
            "menu_finance_payables",
            "menu_finance_receivables",
            "menu_tax_groups",
            "menu_taxes",
        ],
    ),
    (
        "SELECT d.name || '=' || (t.name->>'en_US') || '/' || (SELECT count(*) FROM account_payment_term_line l WHERE l.payment_id = t.id) \
         FROM ir_model_data d JOIN account_payment_term t ON t.id = d.res_id \
         WHERE d.module = 'account' AND d.model = 'account.payment.term' ORDER BY t.sequence",
        &[
            "account_payment_term_immediate=Immediate Payment/1",
            "account_payment_term_15days=15 Days/1",
            "account_payment_term_21days=21 Days/1",
            "account_payment_term_30days=30 Days/1",
            "account_payment_term_45days=45 Days/1",
            "account_payment_term_end_following_month=End of Following Month/1",
            "account_payment_term_10days_after_end_next_month=10 Days after the Following Month Ends/1",
            "account_payment_term_advance_60days=30% Now, Balance 60 Days/2",
            "account_payment_term_90days_on_the_10th=90 days, on the 10th/1",
        ],
    ),
    (
        "SELECT name FROM ir_model_data WHERE module = 'account' AND model = 'res.groups' ORDER BY name",
        &[
            "group_account_manager",
            "group_account_readonly",
            "group_account_user",
            "group_asset_cip",
            "group_budget_avc",
            "group_budget_override",
            "group_cost_center_controlling",
            "group_group_consolidation",
        ],
    ),
    (
        "SELECT d.name || '=' || a.res_model || '/' || a.domain || '/' || a.context FROM ir_model_data d \
         JOIN ir_act_window a ON a.id = d.res_id WHERE d.module = 'account' \
         AND d.model = 'ir.actions.act_window' AND d.name LIKE 'action\\_move\\_%\\_type' ORDER BY d.name",
        &[
            "action_move_in_invoice_type=account.move/[('move_type', '=', 'in_invoice')]/{'default_move_type': 'in_invoice'}",
            "action_move_in_refund_type=account.move/[('move_type', '=', 'in_refund')]/{'default_move_type': 'in_refund'}",
            "action_move_out_invoice_type=account.move/[('move_type', '=', 'out_invoice')]/{'default_move_type': 'out_invoice'}",
            "action_move_out_refund_type=account.move/[('move_type', '=', 'out_refund')]/{'default_move_type': 'out_refund'}",
        ],
    ),
    (
        "SELECT d.name || '=' || v.model || '/' || v.type FROM ir_model_data d JOIN ir_ui_view v \
         ON v.id = d.res_id WHERE d.module = 'account' AND d.model = 'ir.ui.view' \
         AND d.name LIKE 'view\\_invoice\\_%' ORDER BY d.name",
        &[
            "view_invoice_form=account.move/form",
            "view_invoice_line_form=account.move.line/form",
            "view_invoice_line_list=account.move.line/list",
            "view_invoice_list=account.move/list",
            "view_invoice_search=account.move/search",
        ],
    ),
];

/// The default removal strategies, shared locations, the MTO route, the topology version row
/// and the main company's warehouse with its eight
/// operation types, two of them active.
pub const STOCK: &[(&str, &[&str])] = &[
    (
        "SELECT d.name || '=' || r.method || '/' || (r.name->>'en_US') FROM ir_model_data d \
         JOIN product_removal r ON r.id = d.res_id \
         WHERE d.module = 'stock' AND d.model = 'product.removal' ORDER BY d.name",
        &[
            "removal_closest=closest/Closest Location",
            "removal_fifo=fifo/First In First Out (FIFO)",
            "removal_least_packages=least_packages/Least Packages",
            "removal_lifo=lifo/Last In First Out (LIFO)",
        ],
    ),
    (
        "SELECT d.name || '=' || l.usage || '/' || l.complete_name || '/' || l.active::text \
         || '/' || coalesce(l.company_id::text, 'shared') || '/' || count(p.id)::text \
         FROM ir_model_data d JOIN stock_location l ON l.id = d.res_id \
         LEFT JOIN stock_location_policy p ON p.location_id = l.id AND NOT p.forbid_negative \
         WHERE d.module = 'stock' AND d.model = 'stock.location' \
         GROUP BY d.name, l.id ORDER BY d.name",
        &[
            "stock_location_customers=customer/Customers/true/shared/1",
            "stock_location_inter_company=transit/Inter-company transit/false/shared/1",
            "stock_location_inventory=inventory/Virtual Locations/Inventory adjustment/true/shared/1",
            "stock_location_scrap=inventory/Virtual Locations/Scrap/true/shared/1",
            "stock_location_suppliers=supplier/Vendors/true/shared/1",
            "stock_location_virtual=view/Virtual Locations/true/shared/1",
        ],
    ),
    (
        "SELECT d.name || '=' || (r.name->>'en_US') || '/' || r.active::text || '/' || \
         r.sequence::text || '/' || r.product_selectable::text || '/' || \
         r.product_categ_selectable::text || '/' || coalesce(r.company_id::text, 'shared') \
         FROM ir_model_data d JOIN stock_route r ON r.id = d.res_id \
         WHERE d.module = 'stock' AND d.model = 'stock.route' ORDER BY d.name",
        &["route_warehouse0_mto=Replenish on Order (MTO)/false/5/true/true/shared"],
    ),
    (
        "SELECT d.name || '=' || v.generation::text FROM stock_topology_version v \
         LEFT JOIN ir_model_data d ON d.res_id = v.id AND d.model = 'stock.topology.version'",
        &["stock_topology_version=0"],
    ),
    (
        "SELECT w.name || '/' || w.code || '/' || count(t.id) FILTER (WHERE t.active)::text \
         || '/' || count(t.id)::text FROM stock_warehouse w \
         LEFT JOIN stock_picking_type t ON t.warehouse_id = w.id GROUP BY w.id ORDER BY w.id",
        &["Main Company/Main/2/8"],
    ),
    (
        "SELECT w.code || '/' || s.action || '/' || s.procure_method || '/' || t.code \
         FROM stock_warehouse w JOIN stock_route r ON r.id = w.delivery_route_id \
         JOIN stock_rule s ON s.route_id = r.id AND s.company_id = w.company_id \
         JOIN stock_picking_type t ON t.id = s.picking_type_id ORDER BY w.id, s.id",
        &["Main/pull/make_to_stock/outgoing"],
    ),
];

/// The Sales privilege and its groups, the access rows, the default team (no "Website" or "Point
/// of Sale" team) and no menu of `sales_team` (B1).
pub const SALES_TEAM: &[(&str, &[&str])] = &[
    (
        "SELECT d.name || '=' || g.name || '/' || g.sequence::text FROM ir_model_data d \
         JOIN res_groups g ON g.id = d.res_id \
         WHERE d.module = 'sales_team' AND d.model = 'res.groups' ORDER BY d.name",
        &[
            "group_sale_manager=Administrator/30",
            "group_sale_salesman=User: Own Documents Only/10",
            "group_sale_salesman_all_leads=User: All Documents/20",
        ],
    ),
    (
        "SELECT name FROM ir_model_data WHERE module = 'sales_team' AND model = 'res.groups.privilege'",
        &["res_groups_privilege_sales"],
    ),
    (
        "SELECT r FROM (SELECT m.model || '/' || coalesce(g.module || '.' || g.name, '-') || '/' \
         || a.operation AS r FROM ir_access a JOIN ir_model m ON m.id = a.model_id \
         LEFT JOIN ir_model_data g ON g.model = 'res.groups' AND g.res_id = a.group_id \
         WHERE m.model IN ('crm.team', 'crm.team.member', 'crm.tag') AND NOT coalesce(a.generated, false)) access_rows \
         ORDER BY r COLLATE \"C\"",
        &[
            "crm.tag/sales_team.group_sale_manager/crud",
            "crm.tag/sales_team.group_sale_salesman/cru",
            "crm.team.member/base.group_user/r",
            "crm.team.member/sales_team.group_sale_manager/crud",
            "crm.team/base.group_user/r",
            "crm.team/sales_team.group_sale_manager/crud",
        ],
    ),
    (
        "SELECT (name->>'en_US') || '/' || sequence::text || '/' || coalesce(company_id::text, 'shared') \
         || '/' || active::text FROM crm_team ORDER BY id",
        &["Sales/0/shared/true"],
    ),
    (
        "SELECT count(*)::text FROM ir_model_data WHERE module = 'sales_team' AND model = 'ir.ui.menu'",
        &["0"],
    ),
];

/// The order sequence and the Sales administrator's access row of `sale` (B5).
pub const SALE: &[(&str, &[&str])] = &[
    (
        "SELECT d.name || '=' || s.code || '/' || s.prefix || '/' || s.padding::text || '/' \
         || s.number_next::text || '/' || s.number_increment::text || '/' \
         || coalesce(s.company_id::text, 'shared') \
         FROM ir_model_data d JOIN ir_sequence s ON s.id = d.res_id \
         WHERE d.module = 'sale' AND d.model = 'ir.sequence' ORDER BY d.name",
        &["seq_sale_order=sale.order/S/5/1/1/shared"],
    ),
    (
        "SELECT r FROM (SELECT m.model || '/' || coalesce(g.module || '.' || g.name, '-') || '/' \
         || a.operation AS r FROM ir_access a JOIN ir_model m ON m.id = a.model_id \
         LEFT JOIN ir_model_data g ON g.model = 'res.groups' AND g.res_id = a.group_id \
         WHERE m.model IN ('sale.order', 'sale.order.line') AND NOT coalesce(a.generated, false)) access_rows \
         ORDER BY r COLLATE \"C\"",
        &[
            "sale.order.line/sales_team.group_sale_manager/crud",
            "sale.order.line/sales_team.group_sale_salesman/crud",
            "sale.order.line/sales_team.group_sale_salesman_all_leads/crud",
            "sale.order/sales_team.group_sale_manager/crud",
            "sale.order/sales_team.group_sale_salesman/cru",
            "sale.order/sales_team.group_sale_salesman_all_leads/cru",
        ],
    ),
    (
        "SELECT d.name || '=' || g.name || '/' \
         || (SELECT count(*) FROM res_groups_implied_rel r WHERE r.gid = g.id)::text \
         FROM ir_model_data d JOIN res_groups g ON g.id = d.res_id \
         WHERE d.module = 'sale' AND d.model = 'res.groups' ORDER BY d.name",
        &[
            "group_auto_done_setting=Confirmed sales orders locked/0",
            "group_discount_per_so_line=Line discounts on sales orders/0",
            "group_proforma_sales=Sales pro-forma invoices/0",
            "group_services_and_material=Services and materials on sales orders/0",
            "group_warning_sale=Sales warnings on products and customers/0",
        ],
    ),
];
