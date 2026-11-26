// SPDX-License-Identifier: MIT OR Apache-2.0
//! Dynamic Consolidation Reports (`account_reporting.consolidated_bs`,
//! `account_reporting.consolidated_pl`, `account_reporting.ic_reconciliation`):
//! pure caller-scoped ORM reads over the latest executed/locked `account.consolidation.run`,
//! breaking down every Group FS Item into Translated Aggregate, IC Eliminations, Adjustments & NCI,
//! and Consolidated Total, plus an Intercompany Reconciliation Matrix flagging unmatched pairs.

use std::collections::{BTreeMap, BTreeSet};

use erp_addon_reporting::{
    definition::ReportDef,
    line::{
        Cell as ReportCell, CellKind, DrillTarget, ExpandPage, Expansion, Line, LineId, LineKind,
        Segment, SegmentKind,
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

fn day(date: Date) -> String {
    let (y, m, d) = date.ymd();
    format!("{y:04}-{m:02}-{d:02}")
}

fn cond(value: Json) -> Result<Domain> {
    Ok(Domain::from_json(&value)?)
}

fn gnum(value: Option<&GroupValue>) -> Result<Decimal> {
    match value {
        None | Some(GroupValue::Null | GroupValue::False) => Ok(Decimal::ZERO),
        Some(GroupValue::Decimal(d)) => Ok(*d),
        Some(GroupValue::Int(n)) => Ok(Decimal::from(*n)),
        Some(other) => Err(Error::internal(format!(
            "Consolidation Report aggregate is not exact decimal: {other:?}"
        ))),
    }
}

fn gid(value: Option<&GroupValue>) -> Option<i64> {
    match value {
        Some(GroupValue::Int(id)) if *id > 0 => Some(*id),
        Some(other) => other.record_id().map(|id| id.get()),
        None => None,
    }
}

fn gtext(value: Option<&GroupValue>) -> Result<String> {
    match value {
        Some(GroupValue::Str(s)) => Ok(s.to_string()),
        Some(GroupValue::False | GroupValue::Null) | None => Ok(String::new()),
        Some(other) => Err(Error::internal(format!(
            "Consolidation Report group key is not text: {other:?}"
        ))),
    }
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

async fn latest_consolidation_run(
    cx: &mut ReportCx<'_>,
    options: &Options,
) -> Result<Option<ActiveRun>> {
    // Scope to consolidation groups whose member units overlap the caller's selected companies
    // and where every unit of the group is visible to the caller.
    let unit_rec = cx.env().records("account.consolidation.unit")?;
    let mut matching_groups = BTreeSet::<i64>::new();
    if !options.companies.is_empty() {
        let urows = unit_rec
            .search_with(
                cx.tx(),
                &cond(json!([["company_id", "in", options.companies]]))?,
                &SearchOptions::new().order("group_id, id"),
            )
            .await?
            .read_rows(cx.tx(), &["group_id"], Load::Raw)
            .await?;
        for ur in urows.iter().map(Value::Row) {
            matching_groups.insert(int_field(&ur, "group_id")?);
        }
        if matching_groups.is_empty() {
            return Ok(None);
        }
    }

    let mut domain_terms = vec![
        json!(["state", "in", ["executed", "locked"]]),
        json!(["date_to", "<=", day(options.period.to)]),
    ];
    if !matching_groups.is_empty() {
        let gvec: Vec<i64> = matching_groups.into_iter().collect();
        domain_terms.push(json!(["group_id", "in", gvec]));
    }

    let run_rec = cx.env().records("account.consolidation.run")?;
    let found = run_rec
        .search_with(
            cx.tx(),
            &cond(Json::Array(domain_terms))?,
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
    fn is_zero(&self) -> bool {
        self.translated.is_zero() && self.elimination.is_zero() && self.adj_nci.is_zero()
    }
    fn add_signed(&mut self, level: &str, signed_amount: Decimal, sign: Decimal) -> Result<()> {
        let v = signed_amount * sign;
        match level {
            "00_translated" => self.translated += v,
            "20_ic_elimination" => self.elimination += v,
            "10_manual_adjustment" | "30_nci" => self.adj_nci += v,
            other => {
                return Err(Error::internal(format!(
                    "Unsupported consolidation posting level '{other}'."
                )));
            }
        }
        Ok(())
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

async fn load_fs_and_totals(
    cx: &mut ReportCx<'_>,
    run: &ActiveRun,
) -> Result<(
    BTreeMap<i64, (String, String, String, i32)>,
    BTreeMap<i64, LevelCols>,
    LevelCols,
)> {
    let limit = cx.bounds().max_screen_lines;
    let fs_rec = cx.env().records("account.consolidation.account")?;
    let fs_found = fs_rec
        .search_with(
            cx.tx(),
            &cond(json!([["group_id", "=", run.group_id]]))?,
            &SearchOptions::new()
                .order("sequence, code, id")
                .limit(limit as u64 + 1),
        )
        .await?;
    if fs_found.len() > limit {
        return Err(invalid(format!(
            "Group FS Chart exceeds reporting.max_screen_lines ({limit})."
        )));
    }
    let fs_rows = fs_found
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
        let seq = i32::try_from(int_field(&r, "sequence")?)
            .map_err(|_| Error::internal("Group FS Item sequence out of range."))?;
        fs_meta.insert(id, (code, name, section, seq));
    }

    // Aggregate consolidation lines via ORM _read_group so large runs remain bounded in memory.
    let line_rec = cx.env().records("account.consolidation.line")?;
    let grouped = line_rec
        ._read_group(
            cx.tx(),
            ReadGroup::new(cond(json!([["run_id", "=", run.id]]))?)
                .groupby(["consolidation_account_id", "posting_level"])
                .aggregates(["group_amount:sum"]),
        )
        .await?;

    let mut totals = BTreeMap::<i64, LevelCols>::new();
    let mut current_earnings = LevelCols::default();
    for row in grouped.rows() {
        let fs_id = gid(row.first())
            .ok_or_else(|| Error::internal("Consolidation line aggregate missing FS item id."))?;
        let level = gtext(row.get(1))?;
        let amt = gnum(row.get(2))?;
        let (_, _, section, _) = fs_meta.get(&fs_id).ok_or_else(|| {
            Error::internal(format!(
                "Consolidation line references unknown Group FS Item {fs_id}."
            ))
        })?;
        let sign = section_display_sign(section)?;
        totals
            .entry(fs_id)
            .or_default()
            .add_signed(&level, amt, sign)?;
        if matches!(
            section.as_str(),
            "income" | "cogs" | "expense" | "nci_profit"
        ) {
            current_earnings.add_signed(&level, amt, -Decimal::ONE)?;
        }
    }
    Ok((fs_meta, totals, current_earnings))
}

/// Display sign convention: Assets/Goodwill/COGS/Expenses/NCI Profit positive for debits (+1);
/// Liabilities/Equity/CTA/NCI Equity/Income positive for credits (-1).
fn section_display_sign(section: &str) -> Result<Decimal> {
    match section {
        "asset_current" | "asset_non_current" | "goodwill" | "cogs" | "expense" | "nci_profit" => {
            Ok(Decimal::ONE)
        }
        "liability_current"
        | "liability_non_current"
        | "equity_capital"
        | "equity_retained"
        | "equity_cta"
        | "equity_nci"
        | "income" => Ok(-Decimal::ONE),
        other => Err(Error::internal(format!(
            "Unsupported Group FS section '{other}'."
        ))),
    }
}

fn validate_4col_options(report: &ReportDef, options: &Options, name: &str) -> Result<()> {
    if options.order.is_some() {
        return Err(invalid(format!(
            "{name} rows follow the Group FS Chart order."
        )));
    }
    if let Some(key) = options.provider.keys().next() {
        return Err(invalid(format!("{name} does not accept option '{key}'.")));
    }
    if options.column_groups.len() != 1 {
        return Err(invalid(format!(
            "{name} does not support comparison periods."
        )));
    }
    for col in &report.columns {
        if col.kind != CellKind::Monetary
            || !matches!(
                col.label.as_str(),
                "translated" | "elimination" | "adj_nci" | "consolidated"
            )
        {
            return Err(invalid(format!("Unknown {name} column '{}'.", col.label)));
        }
    }
    Ok(())
}

async fn expand_fs_item(
    cx: &mut ReportCx<'_>,
    report: &ReportDef,
    options: &Options,
    line: &LineId,
    page: ExpandPage,
) -> Result<Expansion> {
    if line.report() != report.id {
        return Err(invalid("Line id does not belong to this report."));
    }
    let max_page = cx.bounds().max_screen_lines as u64;
    if page.limit == 0 || page.limit > max_page {
        return Err(invalid(format!(
            "Page limit must be between 1 and {max_page}."
        )));
    }
    let fs_id = match line.segments() {
        [_, seg]
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
    let Some(run) = latest_consolidation_run(cx, options).await? else {
        return Ok(Expansion::new(Vec::new()));
    };
    let fs_rows = cx
        .env()
        .records("account.consolidation.account")?
        .search_with(
            cx.tx(),
            &cond(json!([["id", "=", fs_id], ["group_id", "=", run.group_id]]))?,
            &SearchOptions::new().limit(1),
        )
        .await?
        .read_rows(cx.tx(), &["section"], Load::Raw)
        .await?;
    let fs_row = fs_rows
        .iter()
        .next()
        .map(Value::Row)
        .ok_or_else(|| invalid("Group FS Item does not belong to this consolidation run."))?;
    let section = text_field(&fs_row, "section")?;
    let sign = section_display_sign(&section)?;

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
        cols.add_signed(&level, amt, sign)?;
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
                let Some(run) = latest_consolidation_run(cx, options).await? else {
                    return Ok(Expansion::new(Vec::new()));
                };
                let (fs_meta, totals, current_earnings) = load_fs_and_totals(cx, &run).await?;
                let root = LineId::root(report.id);
                let mut out = Vec::new();

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
                            if options.hide_zero && cols.is_zero() {
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
                            lid,
                            label,
                            LineKind::Group,
                            level_cells(report, run.currency_id, run.scale, cols),
                        );
                        line.unfoldable = true;
                        out.push(line);
                    }
                    if sec_code == "SEC_EQUITY"
                        && (!options.hide_zero || !current_earnings.is_zero())
                    {
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
            options: &'a Options,
            line: &'a LineId,
            page: ExpandPage,
        ) -> BoxFuture<'a, Result<Expansion>> {
            Box::pin(async move { expand_fs_item(cx, report, options, line, page).await })
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
                let Some(run) = latest_consolidation_run(cx, options).await? else {
                    return Ok(Expansion::new(Vec::new()));
                };
                let (fs_meta, totals, _) = load_fs_and_totals(cx, &run).await?;
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
                            if options.hide_zero && cols.is_zero() {
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
                            lid,
                            label,
                            LineKind::Group,
                            level_cells(report, run.currency_id, run.scale, cols),
                        );
                        line.unfoldable = true;
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
            options: &'a Options,
            line: &'a LineId,
            page: ExpandPage,
        ) -> BoxFuture<'a, Result<Expansion>> {
            Box::pin(async move { expand_fs_item(cx, report, options, line, page).await })
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
                if options.order.is_some() {
                    return Err(invalid(
                        "IC Reconciliation Matrix rows are ordered by pair type and entity.",
                    ));
                }
                if options.unfold_all
                    || !options.unfolded.is_empty()
                    || !options.loaded_pages.is_empty()
                {
                    return Err(invalid("IC Reconciliation Matrix lines do not unfold."));
                }
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
                    let ok = match col.label.as_str() {
                        "pair_kind" => col.kind == CellKind::Text,
                        "entity_a" | "entity_b" | "variance" => col.kind == CellKind::Monetary,
                        _ => false,
                    };
                    if !ok {
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
            options: &'a Options,
        ) -> BoxFuture<'a, Result<Expansion>> {
            Box::pin(async move {
                let Some(run) = latest_consolidation_run(cx, options).await? else {
                    return Ok(Expansion::new(Vec::new()));
                };
                let limit = cx.bounds().max_screen_lines;
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
                let grouped = line_rec
                    ._read_group(
                        cx.tx(),
                        ReadGroup::new(cond(json!([
                            ["run_id", "=", run.id],
                            ["posting_level", "=", "00_translated"],
                            ["trading_partner_company_id", "!=", false]
                        ]))?)
                        .groupby([
                            "consolidation_account_id",
                            "company_id",
                            "trading_partner_company_id",
                        ])
                        .aggregates(["group_amount:sum"]),
                    )
                    .await?;

                // Pair (key_prefix, display_kind, a, b) -> (a_mag, b_mag)
                let mut pairs =
                    BTreeMap::<(&'static str, &'static str, i64, i64), (Decimal, Decimal)>::new();
                for r in grouped.rows() {
                    let Some(fs_id) = gid(r.first()) else {
                        continue;
                    };
                    let Some(co) = gid(r.get(1)) else {
                        continue;
                    };
                    let Some(tp) = gid(r.get(2)) else {
                        continue;
                    };
                    let amt = gnum(r.get(3))?;
                    let role = role_by_fs.get(&fs_id).map(|s| s.as_str()).unwrap_or("none");
                    match role {
                        "ic_receivable" => {
                            pairs.entry(("AR_AP", "AR / AP", co, tp)).or_default().0 += amt;
                        }
                        "ic_payable" => {
                            pairs.entry(("AR_AP", "AR / AP", tp, co)).or_default().1 += -amt;
                        }
                        "ic_revenue" => {
                            pairs
                                .entry(("REV_COST", "Revenue / Cost", co, tp))
                                .or_default()
                                .0 += -amt;
                        }
                        "ic_expense" => {
                            pairs
                                .entry(("REV_COST", "Revenue / Cost", tp, co))
                                .or_default()
                                .1 += amt;
                        }
                        _ => {}
                    }
                }
                if pairs.len() > limit {
                    return Err(invalid(format!(
                        "IC Reconciliation Matrix exceeds reporting.max_screen_lines ({limit})."
                    )));
                }

                let root = LineId::root(report.id);
                let mut out = Vec::new();
                let mut tot_a = Decimal::ZERO;
                let mut tot_b = Decimal::ZERO;
                let mut tot_var = Decimal::ZERO;

                for ((key_prefix, kind, a, b), (mag_a, mag_b)) in pairs {
                    let variance = (mag_a - mag_b).round_dp(run.scale);
                    if options.hide_zero && mag_a.is_zero() && mag_b.is_zero() && variance.is_zero()
                    {
                        continue;
                    }
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
                    let key = format!("{key_prefix}_{a}_{b}");
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
            Box::pin(async move {
                Err(invalid(
                    "Intercompany Reconciliation Matrix lines do not unfold.",
                ))
            })
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
