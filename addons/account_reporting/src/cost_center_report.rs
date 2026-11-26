// SPDX-License-Identifier: MIT OR Apache-2.0
//! Cost Center & Profit Center Performance dynamic report provider
//! (`account_reporting.cost_center_report`): pure caller-scoped ORM reads showing Primary Actual,
//! Secondary Allocations, Total Actual, Open PO Commitments, Budget Released, and Available Budget.

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

pub(crate) struct CostCenterReport;
const SPEC: ProviderSpec = ProviderSpec::new("account_reporting.cost_center_report", 1);

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

fn get(row: &Value, field: &str) -> Result<Value> {
    row.dict()
        .ok_or_else(|| Error::internal("Cost Center Report row is not a batch handle."))?
        .get(field)
}

fn int_field(row: &Value, field: &str) -> Result<i64> {
    match get(row, field)? {
        Value::Int(n) => Ok(n),
        other => Err(Error::internal(format!(
            "Cost Center Report {field} is not an integer: {other:?}"
        ))),
    }
}

fn dec_field(row: &Value, field: &str) -> Result<Decimal> {
    match get(row, field)? {
        Value::Decimal(d) => Ok(d),
        Value::Int(n) => Ok(Decimal::from(n)),
        Value::False => Ok(Decimal::ZERO),
        other => Err(Error::internal(format!(
            "Cost Center Report {field} is not exact decimal: {other:?}"
        ))),
    }
}

fn text_field(row: &Value, field: &str) -> Result<String> {
    match get(row, field)? {
        Value::Str(s) => Ok(s.to_string()),
        Value::False => Ok(String::new()),
        other => Err(Error::internal(format!(
            "Cost Center Report {field} is not text: {other:?}"
        ))),
    }
}

fn gnum(value: Option<&GroupValue>) -> Result<Decimal> {
    match value {
        None | Some(GroupValue::Null | GroupValue::False) => Ok(Decimal::ZERO),
        Some(GroupValue::Decimal(d)) => Ok(*d),
        Some(GroupValue::Int(n)) => Ok(Decimal::from(*n)),
        Some(other) => Err(Error::internal(format!(
            "Cost Center Report aggregate is not exact decimal: {other:?}"
        ))),
    }
}

fn gid(value: Option<&GroupValue>) -> Option<i64> {
    match value {
        Some(GroupValue::Int(id)) => Some(*id),
        Some(other) => other.record_id().map(|id| id.get()),
        None => None,
    }
}

async fn company_currency(cx: &mut ReportCx<'_>, company: i64) -> Result<(i64, u32)> {
    let env = cx.env().clone();
    let crows = env
        .records("res.company")?
        .browse([company])?
        .read_rows(cx.tx(), &["currency_id"], Load::Raw)
        .await?;
    let comp = crows
        .iter()
        .next()
        .map(Value::Row)
        .ok_or_else(|| invalid("Selected company is not visible."))?;
    let cid = int_field(&comp, "currency_id")?;
    let cur_rows = env
        .records("res.currency")?
        .browse([cid])?
        .read_rows(cx.tx(), &["decimal_places"], Load::Raw)
        .await?;
    let cur = cur_rows
        .iter()
        .next()
        .map(Value::Row)
        .ok_or_else(|| invalid("Company currency is not visible."))?;
    let scale = u32::try_from(int_field(&cur, "decimal_places")?)
        .map_err(|_| invalid("Invalid currency decimal precision."))?;
    Ok((cid, scale))
}

#[derive(Default, Clone, Copy)]
struct CenterMetrics {
    primary: Decimal,
    secondary: Decimal,
    committed: Decimal,
    budget: Decimal,
}

impl CenterMetrics {
    fn total_actual(&self) -> Decimal {
        self.primary + self.secondary
    }
    fn available(&self) -> Decimal {
        self.budget - self.committed - self.total_actual()
    }
}

fn build_cells(
    report: &ReportDef,
    currency: i64,
    scale: u32,
    m: CenterMetrics,
    drillable: bool,
) -> Vec<ReportCell> {
    let mut cells = Vec::with_capacity(report.columns.len());
    for col in &report.columns {
        let val = match col.label.as_str() {
            "primary" => Some(m.primary),
            "secondary" => Some(m.secondary),
            "actual" => Some(m.total_actual()),
            "committed" => Some(m.committed),
            "budget" => Some(m.budget),
            "available" => Some(m.available()),
            _ => None,
        };
        let mut cell = ReportCell::monetary(val, scale, Some(currency)).with_drillable(drillable);
        if col.label == "available" && m.budget > Decimal::ZERO && m.available() < Decimal::ZERO {
            cell = cell.with_danger(true);
        }
        cells.push(if col.blank_if_zero {
            cell.blank_if_zero()
        } else {
            cell
        });
    }
    cells
}

fn locate(report: &ReportDef, id: &LineId) -> Result<i64> {
    if id.report() != report.id {
        return Err(invalid("Line id does not belong to Cost Center Report."));
    }
    match id.segments() {
        [seg]
            if seg.kind() == &SegmentKind::Group("account_id".to_owned())
                && seg.model() == "account.analytic.account" =>
        {
            seg.value()
                .parse::<i64>()
                .ok()
                .filter(|v| *v > 0)
                .ok_or_else(|| invalid("Invalid cost center line id."))
        }
        _ => Err(invalid("Unsupported Cost Center Report line id.")),
    }
}

impl ReportProvider for CostCenterReport {
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
                    "Cost Center Report does not accept option '{key}'."
                )));
            }
            if options.companies.len() != 1 {
                return Err(invalid("Cost Center Report requires exactly one company."));
            }
            if options.column_groups.len() != 1 {
                return Err(invalid(
                    "Cost Center Report does not support comparison periods.",
                ));
            }
            for col in &report.columns {
                if !matches!(
                    col.label.as_str(),
                    "primary" | "secondary" | "actual" | "committed" | "budget" | "available"
                ) {
                    return Err(invalid(format!(
                        "Unknown Cost Center Report column '{}'.",
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
            let company = options.companies[0];
            let (currency, scale) = company_currency(cx, company).await?;
            let from = options
                .period
                .from
                .ok_or_else(|| invalid("Cost Center Report requires a date range."))?;
            let to = options.period.to;
            let limit = cx.bounds().max_screen_lines;

            let centers_rec = cx.env().records("account.analytic.account")?;
            let found_centers = centers_rec
                .search_with(
                    cx.tx(),
                    &cond(json!([["company_id", "=", company]]))?,
                    &SearchOptions::new()
                        .order("code, name, id")
                        .limit(limit as u64 + 1),
                )
                .await?;
            if found_centers.len() > limit {
                return Err(invalid(format!(
                    "Cost Center Report exceeds reporting.max_screen_lines ({limit})."
                )));
            }
            let center_rows = found_centers
                .read_rows(cx.tx(), &["id", "code", "name", "center_type"], Load::Raw)
                .await?;

            // Aggregate primary vs secondary analytic items as the caller via ORM _read_group.
            let line_rec = cx.env().records("account.analytic.line")?;
            let primary_groups = line_rec
                ._read_group(
                    cx.tx(),
                    ReadGroup::new(cond(json!([
                        ["company_id", "=", company],
                        ["date", ">=", day(from)],
                        ["date", "<=", day(to)],
                        ["cost_kind", "=", "primary"]
                    ]))?)
                    .groupby(["account_id"])
                    .aggregates(["amount:sum"]),
                )
                .await?;
            let secondary_groups = line_rec
                ._read_group(
                    cx.tx(),
                    ReadGroup::new(cond(json!([
                        ["company_id", "=", company],
                        ["date", ">=", day(from)],
                        ["date", "<=", day(to)],
                        ["cost_kind", "!=", "primary"]
                    ]))?)
                    .groupby(["account_id"])
                    .aggregates(["amount:sum"]),
                )
                .await?;

            // Aggregate active budget lines & commitments by Cost Center via ORM.
            let budget_groups = cx
                .env()
                .records("account.budget.line")?
                ._read_group(
                    cx.tx(),
                    ReadGroup::new(cond(json!([
                        ["company_id", "=", company],
                        ["analytic_account_id", "!=", false],
                        ["date_from", "<=", day(to)],
                        ["date_to", ">=", day(from)]
                    ]))?)
                    .groupby(["analytic_account_id"])
                    .aggregates(["released_amount:sum", "committed_amount:sum"]),
                )
                .await?;

            let mut map = BTreeMap::<i64, CenterMetrics>::new();
            for row in primary_groups.rows() {
                if let Some(id) = gid(row.first()) {
                    // Expenses are negative in analytic lines -> flip to positive cost display.
                    map.entry(id).or_default().primary += -gnum(row.get(1))?;
                }
            }
            for row in secondary_groups.rows() {
                if let Some(id) = gid(row.first()) {
                    map.entry(id).or_default().secondary += -gnum(row.get(1))?;
                }
            }
            for row in budget_groups.rows() {
                if let Some(id) = gid(row.first()) {
                    let e = map.entry(id).or_default();
                    e.budget += gnum(row.get(1))?;
                    e.committed += gnum(row.get(2))?;
                }
            }

            let root = LineId::root(report.id);
            let mut out = Vec::new();
            let mut grand = CenterMetrics::default();

            for row in center_rows.iter().map(Value::Row) {
                let id = int_field(&row, "id")?;
                let code = text_field(&row, "code")?;
                let name = text_field(&row, "name")?;
                let label = if code.is_empty() {
                    name
                } else {
                    format!("{code} {name}")
                };
                let m = map.get(&id).copied().unwrap_or_default();
                grand.primary += m.primary;
                grand.secondary += m.secondary;
                grand.committed += m.committed;
                grand.budget += m.budget;

                let lid = root.child(Segment::group(
                    "account_id",
                    "account.analytic.account",
                    &id.to_string(),
                ));
                let mut line = Line::new(
                    lid.clone(),
                    label,
                    LineKind::Group,
                    build_cells(report, currency, scale, m, true),
                );
                line.unfoldable = true;
                line.unfolded = options.unfold_all || options.unfolded.contains(&lid);
                line.caret = vec!["open"];
                out.push(line);
            }

            out.push(Line::new(
                root.child(Segment::total()),
                "Total Cost & Profit Centers",
                LineKind::Total,
                build_cells(report, currency, scale, grand, false),
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
        Box::pin(async move {
            let center_id = locate(report, line)?;
            let company = options.companies[0];
            let (currency, scale) = company_currency(cx, company).await?;
            let from = options
                .period
                .from
                .ok_or_else(|| invalid("Cost Center Report requires a date range."))?;
            let to = options.period.to;

            let domain = cond(json!([
                ["company_id", "=", company],
                ["account_id", "=", center_id],
                ["date", ">=", day(from)],
                ["date", "<=", day(to)]
            ]))?;
            let rec = cx.env().records("account.analytic.line")?;
            let total_count = rec.search_count(cx.tx(), &domain, None).await?;
            let found = rec
                .search_with(
                    cx.tx(),
                    &domain,
                    &SearchOptions::new()
                        .order("date, id")
                        .offset(page.offset)
                        .limit(page.limit),
                )
                .await?;
            let rows = found
                .read_rows(
                    cx.tx(),
                    &["id", "date", "name", "cost_kind", "amount"],
                    Load::Raw,
                )
                .await?;

            let mut out = Vec::new();
            for row in rows.iter().map(Value::Row) {
                let lid_num = int_field(&row, "id")?;
                let name = text_field(&row, "name")?;
                let kind = text_field(&row, "cost_kind")?;
                let cost = -dec_field(&row, "amount")?;
                let mut m = CenterMetrics::default();
                if kind == "primary" {
                    m.primary = cost;
                } else {
                    m.secondary = cost;
                }
                let child_id = line.child(Segment::group(
                    "line_id",
                    "account.analytic.line",
                    &lid_num.to_string(),
                ));
                out.push(Line::new(
                    child_id,
                    name,
                    LineKind::Detail,
                    build_cells(report, currency, scale, m, false),
                ));
            }
            let end = page.offset + out.len() as u64;
            if end < total_count {
                let mut more = Line::new(
                    line.child(Segment::more()),
                    "",
                    LineKind::More,
                    vec![ReportCell::empty(); report.columns.len()],
                );
                more.remaining = Some(total_count - end);
                more.next_offset = Some(end);
                out.push(more);
            }
            Ok(Expansion::new(out))
        })
    }

    fn drill<'a>(
        &'a self,
        _cx: &'a mut ReportCx<'_>,
        report: &'a ReportDef,
        options: &'a Options,
        line: &'a LineId,
        target: &'a DrillTarget,
    ) -> BoxFuture<'a, Result<Json>> {
        Box::pin(async move {
            let center_id = locate(report, line)?;
            let company = options.companies[0];
            let from = options
                .period
                .from
                .ok_or_else(|| invalid("Cost Center Report requires a date range."))?;
            let to = options.period.to;

            match target {
                DrillTarget::Item(item) if item == "open" => Ok(json!({
                    "type": "ir.actions.act_window",
                    "name": "Cost / Profit Center",
                    "res_model": "account.analytic.account",
                    "res_id": center_id,
                    "target": "current",
                    "views": [[false, "form"]]
                })),
                DrillTarget::Cell(_) => {
                    let dom = cond(json!([
                        ["company_id", "=", company],
                        ["account_id", "=", center_id],
                        ["date", ">=", day(from)],
                        ["date", "<=", day(to)]
                    ]))?;
                    Ok(json!({
                        "type": "ir.actions.act_window",
                        "name": "Cost Center Items",
                        "res_model": "account.analytic.line",
                        "target": "current",
                        "views": [[false, "list"], [false, "form"]],
                        "domain": dom.to_json()?,
                        "context": {}
                    }))
                }
                _ => Err(invalid("Unsupported drill target on Cost Center Report.")),
            }
        })
    }
}

erp_addon_reporting::provider_declarations!(CostCenterReport);
