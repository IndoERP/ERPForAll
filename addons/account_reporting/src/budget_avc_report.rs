// SPDX-License-Identifier: MIT OR Apache-2.0
//! Budget Availability Control (AVC) dynamic report provider (`account_reporting.budget_avc`):
//! caller-scoped ORM reads showing Original, Revisions, Released, PO Commitments, Actuals,
//! Available, and Utilization % with danger-tone highlighting on exceeded lines.

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

pub(crate) struct BudgetAvcReport;
const SPEC: ProviderSpec = ProviderSpec::new("account_reporting.budget_avc", 1);

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
        .ok_or_else(|| Error::internal("Budget AVC row is not a batch handle."))?
        .get(field)
}

fn int_field(row: &Value, field: &str) -> Result<i64> {
    match get(row, field)? {
        Value::Int(n) => Ok(n),
        other => Err(Error::internal(format!(
            "Budget AVC {field} is not an integer: {other:?}"
        ))),
    }
}

fn dec_field(row: &Value, field: &str) -> Result<Decimal> {
    match get(row, field)? {
        Value::Decimal(d) => Ok(d),
        Value::Int(n) => Ok(Decimal::from(n)),
        Value::False => Ok(Decimal::ZERO),
        other => Err(Error::internal(format!(
            "Budget AVC {field} is not exact decimal: {other:?}"
        ))),
    }
}

fn text_field(row: &Value, field: &str) -> Result<String> {
    match get(row, field)? {
        Value::Str(s) => Ok(s.to_string()),
        Value::False => Ok(String::new()),
        other => Err(Error::internal(format!(
            "Budget AVC {field} is not text: {other:?}"
        ))),
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
struct BudgetMetrics {
    original: Decimal,
    revision: Decimal,
    released: Decimal,
    committed: Decimal,
    actual: Decimal,
    exceeded: bool,
    is_revenue: bool,
}

impl BudgetMetrics {
    fn available(&self) -> Decimal {
        self.released - self.committed - self.actual
    }
    fn util_pct(&self) -> Decimal {
        if self.released.is_zero() {
            Decimal::ZERO
        } else {
            ((self.committed + self.actual) / self.released * Decimal::from(100)).round_dp(2)
        }
    }
    fn is_zero(&self) -> bool {
        self.original.is_zero()
            && self.revision.is_zero()
            && self.released.is_zero()
            && self.committed.is_zero()
            && self.actual.is_zero()
    }
}

fn build_cells(
    report: &ReportDef,
    currency: i64,
    scale: u32,
    m: BudgetMetrics,
    drillable: bool,
) -> Vec<ReportCell> {
    let mut cells = Vec::with_capacity(report.columns.len());
    for col in &report.columns {
        let mut cell = match col.label.as_str() {
            "original" => ReportCell::monetary(Some(m.original), scale, Some(currency)),
            "revision" => ReportCell::monetary(Some(m.revision), scale, Some(currency)),
            "released" => ReportCell::monetary(Some(m.released), scale, Some(currency)),
            "committed" => ReportCell::monetary(Some(m.committed), scale, Some(currency))
                .with_drillable(drillable),
            "actual" => ReportCell::monetary(Some(m.actual), scale, Some(currency)),
            "available" => {
                let c = ReportCell::monetary(Some(m.available()), scale, Some(currency));
                if !m.is_revenue && (m.exceeded || m.available() < Decimal::ZERO) {
                    c.with_danger(true)
                } else {
                    c
                }
            }
            "util_pct" => {
                let c = ReportCell::number(CellKind::Percent, Some(m.util_pct()), 2);
                if !m.is_revenue && (m.exceeded || m.util_pct() > Decimal::from(100)) {
                    c.with_danger(true)
                } else {
                    c
                }
            }
            _ => ReportCell::empty(),
        };
        if col.blank_if_zero {
            cell = cell.blank_if_zero();
        }
        cells.push(cell);
    }
    cells
}

enum BudgetTarget {
    Budget(i64),
    Line(i64),
}

fn locate(report: &ReportDef, id: &LineId) -> Result<BudgetTarget> {
    if id.report() != report.id {
        return Err(invalid("Line id does not belong to Budget AVC Report."));
    }
    match id.segments() {
        [_, seg]
            if seg.kind() == &SegmentKind::Group("budget_id".to_owned())
                && seg.model() == "account.budget" =>
        {
            let bid = seg
                .value()
                .parse::<i64>()
                .ok()
                .filter(|v| *v > 0)
                .ok_or_else(|| invalid("Invalid budget id."))?;
            Ok(BudgetTarget::Budget(bid))
        }
        [_, _, seg]
            if seg.kind() == &SegmentKind::Group("line_id".to_owned())
                && seg.model() == "account.budget.line" =>
        {
            let lid = seg
                .value()
                .parse::<i64>()
                .ok()
                .filter(|v| *v > 0)
                .ok_or_else(|| invalid("Invalid budget line id."))?;
            Ok(BudgetTarget::Line(lid))
        }
        _ => Err(invalid("Unsupported Budget AVC Report line id.")),
    }
}

impl ReportProvider for BudgetAvcReport {
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
                    "Budget AVC Report rows are ordered by budget start date.",
                ));
            }
            if let Some(key) = options.provider.keys().next() {
                return Err(invalid(format!(
                    "Budget AVC Report does not accept option '{key}'."
                )));
            }
            if options.companies.len() != 1 {
                return Err(invalid("Budget AVC Report requires one company."));
            }
            if options.column_groups.len() != 1 {
                return Err(invalid(
                    "Budget AVC Report does not support comparison periods.",
                ));
            }
            for col in &report.columns {
                let ok = match col.label.as_str() {
                    "util_pct" => col.kind == CellKind::Percent,
                    "original" | "revision" | "released" | "committed" | "actual" | "available" => {
                        col.kind == CellKind::Monetary
                    }
                    _ => false,
                };
                if !ok {
                    return Err(invalid(format!(
                        "Unknown Budget AVC Report column '{}'.",
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
                .ok_or_else(|| invalid("Budget AVC Report requires a date range."))?;
            let to = options.period.to;
            let limit = cx.bounds().max_screen_lines;

            let budget_rec = cx.env().records("account.budget")?;
            let found = budget_rec
                .search_with(
                    cx.tx(),
                    &cond(json!([
                        ["company_id", "=", company],
                        ["state", "in", ["confirm", "validate", "done"]],
                        ["date_from", "<=", day(to)],
                        ["date_to", ">=", day(from)]
                    ]))?,
                    &SearchOptions::new()
                        .order("date_from desc, id desc")
                        .limit(limit as u64 + 1),
                )
                .await?;
            if found.len() > limit {
                return Err(invalid(format!(
                    "Budget AVC Report exceeds reporting.max_screen_lines ({limit})."
                )));
            }
            let rows = found
                .read_rows(
                    cx.tx(),
                    &[
                        "id",
                        "name",
                        "version_code",
                        "budget_type",
                        "total_original",
                        "total_revision",
                        "total_released",
                        "total_committed",
                        "total_practical",
                    ],
                    Load::Raw,
                )
                .await?;

            let root = LineId::root(report.id);
            let mut out = Vec::new();
            let mut grand = BudgetMetrics::default();

            for row in rows.iter().map(Value::Row) {
                let bid = int_field(&row, "id")?;
                let name = text_field(&row, "name")?;
                let ver = text_field(&row, "version_code")?;
                let btype = text_field(&row, "budget_type")?;
                let label = if ver.is_empty() {
                    name
                } else {
                    format!("{name} ({ver})")
                };
                let m = BudgetMetrics {
                    original: dec_field(&row, "total_original")?,
                    revision: dec_field(&row, "total_revision")?,
                    released: dec_field(&row, "total_released")?,
                    committed: dec_field(&row, "total_committed")?,
                    actual: dec_field(&row, "total_practical")?,
                    exceeded: false,
                    is_revenue: btype == "revenue",
                };
                if options.hide_zero && m.is_zero() {
                    continue;
                }
                grand.original += m.original;
                grand.revision += m.revision;
                grand.released += m.released;
                grand.committed += m.committed;
                grand.actual += m.actual;

                let lid = root.child(Segment::group(
                    "budget_id",
                    "account.budget",
                    &bid.to_string(),
                ));
                let mut line = Line::new(
                    lid,
                    label,
                    LineKind::Group,
                    build_cells(report, currency, scale, m, false),
                );
                line.unfoldable = true;
                line.caret = vec!["open"];
                out.push(line);
            }

            out.push(Line::new(
                root.child(Segment::total()),
                "Total Budgets",
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
            let BudgetTarget::Budget(bid) = locate(report, line)? else {
                return Err(invalid("Only Budget rows can be unfolded."));
            };
            let max_page = cx.bounds().max_screen_lines as u64;
            if page.limit == 0 || page.limit > max_page {
                return Err(invalid(format!(
                    "Page limit must be between 1 and {max_page}."
                )));
            }
            let company = options.companies[0];
            let from = options
                .period
                .from
                .ok_or_else(|| invalid("Budget AVC Report requires a date range."))?;
            let to = options.period.to;
            let (currency, scale) = company_currency(cx, company).await?;

            // Verify budget belongs to the selected company, is active, and overlaps the period.
            let b_rows = cx
                .env()
                .records("account.budget")?
                .search_with(
                    cx.tx(),
                    &cond(json!([
                        ["id", "=", bid],
                        ["company_id", "=", company],
                        ["state", "in", ["confirm", "validate", "done"]],
                        ["date_from", "<=", day(to)],
                        ["date_to", ">=", day(from)]
                    ]))?,
                    &SearchOptions::new().limit(1),
                )
                .await?
                .read_rows(cx.tx(), &["budget_type"], Load::Raw)
                .await?;
            let Some(brow) = b_rows.iter().next().map(Value::Row) else {
                return Err(invalid(
                    "Budget is not accessible for this company and period.",
                ));
            };
            let is_revenue = text_field(&brow, "budget_type")? == "revenue";

            let domain = cond(json!([
                ["budget_id", "=", bid],
                ["company_id", "=", company],
                ["date_from", "<=", day(to)],
                ["date_to", ">=", day(from)]
            ]))?;
            let rec = cx.env().records("account.budget.line")?;
            let total_count = rec.search_count(cx.tx(), &domain, None).await?;
            let found = rec
                .search_with(
                    cx.tx(),
                    &domain,
                    &SearchOptions::new()
                        .order("date_from, id")
                        .offset(page.offset)
                        .limit(page.limit),
                )
                .await?;
            let rows = found
                .read_rows(
                    cx.tx(),
                    &[
                        "id",
                        "name",
                        "original_amount",
                        "revision_amount",
                        "released_amount",
                        "committed_amount",
                        "practical_amount",
                        "avc_status",
                    ],
                    Load::Raw,
                )
                .await?;

            let mut out = Vec::new();
            for row in rows.iter().map(Value::Row) {
                let lid = int_field(&row, "id")?;
                let name = text_field(&row, "name")?;
                let status = text_field(&row, "avc_status")?;
                let m = BudgetMetrics {
                    original: dec_field(&row, "original_amount")?,
                    revision: dec_field(&row, "revision_amount")?,
                    released: dec_field(&row, "released_amount")?,
                    committed: dec_field(&row, "committed_amount")?,
                    actual: dec_field(&row, "practical_amount")?,
                    exceeded: status == "exceeded",
                    is_revenue,
                };
                let cid = line.child(Segment::group(
                    "line_id",
                    "account.budget.line",
                    &lid.to_string(),
                ));
                let mut dline = Line::new(
                    cid,
                    name,
                    LineKind::Detail,
                    build_cells(report, currency, scale, m, true),
                );
                dline.caret = vec!["open"];
                out.push(dline);
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
        cx: &'a mut ReportCx<'_>,
        report: &'a ReportDef,
        options: &'a Options,
        line: &'a LineId,
        target: &'a DrillTarget,
    ) -> BoxFuture<'a, Result<Json>> {
        Box::pin(async move {
            let company = options.companies[0];
            match (locate(report, line)?, target) {
                (BudgetTarget::Budget(bid), DrillTarget::Item(item)) if item == "open" => {
                    let visible = cx
                        .env()
                        .records("account.budget")?
                        .search_count(
                            cx.tx(),
                            &cond(json!([["id", "=", bid], ["company_id", "=", company]]))?,
                            None,
                        )
                        .await?;
                    if visible == 0 {
                        return Err(invalid("Budget is not accessible in this company."));
                    }
                    Ok(json!({
                        "type": "ir.actions.act_window",
                        "name": "Budget",
                        "res_model": "account.budget",
                        "res_id": bid,
                        "target": "current",
                        "views": [[false, "form"]]
                    }))
                }
                (BudgetTarget::Line(lid), DrillTarget::Item(item)) if item == "open" => {
                    let visible = cx
                        .env()
                        .records("account.budget.line")?
                        .search_count(
                            cx.tx(),
                            &cond(json!([["id", "=", lid], ["company_id", "=", company]]))?,
                            None,
                        )
                        .await?;
                    if visible == 0 {
                        return Err(invalid("Budget Line is not accessible in this company."));
                    }
                    Ok(json!({
                        "type": "ir.actions.act_window",
                        "name": "Budget Line",
                        "res_model": "account.budget.line",
                        "res_id": lid,
                        "target": "current",
                        "views": [[false, "form"]]
                    }))
                }
                (BudgetTarget::Line(lid), DrillTarget::Cell(col_idx)) => {
                    let col = report
                        .columns
                        .get(*col_idx)
                        .ok_or_else(|| invalid("Budget AVC Report column index out of range."))?;
                    if col.label != "committed" {
                        return Err(invalid(
                            "Only Committed (PO) cells drill into Purchase Commitments.",
                        ));
                    }
                    let visible = cx
                        .env()
                        .records("account.budget.line")?
                        .search_count(
                            cx.tx(),
                            &cond(json!([["id", "=", lid], ["company_id", "=", company]]))?,
                            None,
                        )
                        .await?;
                    if visible == 0 {
                        return Err(invalid("Budget Line is not accessible in this company."));
                    }
                    let dom = cond(json!([
                        ["budget_line_id", "=", lid],
                        ["company_id", "=", company],
                        ["state", "in", ["open", "partially_billed"]]
                    ]))?;
                    Ok(json!({
                        "type": "ir.actions.act_window",
                        "name": "Open Purchase Commitments",
                        "res_model": "account.budget.commitment",
                        "target": "current",
                        "views": [[false, "list"], [false, "form"]],
                        "domain": dom.to_json()?,
                        "context": {}
                    }))
                }
                _ => Err(invalid("Unsupported drill target on Budget AVC Report.")),
            }
        })
    }
}

erp_addon_reporting::provider_declarations!(BudgetAvcReport);
