// SPDX-License-Identifier: MIT OR Apache-2.0
//! Upgrade built-in provider bindings, menu defaults and report definitions cleanly across versions.
//!
//! The report definitions are `noupdate` data, so a new layout reaches an existing database only
//! through these steps. Every step changes a record only while it still holds the value an earlier
//! version shipped, and deletes only shipped records that are unchanged: what an administrator
//! changed or added stays. Where a kept change cannot work with the new layout the upgrade stops
//! with an error naming the record and what to reset.
use erp_sdk::migrations::SemverVersion;
use erp_sdk::prelude::*;

#[erp_sdk::migration(addon = "account_reporting", version = "1.6", stage = post)]
async fn guard_builtin_balance_sheet(tx: &mut Transaction, _from: &SemverVersion) -> Result<()> {
    tx.execute(
        "UPDATE reporting_report r SET provider = 'account_reporting.balance_sheet' \
         FROM ir_model_data d WHERE d.module = 'account_reporting' AND d.name = 'report_balance_sheet' \
           AND d.model = 'reporting.report' AND d.res_id = r.id AND r.provider = 'reporting.definition'",
        &[],
    )
    .await?;
    Ok(())
}

#[erp_sdk::migration(addon = "account_reporting", version = "1.8", stage = post)]
async fn guard_builtin_partner_ledger(tx: &mut Transaction, _from: &SemverVersion) -> Result<()> {
    tx.execute(
        "UPDATE reporting_report r SET provider = 'account_reporting.partner_ledger' \
         FROM ir_model_data d WHERE d.module = 'account_reporting' AND d.name = 'report_partner_ledger' \
           AND d.model = 'reporting.report' AND d.res_id = r.id AND r.provider = 'reporting.definition'",
        &[],
    )
    .await?;
    Ok(())
}

#[erp_sdk::migration(addon = "account_reporting", version = "1.9", stage = post)]
async fn general_ledger_menu_glyph(tx: &mut Transaction, _from: &SemverVersion) -> Result<()> {
    tx.execute(
        "UPDATE ir_ui_menu m SET web_icon = 'lucide:book-open' FROM ir_model_data d \
         WHERE d.module = 'account_reporting' AND d.name = 'menu_general_ledger' \
         AND d.model = 'ir.ui.menu' AND d.res_id = m.id AND COALESCE(m.web_icon, '') = ''",
        &[],
    )
    .await?;
    Ok(())
}

const REPORT: &str = "reporting.report";
const LINE: &str = "reporting.report.line";
const COLUMN: &str = "reporting.report.column";
const EXPR: &str = "reporting.report.expression";
const MODELS: [(&str, &str); 4] = [
    (REPORT, "reporting_report"),
    (LINE, "reporting_report_line"),
    (COLUMN, "reporting_report_column"),
    (EXPR, "reporting_report_expression"),
];

fn table(model: &str) -> Result<&'static str> {
    MODELS
        .iter()
        .find(|(name, _)| *name == model)
        .map(|(_, table)| *table)
        .ok_or_else(|| {
            Error::programming(format!("account_reporting migration: no table for {model}"))
        })
}

fn db(error: impl std::fmt::Display) -> Error {
    Error::db(format!("account_reporting migration: {error}"))
}

/// The record behind a shipped external id, or `None` when the caller has nothing to change.
async fn builtin(tx: &mut Transaction, model: &str, name: &str) -> Result<Option<i64>> {
    let sql = format!(
        "SELECT d.res_id::int8, EXISTS (SELECT 1 FROM {} r WHERE r.id = d.res_id) \
         FROM ir_model_data d WHERE d.module = 'account_reporting' AND d.model = $1 AND d.name = $2",
        table(model)?
    );
    let rows = tx.query(&sql, &[&model, &name]).await?;
    let Some(row) = rows.first() else {
        // The external id is missing: the record was not shipped yet at the installed version
        // (the data files create it after this step), or its external id was removed. Skipped.
        return Ok(None);
    };
    if !row.try_get::<_, bool>(1).map_err(db)? {
        // The external id exists but an administrator deleted its record. Skipped.
        return Ok(None);
    }
    Ok(Some(row.try_get(0).map_err(db)?))
}

/// A column as SQL text: `name` is its source-language text.
fn column_text(column: &str) -> String {
    if column == "name" {
        "r.name->>'en_US'".to_owned()
    } else {
        format!("r.{column}::text")
    }
}

/// `AND` conditions over `$3`, one per `(column, value)` of `when` (`None`: SQL null).
fn conditions(when: &[(&str, Option<&str>)]) -> String {
    when.iter()
        .enumerate()
        .map(|(i, (column, _))| {
            format!(
                " AND {} IS NOT DISTINCT FROM ($3::text[])[{}]",
                column_text(column),
                i + 1
            )
        })
        .collect()
}

/// The record behind `xmlid` when every `(column, value)` of `when` still holds.
async fn shipped(
    tx: &mut Transaction,
    model: &str,
    xmlid: &str,
    when: &[(&str, Option<&str>)],
) -> Result<Option<i64>> {
    let sql = format!(
        "SELECT r.id::int8 FROM {} r JOIN ir_model_data d ON d.res_id = r.id \
         WHERE d.module = 'account_reporting' AND d.model = $1 AND d.name = $2{}",
        table(model)?,
        conditions(when)
    );
    let values: Vec<Option<&str>> = when.iter().map(|(_, value)| *value).collect();
    let rows = tx.query(&sql, &[&model, &xmlid, &values]).await?;
    rows.first()
        .map(|row| row.try_get(0).map_err(db))
        .transpose()
}

/// Set `column` of the record behind `xmlid` to `new` while it still holds `old`; the number of
/// rows changed.
async fn set_if(
    tx: &mut Transaction,
    model: &str,
    xmlid: &str,
    column: &str,
    old: Option<&str>,
    new: Option<&str>,
) -> Result<u64> {
    set_when(tx, model, xmlid, &[(column, old)], column, new).await
}

/// Set `column` of the record behind `xmlid` to `new` while every condition of `when` holds.
async fn set_when(
    tx: &mut Transaction,
    model: &str,
    xmlid: &str,
    when: &[(&str, Option<&str>)],
    column: &str,
    new: Option<&str>,
) -> Result<u64> {
    let value = format!("($3::text[])[{}]", when.len() + 1);
    let assignment = match column {
        "name" => format!("name = jsonb_set(r.name, '{{en_US}}', to_jsonb({value}))"),
        "sequence" => format!("sequence = {value}::int4"),
        "totals_below" | "comparison" | "unfold_all" | "search_bar" | "blank_if_zero"
        | "current_only" => format!("{column} = {value}::bool"),
        _ => format!("{column} = {value}"),
    };
    let sql = format!(
        "UPDATE {} r SET {assignment} FROM ir_model_data d \
         WHERE d.module = 'account_reporting' AND d.model = $1 AND d.name = $2 AND d.res_id = r.id{}",
        table(model)?,
        conditions(when)
    );
    let mut values: Vec<Option<&str>> = when.iter().map(|(_, value)| *value).collect();
    values.push(new);
    tx.execute(&sql, &[&model, &xmlid, &values]).await
}

/// Apply every `(xmlid, column, old, new)` of `changes` to records of `model`.
async fn set_all(
    tx: &mut Transaction,
    model: &str,
    changes: &[(&str, &str, Option<&str>, Option<&str>)],
) -> Result<()> {
    for (xmlid, column, old, new) in changes {
        set_if(tx, model, xmlid, column, *old, *new).await?;
    }
    Ok(())
}

async fn delete(tx: &mut Transaction, model: &str, ids: &[i64]) -> Result<()> {
    if !ids.is_empty() {
        let sql = format!("DELETE FROM {} WHERE id = ANY($1)", table(model)?);
        tx.execute(&sql, &[&ids]).await?;
    }
    Ok(())
}

/// Delete the shipped records `names` of `model` (their sub-records cascade).
async fn delete_shipped(tx: &mut Transaction, model: &str, names: &[&str]) -> Result<()> {
    let mut ids = Vec::new();
    for name in names {
        ids.extend(builtin(tx, model, name).await?);
    }
    delete(tx, model, &ids).await
}

/// Remove the external ids of this addon's report records that no longer exist (deleted here,
/// cascaded from a deleted line, or left behind by the 1.13 steps): `noupdate` ids are never
/// cleaned up by the end-of-upgrade pass.
async fn drop_dangling(tx: &mut Transaction) -> Result<()> {
    for (model, table) in MODELS {
        let sql = format!(
            "DELETE FROM ir_model_data d WHERE d.module = 'account_reporting' AND d.model = $1 \
             AND NOT EXISTS (SELECT 1 FROM {table} r WHERE r.id = d.res_id)"
        );
        tx.execute(&sql, &[&model]).await?;
    }
    Ok(())
}

/// The provider of the report behind `xmlid` is `provider`.
async fn provided_by(tx: &mut Transaction, xmlid: &str, provider: &str) -> Result<bool> {
    Ok(shipped(tx, REPORT, xmlid, &[("provider", Some(provider))])
        .await?
        .is_some())
}

/// A shipped expression of an earlier version, with every value it was shipped with.
struct Expr {
    xmlid: &'static str,
    label: &'static str,
    engine: &'static str,
    model: Option<&'static str>,
    field: Option<&'static str>,
    domain: Option<&'static str>,
    formula: Option<&'static str>,
    date_field: Option<&'static str>,
    date_scope: &'static str,
}

const fn aggregate(
    xmlid: &'static str,
    label: &'static str,
    model: &'static str,
    field: &'static str,
    date_scope: &'static str,
    domain: &'static str,
) -> Expr {
    Expr {
        xmlid,
        label,
        engine: "aggregate",
        model: Some(model),
        field: Some(field),
        domain: Some(domain),
        formula: None,
        date_field: Some("date"),
        date_scope,
    }
}

const fn formula(xmlid: &'static str, label: &'static str, formula: &'static str) -> Expr {
    Expr {
        xmlid,
        label,
        engine: "formula",
        model: None,
        field: None,
        domain: None,
        formula: Some(formula),
        date_field: None,
        date_scope: "strict_range",
    }
}

impl Expr {
    /// The record when it still has every shipped value.
    async fn unchanged(&self, tx: &mut Transaction) -> Result<Option<i64>> {
        let when = [
            ("label", Some(self.label)),
            ("engine", Some(self.engine)),
            ("model", self.model),
            ("field", self.field),
            ("domain", self.domain),
            ("formula", self.formula),
            ("function", None),
            ("date_field", self.date_field),
            ("date_scope", Some(self.date_scope)),
            ("zero_division", Some("error")),
        ];
        shipped(tx, EXPR, self.xmlid, &when).await
    }
}

/// A grouped line of an earlier version that this version gives sub-lines (data files add them).
struct Container {
    line: &'static str,
    groupby: &'static str,
    /// The fold mode it gets from `foldable` (`None`: unchanged).
    fold: Option<&'static str>,
    old: &'static [Expr],
    /// `(label, external id)` of the expressions the data files add to it.
    new: &'static [(&'static str, &'static str)],
}

impl Container {
    async fn convert(&self, tx: &mut Transaction) -> Result<()> {
        let Some(line) = builtin(tx, LINE, self.line).await? else {
            return Ok(());
        };
        // A line has sub-lines or a group-by, never both.
        if set_if(tx, LINE, self.line, "groupby", Some(self.groupby), None).await? == 0 {
            let rows = tx
                .query(
                    "SELECT groupby FROM reporting_report_line WHERE id = $1",
                    &[&line],
                )
                .await?;
            let groupby: Option<String> = rows
                .first()
                .map(|row| row.try_get(0).map_err(db))
                .transpose()?
                .flatten();
            if let Some(groupby) = groupby.filter(|g| !g.is_empty()) {
                return Err(Error::validation(format!(
                    "account_reporting upgrade: report line account_reporting.{} groups by \
                     '{groupby}', but this version gives it sub-lines and a line cannot have both. \
                     Reset its Group By to '{}' or clear it, then upgrade again.",
                    self.line, self.groupby
                )));
            }
        }
        if let Some(fold) = self.fold {
            set_if(
                tx,
                LINE,
                self.line,
                "foldable",
                Some("foldable"),
                Some(fold),
            )
            .await?;
        }
        // The shipped expressions go only together: a changed one may read the others.
        let (mut unchanged, mut changed) = (Vec::new(), false);
        for expr in self.old {
            if builtin(tx, EXPR, expr.xmlid).await?.is_some() {
                match expr.unchanged(tx).await? {
                    Some(id) => unchanged.push(id),
                    None => changed = true,
                }
            }
        }
        if !changed {
            delete(tx, EXPR, &unchanged).await?;
        }
        // An expression kept on the line takes the place of the new one with its label, so the
        // data files do not add a second one (a label is used once per line).
        for (label, xmlid) in self.new {
            if builtin(tx, EXPR, xmlid).await?.is_some() {
                continue;
            }
            let rows = tx
                .query(
                    "SELECT id::int8 FROM reporting_report_expression WHERE line_id = $1 AND label = $2",
                    &[&line, label],
                )
                .await?;
            let Some(row) = rows.first() else {
                continue;
            };
            let kept: i64 = row.try_get(0).map_err(db)?;
            // drop_dangling ran first, so a row of this name is not ours to move: the insert
            // refuses it loudly.
            tx.execute(
                "INSERT INTO ir_model_data (module, name, model, res_id, noupdate) \
                 VALUES ('account_reporting', $1, $2, $3, true)",
                &[xmlid, &EXPR, &kept],
            )
            .await?;
        }
        Ok(())
    }
}

const REPORTS_1_11: [&str; 3] = [
    "report_tax_report",
    "report_invoice_analysis",
    "report_analytic_report",
];

#[erp_sdk::migration(addon = "account_reporting", version = "1.12", stage = post)]
async fn upgrade_aged_and_analysis_ledger_layout(
    tx: &mut Transaction,
    _from: &SemverVersion,
) -> Result<()> {
    for report in ["report_aged_receivable", "report_aged_payable"] {
        set_all(
            tx,
            REPORT,
            &[
                (report, "date_mode", Some("range"), Some("single")),
                (report, "date_default", Some("this_month"), Some("today")),
                (report, "comparison", Some("true"), Some("false")),
                (report, "totals_below", Some("false"), Some("true")),
            ],
        )
        .await?;
    }
    for report in REPORTS_1_11 {
        set_if(
            tx,
            REPORT,
            report,
            "totals_below",
            Some("false"),
            Some("true"),
        )
        .await?;
    }
    Ok(())
}

const BS_ASSET: &str = r#"[["parent_state","=","posted"],["journal_id.journal_group_id","=",false],["account_id.account_type","in",["asset_cash","asset_receivable","asset_current","asset_non_current","asset_prepayments","asset_fixed"]]]"#;
const BS_LIABILITY: &str = r#"[["parent_state","=","posted"],["journal_id.journal_group_id","=",false],["account_id.account_type","in",["liability_payable","liability_credit_card","liability_current","liability_non_current"]]]"#;
const PL_INCOME: &str = r#"[["parent_state","=","posted"],["journal_id.journal_group_id","=",false],["account_id.account_type","in",["income","income_other"]]]"#;
const PL_EXPENSE: &str = r#"[["parent_state","=","posted"],["journal_id.journal_group_id","=",false],["account_id.account_type","in",["expense","expense_other","expense_depreciation","expense_direct_cost"]]]"#;
const AML: &str = "account.move.line";

const CONTAINERS_1_13: [Container; 4] = [
    Container {
        line: "position_asset",
        groupby: "account_id,move_id",
        fold: Some("always_unfolded"),
        old: &[aggregate(
            "position_asset_booked",
            "amount",
            AML,
            "balance",
            "from_beginning",
            BS_ASSET,
        )],
        new: &[("amount", "position_asset_total")],
    },
    Container {
        line: "position_liability",
        groupby: "account_id,move_id",
        fold: Some("always_unfolded"),
        old: &[
            aggregate(
                "position_liability_booked",
                "booked",
                AML,
                "balance",
                "from_beginning",
                BS_LIABILITY,
            ),
            formula("position_liability_display", "amount", "0 - booked"),
        ],
        new: &[("amount", "position_liability_total")],
    },
    Container {
        line: "profit_loss_income",
        groupby: "account_id,move_id",
        fold: Some("always_unfolded"),
        old: &[
            aggregate(
                "profit_loss_income_booked",
                "booked",
                AML,
                "balance",
                "strict_range",
                PL_INCOME,
            ),
            formula("profit_loss_income_amount", "amount", "0 - booked"),
        ],
        new: &[("amount", "profit_loss_income_amount")],
    },
    Container {
        line: "profit_loss_expense",
        groupby: "account_id,move_id",
        fold: Some("always_unfolded"),
        old: &[aggregate(
            "profit_loss_expense_booked",
            "amount",
            AML,
            "balance",
            "strict_range",
            PL_EXPENSE,
        )],
        new: &[("amount", "profit_loss_expense_total")],
    },
];

/// Prepare the shipped `noupdate` lines before the data files add the classified Balance Sheet
/// and Profit and Loss sub-lines and the ledger columns.
#[erp_sdk::migration(addon = "account_reporting", version = "1.13", stage = pre)]
async fn prepare_core_accounting_reports_pre(
    tx: &mut Transaction,
    _from: &SemverVersion,
) -> Result<()> {
    drop_dangling(tx).await?;
    for container in &CONTAINERS_1_13 {
        container.convert(tx).await?;
    }
    // Databases installed before 1.12 still have the first layout: top-level totals at sequences
    // 30 to 100. The data files below add sub-lines with lower sequences under these lines and
    // refuse a sub-line placed before its parent, so the shipped totals move first; the 1.13 post
    // step then finds the values it expects.
    set_all(
        tx,
        LINE,
        &[
            ("profit_loss_income", "sequence", Some("40"), Some("10")),
            (
                "profit_loss_income",
                "name",
                Some("Total income"),
                Some("Income"),
            ),
            ("profit_loss_expense", "sequence", Some("50"), Some("20")),
            (
                "profit_loss_expense",
                "name",
                Some("Total expenses"),
                Some("Expenses"),
            ),
            ("profit_loss_result", "sequence", Some("60"), Some("30")),
            (
                "profit_loss_result",
                "name",
                Some("Net profit (loss)"),
                Some("Net profit"),
            ),
            ("position_asset", "sequence", Some("30"), Some("10")),
            (
                "position_asset",
                "name",
                Some("Total assets"),
                Some("Assets"),
            ),
            ("position_liability", "sequence", Some("60"), Some("20")),
            (
                "position_liability",
                "name",
                Some("Total liabilities"),
                Some("Liabilities"),
            ),
            ("position_capital", "sequence", Some("70"), Some("30")),
            ("position_earnings", "sequence", Some("80"), Some("40")),
            (
                "position_earnings",
                "name",
                Some("Cumulative unallocated earnings"),
                Some("Unallocated earnings"),
            ),
            ("position_equity", "sequence", Some("90"), Some("50")),
            (
                "position_financing",
                "name",
                Some("Total liabilities and equity"),
                Some("Liabilities and equity"),
            ),
            ("position_financing", "sequence", Some("100"), Some("60")),
        ],
    )
    .await?;

    // The native ledger providers never read definition lines: their shipped lines go (with
    // their expressions) while the report still uses the provider; lines an administrator added
    // stay.
    if provided_by(
        tx,
        "report_partner_ledger",
        "account_reporting.partner_ledger",
    )
    .await?
    {
        delete_shipped(
            tx,
            LINE,
            &["partner_ledger_receivable", "partner_ledger_payable"],
        )
        .await?;
    }
    let mut columns = Vec::new();
    for (xmlid, sequence, name, label) in [
        ("partner_ledger_column_opening", "10", "Opening", "opening"),
        ("partner_ledger_column_closing", "40", "Closing", "closing"),
    ] {
        let when = [
            ("sequence", Some(sequence)),
            ("name", Some(name)),
            ("label", Some(label)),
            ("kind", Some("monetary")),
            ("blank_if_zero", Some("false")),
        ];
        columns.extend(shipped(tx, COLUMN, xmlid, &when).await?);
    }
    delete(tx, COLUMN, &columns).await?;
    if provided_by(
        tx,
        "report_general_ledger",
        "account_reporting.general_ledger",
    )
    .await?
    {
        delete_shipped(
            tx,
            LINE,
            &[
                "gl_line_gl",
                "gl_line_opening",
                "gl_line_closing",
                "gl_line_gl_fiscal",
                "gl_line_opening_fiscal",
                "gl_line_closing_fiscal",
                "gl_line_unallocated",
            ],
        )
        .await?;
    }
    set_if(
        tx,
        REPORT,
        "report_ledger_trial_balance",
        "provider",
        Some("reporting.definition"),
        Some("account_reporting.trial_balance"),
    )
    .await?;
    if provided_by(
        tx,
        "report_ledger_trial_balance",
        "account_reporting.trial_balance",
    )
    .await?
    {
        delete_shipped(tx, LINE, &["trial_balance_by_account"]).await?;
    }
    drop_dangling(tx).await
}

const OTHER_REPORTS_1_13: [&str; 4] = [
    "report_cash_flow",
    "report_executive_summary",
    "report_account_movements",
    "report_posted_journal_activity",
];

#[erp_sdk::migration(addon = "account_reporting", version = "1.13", stage = post)]
async fn upgrade_core_accounting_reports_post(
    tx: &mut Transaction,
    _from: &SemverVersion,
) -> Result<()> {
    let (t, f) = (Some("true"), Some("false"));
    let (month, year) = (Some("this_month"), Some("this_year"));
    set_all(
        tx,
        REPORT,
        &[
            ("report_partner_ledger", "date_default", month, year),
            ("report_partner_ledger", "comparison", t, f),
            ("report_partner_ledger", "totals_below", f, t),
            ("report_general_ledger", "date_default", month, year),
            ("report_general_ledger", "totals_below", f, t),
            ("report_general_ledger", "search_bar", f, t),
            (
                "report_ledger_trial_balance",
                "name",
                Some("Ledger Trial Balance"),
                Some("Trial Balance"),
            ),
            ("report_ledger_trial_balance", "date_default", month, year),
            ("report_ledger_trial_balance", "unfold_all", t, f),
            ("report_balance_sheet", "date_default", month, Some("today")),
            ("report_balance_sheet", "totals_below", f, t),
            ("report_profit_loss", "date_default", month, year),
            ("report_profit_loss", "totals_below", f, t),
        ],
    )
    .await?;
    for report in OTHER_REPORTS_1_13 {
        set_all(
            tx,
            REPORT,
            &[
                (report, "date_default", month, year),
                (report, "totals_below", f, t),
            ],
        )
        .await?;
    }
    set_all(
        tx,
        COLUMN,
        &[
            (
                "partner_ledger_column_debit",
                "sequence",
                Some("20"),
                Some("60"),
            ),
            (
                "partner_ledger_column_debit",
                "name",
                Some("Period debit"),
                Some("Debit"),
            ),
            ("partner_ledger_column_debit", "blank_if_zero", f, t),
            (
                "partner_ledger_column_credit",
                "sequence",
                Some("30"),
                Some("70"),
            ),
            (
                "partner_ledger_column_credit",
                "name",
                Some("Period credit"),
                Some("Credit"),
            ),
            ("partner_ledger_column_credit", "blank_if_zero", f, t),
            ("gl_column_date", "name", Some("Posting date"), Some("Date")),
            (
                "gl_column_entry",
                "name",
                Some("Journal entry"),
                Some("Journal Entry"),
            ),
            ("gl_column_debit", "blank_if_zero", f, t),
            ("gl_column_credit", "blank_if_zero", f, t),
            (
                "trial_balance_column_opening",
                "name",
                Some("Opening (raw ledger)"),
                Some("Initial Balance"),
            ),
            (
                "trial_balance_column_debit",
                "name",
                Some("Period debit"),
                Some("Debit"),
            ),
            ("trial_balance_column_debit", "blank_if_zero", f, t),
            (
                "trial_balance_column_credit",
                "name",
                Some("Period credit"),
                Some("Credit"),
            ),
            ("trial_balance_column_credit", "blank_if_zero", f, t),
            (
                "trial_balance_column_closing",
                "name",
                Some("Closing (raw ledger)"),
                Some("End Balance"),
            ),
            (
                "profit_loss_amount",
                "name",
                Some("Amount"),
                Some("Balance"),
            ),
        ],
    )
    .await?;
    set_all(
        tx,
        LINE,
        &[
            (
                "profit_loss_result",
                "name",
                Some("Net profit"),
                Some("Net Profit"),
            ),
            ("position_equity", "sequence", Some("50"), Some("30")),
            (
                "position_equity",
                "name",
                Some("Total equity"),
                Some("Equity"),
            ),
            (
                "position_equity",
                "foldable",
                Some("foldable"),
                Some("always_unfolded"),
            ),
            ("position_earnings", "sequence", Some("40"), Some("31")),
            (
                "position_earnings",
                "groupby",
                Some("account_id,move_id"),
                Some("account_id"),
            ),
            (
                "position_earnings",
                "name",
                Some("Unallocated earnings"),
                Some("Unallocated Earnings"),
            ),
            ("position_capital", "sequence", Some("30"), Some("32")),
            (
                "position_capital",
                "groupby",
                Some("account_id,move_id"),
                Some("account_id"),
            ),
            (
                "position_capital",
                "name",
                Some("Posted equity"),
                Some("Retained Earnings & Capital"),
            ),
            ("position_financing", "sequence", Some("60"), Some("40")),
            (
                "position_financing",
                "name",
                Some("Liabilities and equity"),
                Some("Liabilities and Equity"),
            ),
        ],
    )
    .await?;
    equity_from_sub_lines(tx).await
}

/// Equity becomes the sum of its sub-lines Unallocated Earnings and Retained Earnings & Capital;
/// any other state would leave the sheet unbalanced, so the upgrade stops on it.
async fn equity_from_sub_lines(tx: &mut Transaction) -> Result<()> {
    let Some(equity) = builtin(tx, LINE, "position_equity").await? else {
        return Ok(());
    };
    let refuse = |what: String| {
        Error::validation(format!(
            "account_reporting upgrade: {what} This version computes the Balance Sheet line Equity \
             (account_reporting.position_equity) as the sum of its sub-lines \
             account_reporting.position_earnings and account_reporting.position_capital."
        ))
    };
    let rows = tx
        .query(
            "SELECT groupby FROM reporting_report_line WHERE id = $1",
            &[&equity],
        )
        .await?;
    if let Some(Some(groupby)) = rows
        .first()
        .map(|row| row.try_get::<_, Option<String>>(0).map_err(db))
        .transpose()?
        && !groupby.is_empty()
    {
        return Err(refuse(format!(
            "The line Equity groups by '{groupby}'; clear its Group By, then upgrade again."
        )));
    }
    for child in ["position_earnings", "position_capital"] {
        let Some(id) = builtin(tx, LINE, child).await? else {
            continue;
        };
        tx.execute(
            "UPDATE reporting_report_line SET parent_id = $1 WHERE id = $2 AND parent_id IS NULL",
            &[&equity, &id],
        )
        .await?;
        let rows = tx
            .query(
                "SELECT parent_id::int8 FROM reporting_report_line WHERE id = $1",
                &[&id],
            )
            .await?;
        let parent: Option<i64> = rows
            .first()
            .map(|row| row.try_get(0).map_err(db))
            .transpose()?
            .flatten();
        if parent != Some(equity) {
            return Err(refuse(format!(
                "The line account_reporting.{child} has another parent line; clear its Parent, then \
                 upgrade again."
            )));
        }
    }
    let changed = set_when(
        tx,
        EXPR,
        "position_equity_display",
        &[
            ("engine", Some("formula")),
            ("formula", Some("CAPITAL.amount + EARNINGS.amount")),
        ],
        "formula",
        Some("sum_children"),
    )
    .await?;
    if changed == 1 {
        return Ok(());
    }
    let rows = tx
        .query(
            "SELECT e.engine::text, COALESCE(e.formula, '') FROM reporting_report_expression e \
             JOIN ir_model_data d ON d.res_id = e.id AND d.module = 'account_reporting' \
              AND d.model = 'reporting.report.expression' AND d.name = 'position_equity_display'",
            &[],
        )
        .await?;
    let found: Option<(String, String)> = rows
        .first()
        .map(|row| Ok::<_, Error>((row.try_get(0).map_err(db)?, row.try_get(1).map_err(db)?)))
        .transpose()?;
    match found {
        Some((engine, formula)) if engine == "formula" && formula == "sum_children" => Ok(()),
        Some((engine, formula)) => Err(refuse(format!(
            "Its amount expression account_reporting.position_equity_display is the {engine} \
             '{formula}', not the shipped formula 'CAPITAL.amount + EARNINGS.amount' ({changed} \
             rows changed, 1 expected). Reset that expression to the shipped formula, then upgrade \
             again."
        ))),
        None => Err(refuse(format!(
            "Its amount expression account_reporting.position_equity_display does not exist \
             ({changed} rows changed, 1 expected). Restore it with the shipped formula \
             'CAPITAL.amount + EARNINGS.amount', then upgrade again."
        ))),
    }
}

/// A 1.11 Invoice Analysis line: four document-currency sums of `account.move`.
macro_rules! invoice_container {
    ($line:literal, $u:literal, $t:literal, $tot:literal, $due:literal, $domain:literal) => {
        Container {
            line: $line,
            groupby: "partner_id,journal_id",
            fold: None,
            old: &[
                aggregate(
                    $u,
                    "untaxed",
                    "account.move",
                    "amount_untaxed",
                    "strict_range",
                    $domain,
                ),
                aggregate(
                    $t,
                    "tax",
                    "account.move",
                    "amount_tax",
                    "strict_range",
                    $domain,
                ),
                aggregate(
                    $tot,
                    "total",
                    "account.move",
                    "amount_total",
                    "strict_range",
                    $domain,
                ),
                aggregate(
                    $due,
                    "due",
                    "account.move",
                    "amount_residual",
                    "strict_range",
                    $domain,
                ),
            ],
            new: &[("untaxed", $u), ("tax", $t), ("total", $tot)],
        }
    };
}

const INVOICE_CONTAINERS: [Container; 3] = [
    invoice_container!(
        "inv_an_out_inv",
        "inv_an_out_inv_u",
        "inv_an_out_inv_t",
        "inv_an_out_inv_tot",
        "inv_an_out_inv_due",
        r#"[["state","=","posted"],["move_type","=","out_invoice"]]"#
    ),
    invoice_container!(
        "inv_an_out_ref",
        "inv_an_out_ref_u",
        "inv_an_out_ref_t",
        "inv_an_out_ref_tot",
        "inv_an_out_ref_due",
        r#"[["state","=","posted"],["move_type","=","out_refund"]]"#
    ),
    invoice_container!(
        "inv_an_in_inv",
        "inv_an_in_inv_u",
        "inv_an_in_inv_t",
        "inv_an_in_inv_tot",
        "inv_an_in_inv_due",
        r#"[["state","=","posted"],["move_type","=","in_invoice"]]"#
    ),
];

/// Invoice Analysis moves from document-currency invoice totals to company-currency journal
/// items (sub-lines added by the data files); its Amount Due column goes, since no stored
/// company-currency residual exists.
#[erp_sdk::migration(addon = "account_reporting", version = "1.14", stage = pre)]
async fn invoice_analysis_company_currency_pre(
    tx: &mut Transaction,
    _from: &SemverVersion,
) -> Result<()> {
    drop_dangling(tx).await?;
    for container in &INVOICE_CONTAINERS {
        container.convert(tx).await?;
    }
    let when = [
        ("sequence", Some("40")),
        ("name", Some("Amount Due")),
        ("label", Some("due")),
        ("kind", Some("monetary")),
        ("blank_if_zero", Some("false")),
        ("sortable", Some("false")),
    ];
    if let Some(id) = shipped(tx, COLUMN, "inv_an_col_due", &when).await? {
        delete(tx, COLUMN, &[id]).await?;
    }
    drop_dangling(tx).await
}

#[erp_sdk::migration(addon = "account_reporting", version = "1.14", stage = post)]
async fn tax_report_and_aged_flags_post(tx: &mut Transaction, _from: &SemverVersion) -> Result<()> {
    // The tax report leaves out management-ledger entries, as the ledgers do.
    for (uses, names) in [
        (
            "sale",
            [
                "tax_report_sales_debit",
                "tax_report_sales_credit",
                "tax_report_sales_booked",
            ],
        ),
        (
            "purchase",
            [
                "tax_report_purchase_debit",
                "tax_report_purchase_credit",
                "tax_report_purchase_tax",
            ],
        ),
    ] {
        let old = format!(
            r#"[["parent_state","=","posted"],["tax_line_id","!=",false],["tax_line_id.type_tax_use","=","{uses}"]]"#
        );
        let new = format!(
            r#"[["parent_state","=","posted"],["journal_id.journal_group_id","=",false],["tax_line_id","!=",false],["tax_line_id.type_tax_use","=","{uses}"]]"#
        );
        for name in names {
            set_if(tx, EXPR, name, "domain", Some(&old), Some(&new)).await?;
        }
    }
    // Net Tax Payable takes the group member taxes of the new Other Taxes line too.
    set_if(
        tx,
        EXPR,
        "tax_report_net_tax",
        "formula",
        Some("SALES_TAX.tax - PURCHASE_TAX.tax"),
        Some("SALES_TAX.tax - PURCHASE_TAX.tax + OTHER_TAX.tax"),
    )
    .await?;
    // Aged balances are computed as of today only.
    for report in ["report_aged_receivable", "report_aged_payable"] {
        set_when(
            tx,
            REPORT,
            report,
            &[
                ("current_only", Some("false")),
                ("date_mode", Some("single")),
                ("date_default", Some("today")),
                ("comparison", Some("false")),
            ],
            "current_only",
            Some("true"),
        )
        .await?;
    }
    check_definitions(tx).await
}

/// Every report of this addon loads as the report screen loads it; a kept administrator change
/// that no longer fits stops the upgrade here, named, instead of failing on the screen.
async fn check_definitions(tx: &mut Transaction) -> Result<()> {
    let env = tx.env().sudo(true);
    let bounds = erp_addon_reporting::settings::read(&env, tx).await?;
    let rows = tx
        .query(
            "SELECT d.name::text, d.res_id::int8 FROM ir_model_data d \
             WHERE d.module = 'account_reporting' AND d.model = 'reporting.report' ORDER BY d.name",
            &[],
        )
        .await?;
    let mut reports = Vec::new();
    for row in &rows {
        reports.push((
            row.try_get::<_, String>(0).map_err(db)?,
            row.try_get::<_, i64>(1).map_err(db)?,
        ));
    }
    for (name, id) in reports {
        if let Err(error) = erp_addon_reporting::definition::load(&env, tx, id, true, &bounds).await
        {
            return Err(Error::validation(format!(
                "account_reporting upgrade: report account_reporting.{name} is not valid with the \
                 new layout: {} Reset the record the message names to its shipped value, then \
                 upgrade again.",
                error.message()
            )));
        }
    }
    Ok(())
}
