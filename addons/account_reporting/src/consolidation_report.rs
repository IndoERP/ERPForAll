// SPDX-License-Identifier: MIT OR Apache-2.0
//! Dynamic Consolidation Reports (`account_reporting.consolidated_bs`,
//! `account_reporting.consolidated_pl`, `account_reporting.ic_reconciliation`):
//! pure caller-scoped ORM reads over the latest executed/locked `account.consolidation.run`,
//! breaking down every Group FS Item into Translated Aggregate, IC Eliminations, Adjustments & NCI,
//! and Consolidated Total, plus an Intercompany Reconciliation Matrix flagging unmatched pairs.

use std::collections::BTreeMap;

use erp_addon_reporting::{
    definition::ReportDef,
    line::{
        Cell as ReportCell, DrillTarget, ExpandPage, Expansion, Line, LineId, LineKind, Segment,
        SegmentKind,
    },
    options::Options,
    provider::{ProviderSpec, ReportCx, ReportProvider},
};
use erp_sdk::prelude::*;
use erp_sdk::serde_json::{Value as Json, json};
use erp_sdk::{BoxFuture, Load};

fn invalid(msg: impl Into<String>) -> Error {
    Error::validation(msg.into())
}

fn cond(value: Json) -> Result<Domain> {
    Ok(Domain::from_json(&value)?)
}

fn get(row: &Value, field: &str) -> Result<Value> {
    row.dict()
        .ok_or_else(|| Error::internal("Consolidation Report row is not a batch handle."))?
        .get(field)
}

fn int_field(row: &Value, field: &str) -> Result<i64> {
    match get(row, field)? {
        Value::Int(n) => Ok(n),
        other => Err(Error::internal(format!(
            "Consolidation Report {field} is not an integer: {other:?}"
        ))),
    }
}

fn opt_int_field(row: &Value, field: &str) -> Result<Option<i64>> {
    match get(row, field)? {
        Value::Int(n) if n > 0 => Ok(Some(n)),
        Value::False | Value::Int(_) => Ok(None),
        other => Err(Error::internal(format!(
            "Consolidation Report {field} is not an optional id: {other:?}"
        ))),
    }
}

fn dec_field(row: &Value, field: &str) -> Result<Decimal> {
    match get(row, field)? {
        Value::Decimal(d) => Ok(d),
        Value::Int(n) => Ok(Decimal::from(n)),
        Value::False => Ok(Decimal::ZERO),
        other => Err(Error::internal(format!(
            "Consolidation Report {field} is not exact decimal: {other:?}"
        ))),
    }
}

fn text_field(row: &Value, field: &str) -> Result<String> {
    match get(row, field)? {
        Value::Str(s) => Ok(s.to_string()),
        Value::False => Ok(String::new()),
        other => Err(Error::internal(format!(
            "Consolidation Report {field} is not text: {other:?}"
        ))),
    }
}

struct ActiveRun {
    id: i64,
    group_id: i64,
    currency_id: i64,
    scale: u32,
}

async fn latest_consolidation_run(cx: &mut ReportCx<'_>) -> Result<Option<ActiveRun>> {
    let run_rec = cx.env().records("account.consolidation.run")?;
    let found = run_rec
        .search_with(
            cx.tx(),
            &cond(json!([["state", "in", ["executed", "locked"]]]))?,
            &SearchOptions::new().order("date_to desc, id desc").limit(1),
        )
        .await?;
    if found.is_empty() {
        return Ok(None);
    }
    let rows = found
        .read_rows(cx.tx(), &["id", "group_id", "currency_id"], Load::Raw)
        .await?;
    let Some(row) = rows.iter().next().map(Value::Row) else {
        return Ok(None);
    };
    let run_id = int_field(&row, "id")?;
    let group_id = int_field(&row, "group_id")?;
    let currency_id = int_field(&row, "currency_id")?;
    let cur_rows = cx
        .env()
        .records("res.currency")?
        .browse([currency_id])?
        .read_rows(cx.tx(), &["decimal_places"], Load::Raw)
        .await?;
    let cur = cur_rows
        .iter()
        .next()
        .map(Value::Row)
        .ok_or_else(|| invalid("Group currency is not visible."))?;
    let scale = u32::try_from(int_field(&cur, "decimal_places")?)
        .map_err(|_| invalid("Invalid currency precision."))?;
    Ok(Some(ActiveRun {
        id: run_id,
        group_id,
        currency_id,
        scale,
    }))
}

#[derive(Default, Clone, Copy)]
struct LevelCols {
    translated: Decimal,
    elimination: Decimal,
    adj_nci: Decimal,
}

impl LevelCols {
    fn consolidated(&self) -> Decimal {
        self.translated + self.elimination + self.adj_nci
    }
    fn add_signed(&mut self, level: &str, signed_amount: Decimal, sign: Decimal) {
        let v = signed_amount * sign;
        match level {
            "00_translated" => self.translated += v,
            "20_ic_elimination" => self.elimination += v,
            _ => self.adj_nci += v,
        }
    }
    fn add(&mut self, other: LevelCols) {
        self.translated += other.translated;
        self.elimination += other.elimination;
        self.adj_nci += other.adj_nci;
    }
}

fn level_cells(report: &ReportDef, cur: i64, scale: u32, cols: LevelCols) -> Vec<ReportCell> {
    let mut out = Vec::with_capacity(report.columns.len());
    for col in &report.columns {
        let val = match col.label.as_str() {
            "translated" => Some(cols.translated),
            "elimination" => Some(cols.elimination),
            "adj_nci" => Some(cols.adj_nci),
            "consolidated" => Some(cols.consolidated()),
            _ => None,
        };
        let cell = ReportCell::monetary(val, scale, Some(cur));
        out.push(if col.blank_if_zero {
            cell.blank_if_zero()
        } else {
            cell
        });
    }
    out
}

fn empty_cells(report: &ReportDef) -> Vec<ReportCell> {
    vec![ReportCell::empty(); report.columns.len()]
}

async fn load_fs_and_lines(
    cx: &mut ReportCx<'_>,
    run: &ActiveRun,
) -> Result<(
    BTreeMap<i64, (String, String, String, i32)>,
    BTreeMap<i64, LevelCols>,
    Vec<(i64, String, String, Decimal)>,
)> {
    let fs_rec = cx.env().records("account.consolidation.account")?;
    let fs_rows = fs_rec
        .search_with(
            cx.tx(),
            &cond(json!([["group_id", "=", run.group_id]]))?,
            &SearchOptions::new().order("sequence, code, id"),
        )
        .await?
        .read_rows(
            cx.tx(),
            &["id", "code", "name", "section", "sequence"],
            Load::Raw,
        )
        .await?;
    let mut fs_meta = BTreeMap::new();
    for r in fs_rows.iter().map(Value::Row) {
        let id = int_field(&r, "id")?;
        let code = text_field(&r, "code")?;
        let name = text_field(&r, "name")?;
        let section = text_field(&r, "section")?;
        let seq = int_field(&r, "sequence")? as i32;
        fs_meta.insert(id, (code, name, section, seq));
    }

    let line_rec = cx.env().records("account.consolidation.line")?;
    let line_rows = line_rec
        .search_with(
            cx.tx(),
            &cond(json!([["run_id", "=", run.id]]))?,
            &SearchOptions::new().order("id"),
        )
        .await?
        .read_rows(
            cx.tx(),
            &[
                "consolidation_account_id",
                "posting_level",
                "label",
                "group_amount",
            ],
            Load::Raw,
        )
        .await?;

    let mut totals = BTreeMap::<i64, LevelCols>::new();
    let mut details = Vec::new();
    for r in line_rows.iter().map(Value::Row) {
        let fs_id = int_field(&r, "consolidation_account_id")?;
        let level = text_field(&r, "posting_level")?;
        let label = text_field(&r, "label")?;
        let amt = dec_field(&r, "group_amount")?;
        let section = fs_meta
            .get(&fs_id)
            .map(|(_, _, s, _)| s.as_str())
            .unwrap_or("");
        let sign = section_display_sign(section);
        totals
            .entry(fs_id)
            .or_default()
            .add_signed(&level, amt, sign);
        details.push((fs_id, level, label, amt));
    }
    Ok((fs_meta, totals, details))
}

/// Display sign convention: Assets/Goodwill/COGS/Expenses/NCI Profit positive for debits (+1);
/// Liabilities/Equity/CTA/NCI Equity/Income positive for credits (-1).
fn section_display_sign(section: &str) -> Decimal {
    match section {
        "asset_current" | "asset_non_current" | "goodwill" | "cogs" | "expense" | "nci_profit" => {
            Decimal::ONE
        }
        _ => -Decimal::ONE,
    }
}

fn validate_4col_options(report: &ReportDef, options: &Options, name: &str) -> Result<()> {
    if let Some(key) = options.provider.keys().next() {
        return Err(invalid(format!("{name} does not accept option '{key}'.")));
    }
    if options.column_groups.len() != 1 {
        return Err(invalid(format!(
            "{name} does not support comparison periods."
        )));
    }
    for col in &report.columns {
        if !matches!(
            col.label.as_str(),
            "translated" | "elimination" | "adj_nci" | "consolidated"
        ) {
            return Err(invalid(format!("Unknown {name} column '{}'.", col.label)));
        }
    }
    Ok(())
}

async fn expand_fs_item(
    cx: &mut ReportCx<'_>,
    report: &ReportDef,
    line: &LineId,
    page: ExpandPage,
) -> Result<Expansion> {
    let Some(run) = latest_consolidation_run(cx).await? else {
        return Ok(Expansion::new(Vec::new()));
    };
    let fs_id = match line.segments() {
        [seg]
            if seg.kind() == &SegmentKind::Group("fs_id".to_owned())
                && seg.model() == "account.consolidation.account" =>
        {
            seg.value()
                .parse::<i64>()
                .ok()
                .filter(|v| *v > 0)
                .ok_or_else(|| invalid("Invalid Group FS Item line id."))?
        }
        _ => return Err(invalid("Only Group FS Item rows can be unfolded.")),
    };
    let fs_rows = cx
        .env()
        .records("account.consolidation.account")?
        .browse([fs_id])?
        .read_rows(cx.tx(), &["section"], Load::Raw)
        .await?;
    let section = fs_rows
        .iter()
        .next()
        .map(Value::Row)
        .map(|r| text_field(&r, "section"))
        .transpose()?
        .unwrap_or_default();
    let sign = section_display_sign(&section);

    let domain = cond(json!([
        ["run_id", "=", run.id],
        ["consolidation_account_id", "=", fs_id]
    ]))?;
    let rec = cx.env().records("account.consolidation.line")?;
    let total_count = rec.search_count(cx.tx(), &domain, None).await?;
    let rows = rec
        .search_with(
            cx.tx(),
            &domain,
            &SearchOptions::new()
                .order("posting_level, id")
                .offset(page.offset)
                .limit(page.limit),
        )
        .await?
        .read_rows(
            cx.tx(),
            &["id", "posting_level", "label", "group_amount"],
            Load::Raw,
        )
        .await?;
    let mut out = Vec::new();
    for r in rows.iter().map(Value::Row) {
        let lid = int_field(&r, "id")?;
        let level = text_field(&r, "posting_level")?;
        let label = text_field(&r, "label")?;
        let amt = dec_field(&r, "group_amount")?;
        let mut cols = LevelCols::default();
        cols.add_signed(&level, amt, sign);
        out.push(Line::new(
            line.child(Segment::group(
                "line_id",
                "account.consolidation.line",
                &lid.to_string(),
            )),
            label,
            LineKind::Detail,
            level_cells(report, run.currency_id, run.scale, cols),
        ));
    }
    let end = page.offset + out.len() as u64;
    if end < total_count {
        let mut more = Line::new(
            line.child(Segment::more()),
            "",
            LineKind::More,
            empty_cells(report),
        );
        more.remaining = Some(total_count - end);
        more.next_offset = Some(end);
        out.push(more);
    }
    Ok(Expansion::new(out))
}

mod bs {
    use super::*;

    pub(crate) struct ConsolidatedBalanceSheet;
    const SPEC: ProviderSpec = ProviderSpec::new("account_reporting.consolidated_bs", 1);

    impl ReportProvider for ConsolidatedBalanceSheet {
        fn spec(&self) -> &ProviderSpec {
            &SPEC
        }
        fn init_options<'a>(
            &'a self,
            _cx: &'a mut ReportCx<'_>,
            report: &'a ReportDef,
            options: &'a mut Options,
            _strict: bool,
        ) -> BoxFuture<'a, Result<()>> {
            Box::pin(
                async move { validate_4col_options(report, options, "Consolidated Balance Sheet") },
            )
        }
        fn lines<'a>(
            &'a self,
            cx: &'a mut ReportCx<'_>,
            report: &'a ReportDef,
            options: &'a Options,
        ) -> BoxFuture<'a, Result<Expansion>> {
            Box::pin(async move {
                let Some(run) = latest_consolidation_run(cx).await? else {
                    return Ok(Expansion::new(Vec::new()));
                };
                let (fs_meta, totals, details) = load_fs_and_lines(cx, &run).await?;
                let root = LineId::root(report.id);
                let mut out = Vec::new();

                // Compute Current-Year Earnings attributable to Parent (income - cogs - expense - nci_profit)
                let mut current_earnings = LevelCols::default();
                for (fs_id, level, _, amt) in &details {
                    if let Some((_, _, sec, _)) = fs_meta.get(fs_id)
                        && matches!(sec.as_str(), "income" | "cogs" | "expense" | "nci_profit")
                    {
                        current_earnings.add_signed(level, *amt, -Decimal::ONE);
                    }
                }

                let groups: &[(&str, &str, &[&str])] = &[
                    (
                        "SEC_ASSETS",
                        "Assets",
                        &["asset_current", "asset_non_current", "goodwill"],
                    ),
                    (
                        "SEC_LIAB",
                        "Liabilities",
                        &["liability_current", "liability_non_current"],
                    ),
                    (
                        "SEC_EQUITY",
                        "Equity & Reserves",
                        &[
                            "equity_capital",
                            "equity_retained",
                            "equity_cta",
                            "equity_nci",
                        ],
                    ),
                ];

                let mut total_assets = LevelCols::default();
                let mut total_liab_eq = LevelCols::default();

                for &(sec_code, sec_title, sections) in groups {
                    let mut items: Vec<(i32, String, i64, String, LevelCols)> = Vec::new();
                    let mut sec_sum = LevelCols::default();
                    for (&fs_id, (code, name, section, seq)) in &fs_meta {
                        if sections.contains(&section.as_str()) {
                            let cols = totals.get(&fs_id).copied().unwrap_or_default();
                            if cols.translated.is_zero()
                                && cols.elimination.is_zero()
                                && cols.adj_nci.is_zero()
                            {
                                continue;
                            }
                            sec_sum.add(cols);
                            items.push((*seq, code.clone(), fs_id, format!("{code} {name}"), cols));
                        }
                    }
                    if sec_code == "SEC_EQUITY" {
                        sec_sum.add(current_earnings);
                    }
                    if sec_code == "SEC_ASSETS" {
                        total_assets = sec_sum;
                    } else {
                        total_liab_eq.add(sec_sum);
                    }
                    items.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));

                    out.push(Line::new(
                        root.child(Segment::line(sec_code)),
                        sec_title,
                        LineKind::Section,
                        level_cells(report, run.currency_id, run.scale, sec_sum),
                    ));
                    for (_, _, fs_id, label, cols) in items {
                        let lid = root.child(Segment::group(
                            "fs_id",
                            "account.consolidation.account",
                            &fs_id.to_string(),
                        ));
                        let mut line = Line::new(
                            lid.clone(),
                            label,
                            LineKind::Group,
                            level_cells(report, run.currency_id, run.scale, cols),
                        );
                        line.unfoldable = true;
                        line.unfolded = options.unfold_all || options.unfolded.contains(&lid);
                        out.push(line);
                    }
                    if sec_code == "SEC_EQUITY" {
                        out.push(Line::new(
                            root.child(Segment::line("CURR_EARNINGS")),
                            "Current-Year Earnings (Attributable to Parent)",
                            LineKind::Detail,
                            level_cells(report, run.currency_id, run.scale, current_earnings),
                        ));
                    }
                }

                out.push(Line::new(
                    root.child(Segment::line("TOT_ASSETS")),
                    "Total Consolidated Assets",
                    LineKind::Total,
                    level_cells(report, run.currency_id, run.scale, total_assets),
                ));
                out.push(Line::new(
                    root.child(Segment::total()),
                    "Total Consolidated Liabilities & Equity",
                    LineKind::Total,
                    level_cells(report, run.currency_id, run.scale, total_liab_eq),
                ));
                Ok(Expansion::new(out))
            })
        }
        fn expand<'a>(
            &'a self,
            cx: &'a mut ReportCx<'_>,
            report: &'a ReportDef,
            _options: &'a Options,
            line: &'a LineId,
            page: ExpandPage,
        ) -> BoxFuture<'a, Result<Expansion>> {
            Box::pin(async move { expand_fs_item(cx, report, line, page).await })
        }
        fn drill<'a>(
            &'a self,
            _cx: &'a mut ReportCx<'_>,
            _report: &'a ReportDef,
            _options: &'a Options,
            _line: &'a LineId,
            _target: &'a DrillTarget,
        ) -> BoxFuture<'a, Result<Json>> {
            Box::pin(async move {
                Err(invalid(
                    "Unfold any Group FS Item to inspect entity and elimination lines.",
                ))
            })
        }
    }

    erp_addon_reporting::provider_declarations!(ConsolidatedBalanceSheet);
}

mod pl {
    use super::*;

    pub(crate) struct ConsolidatedProfitLoss;
    const SPEC: ProviderSpec = ProviderSpec::new("account_reporting.consolidated_pl", 1);

    impl ReportProvider for ConsolidatedProfitLoss {
        fn spec(&self) -> &ProviderSpec {
            &SPEC
        }
        fn init_options<'a>(
            &'a self,
            _cx: &'a mut ReportCx<'_>,
            report: &'a ReportDef,
            options: &'a mut Options,
            _strict: bool,
        ) -> BoxFuture<'a, Result<()>> {
            Box::pin(async move {
                validate_4col_options(report, options, "Consolidated Profit and Loss")
            })
        }
        fn lines<'a>(
            &'a self,
            cx: &'a mut ReportCx<'_>,
            report: &'a ReportDef,
            options: &'a Options,
        ) -> BoxFuture<'a, Result<Expansion>> {
            Box::pin(async move {
                let Some(run) = latest_consolidation_run(cx).await? else {
                    return Ok(Expansion::new(Vec::new()));
                };
                let (fs_meta, totals, _) = load_fs_and_lines(cx, &run).await?;
                let root = LineId::root(report.id);
                let mut out = Vec::new();

                let mut rev_tot = LevelCols::default();
                let mut cogs_tot = LevelCols::default();
                let mut exp_tot = LevelCols::default();
                let mut nci_tot = LevelCols::default();

                let groups: &[(&str, &str, &[&str])] = &[
                    ("SEC_REV", "Operating & Other Revenue", &["income"]),
                    ("SEC_COGS", "Cost of Revenue", &["cogs"]),
                    ("SEC_EXP", "Operating Expenses", &["expense"]),
                ];

                for &(sec_code, sec_title, sections) in groups {
                    let mut items: Vec<(i32, String, i64, String, LevelCols)> = Vec::new();
                    let mut sec_sum = LevelCols::default();
                    for (&fs_id, (code, name, section, seq)) in &fs_meta {
                        if sections.contains(&section.as_str()) {
                            let cols = totals.get(&fs_id).copied().unwrap_or_default();
                            if cols.translated.is_zero()
                                && cols.elimination.is_zero()
                                && cols.adj_nci.is_zero()
                            {
                                continue;
                            }
                            sec_sum.add(cols);
                            items.push((*seq, code.clone(), fs_id, format!("{code} {name}"), cols));
                        }
                    }
                    match sec_code {
                        "SEC_REV" => rev_tot = sec_sum,
                        "SEC_COGS" => cogs_tot = sec_sum,
                        _ => exp_tot = sec_sum,
                    }
                    items.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
                    out.push(Line::new(
                        root.child(Segment::line(sec_code)),
                        sec_title,
                        LineKind::Section,
                        level_cells(report, run.currency_id, run.scale, sec_sum),
                    ));
                    for (_, _, fs_id, label, cols) in items {
                        let lid = root.child(Segment::group(
                            "fs_id",
                            "account.consolidation.account",
                            &fs_id.to_string(),
                        ));
                        let mut line = Line::new(
                            lid.clone(),
                            label,
                            LineKind::Group,
                            level_cells(report, run.currency_id, run.scale, cols),
                        );
                        line.unfoldable = true;
                        line.unfolded = options.unfold_all || options.unfolded.contains(&lid);
                        out.push(line);
                    }
                }

                for (&fs_id, (_, _, section, _)) in &fs_meta {
                    if section == "nci_profit" {
                        nci_tot.add(totals.get(&fs_id).copied().unwrap_or_default());
                    }
                }

                let group_net = LevelCols {
                    translated: rev_tot.translated - cogs_tot.translated - exp_tot.translated,
                    elimination: rev_tot.elimination - cogs_tot.elimination - exp_tot.elimination,
                    adj_nci: rev_tot.adj_nci - cogs_tot.adj_nci - exp_tot.adj_nci,
                };
                let parent_net = LevelCols {
                    translated: group_net.translated - nci_tot.translated,
                    elimination: group_net.elimination - nci_tot.elimination,
                    adj_nci: group_net.adj_nci - nci_tot.adj_nci,
                };

                out.push(Line::new(
                    root.child(Segment::line("GROUP_NET")),
                    "Consolidated Net Profit (Group Total)",
                    LineKind::Section,
                    level_cells(report, run.currency_id, run.scale, group_net),
                ));
                out.push(Line::new(
                    root.child(Segment::line("NCI_PROFIT")),
                    "Attributable to Non-Controlling Interest",
                    LineKind::Detail,
                    level_cells(report, run.currency_id, run.scale, nci_tot),
                ));
                out.push(Line::new(
                    root.child(Segment::total()),
                    "Net Profit Attributable to Owners of Parent",
                    LineKind::Total,
                    level_cells(report, run.currency_id, run.scale, parent_net),
                ));
                Ok(Expansion::new(out))
            })
        }
        fn expand<'a>(
            &'a self,
            cx: &'a mut ReportCx<'_>,
            report: &'a ReportDef,
            _options: &'a Options,
            line: &'a LineId,
            page: ExpandPage,
        ) -> BoxFuture<'a, Result<Expansion>> {
            Box::pin(async move { expand_fs_item(cx, report, line, page).await })
        }
        fn drill<'a>(
            &'a self,
            _cx: &'a mut ReportCx<'_>,
            _report: &'a ReportDef,
            _options: &'a Options,
            _line: &'a LineId,
            _target: &'a DrillTarget,
        ) -> BoxFuture<'a, Result<Json>> {
            Box::pin(async move {
                Err(invalid(
                    "Unfold any Group FS Item to inspect entity and elimination lines.",
                ))
            })
        }
    }

    erp_addon_reporting::provider_declarations!(ConsolidatedProfitLoss);
}

mod ic {
    use super::*;

    pub(crate) struct IcReconciliationMatrix;
    const SPEC: ProviderSpec = ProviderSpec::new("account_reporting.ic_reconciliation", 1);

    impl ReportProvider for IcReconciliationMatrix {
        fn spec(&self) -> &ProviderSpec {
            &SPEC
        }
        fn init_options<'a>(
            &'a self,
            _cx: &'a mut ReportCx<'_>,
            report: &'a ReportDef,
            options: &'a mut Options,
            _strict: bool,
        ) -> BoxFuture<'a, Result<()>> {
            Box::pin(async move {
                if let Some(key) = options.provider.keys().next() {
                    return Err(invalid(format!(
                        "IC Reconciliation Matrix does not accept option '{key}'."
                    )));
                }
                if options.column_groups.len() != 1 {
                    return Err(invalid(
                        "IC Reconciliation Matrix does not support comparison periods.",
                    ));
                }
                for col in &report.columns {
                    if !matches!(
                        col.label.as_str(),
                        "pair_kind" | "entity_a" | "entity_b" | "variance"
                    ) {
                        return Err(invalid(format!(
                            "Unknown IC Reconciliation column '{}'.",
                            col.label
                        )));
                    }
                }
                Ok(())
            })
        }
        fn lines<'a>(
            &'a self,
            cx: &'a mut ReportCx<'_>,
            report: &'a ReportDef,
            _options: &'a Options,
        ) -> BoxFuture<'a, Result<Expansion>> {
            Box::pin(async move {
                let Some(run) = latest_consolidation_run(cx).await? else {
                    return Ok(Expansion::new(Vec::new()));
                };
                // Load company names via ORM.
                let unit_rec = cx.env().records("account.consolidation.unit")?;
                let unit_rows = unit_rec
                    .search_with(
                        cx.tx(),
                        &cond(json!([["group_id", "=", run.group_id]]))?,
                        &SearchOptions::new(),
                    )
                    .await?
                    .read_rows(cx.tx(), &["company_id"], Load::Raw)
                    .await?;
                let comp_ids: Vec<i64> = unit_rows
                    .iter()
                    .map(Value::Row)
                    .map(|r| int_field(&r, "company_id"))
                    .collect::<Result<_>>()?;
                let mut comp_names = BTreeMap::<i64, String>::new();
                if !comp_ids.is_empty() {
                    let crows = cx
                        .env()
                        .records("res.company")?
                        .browse(comp_ids)?
                        .read_rows(cx.tx(), &["id", "name"], Load::Raw)
                        .await?;
                    for cr in crows.iter().map(Value::Row) {
                        comp_names.insert(int_field(&cr, "id")?, text_field(&cr, "name")?);
                    }
                }

                // Load FS item ic_roles.
                let fs_rec = cx.env().records("account.consolidation.account")?;
                let fs_rows = fs_rec
                    .search_with(
                        cx.tx(),
                        &cond(json!([["group_id", "=", run.group_id]]))?,
                        &SearchOptions::new(),
                    )
                    .await?
                    .read_rows(cx.tx(), &["id", "ic_role"], Load::Raw)
                    .await?;
                let mut role_by_fs = BTreeMap::<i64, String>::new();
                for fr in fs_rows.iter().map(Value::Row) {
                    role_by_fs.insert(int_field(&fr, "id")?, text_field(&fr, "ic_role")?);
                }

                let line_rec = cx.env().records("account.consolidation.line")?;
                let line_rows = line_rec
                    .search_with(
                        cx.tx(),
                        &cond(json!([
                            ["run_id", "=", run.id],
                            ["posting_level", "=", "00_translated"],
                            ["trading_partner_company_id", "!=", false]
                        ]))?,
                        &SearchOptions::new().order("id"),
                    )
                    .await?
                    .read_rows(
                        cx.tx(),
                        &[
                            "consolidation_account_id",
                            "company_id",
                            "trading_partner_company_id",
                            "group_amount",
                        ],
                        Load::Raw,
                    )
                    .await?;

                // Pair (kind, a, b) -> (a_mag, b_mag)
                let mut pairs = BTreeMap::<(&'static str, i64, i64), (Decimal, Decimal)>::new();
                for r in line_rows.iter().map(Value::Row) {
                    let fs_id = int_field(&r, "consolidation_account_id")?;
                    let Some(co) = opt_int_field(&r, "company_id")? else {
                        continue;
                    };
                    let Some(tp) = opt_int_field(&r, "trading_partner_company_id")? else {
                        continue;
                    };
                    let amt = dec_field(&r, "group_amount")?;
                    let role = role_by_fs.get(&fs_id).map(|s| s.as_str()).unwrap_or("none");
                    match role {
                        "ic_receivable" => {
                            pairs.entry(("AR / AP", co, tp)).or_default().0 += amt;
                        }
                        "ic_payable" => {
                            pairs.entry(("AR / AP", tp, co)).or_default().1 += -amt;
                        }
                        "ic_revenue" => {
                            pairs.entry(("Revenue / Cost", co, tp)).or_default().0 += -amt;
                        }
                        "ic_expense" => {
                            pairs.entry(("Revenue / Cost", tp, co)).or_default().1 += amt;
                        }
                        _ => {}
                    }
                }

                let root = LineId::root(report.id);
                let mut out = Vec::new();
                let mut tot_a = Decimal::ZERO;
                let mut tot_b = Decimal::ZERO;
                let mut tot_var = Decimal::ZERO;

                for ((kind, a, b), (mag_a, mag_b)) in pairs {
                    let variance = (mag_a - mag_b).round_dp(2);
                    tot_a += mag_a;
                    tot_b += mag_b;
                    tot_var += variance;
                    let name_a = comp_names
                        .get(&a)
                        .cloned()
                        .unwrap_or_else(|| format!("Co {a}"));
                    let name_b = comp_names
                        .get(&b)
                        .cloned()
                        .unwrap_or_else(|| format!("Co {b}"));
                    let label = format!("{name_a} <-> {name_b}");
                    let mut cells = Vec::with_capacity(report.columns.len());
                    for col in &report.columns {
                        let cell = match col.label.as_str() {
                            "pair_kind" => ReportCell::text(kind.to_owned()),
                            "entity_a" => {
                                ReportCell::monetary(Some(mag_a), run.scale, Some(run.currency_id))
                            }
                            "entity_b" => {
                                ReportCell::monetary(Some(mag_b), run.scale, Some(run.currency_id))
                            }
                            "variance" => {
                                let c = ReportCell::monetary(
                                    Some(variance),
                                    run.scale,
                                    Some(run.currency_id),
                                );
                                if !variance.is_zero() {
                                    c.with_danger(true)
                                } else {
                                    c
                                }
                            }
                            _ => ReportCell::empty(),
                        };
                        cells.push(cell);
                    }
                    let key = format!("{}_{}_{}", kind.replace(' ', ""), a, b);
                    out.push(Line::new(
                        root.child(Segment::line(&key)),
                        label,
                        LineKind::Detail,
                        cells,
                    ));
                }

                let mut tot_cells = Vec::with_capacity(report.columns.len());
                for col in &report.columns {
                    let cell = match col.label.as_str() {
                        "pair_kind" => ReportCell::empty(),
                        "entity_a" => {
                            ReportCell::monetary(Some(tot_a), run.scale, Some(run.currency_id))
                        }
                        "entity_b" => {
                            ReportCell::monetary(Some(tot_b), run.scale, Some(run.currency_id))
                        }
                        "variance" => {
                            let c = ReportCell::monetary(
                                Some(tot_var),
                                run.scale,
                                Some(run.currency_id),
                            );
                            if !tot_var.is_zero() {
                                c.with_danger(true)
                            } else {
                                c
                            }
                        }
                        _ => ReportCell::empty(),
                    };
                    tot_cells.push(cell);
                }
                out.push(Line::new(
                    root.child(Segment::total()),
                    "Total Intercompany Balances",
                    LineKind::Total,
                    tot_cells,
                ));
                Ok(Expansion::new(out))
            })
        }
        fn expand<'a>(
            &'a self,
            _cx: &'a mut ReportCx<'_>,
            _report: &'a ReportDef,
            _options: &'a Options,
            _line: &'a LineId,
            _page: ExpandPage,
        ) -> BoxFuture<'a, Result<Expansion>> {
            Box::pin(async move { Ok(Expansion::new(Vec::new())) })
        }
        fn drill<'a>(
            &'a self,
            _cx: &'a mut ReportCx<'_>,
            _report: &'a ReportDef,
            _options: &'a Options,
            _line: &'a LineId,
            _target: &'a DrillTarget,
        ) -> BoxFuture<'a, Result<Json>> {
            Box::pin(async move {
                Err(invalid(
                    "Open the Consolidation Run to inspect detailed intercompany lines.",
                ))
            })
        }
    }

    erp_addon_reporting::provider_declarations!(IcReconciliationMatrix);
}
