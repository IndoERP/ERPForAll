<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset=".github/assets/logo-dark.svg">
    <img src=".github/assets/logo.svg" alt="ERPForAll" width="360">
  </picture>
</p>

<h3 align="center">An open-source ERP framework in Rust, compatible with the way Odoo works.</h3>

<p align="center">
  Models, views, menus, access rules and addons that extend each other — built on a typed ORM over PostgreSQL 18<br>
  and a fast, modern web client of its own. Self-hosted, open source, and yours to shape.
</p>

<p align="center">
  <a href="LICENSE-MIT"><img src="https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-2f6f6b?style=flat-square" alt="License: MIT OR Apache-2.0"></a>
  <a href="rust-toolchain.toml"><img src="https://img.shields.io/badge/Rust-stable-704f69?style=flat-square&logo=rust&logoColor=white" alt="Rust"></a>
  <a href="docs/install.md"><img src="https://img.shields.io/badge/PostgreSQL-18-2f6f6b?style=flat-square&logo=postgresql&logoColor=white" alt="PostgreSQL 18"></a>
  <a href="docs/showcase/README.md"><img src="https://img.shields.io/badge/demo-full--cycle%20walkthrough-0f766e?style=flat-square" alt="Full-cycle walkthrough"></a>
  <a href="https://github.com/IndoERP/ERPForAll/discussions"><img src="https://img.shields.io/github/discussions/IndoERP/ERPForAll?style=flat-square&color=704f69" alt="Discussions"></a>
</p>

<p align="center">
  <a href="https://indoerp.github.io/ERPForAll/"><b>Documentation</b></a> ·
  <a href="#end-to-end-full-cycle-walkthrough">Video Tour</a> ·
  <a href="#screenshot-gallery">Screenshots</a> ·
  <a href="#what-works-today">What Works Today</a> ·
  <a href="#how-an-addon-looks">Addons</a> ·
  <a href="#get-started">Get Started</a> ·
  <a href="#contributing">Contributing</a>
</p>

---

## End-to-End Full-Cycle Walkthrough

Follow a live walkthrough across **Accounting → Purchase → Inventory → Sales → Dynamic Financial Reports → HR**, recorded directly from a self-hosted ERPForAll instance with sample data:

<p align="center">
  <a href="docs/showcase/full-cycle-demo.mp4">
    <img src="docs/showcase/full-cycle-preview.gif" alt="ERPForAll full-cycle end-to-end ERP walkthrough: Accounting, Purchase, Inventory, Sales, Dynamic Reports and HR" width="1020">
  </a><br>
  <sub>
    <a href="docs/showcase/full-cycle-demo.mp4"><b>▶ Watch / Download Full HD Video (MP4 · 1280×720)</b></a>
    &nbsp;·&nbsp;
    <a href="docs/showcase/README.md"><b>Browse All 16 HiDPI Screenshots →</b></a>
  </sub>
</p>

> **Active development.** Try ERPForAll today with sample data — run a complete procure-to-pay and order-to-cash cycle, reconcile accounts, drill into live dynamic financial reports, and build your own addons in Rust, TOML/XML, or WebAssembly.

---

## Why ERPForAll

Companies shape a great ERP to fit their operations through addons, without forking the core. ERPForAll keeps that modular architecture — familiar to Odoo developers — and rebuilds the engine from the ground up:

- **Familiar by design.** Models with `_inherit`, XML views extended via XPath, menus, actions, access rules, record rules, external XML IDs and demo data. If you know how an Odoo addon is structured, you already know ERPForAll.
- **Typed Rust & PostgreSQL 18.** Exact `Decimal` arithmetic for quantities and money, domains compiled straight to SQL, and row/field access policies enforced inside every query.
- **Addons all the way down.** The core stays lean. Accounting, Inventory, Sales, Purchase, HR, Contacts, Products, Mail and Dynamic Reports are independent addons — written as native Rust crates, zero-compile data-only folders, or sandboxed WebAssembly modules.
- **Fail loudly.** Unsupported operations and missing configurations return clear, actionable errors — never silent defaults or hidden data loss.
- **Clean-room implementation.** Zero copied Odoo source code; behaviour is verified by integration and browser tests.

---

## Screenshot Gallery

Select any screenshot to view it at full `1440×900` HiDPI resolution, or browse the complete index in [`docs/showcase/`](docs/showcase/README.md).

### Operations — Accounting, Purchase, Inventory & Sales

<table>
  <tr>
    <td width="50%" valign="top">
      <a href="docs/showcase/screenshots/01-accounting-dashboard.png">
        <img src="docs/showcase/screenshots/01-accounting-dashboard.png" alt="Accounting Dashboard showing Customer Invoices, Vendor Bills, Main Bank and Petty Cash charts">
      </a><br>
      <b>Accounting Dashboard</b> — Live journal cards, unpaid invoices, vendor bills, bank balances and monthly activity charts.
    </td>
    <td width="50%" valign="top">
      <a href="docs/showcase/screenshots/02-purchase-order.png">
        <img src="docs/showcase/screenshots/02-purchase-order.png" alt="Confirmed Purchase Order PO00001 with smart buttons for Alternatives, Vendor Bills, Receipt and Chatter">
      </a><br>
      <b>Purchase Order</b> — Confirmed vendor orders with 3-way match, alternatives, linked receipts, bills and chatter history.
    </td>
  </tr>
  <tr>
    <td width="50%" valign="top">
      <a href="docs/showcase/screenshots/03-inventory-overview.png">
        <img src="docs/showcase/screenshots/03-inventory-overview.png" alt="Inventory Overview with Receiving and Deliveries operation cards">
      </a><br>
      <b>Inventory Overview</b> — Warehouse operation cards tracking pending, late and backorder receipts and deliveries.
    </td>
    <td width="50%" valign="top">
      <a href="docs/showcase/screenshots/04-stock-transfer.png">
        <img src="docs/showcase/screenshots/04-stock-transfer.png" alt="Validated Delivery Transfer Main/OUT/00002 with return, scrap and carrier actions">
      </a><br>
      <b>Warehouse Delivery</b> — Validated stock transfers with returns, scrap, shipping rates, carrier dispatch and activity log.
    </td>
  </tr>
  <tr>
    <td width="50%" valign="top">
      <a href="docs/showcase/screenshots/05-sales-order.png">
        <img src="docs/showcase/screenshots/05-sales-order.png" alt="Sales Quotation SO-DEMO-002 with Optional Products, Discounts, Coupons, Online Signature and Margins">
      </a><br>
      <b>Sales Quotation &amp; Order</b> — Quotation templates, optional products, online signature, discounts, coupons and exact margins.
    </td>
    <td width="50%" valign="top">
      <a href="docs/showcase/screenshots/06-customer-invoice.png">
        <img src="docs/showcase/screenshots/06-customer-invoice.png" alt="Posted Customer Invoice INV/2026/00005 with Register Payment, Credit Note and Chatter">
      </a><br>
      <b>Customer Invoice</b> — Posted invoices with tax breakdown, payment registration, credit notes and audit trail.
    </td>
  </tr>
</table>

### Dynamic Financial & Business Reports

Every financial and operational report shares one interactive tree-grid engine with live unfolding, running balances, multi-period comparisons, source drill-down, and server-side **PDF / XLSX** export:

<table>
  <tr>
    <td width="50%" valign="top">
      <a href="docs/showcase/screenshots/07-partner-ledger.png">
        <img src="docs/showcase/screenshots/07-partner-ledger.png" alt="Partner Ledger unfolded with chronological journal items, overdue due dates, running balance and totals">
      </a><br>
      <b>Partner Ledger</b> — Partner-level unfolding with chronological items, overdue due-date highlighting, running balance and unreconciled cutoff.
    </td>
    <td width="50%" valign="top">
      <a href="docs/showcase/screenshots/08-general-ledger.png">
        <img src="docs/showcase/screenshots/08-general-ledger.png" alt="General Ledger showing accounts by code and name with unfolded journal items and running balance">
      </a><br>
      <b>General Ledger</b> — Accounts ordered by code and name, chronological entries, running balances and fiscal-year opening mode.
    </td>
  </tr>
  <tr>
    <td width="50%" valign="top">
      <a href="docs/showcase/screenshots/09-trial-balance.png">
        <img src="docs/showcase/screenshots/09-trial-balance.png" alt="Trial Balance with side-by-side multi-period comparison and balanced debit/credit totals">
      </a><br>
      <b>Trial Balance</b> — Initial balance, debit, credit and end balance per account with side-by-side multi-period comparison.
    </td>
    <td width="50%" valign="top">
      <a href="docs/showcase/screenshots/10-balance-sheet.png">
        <img src="docs/showcase/screenshots/10-balance-sheet.png" alt="Classified Balance Sheet showing Current Assets, Bank and Cash Accounts unfolded and totals">
      </a><br>
      <b>Balance Sheet</b> — Classified Assets, Liabilities and Equity with live account unfolding and automatic subtotals.
    </td>
  </tr>
  <tr>
    <td width="50%" valign="top">
      <a href="docs/showcase/screenshots/11-profit-and-loss.png">
        <img src="docs/showcase/screenshots/11-profit-and-loss.png" alt="Classified Profit and Loss showing Income, Gross Profit, Expenses, Depreciation and Net Profit">
      </a><br>
      <b>Profit &amp; Loss</b> — Operating Income, Cost of Revenue, Gross Profit, Expenses, Depreciation and Net Profit.
    </td>
    <td width="50%" valign="top">
      <a href="docs/showcase/screenshots/12-purchase-report.png">
        <img src="docs/showcase/screenshots/12-purchase-report.png" alt="Purchase Dynamic Report grouped by vendor with overdue arrival highlighting and totals">
      </a><br>
      <b>Purchase &amp; Sales Reports</b> — Vendor and customer drill-down reports with ordered/received/billed quantities, margins and overdue highlights.
    </td>
  </tr>
</table>

### Human Resources — Employees, Org Chart & Analytics

<table>
  <tr>
    <td width="50%" valign="top">
      <a href="docs/showcase/screenshots/13-hr-employees-kanban.png">
        <img src="docs/showcase/screenshots/13-hr-employees-kanban.png" alt="Employees Kanban directory with avatar cards, department tree sidebar, tenure and role tags">
      </a><br>
      <b>Employees Directory (Kanban)</b> — Card directory with department tree sidebar, contact info, tenure and role tags.
    </td>
    <td width="50%" valign="top">
      <a href="docs/showcase/screenshots/14-hr-org-chart.png">
        <img src="docs/showcase/screenshots/14-hr-org-chart.png" alt="Interactive visual Organization Chart hierarchy tree from CEO down to department leads">
      </a><br>
      <b>Visual Organization Chart</b> — Interactive reporting hierarchy from leadership down through every team and direct report.
    </td>
  </tr>
  <tr>
    <td width="50%" valign="top">
      <a href="docs/showcase/screenshots/15-hr-employee-profile.png">
        <img src="docs/showcase/screenshots/15-hr-employee-profile.png" alt="Employee Profile Form for Budi Santoso with Work, Payroll, Resume tabs and embedded Organization Chart">
      </a><br>
      <b>Employee Profile &amp; Inline Org Chart</b> — Complete employee record with work info, payroll, resume, skills, history and embedded org chart.
    </td>
    <td width="50%" valign="top">
      <a href="docs/showcase/screenshots/16-hr-headcount-analytics.png">
        <img src="docs/showcase/screenshots/16-hr-headcount-analytics.png" alt="HR Headcount Analysis interactive bar chart grouped by department">
      </a><br>
      <b>Headcount Analysis</b> — Interactive bar, line and pie charts alongside pivot tables for workforce reporting.
    </td>
  </tr>
</table>

---

## What Works Today

### Framework & Platform

| Area | Capabilities |
|---|---|
| **Typed ORM** | Models, `_inherit` & extension mixins, relational/computed/related/binary/image fields, exact `Decimal` quantities & money, domains compiled to SQL, prefetch & cache, constraints, onchange, `read_group`, sequences, translations, multi-company |
| **Addon Platform** | `manifest.toml` dependency graph, atomic install/upgrade/uninstall per database, `pre`/`post`/`end` migrations, data & demo XML/CSV loaders, data-only addons, sandboxed WebAssembly packages |
| **Security** | Role & record rules compiled directly into SQL, field-level `groups`, company-scoped access, sessions, CSRF protection and API keys |
| **Web Client** | Operational app dashboards (**CRM Overview**, **Sales Overview**, **Purchase Overview**, **Contacts Overview**, **Approvals Overview**, **Project Overview**), declared sidebar/submenu icons, list/form/kanban/pivot/graph/calendar/org views; search facets & group-by; rich-text editor; modal wizards; guided CSV/XLSX import & export; **Discuss** workspace; Chatter with streaming HTTP uploads (up to 128 MiB); light/dark & custom themes |
| **Dynamic Reporting** | Unified interactive tree-grid engine with lazy unfolding, `totals_below`, period comparisons, locale dates & currency symbols, per-order exchange-rate conversion, source drill-down, and PDF/XLSX exports |
| **Printed Documents & Jobs** | QWeb PDF reports with company letterhead and paper formats; durable background jobs with deduplication and chunked steps |
| **API & CLI** | JSON-2 RPC for every exposed model method under full access rules; unified `erp` CLI for databases, modules, scaffolding and server ops |

### Business Applications

| Application | What You Can Run End to End |
|---|---|
| **Accounting & Reporting** | Customer invoices, vendor bills, credit notes & refunds, multi-currency payments & partial write-offs, bank statements & reconciliation, follow-up levels, fixed assets, analytic accounting & budgets, recurring/deferred entries, fiscal periods & locks, and full Dynamic Reports (**Partner Ledger, General Ledger, Trial Balance, Balance Sheet, Profit & Loss, Cash Flow, Aged Receivable/Payable, Tax, Analytic, Executive Summary**). |
| **Inventory & Warehouse** | Multi-step receipts, deliveries & internal transfers, routes & putaway rules, storage categories, packages & packagings, lots/serials with expiry dates, physical inventory & cycle counts, replenishment rules, returns, scrap, backorders, landed costs, stock valuation, **Barcode scanning**, **Quality Control** checks and **Transport Dispatch**. |
| **CRM & Sales** | **CRM Overview** dashboard with one-click **Submit Lead** modal, lead scoring & assignment rules, opportunity pipeline Kanban, quotas & forecasting, direct quotation creation (`sale_crm`), multi-currency pricelists, quotation templates, optional products, send by email & pro-forma PDFs, online customer signature, discounts & loyalty/coupons, down-payment invoicing, exact-decimal profit margins, Kanban/Calendar/Pivot/Graph views and **Sales Dynamic Reports**. |
| **Purchase** | **Purchase Overview** dashboard, RFQs & email sending, vendor pricelists, purchase agreements (blanket orders & calls for tenders), alternative RFQ comparison, receipt reminders, 3-way matching, vendor on-time delivery rate, Kanban/Calendar/Pivot/Graph views and **Purchase Dynamic Reports**. |
| **Human Resources** | Employee directory & department hierarchy, interactive org chart, contracts & history, job positions, work schedules, skills & certifications, resume entries, headcount analysis and departure tracking. |
| **Project** | **Project Overview** dashboard, projects with their own task stages (copied from default stages), task board per project with drag and drop, task status, assignees, tags, deadlines and subtasks that inherit project and customer, task counts on projects, chatter tracking of stage changes, and List/Calendar views. |
| **AI Assistant & Approvals** | **AI Assistant** with formatted replies, role-scoped ERP actions, **Canvas Studio**, **Saved Insights**, and **Implementation Projects**; native **Approvals** with multistage workflows, immutable policy revisions, and an activity inbox. |
| **Products, Contacts & Mail** | Product variants, barcodes, packaging & UoM conversions; companies/contacts directory (**Contacts Overview**, List & Kanban); **Discuss** real-time workspace, outbound SMTP email, and integrated Chatter with messages, internal notes, followers, activities and streaming HTTP attachments across documents. |

---

## How an Addon Looks

An addon is a folder with a `manifest.toml`. It can declare models, extend models of other addons, add or patch views via XPath, define menus, actions, security rules, migrations and data, or extend the web client.

### Data-Only Addon (Zero Compilation)

From [`addons/sample_library`](addons/sample_library):

```toml
[[model]]
name = "sample_library.genre"
description = "Library Genre"
order = "name, id"

[[model.field]]
name = "name"
type = "char"
string = "Name"
required = true
index = true
```

### Native Rust Addon

Extend `product.template` with Chatter in [`addons/product_mail/src/lib.rs`](addons/product_mail/src/lib.rs):

```rust
erp_sdk::addon!(name = "product_mail", depends = ["base", "product", "mail"]);

use erp_addon_mail::MailThread;
use erp_addon_product::ProductTemplate;
use erp_sdk::Operation;
use erp_sdk::prelude::*;

#[extend(ProductTemplate, addon = "product_mail", inherit = [MailThread])]
pub struct ProductTemplateThread {}

#[methods(ProductTemplate, addon = "product_mail")]
impl ProductTemplateThread {
    #[method(override, sealed)]
    async fn _mail_post_access(
        _this: &RecordSet<ProductTemplate>,
        _tx: &mut Transaction,
        _sup: Super,
    ) -> Result<Operation> {
        Ok(Operation::Read)
    }
}
```

Scaffold either style in seconds:

```sh
erp scaffold my_addon --summary "My custom module"            # Native Rust addon
erp scaffold my_tags --data-only --summary "Declarative tags" # Data-only addon
```

---

## Get Started

On **Linux (x86_64)** or **macOS (Apple Silicon)** with Rust ([rustup](https://rustup.rs)), `git`, `rsync`, `curl`, and Docker (or local PostgreSQL 18), one command builds ERPForAll, initializes a demo database and starts the server:

```sh
git clone https://github.com/IndoERP/ERPForAll.git
cd ERPForAll
scripts/install.sh
```

Once ready, open **<http://127.0.0.1:8069/erp>** and sign in as `admin` (password saved at `~/.erpforall/local/private/admin-password`).

### Useful Local Commands

```sh
scripts/install.sh --rebuild --fast   # Incremental rebuild & restart without resetting the database
scripts/install.sh --reset --yes      # Cleanly remove the local installation under ~/.erpforall/local
scripts/check.sh --affected           # Fast format, layer & clippy checks for changed crates
scripts/check.sh --tests              # Full check suite including integration tests
```

See [docs/install.md](docs/install.md) for manual setup, custom PostgreSQL servers, and production configuration.

---

## Roadmap

- **Now:** polish daily UX across CRM, Accounting, Inventory, Sales, Purchase, HR, Approvals, and AI Assistant; expand test coverage and localization hooks.
- **Next:** additional views (Gantt, Map), Manufacturing (MRP), and Project management addons.
- **Later:** continuous performance profiling, multi-node clustering enhancements and ecosystem addon packaging.

---

## Repository Layout

| Path | Description |
|---|---|
| [`crates/`](crates/) | Core Rust framework: typed ORM, registry, module system, data loader, security engine, HTTP/RPC server, QWeb/PDF reports, addon SDK and `erp` CLI. |
| [`addons/`](addons/) | Built-in applications: `base`, `web`, `uom`, `contacts`, `product`, `mail`, `reporting`, `account`, `account_reporting`, `stock`, `stock_account`, `stock_barcode`, `quality_control`, `crm`, `sale`, `sale_crm`, `purchase`, `hr`, `project`, `approvals`, `ai_base`, bridges and `sample_library`. |
| [`web/`](web/) | Web client runtime (TypeScript) and browser benchmark/verification scripts. |
| [`docs/`](docs/) | Guides ([install](docs/install.md), [development](docs/development.md), [deployment](docs/deployment.md)) and the [full-cycle media showcase](docs/showcase/README.md). |
| [`examples/`](examples/) & [`wit/`](wit/) | Sandboxed WebAssembly addon examples and WIT component interfaces. |
| [`scripts/`](scripts/) | One-command installer, affected-crate checks, lints and pinned web toolchain. |

---

## Contributing & Community

ERPForAll is built in the open and welcomes contributions of every size — code, addons, tests, docs, translations and bug reports.

- **Guides:** [CONTRIBUTING.md](CONTRIBUTING.md) · [docs/development.md](docs/development.md) · [AGENTS.md](AGENTS.md)
- **Community:** [GitHub Discussions](https://github.com/IndoERP/ERPForAll/discussions) · [Issues](https://github.com/IndoERP/ERPForAll/issues) · [Documentation Site](https://indoerp.github.io/ERPForAll/) · [erpforall-agent-kit](https://github.com/IndoERP/erpforall-agent-kit)
- **Commit sign-off:** please sign off commits with `git commit -s` (Developer Certificate of Origin).
- **Security:** report vulnerabilities privately per [SECURITY.md](SECURITY.md).

---

## License

Dual-licensed under either of **[Apache License, Version 2.0](LICENSE-APACHE)** or **[MIT License](LICENSE-MIT)** at your option. See [`NOTICE`](NOTICE) and `vendor/` for third-party notices.

<sub>Odoo is a trademark of Odoo S.A. ERPForAll is an independent open-source project, not affiliated with, sponsored by or endorsed by Odoo S.A.</sub>
