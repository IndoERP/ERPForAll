// SPDX-License-Identifier: MIT OR Apache-2.0
//! Customer invoices and vendor bills in draft: the invoice line sync (tax lines, the
//! receivable/payable line, the totals) for the four invoice types, the posting checks the posting
//! service calls, and company isolation of the accounts the sync picks.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]
use erp_addon_account::invoice::post::{before_post, check_postable};
use erp_addon_account::invoice::{sync_lines, sync_lines_redated};
use erp_sdk::prelude::*;
use erp_sdk::{Command, ErrorKind};
use erp_testkit::Harness;

async fn harness() -> Harness {
    Harness::builder([
        &erp_addon_base::ADDON,
        &erp_addon_web::ADDON,
        &erp_addon_uom::ADDON,
        &erp_addon_product::ADDON,
        &erp_addon_analytic::ADDON,
        &erp_addon_account::ADDON,
    ])
    .require_sql_objects([&erp_addon_account::reconcile::history_guard::MANIFEST])
    .schema_sync_options(erp_testkit::SchemaSyncOptions::inline().lifecycle(true))
    .build()
    .await
    .unwrap()
}

fn text(value: &str) -> Cell {
    Cell::Str(value.into())
}

fn d(value: &str) -> Decimal {
    value.parse().unwrap()
}

fn rec(id: i64) -> RecId {
    RecId::from_real(id).unwrap()
}

async fn company(tx: &mut Transaction, name: &str) -> i64 {
    let rows = tx
        .query("SELECT id::int8 FROM res_currency ORDER BY id LIMIT 1", &[])
        .await
        .unwrap();
    let currency = match rows.first() {
        Some(row) => row.get(0),
        None => erp_testkit::fixtures::seed_currency_in(tx).await.unwrap(),
    };
    let vals = RawVals::new()
        .set_cell("name", text(name))
        .set_cell("currency_id", Cell::Id(currency));
    tx.env()
        .records("res.company")
        .unwrap()
        .create(tx, vec![vals])
        .await
        .unwrap()
        .ids()[0]
}

/// A branch of `parent`: it takes its root company's currency.
async fn branch(tx: &mut Transaction, name: &str, parent: i64) -> i64 {
    let vals = RawVals::new()
        .set_cell("name", text(name))
        .set_cell("parent_id", Cell::Id(parent));
    tx.env()
        .records("res.company")
        .unwrap()
        .create(tx, vec![vals])
        .await
        .unwrap()
        .ids()[0]
}

/// A journal written directly: the journal rules read declared settings that a harness without
/// data files does not install.
async fn journal(
    tx: &mut Transaction,
    company: i64,
    code: &str,
    kind: &str,
    default_account: Option<i64>,
) -> i64 {
    tx.query(
        "INSERT INTO account_journal (name, code, active, type, company_id, sequence, \
         refund_sequence, payment_sequence, sequence_reset, sequence_padding, color, show_on_dashboard, \
         default_account_id) \
         VALUES (jsonb_build_object('en_US', $2::text), $2::text, true, $3::text, $1, 10, \
         false, false, 'month', 4, 0, true, $4) RETURNING id::int8",
        &[&company, &code, &kind, &default_account],
    )
    .await
    .unwrap()[0]
        .get(0)
}

async fn account(tx: &mut Transaction, company: i64, code: &str, kind: &str) -> i64 {
    tx.env()
        .with_company(company)
        .unwrap()
        .records("account.account")
        .unwrap()
        .create(
            tx,
            vec![
                RawVals::new()
                    .set_cell("name", text(code))
                    .set_cell("code", text(code))
                    .set_cell("account_type", text(kind)),
            ],
        )
        .await
        .unwrap()
        .ids()[0]
}

async fn partner(tx: &mut Transaction, name: &str) -> i64 {
    tx.env()
        .records("res.partner")
        .unwrap()
        .create(tx, vec![RawVals::new().set_cell("name", text(name))])
        .await
        .unwrap()
        .ids()[0]
}

/// A tax of `company` without repartition lines, written directly (the tax rules read settings
/// the harness does not install). `included` makes its prices tax-included.
async fn tax_row(
    tx: &mut Transaction,
    company: i64,
    name: &str,
    use_: &str,
    amount_type: &str,
    amount: &str,
    included: bool,
) -> i64 {
    let country: i64 = match tx
        .query("SELECT id::int8 FROM res_country ORDER BY id LIMIT 1", &[])
        .await
        .unwrap()
        .first()
    {
        Some(row) => row.get(0),
        None => tx
            .env()
            .records("res.country")
            .unwrap()
            .create(
                tx,
                vec![
                    RawVals::new()
                        .set_cell("name", text("Testland"))
                        .set_cell("code", text("TL")),
                ],
            )
            .await
            .unwrap()
            .ids()[0],
    };
    let group: i64 = tx
        .query(
            "INSERT INTO account_tax_group (name, sequence, company_id) \
             VALUES (jsonb_build_object('en_US', 'VAT'), 10, $1) RETURNING id::int8",
            &[&company],
        )
        .await
        .unwrap()[0]
        .get(0);
    let price_include: Option<&str> = included.then_some("tax_included");
    tx.query(
        "INSERT INTO account_tax (name, type_tax_use, amount_type, active, company_id, \
         root_company_id, sequence, amount, include_base_amount, is_base_affected, tax_group_id, \
         country_id, repartition_serial, analytic, price_include_override) \
         VALUES (jsonb_build_object('en_US', $2::text), $3::text, $7::text, true, $1, $1, 1, \
         $4::text::numeric, false, true, $5, $6, 0, false, $8::text) RETURNING id::int8",
        &[
            &company,
            &name,
            &use_,
            &amount,
            &group,
            &country,
            &amount_type,
            &price_include,
        ],
    )
    .await
    .unwrap()[0]
        .get(0)
}

/// The repartition lines of `tax` for one document kind (`invoice` or `refund`): the base line
/// and one `tax` line per (factor in percent, account) of `parts`, in that order.
async fn repartition(
    tx: &mut Transaction,
    tax: i64,
    company: i64,
    document: &str,
    parts: &[(&str, i64)],
) {
    tx.query(
        "INSERT INTO account_tax_repartition_line (factor_percent, repartition_type, document_type, \
         account_id, tax_id, company_id, sequence, use_in_tax_closing) VALUES \
         (100, 'base', $3::text, NULL, $1, $2, 1, false)",
        &[&tax, &company, &document],
    )
    .await
    .unwrap();
    for (i, (factor, account)) in parts.iter().enumerate() {
        let sequence = i32::try_from(i).unwrap() + 2;
        tx.query(
            "INSERT INTO account_tax_repartition_line (factor_percent, repartition_type, \
             document_type, account_id, tax_id, company_id, sequence, use_in_tax_closing) VALUES \
             ($4::text::numeric, 'tax', $3::text, $5, $1, $2, $6, true)",
            &[&tax, &company, &document, factor, account, &sequence],
        )
        .await
        .unwrap();
    }
}

/// A percent tax of `company` with one base and one tax repartition line per document kind,
/// both on `account`.
async fn tax(
    tx: &mut Transaction,
    company: i64,
    name: &str,
    rate: &str,
    use_: &str,
    account: i64,
) -> i64 {
    let id = tax_row(tx, company, name, use_, "percent", rate, false).await;
    repartition(tx, id, company, "invoice", &[("100", account)]).await;
    repartition(tx, id, company, "refund", &[("100", account)]).await;
    id
}

/// A company with sale and purchase journals, their accounts and a 15 % sale and purchase tax.
struct Books {
    company: i64,
    sale: i64,
    purchase: i64,
    income: i64,
    expense: i64,
    receivable: i64,
    payable: i64,
    sale_tax_account: i64,
    sale_tax: i64,
    purchase_tax: i64,
    customer: i64,
}

async fn books(tx: &mut Transaction, name: &str) -> Books {
    let company = company(tx, name).await;
    let income = account(tx, company, "400000", "income").await;
    let expense = account(tx, company, "600000", "expense").await;
    let receivable = account(tx, company, "121000", "asset_receivable").await;
    let payable = account(tx, company, "211000", "liability_payable").await;
    let sale_tax_account = account(tx, company, "251000", "liability_current").await;
    let purchase_tax_account = account(tx, company, "131000", "asset_current").await;
    let sale = journal(tx, company, "INV", "sale", Some(income)).await;
    let purchase = journal(tx, company, "BILL", "purchase", Some(expense)).await;
    let sale_tax = tax(tx, company, "VAT 15%", "15", "sale", sale_tax_account).await;
    let purchase_tax = tax(
        tx,
        company,
        "VAT 15% (purchase)",
        "15",
        "purchase",
        purchase_tax_account,
    )
    .await;
    let customer = partner(tx, &format!("{name} customer")).await;
    Books {
        company,
        sale,
        purchase,
        income,
        expense,
        receivable,
        payable,
        sale_tax_account,
        sale_tax,
        purchase_tax,
        customer,
    }
}

fn moves(tx: &Transaction, company: i64) -> Records {
    tx.env()
        .with_company(company)
        .unwrap()
        .records("account.move")
        .unwrap()
}

fn product_line(quantity: &str, price: &str, discount: &str, taxes: &[i64]) -> Command {
    Command::create(
        RawVals::new()
            .set_cell("name", text("Consulting"))
            .set_cell("quantity", Cell::Decimal(d(quantity)))
            .set_cell("price_unit", Cell::Decimal(d(price)))
            .set_cell("discount", Cell::Decimal(d(discount)))
            .set_commands("tax_ids", [Command::set(taxes.iter().map(|t| rec(*t)))]),
    )
}

fn invoice(move_type: &str, partner: i64, lines: Vec<Command>) -> RawVals {
    RawVals::new()
        .set_cell("move_type", text(move_type))
        .set_cell("partner_id", Cell::Id(partner))
        .set_cell(
            "invoice_date",
            Cell::Date(Date::parse_odoo("2026-11-16").unwrap()),
        )
        .set_commands("invoice_line_ids", lines)
}

async fn try_create(model: &Records, tx: &mut Transaction, vals: RawVals) -> Result<Records> {
    let model = model.clone();
    tx.savepoint(async move |tx| model.create(tx, vec![vals]).await)
        .await
}

async fn decimal(record: &Records, tx: &mut Transaction, field: &str) -> Decimal {
    match record.cell(tx, field).await.unwrap() {
        Cell::Decimal(value) => value,
        Cell::Null => Decimal::ZERO,
        other => panic!("{field} is {other:?}"),
    }
}

/// (display type, account, balance) of every journal item of `entry`, sorted.
async fn items(entry: &Records, tx: &mut Transaction) -> Vec<(String, i64, Decimal)> {
    tx.flush().await.unwrap();
    let id = entry.ids()[0];
    let mut rows: Vec<(String, i64, Decimal)> = tx
        .query(
            "SELECT display_type, account_id::int8, balance::text FROM account_move_line \
             WHERE move_id = $1 ORDER BY display_type, balance",
            &[&id],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| (r.get(0), r.get(1), d(&r.get::<_, String>(2))))
        .collect();
    rows.sort();
    rows
}

#[tokio::test]
async fn a_customer_invoice_gets_its_tax_and_receivable_lines_and_totals() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let b = books(&mut tx, "Totals").await;
    let entry = moves(&tx, b.company)
        .create(
            &mut tx,
            vec![invoice(
                "out_invoice",
                b.customer,
                vec![
                    // 2 * 100 less 10 % = 180, 15 % tax = 27
                    product_line("2", "100", "10", &[b.sale_tax]),
                    product_line("1", "50", "0", &[]),
                ],
            )],
        )
        .await
        .unwrap();
    assert_eq!(
        entry.cell(&mut tx, "journal_id").await.unwrap(),
        Cell::Id(b.sale)
    );
    let mut expected = vec![
        ("payment_term".to_owned(), b.receivable, d("257")),
        ("product".to_owned(), b.income, d("-180")),
        ("product".to_owned(), b.income, d("-50")),
        ("tax".to_owned(), b.sale_tax_account, d("-27")),
    ];
    expected.sort();
    assert_eq!(items(&entry, &mut tx).await, expected);
    assert_eq!(decimal(&entry, &mut tx, "amount_untaxed").await, d("230"));
    assert_eq!(decimal(&entry, &mut tx, "amount_tax").await, d("27"));
    assert_eq!(decimal(&entry, &mut tx, "amount_total").await, d("257"));
    assert_eq!(decimal(&entry, &mut tx, "amount_residual").await, d("257"));
    assert_eq!(
        entry.cell(&mut tx, "payment_state").await.unwrap(),
        text("not_paid")
    );
    assert_eq!(
        entry.cell(&mut tx, "invoice_date_due").await.unwrap(),
        entry.cell(&mut tx, "invoice_date").await.unwrap(),
        "the due date is the invoice date until payment terms exist"
    );
    let lines = entry.related(&mut tx, "invoice_line_ids").await.unwrap();
    assert_eq!(
        lines.len(),
        2,
        "invoice_line_ids holds only the product lines"
    );
    let subtotals = lines.cells(&mut tx, "price_subtotal").await.unwrap();
    let totals = lines.cells(&mut tx, "price_total").await.unwrap();
    assert!(
        subtotals.contains(&Cell::Decimal(d("180"))) && totals.contains(&Cell::Decimal(d("207")))
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn invoice_currency_selects_the_default_journal() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let b = books(&mut tx, "Currency journals").await;
    let foreign = foreign_currency(&mut tx, b.company, &[("2026-11-01", "2")]).await;
    let domestic = tx
        .env()
        .records("res.company")
        .unwrap()
        .browse([b.company])
        .unwrap()
        .cell(&mut tx, "currency_id")
        .await
        .unwrap();
    let sale_fx = journal(&mut tx, b.company, "FX", "sale", Some(b.income)).await;
    let sale_home = journal(&mut tx, b.company, "HOME", "sale", Some(b.income)).await;
    let Cell::Id(home) = domestic else {
        panic!("expected a company currency")
    };
    for (journal, currency) in [(sale_fx, foreign), (sale_home, home)] {
        tx.query(
            "UPDATE account_journal SET currency_id = $2, sequence = 20 WHERE id = $1",
            &[&journal, &currency],
        )
        .await
        .unwrap();
    }
    let sibling = company(&mut tx, "Independent books").await;
    let sibling_purchase = journal(&mut tx, sibling, "FXBUY", "purchase", None).await;
    tx.query(
        "UPDATE account_journal SET currency_id = $2, sequence = 1 WHERE id = $1",
        &[&sibling_purchase, &foreign],
    )
    .await
    .unwrap();
    let model = moves(&tx, b.company);
    let from_context = model.with_context([("default_currency_id", Value::Int(foreign))]);
    for (model, kind, currency, explicit_journal, expected_journal, expected_currency) in [
        (&model, "out_invoice", Some(foreign), None, sale_fx, foreign),
        (&from_context, "out_refund", None, None, sale_fx, foreign),
        (&from_context, "out_invoice", Some(home), None, b.sale, home),
        (&from_context, "in_invoice", None, None, b.purchase, foreign),
        (
            &from_context,
            "out_invoice",
            None,
            Some(b.sale),
            b.sale,
            foreign,
        ),
    ] {
        let mut vals = invoice(kind, b.customer, vec![]);
        if let Some(currency) = currency {
            vals.insert("currency_id".into(), Cell::Id(currency));
        }
        if let Some(journal) = explicit_journal {
            vals.insert("journal_id".into(), Cell::Id(journal));
        }
        let entry = model.create(&mut tx, vec![vals]).await.unwrap();
        assert_eq!(
            entry.cell(&mut tx, "journal_id").await.unwrap(),
            Cell::Id(expected_journal)
        );
        assert_eq!(
            entry.cell(&mut tx, "currency_id").await.unwrap(),
            Cell::Id(expected_currency)
        );
    }
    let sibling_moves =
        moves(&tx, sibling).with_context([("default_currency_id", Value::Int(foreign))]);
    let error = try_create(
        &sibling_moves,
        &mut tx,
        invoice("out_invoice", b.customer, vec![]),
    )
    .await
    .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::UserError);
    assert!(
        error.to_string().contains(&format!(
            "No sale journal is available for company {sibling}"
        )),
        "{error}"
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn each_invoice_type_books_its_lines_on_the_right_side() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let b = books(&mut tx, "Signs").await;
    // (type, journal, product account, term account, sign of the product line)
    let cases = [
        (
            "out_invoice",
            b.sale,
            b.income,
            b.receivable,
            b.sale_tax,
            "-1",
        ),
        (
            "out_refund",
            b.sale,
            b.income,
            b.receivable,
            b.sale_tax,
            "1",
        ),
        (
            "in_invoice",
            b.purchase,
            b.expense,
            b.payable,
            b.purchase_tax,
            "1",
        ),
        (
            "in_refund",
            b.purchase,
            b.expense,
            b.payable,
            b.purchase_tax,
            "-1",
        ),
    ];
    for (move_type, journal, product_account, term_account, tax, sign) in cases {
        let entry = moves(&tx, b.company)
            .create(
                &mut tx,
                vec![invoice(
                    move_type,
                    b.customer,
                    vec![product_line("1", "100", "0", &[tax])],
                )],
            )
            .await
            .unwrap();
        assert_eq!(
            entry.cell(&mut tx, "journal_id").await.unwrap(),
            Cell::Id(journal),
            "{move_type}"
        );
        let sign = d(sign);
        let found = items(&entry, &mut tx).await;
        let product = found.iter().find(|r| r.0 == "product").unwrap();
        assert_eq!(
            (product.1, product.2),
            (product_account, sign * d("100")),
            "{move_type}"
        );
        let tax_line = found.iter().find(|r| r.0 == "tax").unwrap();
        assert_eq!(tax_line.2, sign * d("15"), "{move_type}");
        let term = found.iter().find(|r| r.0 == "payment_term").unwrap();
        assert_eq!(
            (term.1, term.2),
            (term_account, -sign * d("115")),
            "{move_type}"
        );
        assert_eq!(
            decimal(&entry, &mut tx, "amount_total").await,
            d("115"),
            "{move_type}"
        );
    }
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn editing_the_lines_rebuilds_the_tax_and_receivable_lines() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let b = books(&mut tx, "Edits").await;
    let entry = moves(&tx, b.company)
        .create(
            &mut tx,
            vec![invoice(
                "out_invoice",
                b.customer,
                vec![product_line("1", "100", "0", &[b.sale_tax])],
            )],
        )
        .await
        .unwrap();
    let line = entry
        .related(&mut tx, "invoice_line_ids")
        .await
        .unwrap()
        .ids()[0];
    entry
        .write(
            &mut tx,
            RawVals::new().set_commands(
                "invoice_line_ids",
                [Command::update(
                    rec(line),
                    RawVals::new().set_cell("price_unit", Cell::Decimal(d("200"))),
                )],
            ),
        )
        .await
        .unwrap();
    let found = items(&entry, &mut tx).await;
    assert_eq!(found.iter().find(|r| r.0 == "tax").unwrap().2, d("-30"));
    assert_eq!(
        found.iter().find(|r| r.0 == "payment_term").unwrap().2,
        d("230")
    );
    entry
        .write(
            &mut tx,
            RawVals::new().set_commands(
                "invoice_line_ids",
                [Command::update(
                    rec(line),
                    RawVals::new().set_commands("tax_ids", [Command::clear()]),
                )],
            ),
        )
        .await
        .unwrap();
    let found = items(&entry, &mut tx).await;
    assert!(!found.iter().any(|r| r.0 == "tax"), "{found:?}");
    assert_eq!(decimal(&entry, &mut tx, "amount_total").await, d("200"));
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn a_zero_tax_makes_no_tax_line() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let b = books(&mut tx, "Zero").await;
    let zero = tax(
        &mut tx,
        b.company,
        "VAT 0%",
        "0",
        "sale",
        b.sale_tax_account,
    )
    .await;
    let entry = moves(&tx, b.company)
        .create(
            &mut tx,
            vec![invoice(
                "out_invoice",
                b.customer,
                vec![product_line("1", "1000", "0", &[zero])],
            )],
        )
        .await
        .unwrap();
    let kinds: Vec<String> = items(&entry, &mut tx)
        .await
        .into_iter()
        .map(|r| r.0)
        .collect();
    assert_eq!(kinds, ["payment_term", "product"]);
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn missing_configuration_is_refused_by_name() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let b = books(&mut tx, "Config").await;
    let model = moves(&tx, b.company);
    let mut vals = invoice(
        "out_invoice",
        b.customer,
        vec![product_line("1", "10", "0", &[])],
    );
    vals.insert("journal_id".into(), Cell::Id(b.purchase));
    let err = try_create(&model, &mut tx, vals).await.unwrap_err();
    assert!(err.to_string().contains("needs a sale journal"), "{err}");

    let bare = journal(&mut tx, b.company, "INV2", "sale", None).await;
    let mut vals = invoice(
        "out_invoice",
        b.customer,
        vec![product_line("1", "10", "0", &[])],
    );
    vals.insert("journal_id".into(), Cell::Id(bare));
    let err = try_create(&model, &mut tx, vals).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::UserError);
    assert!(err.to_string().contains("default account"), "{err}");
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn the_receivable_account_comes_from_the_invoice_company_only() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let other = books(&mut tx, "Other").await;
    let mine = company(&mut tx, "Mine").await;
    let income = account(&mut tx, mine, "400000", "income").await;
    journal(&mut tx, mine, "INV", "sale", Some(income)).await;
    let model = moves(&tx, mine);
    let vals = invoice(
        "out_invoice",
        other.customer,
        vec![product_line("1", "10", "0", &[])],
    );
    let err = try_create(&model, &mut tx, vals.clone()).await.unwrap_err();
    assert!(err.to_string().contains("no receivable account"), "{err}");
    let receivable = account(&mut tx, mine, "121000", "asset_receivable").await;
    let entry = try_create(&model, &mut tx, vals).await.unwrap();
    let term = items(&entry, &mut tx)
        .await
        .into_iter()
        .find(|r| r.0 == "payment_term")
        .unwrap();
    assert_eq!(term.1, receivable);
    assert_ne!(term.1, other.receivable);
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn posting_checks_list_every_problem() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let b = books(&mut tx, "Checks").await;
    let model = moves(&tx, b.company);
    let good = model
        .create(
            &mut tx,
            vec![invoice(
                "out_invoice",
                b.customer,
                vec![product_line("1", "10", "0", &[])],
            )],
        )
        .await
        .unwrap();
    let env = tx.env().with_company(b.company).unwrap();
    check_postable(&mut tx, &env, &good.ids()).await.unwrap();

    let empty = model
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("move_type", text("out_invoice"))],
        )
        .await
        .unwrap();
    let negative = model
        .create(
            &mut tx,
            vec![invoice(
                "out_invoice",
                b.customer,
                vec![product_line("-1", "10", "0", &[])],
            )],
        )
        .await
        .unwrap();
    let mut bill = invoice(
        "in_invoice",
        b.customer,
        vec![product_line("1", "10", "0", &[])],
    );
    bill.remove("invoice_date");
    let bill = model.create(&mut tx, vec![bill]).await.unwrap();
    let ids = [empty.ids(), negative.ids(), bill.ids()].concat();
    let err = check_postable(&mut tx, &env, &ids).await.unwrap_err();
    let message = err.to_string();
    assert_eq!(err.kind(), ErrorKind::UserError);
    for part in [
        "choose the customer",
        "add at least one product line",
        "the total is negative",
        "enter the bill date",
    ] {
        assert!(message.contains(part), "{part:?} missing in {message}");
    }
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn before_post_dates_customer_documents_only() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let b = books(&mut tx, "Dates").await;
    let model = moves(&tx, b.company);
    let mut sale = invoice(
        "out_invoice",
        b.customer,
        vec![product_line("1", "10", "0", &[])],
    );
    sale.remove("invoice_date");
    let sale = model.create(&mut tx, vec![sale]).await.unwrap();
    let mut bill = invoice(
        "in_invoice",
        b.customer,
        vec![product_line("1", "10", "0", &[])],
    );
    bill.remove("invoice_date");
    let bill = model.create(&mut tx, vec![bill]).await.unwrap();
    let env = tx.env().with_company(b.company).unwrap();
    before_post(&mut tx, &env, &[sale.ids(), bill.ids()].concat())
        .await
        .unwrap();
    let today = env.context_today().unwrap();
    assert_eq!(
        sale.cell(&mut tx, "invoice_date").await.unwrap(),
        Cell::Date(today)
    );
    assert_eq!(sale.cell(&mut tx, "date").await.unwrap(), Cell::Date(today));
    assert_eq!(
        sale.cell(&mut tx, "invoice_date_due").await.unwrap(),
        Cell::Date(today)
    );
    assert!(!matches!(
        bill.cell(&mut tx, "invoice_date").await.unwrap(),
        Cell::Date(_)
    ));
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// Plain journal entries are not touched by the invoice sync or checks.
#[tokio::test]
async fn journal_entries_are_left_alone() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let b = books(&mut tx, "Entries").await;
    let misc = journal(&mut tx, b.company, "MISC", "general", None).await;
    let entry = moves(&tx, b.company)
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("journal_id", Cell::Id(misc))],
        )
        .await
        .unwrap();
    assert_eq!(
        entry.cell(&mut tx, "move_type").await.unwrap(),
        text("entry")
    );
    assert!(items(&entry, &mut tx).await.is_empty());
    let env = tx.env().with_company(b.company).unwrap();
    check_postable(&mut tx, &env, &entry.ids()).await.unwrap();
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn action_post_runs_the_invoice_checks() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let b = books(&mut tx, "Wired").await;
    let model = moves(&tx, b.company);
    let mut bill = invoice(
        "in_invoice",
        b.customer,
        vec![product_line("1", "10", "0", &[])],
    );
    bill.remove("invoice_date");
    let bill = model.create(&mut tx, vec![bill]).await.unwrap();
    let err = bill
        .call::<(), ()>(&mut tx, "action_post", ())
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::UserError);
    let message = err.to_string();
    assert!(message.contains("enter the bill date"), "{message}");
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// receivable lines to the new currency, the balance converted at its rate; sections and notes
/// keep theirs)
#[tokio::test]
async fn a_currency_change_moves_every_amount_line_to_the_new_currency() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let b = books(&mut tx, "Currency").await;
    tx.env()
        .records("res.currency.rate.type")
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", text("Spot"))
                    .set_cell("code", text("spot"))
                    .set_cell("date_rule", text("strictly_before"))
                    .set_cell("later_fallback", Cell::Bool(true)),
            ],
        )
        .await
        .unwrap();
    let foreign = tx
        .env()
        .records("res.currency")
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", text("XEU"))
                    .set_cell("symbol", text("XEU"))
                    .set_cell("rounding", Cell::Decimal(d("0.01")))
                    .set_cell("active", Cell::Bool(true)),
            ],
        )
        .await
        .unwrap()
        .ids()[0];
    // two units of the foreign currency per unit of the company currency
    tx.env()
        .with_company(b.company)
        .unwrap()
        .records("res.currency.rate")
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("currency_id", Cell::Id(foreign))
                    .set_cell("company_id", Cell::Id(b.company))
                    .set_cell("name", Cell::Date(Date::parse_odoo("2026-01-01").unwrap()))
                    .set_cell("rate", Cell::Decimal(d("2"))),
            ],
        )
        .await
        .unwrap();
    let entry = moves(&tx, b.company)
        .create(
            &mut tx,
            vec![invoice(
                "out_invoice",
                b.customer,
                vec![product_line("1", "100", "0", &[b.sale_tax])],
            )],
        )
        .await
        .unwrap();
    entry
        .write(
            &mut tx,
            RawVals::new().set_cell("currency_id", Cell::Id(foreign)),
        )
        .await
        .unwrap();
    tx.flush().await.unwrap();
    let id = entry.ids()[0];
    let rows: Vec<(String, i64, Decimal, Decimal)> = tx
        .query(
            "SELECT display_type, currency_id::int8, amount_currency::text, balance::text \
             FROM account_move_line WHERE move_id = $1 ORDER BY display_type",
            &[&id],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| {
            (
                r.get(0),
                r.get(1),
                d(&r.get::<_, String>(2)),
                d(&r.get::<_, String>(3)),
            )
        })
        .collect();
    assert_eq!(
        rows,
        [
            ("payment_term".to_owned(), foreign, d("115"), d("57.5")),
            ("product".to_owned(), foreign, d("-100"), d("-50")),
            ("tax".to_owned(), foreign, d("-15"), d("-7.5")),
        ]
    );
    assert_eq!(decimal(&entry, &mut tx, "amount_total").await, d("115"));
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// (display type, amount in currency, balance) of every journal item of `entry`, by display type.
async fn amounts(entry: &Records, tx: &mut Transaction) -> Vec<(String, Decimal, Decimal)> {
    tx.flush().await.unwrap();
    let id = entry.ids()[0];
    tx.query(
        "SELECT display_type, amount_currency::text, balance::text FROM account_move_line \
         WHERE move_id = $1 ORDER BY display_type",
        &[&id],
    )
    .await
    .unwrap()
    .iter()
    .map(|r| {
        (
            r.get(0),
            d(&r.get::<_, String>(1)),
            d(&r.get::<_, String>(2)),
        )
    })
    .collect()
}

/// A second currency with one rate of `company` per (date, units per unit of the company
/// currency) of `rates`; the rate of a date is the latest one dated before it.
async fn foreign_currency(tx: &mut Transaction, company: i64, rates: &[(&str, &str)]) -> i64 {
    tx.env()
        .records("res.currency.rate.type")
        .unwrap()
        .create(
            tx,
            vec![
                RawVals::new()
                    .set_cell("name", text("Spot"))
                    .set_cell("code", text("spot"))
                    .set_cell("date_rule", text("strictly_before"))
                    .set_cell("later_fallback", Cell::Bool(true)),
            ],
        )
        .await
        .unwrap();
    let foreign = tx
        .env()
        .records("res.currency")
        .unwrap()
        .create(
            tx,
            vec![
                RawVals::new()
                    .set_cell("name", text("XEU"))
                    .set_cell("symbol", text("XEU"))
                    .set_cell("rounding", Cell::Decimal(d("0.01")))
                    .set_cell("active", Cell::Bool(true)),
            ],
        )
        .await
        .unwrap()
        .ids()[0];
    let vals: Vec<RawVals> = rates
        .iter()
        .map(|(date, rate)| {
            RawVals::new()
                .set_cell("currency_id", Cell::Id(foreign))
                .set_cell("company_id", Cell::Id(company))
                .set_cell("name", Cell::Date(Date::parse_odoo(date).unwrap()))
                .set_cell("rate", Cell::Decimal(d(rate)))
        })
        .collect();
    tx.env()
        .with_company(company)
        .unwrap()
        .records("res.currency.rate")
        .unwrap()
        .create(tx, vals)
        .await
        .unwrap();
    foreign
}

/// A customer invoice books its tax on the account of the tax's invoice repartition line, a
/// customer credit note on the account of its refund repartition line.
#[tokio::test]
async fn a_credit_note_books_its_tax_on_the_refund_repartition_account() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let b = books(&mut tx, "Refunds").await;
    let refund_account = account(&mut tx, b.company, "252000", "liability_current").await;
    let vat = tax_row(
        &mut tx, b.company, "VAT 10%", "sale", "percent", "10", false,
    )
    .await;
    repartition(
        &mut tx,
        vat,
        b.company,
        "invoice",
        &[("100", b.sale_tax_account)],
    )
    .await;
    repartition(
        &mut tx,
        vat,
        b.company,
        "refund",
        &[("100", refund_account)],
    )
    .await;
    for (move_type, tax_account, tax_balance) in [
        ("out_invoice", b.sale_tax_account, "-10"),
        ("out_refund", refund_account, "10"),
    ] {
        let entry = moves(&tx, b.company)
            .create(
                &mut tx,
                vec![invoice(
                    move_type,
                    b.customer,
                    vec![product_line("1", "100", "0", &[vat])],
                )],
            )
            .await
            .unwrap();
        let tax_lines: Vec<(i64, Decimal)> = items(&entry, &mut tx)
            .await
            .into_iter()
            .filter(|r| r.0 == "tax")
            .map(|r| (r.1, r.2))
            .collect();
        assert_eq!(tax_lines, [(tax_account, d(tax_balance))], "{move_type}");
    }
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn included_fixed_and_split_taxes_make_their_tax_lines_and_totals() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let b = books(&mut tx, "Kinds").await;
    let second = account(&mut tx, b.company, "252000", "liability_current").await;
    let included = tax_row(
        &mut tx,
        b.company,
        "VAT 15% included",
        "sale",
        "percent",
        "15",
        true,
    )
    .await;
    let fixed = tax_row(&mut tx, b.company, "Levy 5", "sale", "fixed", "5", false).await;
    let split = tax_row(
        &mut tx,
        b.company,
        "VAT 15% split",
        "sale",
        "percent",
        "15",
        false,
    )
    .await;
    for id in [included, fixed] {
        for document in ["invoice", "refund"] {
            repartition(
                &mut tx,
                id,
                b.company,
                document,
                &[("100", b.sale_tax_account)],
            )
            .await;
        }
    }
    for document in ["invoice", "refund"] {
        repartition(
            &mut tx,
            split,
            b.company,
            document,
            &[("50", b.sale_tax_account), ("50", second)],
        )
        .await;
    }
    // (line, tax lines as (account, balance), product balance, untaxed, tax, total)
    let cases = [
        (
            // 115 with 15 % inside: 100 + 15
            product_line("1", "115", "0", &[included]),
            vec![(b.sale_tax_account, d("-15"))],
            "-100",
            "100",
            "15",
            "115",
        ),
        (
            // 3 units at 50 with 5 per unit: 150 + 15
            product_line("3", "50", "0", &[fixed]),
            vec![(b.sale_tax_account, d("-15"))],
            "-150",
            "150",
            "15",
            "165",
        ),
        (
            // 15 % of 100 in two halves on two accounts
            product_line("1", "100", "0", &[split]),
            vec![(b.sale_tax_account, d("-7.5")), (second, d("-7.5"))],
            "-100",
            "100",
            "15",
            "115",
        ),
    ];
    for (line, mut tax_lines, product, untaxed, tax_amount, total) in cases {
        let entry = moves(&tx, b.company)
            .create(
                &mut tx,
                vec![invoice("out_invoice", b.customer, vec![line])],
            )
            .await
            .unwrap();
        let found = items(&entry, &mut tx).await;
        let mut found_tax: Vec<(i64, Decimal)> = found
            .iter()
            .filter(|r| r.0 == "tax")
            .map(|r| (r.1, r.2))
            .collect();
        found_tax.sort();
        tax_lines.sort();
        assert_eq!(found_tax, tax_lines, "total {total}");
        let product_item = found.iter().find(|r| r.0 == "product").unwrap();
        assert_eq!(
            (product_item.1, product_item.2),
            (b.income, d(product)),
            "total {total}"
        );
        let term = found.iter().find(|r| r.0 == "payment_term").unwrap();
        assert_eq!((term.1, term.2), (b.receivable, d(total)));
        assert_eq!(decimal(&entry, &mut tx, "amount_untaxed").await, d(untaxed));
        assert_eq!(decimal(&entry, &mut tx, "amount_tax").await, d(tax_amount));
        assert_eq!(decimal(&entry, &mut tx, "amount_total").await, d(total));
    }
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// The balances of a foreign-currency draft invoice are converted at its accounting date; after
/// that date changes, the redated sync converts the product, tax and receivable balances again at
/// the new date's rate and leaves the amounts in currency as they are.
#[tokio::test]
async fn a_new_accounting_date_converts_foreign_balances_again() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let b = books(&mut tx, "Redated").await;
    // two units of the foreign currency per company unit from January, four from June
    let foreign = foreign_currency(
        &mut tx,
        b.company,
        &[("2026-01-01", "2"), ("2026-06-01", "4")],
    )
    .await;
    let mut vals = invoice(
        "out_invoice",
        b.customer,
        vec![product_line("1", "100", "0", &[b.sale_tax])],
    );
    vals.insert(
        "invoice_date".into(),
        Cell::Date(Date::parse_odoo("2026-03-10").unwrap()),
    );
    let entry = moves(&tx, b.company)
        .create(&mut tx, vec![vals])
        .await
        .unwrap();
    entry
        .write(
            &mut tx,
            RawVals::new().set_cell("currency_id", Cell::Id(foreign)),
        )
        .await
        .unwrap();
    assert_eq!(
        amounts(&entry, &mut tx).await,
        [
            ("payment_term".to_owned(), d("115"), d("57.5")),
            ("product".to_owned(), d("-100"), d("-50")),
            ("tax".to_owned(), d("-15"), d("-7.5")),
        ],
        "at the March date the January rate applies"
    );
    let new_date = Date::parse_odoo("2026-07-10").unwrap();
    entry
        .write(
            &mut tx,
            RawVals::new().set_cell("date", Cell::Date(new_date)),
        )
        .await
        .unwrap();
    let env = tx.env().with_company(b.company).unwrap();
    sync_lines_redated(&mut tx, &env, &entry.ids())
        .await
        .unwrap();
    assert_eq!(
        entry.cell(&mut tx, "date").await.unwrap(),
        Cell::Date(new_date)
    );
    assert_eq!(
        amounts(&entry, &mut tx).await,
        [
            ("payment_term".to_owned(), d("115"), d("28.75")),
            ("product".to_owned(), d("-100"), d("-25")),
            ("tax".to_owned(), d("-15"), d("-3.75")),
        ],
        "at the July date the June rate applies"
    );
    assert_eq!(decimal(&entry, &mut tx, "amount_total").await, d("115"));

    // an invoice in the company currency is not touched by the redated sync
    let plain = moves(&tx, b.company)
        .create(
            &mut tx,
            vec![invoice(
                "out_invoice",
                b.customer,
                vec![product_line("1", "100", "0", &[b.sale_tax])],
            )],
        )
        .await
        .unwrap();
    let before = amounts(&plain, &mut tx).await;
    sync_lines_redated(&mut tx, &env, &plain.ids())
        .await
        .unwrap();
    assert_eq!(amounts(&plain, &mut tx).await, before);
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// Whether the stored `write_date` of `entry` is still the marker date set by [`mark_header`].
async fn header_untouched(entry: &Records, tx: &mut Transaction) -> bool {
    tx.flush().await.unwrap();
    let id = entry.ids()[0];
    tx.query(
        "SELECT write_date = '2000-01-01'::timestamp FROM account_move WHERE id = $1",
        &[&id],
    )
    .await
    .unwrap()[0]
        .get(0)
}

/// Stores a marker `write_date` on `entry`; every write of the header replaces it.
async fn mark_header(entry: &Records, tx: &mut Transaction) {
    tx.flush().await.unwrap();
    let id = entry.ids()[0];
    tx.query(
        "UPDATE account_move SET write_date = '2000-01-01'::timestamp WHERE id = $1",
        &[&id],
    )
    .await
    .unwrap();
}

/// A sync that changes a total writes the header; a sync of an unchanged invoice does not.
#[tokio::test]
async fn syncing_an_unchanged_invoice_does_not_write_its_header() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let b = books(&mut tx, "Unchanged").await;
    let entry = moves(&tx, b.company)
        .create(
            &mut tx,
            vec![invoice(
                "out_invoice",
                b.customer,
                vec![product_line("1", "100", "0", &[b.sale_tax])],
            )],
        )
        .await
        .unwrap();
    let env = tx.env().with_company(b.company).unwrap();
    // the control: a changed price makes the sync write the header, which replaces the marker
    entry
        .related(&mut tx, "invoice_line_ids")
        .await
        .unwrap()
        .write(
            &mut tx,
            RawVals::new().set_cell("price_unit", Cell::Decimal(d("200"))),
        )
        .await
        .unwrap();
    mark_header(&entry, &mut tx).await;
    sync_lines(&mut tx, &env, &entry.ids()).await.unwrap();
    assert_eq!(decimal(&entry, &mut tx, "amount_total").await, d("230"));
    assert!(
        !header_untouched(&entry, &mut tx).await,
        "a changed total writes the header"
    );
    // nothing changed since: the header is left alone
    mark_header(&entry, &mut tx).await;
    sync_lines(&mut tx, &env, &entry.ids()).await.unwrap();
    assert!(
        header_untouched(&entry, &mut tx).await,
        "an unchanged invoice keeps its header"
    );
    assert_eq!(decimal(&entry, &mut tx, "amount_total").await, d("230"));
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn a_branch_uses_the_receivable_account_of_its_parent_company() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let parent = company(&mut tx, "Group").await;
    let receivable = account(&mut tx, parent, "121000", "asset_receivable").await;
    let child = branch(&mut tx, "Group branch", parent).await;
    // an account's code belongs to the root company, so the branch books on the group's accounts
    let income = account(&mut tx, parent, "400100", "income").await;
    journal(&mut tx, child, "INV", "sale", Some(income)).await;
    let customer = partner(&mut tx, "Branch customer").await;
    let entry = moves(&tx, child)
        .create(
            &mut tx,
            vec![invoice(
                "out_invoice",
                customer,
                vec![product_line("1", "10", "0", &[])],
            )],
        )
        .await
        .unwrap();
    assert_eq!(
        entry.cell(&mut tx, "company_id").await.unwrap(),
        Cell::Id(child)
    );
    let term = items(&entry, &mut tx)
        .await
        .into_iter()
        .find(|r| r.0 == "payment_term")
        .unwrap();
    assert_eq!((term.1, term.2), (receivable, d("10")));
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn live_onchange_computes_draft_invoice_line_and_header_totals_before_save() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let b = books(&mut tx, "Live Onchange Co").await;
    let env = tx.env().with_company(b.company).unwrap();
    let currency = match env
        .records("res.company")
        .unwrap()
        .browse([b.company])
        .unwrap()
        .cell(&mut tx, "currency_id")
        .await
        .unwrap()
    {
        Cell::Id(id) => id,
        other => panic!("company currency is {other:?}"),
    };
    let uom: i64 = tx
        .query(
            "INSERT INTO uom_uom (name, active, relative_factor, factor, sequence) \
             VALUES (jsonb_build_object('en_US', 'Units'), true, 1, 1, 1) RETURNING id::int8",
            &[],
        )
        .await
        .unwrap()[0]
        .get(0);
    let tmpl: i64 = tx
        .query(
            "INSERT INTO product_template (name, active, type, uom_id, list_price) \
             VALUES (jsonb_build_object('en_US', 'Consulting'), true, 'service', $1, 100) \
             RETURNING id::int8",
            &[&uom],
        )
        .await
        .unwrap()[0]
        .get(0);
    let prod: i64 = tx
        .query(
            "INSERT INTO product_product (product_tmpl_id, active, standard_price, lst_price) \
             VALUES ($1, true, 50, 100) RETURNING id::int8",
            &[&tmpl],
        )
        .await
        .unwrap()[0]
        .get(0);
    tx.execute(
        "INSERT INTO product_taxes_rel (prod_id, tax_id) VALUES ($1, $2)",
        &[&tmpl, &b.sale_tax],
    )
    .await
    .unwrap();

    let obj = |entries: Vec<(&str, Value)>| -> Value {
        let mut map = erp_sdk::ValueMap::new();
        for (k, v) in entries {
            map.insert(k.to_owned(), v);
        }
        Value::Map(std::sync::Arc::new(map))
    };

    // 1. Line onchange on an unsaved invoice line with a virtual unsaved `account.move` parent.
    let line_spec = obj(vec![
        ("name", obj(vec![])),
        ("product_uom_id", obj(vec![])),
        ("account_id", obj(vec![])),
        ("price_unit", obj(vec![])),
        ("tax_ids", obj(vec![])),
        ("price_subtotal", obj(vec![])),
        ("price_total", obj(vec![])),
    ]);
    let parent_dict = obj(vec![
        ("id", Value::False),
        ("move_type", Value::str("out_invoice")),
        ("company_id", Value::Int(b.company)),
        ("currency_id", Value::Int(currency)),
        ("journal_id", Value::Int(b.sale)),
        ("partner_id", Value::Int(b.customer)),
    ]);

    let res = env
        .records("account.move.line")
        .unwrap()
        .web_call(
            &mut tx,
            "onchange",
            &obj(vec![
                (
                    "values",
                    obj(vec![
                        ("move_id", parent_dict.clone()),
                        ("display_type", Value::str("product")),
                        ("product_id", Value::Int(prod)),
                        ("quantity", Value::Decimal(d("2"))),
                    ]),
                ),
                ("field_names", Value::list([Value::str("product_id")])),
                ("fields_spec", line_spec.clone()),
            ]),
        )
        .await
        .unwrap();
    let lval = res.dict().unwrap().get("value").unwrap();
    let lmap = lval.dict().unwrap();
    assert_eq!(lmap.get("name").unwrap(), Value::str("Consulting"));
    assert_eq!(lmap.get("product_uom_id").unwrap(), Value::Int(uom));
    assert_eq!(lmap.get("account_id").unwrap(), Value::Int(b.income));
    assert_eq!(lmap.get("price_unit").unwrap(), Value::Decimal(d("100")));
    assert_eq!(
        lmap.get("price_subtotal").unwrap(),
        Value::Decimal(d("200"))
    );
    assert_eq!(lmap.get("price_total").unwrap(), Value::Decimal(d("230")));

    // Editing quantity + discount refreshes line amounts immediately.
    let res = env
        .records("account.move.line")
        .unwrap()
        .web_call(
            &mut tx,
            "onchange",
            &obj(vec![
                (
                    "values",
                    obj(vec![
                        ("move_id", parent_dict),
                        ("display_type", Value::str("product")),
                        ("product_id", Value::Int(prod)),
                        ("quantity", Value::Decimal(d("4"))),
                        ("price_unit", Value::Decimal(d("100"))),
                        ("discount", Value::Decimal(d("10"))),
                        (
                            "tax_ids",
                            Value::list([Value::list([
                                Value::Int(6),
                                Value::Int(0),
                                Value::list([Value::Int(b.sale_tax)]),
                            ])]),
                        ),
                        ("price_subtotal", Value::Decimal(d("200"))),
                        ("price_total", Value::Decimal(d("230"))),
                    ]),
                ),
                (
                    "field_names",
                    Value::list([Value::str("quantity"), Value::str("discount")]),
                ),
                ("fields_spec", line_spec),
            ]),
        )
        .await
        .unwrap();
    let lval = res.dict().unwrap().get("value").unwrap();
    let lmap = lval.dict().unwrap();
    // 4 * 100 * 0.9 = 360 subtotal, 15% tax = 54, total = 414
    assert_eq!(
        lmap.get("price_subtotal").unwrap(),
        Value::Decimal(d("360"))
    );
    assert_eq!(lmap.get("price_total").unwrap(), Value::Decimal(d("414")));

    // 2. Invoice header onchange computes untaxed, tax, total, residual and due date before Save.
    let inv_spec = obj(vec![
        ("amount_untaxed", obj(vec![])),
        ("amount_tax", obj(vec![])),
        ("amount_total", obj(vec![])),
        ("amount_residual", obj(vec![])),
        ("invoice_date_due", obj(vec![])),
    ]);
    let inv_date = Date::from_ymd(2026, 11, 26).unwrap();
    let inv_res = env
        .records("account.move")
        .unwrap()
        .web_call(
            &mut tx,
            "onchange",
            &obj(vec![
                (
                    "values",
                    obj(vec![
                        ("move_type", Value::str("out_invoice")),
                        ("state", Value::str("draft")),
                        ("company_id", Value::Int(b.company)),
                        ("currency_id", Value::Int(currency)),
                        ("journal_id", Value::Int(b.sale)),
                        ("partner_id", Value::Int(b.customer)),
                        ("invoice_date", Value::Date(inv_date)),
                        ("amount_untaxed", Value::Decimal(Decimal::ZERO)),
                        ("amount_tax", Value::Decimal(Decimal::ZERO)),
                        ("amount_total", Value::Decimal(Decimal::ZERO)),
                        ("amount_residual", Value::Decimal(Decimal::ZERO)),
                        (
                            "invoice_line_ids",
                            Value::list([Value::list([
                                Value::Int(0),
                                Value::str("line_1"),
                                obj(vec![
                                    ("display_type", Value::str("product")),
                                    ("name", Value::str("Consulting")),
                                    ("account_id", Value::Int(b.income)),
                                    ("quantity", Value::Decimal(d("2"))),
                                    ("price_unit", Value::Decimal(d("100"))),
                                    ("discount", Value::Decimal(Decimal::ZERO)),
                                    (
                                        "tax_ids",
                                        Value::list([Value::list([
                                            Value::Int(6),
                                            Value::Int(0),
                                            Value::list([Value::Int(b.sale_tax)]),
                                        ])]),
                                    ),
                                ]),
                            ])]),
                        ),
                    ]),
                ),
                ("field_names", Value::list([Value::str("invoice_line_ids")])),
                ("fields_spec", inv_spec),
            ]),
        )
        .await
        .unwrap();
    let ival = inv_res.dict().unwrap().get("value").unwrap();
    let imap = ival.dict().unwrap();
    assert_eq!(
        imap.get("amount_untaxed").unwrap(),
        Value::Decimal(d("200"))
    );
    assert_eq!(imap.get("amount_tax").unwrap(), Value::Decimal(d("30")));
    assert_eq!(imap.get("amount_total").unwrap(), Value::Decimal(d("230")));
    assert_eq!(
        imap.get("amount_residual").unwrap(),
        Value::Decimal(d("230"))
    );
    assert_eq!(imap.get("invoice_date_due").unwrap(), Value::Date(inv_date));

    // SYNC_OWNED_FIELDS are still refused on real persistence writes.
    let err = tx
        .savepoint(async |tx| {
            moves(tx, b.company)
                .create(
                    tx,
                    vec![
                        invoice(
                            "out_invoice",
                            b.customer,
                            vec![product_line("1", "10", "0", &[])],
                        )
                        .set_cell("amount_total", Cell::Decimal(d("999"))),
                    ],
                )
                .await
        })
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::UserError);

    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}
