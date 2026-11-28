// SPDX-License-Identifier: MIT OR Apache-2.0
//! Implementation guidance only. Business operations still run in their owning addon.

/// A master-data or process entry point and an observable acceptance criterion.
pub struct Scenario {
    pub title: &'static str,
    pub model: &'static str,
    pub setup: &'static str,
    pub acceptance: &'static str,
}

/// Unrecognised third-party addons become explicit fit-gap tasks.
pub enum Coverage {
    Scenario(Scenario),
    Platform,
    Unknown,
}

/// Describe a shipped domain or extension without requiring it as a dependency.
pub fn coverage(addon: &str) -> Coverage {
    let (title, model, setup, acceptance) = match addon {
        "base" => (
            "Company setup",
            "res.company",
            "Verify company details, base currency, language and permitted companies in Settings.",
            "Sign in as a test operator and verify the company, currency and permitted records.",
        ),
        "contacts" => (
            "Contacts",
            "res.partner",
            "Map and validate customer and supplier records, addresses, tax IDs and external identifiers.",
            "Re-import a test contact using the same external ID and verify that its address updates without a duplicate.",
        ),
        "product" => (
            "Product catalogue",
            "product.template",
            "Map categories, units, sales prices and purchase information before importing products.",
            "Open a test product and verify that a quotation and a request for quotation use its configured values.",
        ),
        "uom" => (
            "Units of measure",
            "uom.uom",
            "Configure stock and purchase units, conversion factors and rounding.",
            "Convert a test purchase quantity into stock units and reconcile the rounded result.",
        ),
        "barcodes" => (
            "Barcode identification",
            "barcode.nomenclature",
            "Configure the scan nomenclature and barcode rules used on product labels.",
            "Scan a test label and verify that it resolves to the intended product.",
        ),
        "barcodes_gs1_nomenclature" => (
            "GS1 identification",
            "barcode.nomenclature",
            "Select the GS1 nomenclature and map the identifiers printed by suppliers.",
            "Parse a test GS1 label and compare its product and lot identifiers with the printed values.",
        ),
        "sales_team" => (
            "Sales teams",
            "crm.team",
            "Assign sales teams, responsible users and the sales access groups.",
            "Sign in as a test salesperson and verify team assignment and opportunity visibility.",
        ),
        "crm" => (
            "CRM pipeline",
            "crm.pipeline",
            "Configure pipelines, stages, qualification criteria and forecast categories.",
            "Qualify a test lead and verify that its owner, stage and forecast appear in the pipeline.",
        ),
        "sale" => (
            "Quotation to order",
            "sale.order",
            "Define quotation terms, pricing and the sales confirmation process.",
            "Confirm a test quotation and reconcile its customer, tax, order lines and total.",
        ),
        "sale_management" => (
            "Sales operations",
            "sale.order",
            "Set sales responsibilities and review the quotation workflow with sales operators.",
            "Open a test quotation from Sales and trace the confirmed order under the assigned salesperson.",
        ),
        "sale_crm" => (
            "Opportunity to quotation",
            "crm.lead",
            "Set the handoff from a qualified opportunity to a sales quotation.",
            "Create a test quotation from an opportunity, confirm it and reconcile its booked amount.",
        ),
        "purchase" => (
            "Request for quotation",
            "purchase.order",
            "Set up vendors, purchase units, vendor prices and purchase approval rules.",
            "Confirm a test vendor quotation and verify the agreed prices, quantities and vendor.",
        ),
        "stock" => (
            "Warehouses and inventory",
            "stock.warehouse",
            "Configure warehouses, locations, routes, operation types and opening quantities.",
            "Receive and deliver a test product, then reconcile movements and closing quantities.",
        ),
        "purchase_stock" => (
            "Purchasing and receipts",
            "purchase.order",
            "Align warehouse destinations, receipt operations and vendor billable quantities.",
            "Trace a test purchase order through receipt and vendor bill and reconcile its quantities.",
        ),
        "sale_stock" => (
            "Sales fulfilment",
            "sale.order",
            "Set warehouse routes, delivery operations and the invoicing policy.",
            "Trace a confirmed test order through delivery and invoice, including a partial delivery.",
        ),
        "account" => (
            "Accounting setup",
            "account.journal",
            "Review the chart of accounts, journals, taxes, payment terms and opening balances.",
            "Post a test invoice, register its payment and reconcile the receivable with the general ledger.",
        ),
        "account_reporting" => (
            "Financial reporting",
            "reporting.report",
            "Review financial statements, reporting periods and comparison settings.",
            "Reconcile a trial balance for a test period to journal entries and drill down to its source.",
        ),
        "analytic" => (
            "Analytic accounting",
            "account.analytic.account",
            "Define analytic plans, cost centres and their allocation structure.",
            "Post a test allocation and reconcile its analytic balance to the source transaction.",
        ),
        "quality_control" => (
            "Quality controls",
            "quality.point",
            "Configure quality teams, control points and inspection criteria for transfers.",
            "Receive a test item, record a failed inspection and trace the quality alert to the transfer.",
        ),
        "hr" => (
            "Employee directory",
            "hr.department",
            "Set up departments, jobs, reporting lines and employee access before import.",
            "Open a test employee as HR and verify that a regular employee sees only permitted details.",
        ),
        "approvals" => (
            "Approval workflows",
            "approval.workflow",
            "Define reviewers, operation scope, levels and the approval expiry for each workflow.",
            "Submit a staged test request and verify that only the eligible reviewer can advance each level.",
        ),
        "mail" => (
            "Messaging and outbox",
            "mail.activity",
            "Set up notification preferences, outgoing mail and activity assignment.",
            "Assign a test activity and send a message to an approved test mailbox, checking the delivery receipt.",
        ),
        "reporting" => (
            "Business reports",
            "reporting.report",
            "Define report lines, measures, dates and the drill-down source.",
            "Run a report with a restricted test user and match a visible figure to its underlying records.",
        ),
        "account_mail" => (
            "Accounting collaboration",
            "account.move",
            "Review document followers and activities for invoices and vendor bills.",
            "Schedule an activity on a test invoice and verify its recipient sees the invoice activity.",
        ),
        "purchase_mail" => (
            "Purchase collaboration",
            "purchase.order",
            "Define purchase document followers and purchasing activities.",
            "Schedule an activity on a test purchase order and verify its assignment from the activity menu.",
        ),
        "product_mail" => (
            "Product collaboration",
            "product.template",
            "Define the audience for internal product notes and updates.",
            "Write a test product note and verify its visibility to an authorised follower.",
        ),
        "stock_mail" => (
            "Warehouse collaboration",
            "stock.picking",
            "Route transfer activities and chatter to warehouse users.",
            "Assign a test transfer activity and verify its link and recipient in the activity menu.",
        ),
        "delivery" => (
            "Delivery methods",
            "delivery.carrier",
            "Configure delivery methods, carriers and price rules.",
            "Rate a test shipment and compare the selected carrier and price with its configuration.",
        ),
        "product_expiry" => (
            "Tracked product expiry",
            "product.template",
            "Configure expiry and alert periods on lot-tracked products.",
            "Receive a dated test lot and verify its expiration and alert dates.",
        ),
        "purchase_three_way_match" => (
            "Three-way matching",
            "purchase.order",
            "Set receipt and vendor billing policies for goods bought on purchase orders.",
            "Compare a test vendor bill against its purchase and receipt and identify a quantity discrepancy.",
        ),
        "stock_account" => (
            "Inventory valuation",
            "product.category",
            "Configure product category valuation, costing and journal accounts.",
            "Reconcile the valuation of a test receipt to its posted accounting entry.",
        ),
        "valuation" => (
            "Valuation controls",
            "product.category",
            "Review receipting controls with the inventory accounting owner.",
            "Validate a test receipt and verify that its valuation entries satisfy the configured controls.",
        ),
        "stock_barcode" => (
            "Warehouse scanning",
            "stock.picking",
            "Configure operation types, product barcodes and scan responsibilities.",
            "Scan a test receipt and verify the resulting transfer moves and processed quantity.",
        ),
        "stock_delivery" => (
            "Carrier fulfilment",
            "delivery.carrier",
            "Associate delivery methods with the carrier dispatch process.",
            "Prepare a test transfer for its carrier and verify the shipment status in the transfer.",
        ),
        "stock_dropshipping" => (
            "Drop shipping",
            "stock.route",
            "Configure drop-ship routes and supplier information on relevant products.",
            "Trace a test sale through supplier purchase and direct shipment to the customer.",
        ),
        "stock_fleet" => (
            "Fleet dispatch",
            "fleet.vehicle",
            "Enter vehicles, capacities and dispatch responsibilities.",
            "Assign a test batch to a vehicle and verify its load against the vehicle capacity.",
        ),
        "stock_landed_costs" => (
            "Landed costs",
            "stock.landed.cost",
            "Configure landed cost products, allocation methods and clearing accounts.",
            "Allocate a test freight charge and reconcile the valuation adjustment with its journal entry.",
        ),
        "stock_sms" => (
            "Delivery notifications",
            "stock.picking",
            "Configure transfer notification channels, recipients and templates.",
            "Validate a test transfer to an approved recipient and verify the notification outcome.",
        ),
        "web_map" => (
            "Contact maps",
            "res.partner",
            "Map coordinates and verify contact address data before import.",
            "Open a test contact on the map and compare its marker with the saved location.",
        ),
        "sample_library" => (
            "Library pilot",
            "sample_library.book",
            "Define the sample catalogue, authors and loan responsibilities.",
            "Lend and return a test book and confirm its resulting availability.",
        ),
        "acceptance_notes" => (
            "Shared notes",
            "acceptance_notes.note",
            "Identify the owners and audience for shared acceptance notes.",
            "Save a test note and verify that the intended reviewer can read it.",
        ),
        "web" | "theme_office_blue" | "report_fonts" | "ai_base" => return Coverage::Platform,
        _ => return Coverage::Unknown,
    };
    Coverage::Scenario(Scenario {
        title,
        model,
        setup,
        acceptance,
    })
}
