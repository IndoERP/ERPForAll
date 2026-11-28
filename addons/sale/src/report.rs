// SPDX-License-Identifier: MIT OR Apache-2.0
//! Sales Report provider (`sale.sales_report`): customer -> sales orders, read through the ORM as
//! the caller (record rules and field groups apply), one company and one period, amounts in the
//! company currency at each order's stored rate, orders paged per customer.
//!
//! Period: `date_order` is a datetime; the period's days are compared as dates, which the domain
//! layer turns into midnight of the caller's timezone (`env.tz`), the ORM's own convention.

use std::collections::BTreeMap;

use erp_addon_base::money::{CURRENCY_ROUNDING_MODE, CurrencyRounding, RoundingTable};
use erp_addon_reporting::{
    definition::ReportDef,
    line::{
        Cell as ReportCell, CellKind, DrillTarget, ExpandPage, Expansion, Line, LineId, LineKind,
        Segment, SegmentKind,
    },
    options::Options,
    provider::{ProviderSpec, ReportCx, ReportProvider},
};
use erp_sdk::money::{ExactRatio, Increment};
use erp_sdk::prelude::*;
use erp_sdk::serde_json::{Value as Json, json};
use erp_sdk::{BoxFuture, FieldType, Load, SearchOptions};

pub(crate) struct SalesReport;
const SPEC: ProviderSpec = ProviderSpec::new("sale.sales_report", 3);
const ORDER: &str = "sale.order";

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

fn num(value: Option<&GroupValue>) -> Result<Decimal> {
    match value {
        None | Some(GroupValue::Null | GroupValue::False) => Ok(Decimal::ZERO),
        Some(GroupValue::Decimal(n)) => Ok(*n),
        Some(GroupValue::Int(n)) => Ok(Decimal::from(*n)),
        Some(other) => Err(Error::internal(format!(
            "Sales Report amount is not exact decimal: {other:?}"
        ))),
    }
}

fn group_id(value: Option<&GroupValue>) -> Option<i64> {
    value.and_then(GroupValue::record_id).map(|id| id.get())
}

fn get(row: &Value, field: &str) -> Result<Value> {
    row.dict()
        .ok_or_else(|| Error::internal("Sales Report row is not a batch handle."))?
        .get(field)
}

fn int_field(row: &Value, field: &str) -> Result<i64> {
    match get(row, field)? {
        Value::Int(n) => Ok(n),
        other => Err(Error::internal(format!(
            "Sales Report {field} is not an id: {other:?}"
        ))),
    }
}

fn dec_field(row: &Value, field: &str) -> Result<Decimal> {
    match get(row, field)? {
        Value::Decimal(n) => Ok(n),
        Value::Int(n) => Ok(Decimal::from(n)),
        Value::False => Ok(Decimal::ZERO),
        other => Err(Error::internal(format!(
            "Sales Report {field} is not decimal: {other:?}"
        ))),
    }
}

fn text_field(row: &Value, field: &str) -> Result<String> {
    match get(row, field)? {
        Value::Str(s) => Ok(s.to_string()),
        Value::False => Ok(String::new()),
        other => Err(Error::internal(format!(
            "Sales Report {field} is not text: {other:?}"
        ))),
    }
}

/// One company, one period with a start.
struct Scope {
    company: i64,
    from: Date,
    to: Date,
}

fn scope(options: &Options) -> Result<Scope> {
    let [company] = options.companies.as_slice() else {
        return Err(invalid("Select one company for the Sales Report."));
    };
    if options.column_groups.len() != 1 {
        return Err(invalid("Use a single period for the Sales Report."));
    }
    let from = options
        .period
        .from
        .ok_or_else(|| invalid("The Sales Report requires a date range with a start date."))?;
    Ok(Scope {
        company: *company,
        from,
        to: options.period.to,
    })
}

/// The orders of the report (company, period, not cancelled), as a JSON domain.
fn orders_json(s: &Scope) -> Json {
    json!([
        ["company_id", "=", s.company],
        ["state", "!=", "cancel"],
        ["date_order", ">=", day(s.from)],
        ["date_order", "<=", day(s.to)]
    ])
}

fn partner_cond(partner: Option<i64>) -> Json {
    match partner {
        Some(pid) => json!([["partner_id", "=", pid]]),
        None => json!([["partner_id", "=", false]]),
    }
}

fn partner_orders(s: &Scope, partner: Option<i64>) -> Result<Domain> {
    Ok(cond(orders_json(s))? & cond(partner_cond(partner))?)
}

/// The product lines of the orders matching `orders` (sections, notes and down payments left out).
fn lines_domain(orders: Json) -> Result<Domain> {
    cond(json!([
        ["order_id", "any", orders],
        ["display_type", "=", false],
        ["is_downpayment", "=", false]
    ]))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Col {
    Date,
    Status,
    Qty,
    Untaxed,
    Total,
    Margin,
    MarginPct,
}

impl Col {
    fn parse(label: &str, kind: CellKind) -> Result<Self> {
        Ok(match (label, kind) {
            ("date_order", CellKind::Date) => Self::Date,
            ("status", CellKind::Text) => Self::Status,
            ("qty", CellKind::Decimal) => Self::Qty,
            ("untaxed", CellKind::Monetary) => Self::Untaxed,
            ("total", CellKind::Monetary) => Self::Total,
            ("margin", CellKind::Monetary) => Self::Margin,
            ("margin_pct", CellKind::Percent) => Self::MarginPct,
            _ => {
                return Err(invalid(format!(
                    "The Sales Report has no column {label} of kind {}. Use date_order (date), status (text), qty (decimal), untaxed, total, margin (monetary) or margin_pct (percent).",
                    kind.as_str()
                )));
            }
        })
    }
    fn amount(self) -> bool {
        matches!(self, Self::Qty | Self::Untaxed | Self::Total | Self::Margin)
    }
}

#[derive(Clone, Copy, Default)]
struct Sums {
    qty: Decimal,
    untaxed: Decimal,
    total: Decimal,
    margin: Decimal,
}

impl Sums {
    fn add(&mut self, other: Sums) -> Result<()> {
        let add = |a: Decimal, b| {
            a.checked_add(b)
                .ok_or_else(|| invalid("A Sales Report total is out of range."))
        };
        self.qty = add(self.qty, other.qty)?;
        self.untaxed = add(self.untaxed, other.untaxed)?;
        self.total = add(self.total, other.total)?;
        self.margin = add(self.margin, other.margin)?;
        Ok(())
    }
}

/// The stored rate quotes order currency per company currency. Round at the order boundary
/// so that summing displayed order rows reproduces each customer subtotal and the grand total.
fn company_amount(amount: Decimal, rate: Decimal, increment: &Increment) -> Result<Decimal> {
    if rate <= Decimal::ZERO {
        return Err(invalid(
            "A foreign-currency sales order has a missing or non-positive stored currency rate.",
        ));
    }
    Ok(ExactRatio::quotient(amount, rate)?.quantize(increment, CURRENCY_ROUNDING_MODE)?)
}

struct Layout {
    cols: Vec<Col>,
    scope: Scope,
    currency: CurrencyRounding,
    margin: bool,
}

impl Layout {
    async fn load(cx: &mut ReportCx<'_>, report: &ReportDef, options: &Options) -> Result<Self> {
        let cols = report
            .columns
            .iter()
            .map(|c| Col::parse(&c.label, c.kind))
            .collect::<Result<Vec<_>>>()?;
        let scope = scope(options)?;
        let env = cx.env().clone();
        let company = env
            .records("res.company")?
            .browse([scope.company])?
            .read_rows(cx.tx(), &["currency_id"], Load::Raw)
            .await?;
        let row = company
            .iter()
            .next()
            .map(Value::Row)
            .ok_or_else(|| invalid("The selected Sales Report company is not visible."))?;
        let currency = match get(&row, "currency_id")? {
            Value::Int(id) if id > 0 => id,
            _ => return Err(invalid("The Sales Report company has no currency.")),
        };
        let id = RecId::from_real(currency)
            .ok_or_else(|| invalid("The selected Sales Report company has no currency."))?;
        let currency = RoundingTable::load(&env, cx.tx(), &[id])
            .await?
            .get(id)?
            .clone();
        let margin = cols
            .iter()
            .any(|c| matches!(c, Col::Margin | Col::MarginPct));
        Ok(Self {
            cols,
            scope,
            currency,
            margin,
        })
    }

    fn cells(
        &self,
        report: &ReportDef,
        date: Option<Date>,
        status: Option<&str>,
        sums: Option<Sums>,
        drillable: bool,
    ) -> Vec<ReportCell> {
        let money = |v: Option<Decimal>| {
            ReportCell::monetary(v, self.currency.decimal_places(), Some(self.currency.id()))
                .with_drillable(drillable)
        };
        self.cols
            .iter()
            .zip(&report.columns)
            .map(|(col, def)| {
                let cell = match col {
                    Col::Date => date.map_or_else(ReportCell::empty, ReportCell::date),
                    Col::Status => status.map_or_else(ReportCell::empty, ReportCell::text),
                    Col::Qty => {
                        ReportCell::number(CellKind::Decimal, sums.map(|s| s.qty), def.scale)
                            .with_drillable(drillable)
                    }
                    Col::Untaxed => money(sums.map(|s| s.untaxed)),
                    Col::Total => money(sums.map(|s| s.total)),
                    Col::Margin => money(sums.map(|s| s.margin)),
                    Col::MarginPct => ReportCell::number(
                        CellKind::Percent,
                        sums.filter(|s| !s.untaxed.is_zero()).map(|s| {
                            (s.margin * Decimal::from(100) / s.untaxed).round_dp(def.scale)
                        }),
                        def.scale,
                    ),
                };
                if def.blank_if_zero {
                    cell.blank_if_zero()
                } else {
                    cell
                }
            })
            .collect()
    }

    fn order_sums(&self, row: &Value) -> Result<Sums> {
        let domestic = int_field(row, "currency_id")? == self.currency.id();
        let rate = dec_field(row, "currency_rate")?;
        let amount = |field| {
            let value = dec_field(row, field)?;
            if domestic {
                Ok(value)
            } else {
                company_amount(value, rate, self.currency.increment())
            }
        };
        Ok(Sums {
            untaxed: amount("amount_untaxed")?,
            total: amount("amount_total")?,
            margin: if self.margin {
                amount("margin")?
            } else {
                Decimal::ZERO
            },
            qty: Decimal::ZERO,
        })
    }

    fn amount_fields(&self) -> Vec<&'static str> {
        let mut fields = vec![
            "currency_id",
            "currency_rate",
            "amount_untaxed",
            "amount_total",
        ];
        if self.margin {
            fields.push("margin");
        }
        fields
    }

    fn order_aggregates(&self) -> Vec<&'static str> {
        let mut aggs = vec!["amount_untaxed:sum", "amount_total:sum"];
        if self.margin {
            aggs.push("margin:sum");
        }
        aggs
    }
}

#[derive(Clone, Copy)]
enum Located {
    Partner(Option<i64>),
    Order(Option<i64>, i64),
}

fn positive(text: &str) -> Option<i64> {
    text.parse::<i64>().ok().filter(|n| *n > 0)
}

fn locate(report: &ReportDef, id: &LineId) -> Result<Located> {
    if id.report() != report.id {
        return Err(invalid("This line is not part of this Sales Report."));
    }
    let bad = || invalid("This is not a Sales Report line.");
    let segs = id.segments();
    let partner_seg = segs.get(1).ok_or_else(bad)?;
    if !matches!(partner_seg.kind(), SegmentKind::Group(f) if f == "partner_id")
        || partner_seg.model() != "res.partner"
    {
        return Err(bad());
    }
    let partner = match partner_seg.value() {
        "" => None,
        value => Some(positive(value).ok_or_else(bad)?),
    };
    match segs.get(2) {
        None => Ok(Located::Partner(partner)),
        Some(seg)
            if segs.len() == 3
                && matches!(seg.kind(), SegmentKind::Group(f) if f == "order_id")
                && seg.model() == ORDER =>
        {
            Ok(Located::Order(
                partner,
                positive(seg.value()).ok_or_else(bad)?,
            ))
        }
        Some(_) => Err(bad()),
    }
}

fn partner_segment(partner: Option<i64>) -> Segment {
    let value = partner.map(|p| p.to_string()).unwrap_or_default();
    Segment::group("partner_id", "res.partner", &value)
}

fn state_labels(env: &Env) -> Result<Vec<(String, String)>> {
    match env
        .registry()
        .model(ORDER)
        .and_then(|m| m.field("state"))
        .map(|f| &f.ty)
    {
        Some(FieldType::Selection {
            values: Some(values),
            ..
        }) => Ok(values.clone()),
        _ => Err(Error::programming(
            "sale.order.state is not a static selection field.",
        )),
    }
}

async fn top_lines(
    cx: &mut ReportCx<'_>,
    report: &ReportDef,
    options: &Options,
) -> Result<Expansion> {
    let layout = Layout::load(cx, report, options).await?;
    let env = cx.env().clone();
    let limit = cx.bounds().max_screen_lines;
    let orders = env.records(ORDER)?;
    let groups = orders
        ._read_group(
            cx.tx(),
            ReadGroup::new(
                cond(orders_json(&layout.scope))?
                    & cond(json!([["currency_id", "=", layout.currency.id()]]))?,
            )
            .groupby(["partner_id"])
            .aggregates(layout.order_aggregates())
            .limit(limit as u64 + 1),
        )
        .await?;
    if groups.len() > limit {
        return Err(invalid(
            "The Sales Report has more customers than reporting.max_screen_lines allows; narrow the period.",
        ));
    }
    let mut by_partner: BTreeMap<Option<i64>, Sums> = BTreeMap::new();
    for row in groups.rows() {
        let sums = by_partner.entry(group_id(row.first())).or_default();
        sums.untaxed = num(row.get(1))?;
        sums.total = num(row.get(2))?;
        if layout.margin {
            sums.margin = num(row.get(3))?;
        }
    }
    // Domestic orders aggregate directly in PostgreSQL. Foreign orders need individual rounding,
    // even when they share a rate. Read only the required fields in bounded keyset pages.
    let foreign = cond(orders_json(&layout.scope))?
        & cond(json!([
            "|",
            ["currency_id", "!=", layout.currency.id()],
            ["currency_id", "=", false]
        ]))?;
    let page_options = SearchOptions::new().order("id asc").limit(512);
    let mut fields = layout.amount_fields();
    fields.push("partner_id");
    let mut after = None;
    loop {
        let page = orders
            .search_after(cx.tx(), &foreign, &page_options, after.as_ref())
            .await?;
        let rows = page.records.read_rows(cx.tx(), &fields, Load::Raw).await?;
        for row in rows.iter().map(Value::Row) {
            let partner = match get(&row, "partner_id")? {
                Value::Int(id) => Some(id),
                Value::False => None,
                _ => return Err(Error::internal("Invalid customer in the Sales Report.")),
            };
            by_partner
                .entry(partner)
                .or_default()
                .add(layout.order_sums(&row)?)?;
            if by_partner.len() > limit {
                return Err(invalid(
                    "The Sales Report has more customers than reporting.max_screen_lines allows; narrow the period.",
                ));
            }
        }
        after = page.next;
        if after.is_none() {
            break;
        }
    }
    if by_partner.is_empty() {
        return Ok(Expansion::new(Vec::new()));
    }
    let qty_groups = env
        .records("sale.order.line")?
        ._read_group(
            cx.tx(),
            ReadGroup::new(lines_domain(orders_json(&layout.scope))?)
                .groupby(["order_partner_id"])
                .aggregates(["product_uom_qty:sum"]),
        )
        .await?;
    for row in qty_groups.rows() {
        if let Some(sums) = by_partner.get_mut(&group_id(row.first())) {
            sums.qty = num(row.get(1))?;
        }
    }

    let ids: Vec<i64> = by_partner.keys().flatten().copied().collect();
    let mut names: BTreeMap<i64, String> = BTreeMap::new();
    if !ids.is_empty() {
        let found = env
            .records("res.partner")?
            .browse(ids.iter().copied())?
            .read_rows(cx.tx(), &["id", "display_name"], Load::Raw)
            .await?;
        for row in found.iter().map(Value::Row) {
            names.insert(int_field(&row, "id")?, text_field(&row, "display_name")?);
        }
    }
    let mut ordered: Vec<(Option<i64>, String, Sums)> = by_partner
        .into_iter()
        .map(|(pid, sums)| {
            let name = match pid {
                Some(p) => names
                    .get(&p)
                    .cloned()
                    .unwrap_or_else(|| format!("Customer #{p}")),
                None => "No customer".to_owned(),
            };
            (pid, name, sums)
        })
        .collect();
    ordered.sort_by(|a, b| {
        a.0.is_none()
            .cmp(&b.0.is_none())
            .then_with(|| a.1.to_lowercase().cmp(&b.1.to_lowercase()))
            .then_with(|| a.0.cmp(&b.0))
    });

    let root = LineId::root(report.id);
    let mut grand = Sums::default();
    let mut lines = Vec::with_capacity(ordered.len() + 1);
    for (pid, name, sums) in ordered {
        grand.add(sums)?;
        let mut line = Line::new(
            root.child(partner_segment(pid)),
            name,
            LineKind::Group,
            layout.cells(report, None, None, Some(sums), true),
        );
        line.unfoldable = true;
        line.caret = if pid.is_some() {
            vec!["open", "partner"]
        } else {
            vec!["open"]
        };
        lines.push(line);
    }
    lines.push(Line::new(
        root.child(Segment::total()),
        "Total Sales",
        LineKind::Total,
        layout.cells(report, None, None, Some(grand), false),
    ));
    Ok(Expansion::new(lines))
}

async fn expand_partner(
    cx: &mut ReportCx<'_>,
    report: &ReportDef,
    options: &Options,
    id: &LineId,
    page: ExpandPage,
) -> Result<Expansion> {
    let Located::Partner(partner) = locate(report, id)? else {
        return Err(invalid("This Sales Report line has no children."));
    };
    let end = page
        .offset
        .checked_add(page.limit)
        .filter(|n| *n <= cx.bounds().max_screen_lines as u64)
        .ok_or_else(|| {
            invalid("Sales Report paging exceeds reporting.max_screen_lines; narrow the period.")
        })?;
    let layout = Layout::load(cx, report, options).await?;
    let env = cx.env().clone();
    let orders = env.records(ORDER)?;
    let domain = partner_orders(&layout.scope, partner)?;
    let count = orders.search_count(cx.tx(), &domain, None).await?;
    let found = orders
        .search_with(
            cx.tx(),
            &domain,
            &SearchOptions::new()
                .order("date_order desc, id desc")
                .offset(page.offset)
                .limit(page.limit),
        )
        .await?;
    let mut fields = layout.amount_fields();
    fields.extend(["id", "name", "date_order", "state"]);
    let rows = found.read_rows(cx.tx(), &fields, Load::Raw).await?;
    let ids: Vec<i64> = rows
        .iter()
        .map(|r| int_field(&Value::Row(r), "id"))
        .collect::<Result<_>>()?;
    let mut qty: BTreeMap<i64, Decimal> = BTreeMap::new();
    if !ids.is_empty() {
        let groups = env
            .records("sale.order.line")?
            ._read_group(
                cx.tx(),
                ReadGroup::new(lines_domain(json!([["id", "in", ids]]))?)
                    .groupby(["order_id"])
                    .aggregates(["product_uom_qty:sum"]),
            )
            .await?;
        for row in groups.rows() {
            if let Some(oid) = group_id(row.first()) {
                qty.insert(oid, num(row.get(1))?);
            }
        }
    }
    let states = state_labels(&env)?;
    let tz = env.tz().to_owned();
    let mut lines = Vec::with_capacity(ids.len() + 1);
    for row in rows.iter().map(Value::Row) {
        let oid = int_field(&row, "id")?;
        let date = match get(&row, "date_order")? {
            Value::DateTime(dt) => Date::context_today_at(&tz, dt)?,
            other => {
                return Err(Error::internal(format!(
                    "Sales order {oid} has no order date: {other:?}"
                )));
            }
        };
        let code = text_field(&row, "state")?;
        let status = states
            .iter()
            .find(|(value, _)| *value == code)
            .map(|(_, label)| label.as_str())
            .ok_or_else(|| {
                Error::internal(format!("Sales order {oid} has the unknown state '{code}'."))
            })?;
        let sums = Sums {
            qty: qty.get(&oid).copied().unwrap_or_default(),
            ..layout.order_sums(&row)?
        };
        let mut line = Line::new(
            id.child(Segment::group("order_id", ORDER, &oid.to_string())),
            text_field(&row, "name")?,
            LineKind::Detail,
            layout.cells(report, Some(date), Some(status), Some(sums), true),
        );
        line.caret = vec!["open"];
        lines.push(line);
    }
    if count > end {
        let mut more = Line::new(
            id.child(Segment::more()),
            "",
            LineKind::More,
            layout.cells(report, None, None, None, false),
        );
        more.remaining = Some(count - end);
        more.next_offset = Some(end);
        lines.push(more);
    }
    Ok(Expansion::new(lines))
}

fn list_action(name: &str, domain: &Domain) -> Result<Json> {
    Ok(json!({
        "type": "ir.actions.act_window",
        "name": name,
        "res_model": ORDER,
        "target": "current",
        "views": [[false, "list"], [false, "form"]],
        "domain": domain.to_json()?,
        "context": {},
    }))
}

fn form_action(name: &str, model: &str, id: i64) -> Json {
    json!({
        "type": "ir.actions.act_window",
        "name": name,
        "res_model": model,
        "res_id": id,
        "target": "current",
        "views": [[false, "form"]],
    })
}

async fn drill_line(
    cx: &mut ReportCx<'_>,
    report: &ReportDef,
    options: &Options,
    id: &LineId,
    target: &DrillTarget,
) -> Result<Json> {
    let located = locate(report, id)?;
    let layout = Layout::load(cx, report, options).await?;
    if let DrillTarget::Cell(index) = target {
        let col = layout
            .cols
            .get(*index)
            .ok_or_else(|| invalid("The Sales Report has no such column."))?;
        if !col.amount() {
            return Err(invalid("This Sales Report cell has no amount to drill."));
        }
    }
    match (located, target) {
        (Located::Partner(Some(pid)), DrillTarget::Item(item)) if item == "partner" => {
            Ok(form_action("Customer", "res.partner", pid))
        }
        (Located::Partner(partner), DrillTarget::Cell(_)) => {
            list_action("Sales Orders", &partner_orders(&layout.scope, partner)?)
        }
        (Located::Partner(partner), DrillTarget::Item(item)) if item == "open" => {
            list_action("Sales Orders", &partner_orders(&layout.scope, partner)?)
        }
        (Located::Order(partner, oid), target)
            if matches!(target, DrillTarget::Cell(_))
                || matches!(target, DrillTarget::Item(item) if item == "open") =>
        {
            let domain = partner_orders(&layout.scope, partner)? & cond(json!([["id", "=", oid]]))?;
            if cx
                .env()
                .records(ORDER)?
                .search_count(cx.tx(), &domain, Some(1))
                .await?
                == 0
            {
                return Err(invalid("This order is not in the Sales Report scope."));
            }
            Ok(form_action("Sales Order", ORDER, oid))
        }
        (_, DrillTarget::Item(item)) => Err(invalid(format!(
            "This Sales Report line has no action {item}."
        ))),
        _ => Err(invalid("This Sales Report action is not supported.")),
    }
}

impl ReportProvider for SalesReport {
    fn spec(&self) -> &ProviderSpec {
        &SPEC
    }
    fn init_options<'a>(
        &'a self,
        _cx: &'a mut ReportCx<'_>,
        _report: &'a ReportDef,
        options: &'a mut Options,
        _strict: bool,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            match options.provider.keys().next() {
                Some(key) => Err(invalid(format!("The Sales Report has no option {key}."))),
                None => Ok(()),
            }
        })
    }
    fn lines<'a>(
        &'a self,
        cx: &'a mut ReportCx<'_>,
        report: &'a ReportDef,
        options: &'a Options,
    ) -> BoxFuture<'a, Result<Expansion>> {
        Box::pin(top_lines(cx, report, options))
    }
    fn expand<'a>(
        &'a self,
        cx: &'a mut ReportCx<'_>,
        report: &'a ReportDef,
        options: &'a Options,
        line: &'a LineId,
        page: ExpandPage,
    ) -> BoxFuture<'a, Result<Expansion>> {
        Box::pin(expand_partner(cx, report, options, line, page))
    }
    fn drill<'a>(
        &'a self,
        cx: &'a mut ReportCx<'_>,
        report: &'a ReportDef,
        options: &'a Options,
        line: &'a LineId,
        target: &'a DrillTarget,
    ) -> BoxFuture<'a, Result<Json>> {
        Box::pin(drill_line(cx, report, options, line, target))
    }
}

erp_addon_reporting::provider_declarations!(SalesReport);

/// The report definitions are `noupdate` data: a database that installed them before the margin
/// columns carried their field's group gets the group here, so the columns and their values exist
/// only for callers who may read `margin`.
#[erp_sdk::migration(addon = "sale", version = "1.9", stage = post)]
async fn restrict_margin_columns(
    tx: &mut Transaction,
    _from: &erp_sdk::migrations::SemverVersion,
) -> Result<()> {
    tx.execute(
        "UPDATE reporting_report_column c SET groups = 'sales_team.group_sale_salesman'
         FROM ir_model_data d
         WHERE d.module = 'sale' AND d.model = 'reporting.report.column' AND d.res_id = c.id
           AND d.name IN ('sale_rep_col_margin', 'sale_rep_col_margin_pct', 'sale_prod_col_margin')",
        &[],
    )
    .await?;
    Ok(())
}

/// Add the `Margin %` column and formulas to `Sales by Product` on upgraded databases (where
/// `sale_report_data.xml` is `noupdate="1"`), enforce its salesman group, and reconcile any stale
/// stored line/order margins using currency precision.
#[erp_sdk::migration(addon = "sale", version = "1.11", stage = post)]
async fn align_sales_by_product_margin(
    tx: &mut Transaction,
    _from: &erp_sdk::migrations::SemverVersion,
) -> Result<()> {
    tx.flush().await?;
    let report_rows = tx
        .query(
            "SELECT r.id::int8 \
             FROM ir_model_data d \
             JOIN reporting_report r ON r.id = d.res_id \
             WHERE d.module = 'sale' AND d.model = 'reporting.report' AND d.name = 'report_sales_by_product'",
            &[],
        )
        .await?;
    if let Some(row) = report_rows.first() {
        let report_id: i64 = row
            .try_get(0)
            .map_err(|e| Error::db("Could not read Sales by Product report id.").with_source(e))?;
        let col_xmlid_exists = tx
            .query(
                "SELECT 1 FROM ir_model_data \
                 WHERE module = 'sale' AND model = 'reporting.report.column' AND name = 'sale_prod_col_margin_pct'",
                &[],
            )
            .await?;
        let col_label_exists = tx
            .query(
                "SELECT id::int8 FROM reporting_report_column WHERE report_id = $1 AND label = 'margin_pct'",
                &[&report_id],
            )
            .await?;
        if col_xmlid_exists.is_empty() && col_label_exists.is_empty() {
            let inserted = tx
                .query(
                    "INSERT INTO reporting_report_column \
                     (report_id, sequence, name, label, kind, scale, sortable, blank_if_zero, rollup, groups) \
                     VALUES ($1, 40, jsonb_build_object('en_US', 'Margin %'), 'margin_pct', 'percent', 2, true, false, 'sum', 'sales_team.group_sale_salesman') \
                     RETURNING id::int8",
                    &[&report_id],
                )
                .await?;
            if let Some(col_row) = inserted.first() {
                let col_id: i64 = col_row
                    .try_get(0)
                    .map_err(|e| Error::db("Could not read inserted column id.").with_source(e))?;
                tx.execute(
                    "INSERT INTO ir_model_data (module, name, model, res_id, noupdate) \
                     VALUES ('sale', 'sale_prod_col_margin_pct', 'reporting.report.column', $1, true) \
                     ON CONFLICT (module, name) DO NOTHING",
                    &[&col_id],
                )
                .await?;
            }
        }
        tx.execute(
            "UPDATE reporting_report_column c SET groups = 'sales_team.group_sale_salesman' \
             FROM ir_model_data d \
             WHERE d.module = 'sale' AND d.model = 'reporting.report.column' AND d.res_id = c.id \
               AND d.name = 'sale_prod_col_margin_pct'",
            &[],
        )
        .await?;

        for (line_xmlid, expr_xmlid) in [
            ("sale_prod_line_confirmed", "sale_prod_conf_mar_pct"),
            ("sale_prod_line_quotations", "sale_prod_quot_mar_pct"),
        ] {
            let line_rows = tx
                .query(
                    "SELECT l.id::int8 \
                     FROM ir_model_data d \
                     JOIN reporting_report_line l ON l.id = d.res_id \
                     WHERE d.module = 'sale' AND d.model = 'reporting.report.line' AND d.name = $1",
                    &[&line_xmlid],
                )
                .await?;
            if let Some(lrow) = line_rows.first() {
                let line_id: i64 = lrow.try_get(0).map_err(|e| {
                    Error::db("Could not read Sales by Product line id.").with_source(e)
                })?;
                let expr_xmlid_exists = tx
                    .query(
                        "SELECT 1 FROM ir_model_data \
                         WHERE module = 'sale' AND model = 'reporting.report.expression' AND name = $1",
                        &[&expr_xmlid],
                    )
                    .await?;
                let expr_label_exists = tx
                    .query(
                        "SELECT id::int8 FROM reporting_report_expression WHERE line_id = $1 AND label = 'margin_pct'",
                        &[&line_id],
                    )
                    .await?;
                let prereq_count: i64 = tx
                    .query(
                        "SELECT COUNT(DISTINCT label)::int8 FROM reporting_report_expression \
                         WHERE line_id = $1 AND label IN ('margin', 'untaxed')",
                        &[&line_id],
                    )
                    .await?
                    .first()
                    .ok_or_else(|| Error::db("Could not count prerequisite expressions."))?
                    .try_get(0)
                    .map_err(|e| {
                        Error::db("Could not decode prerequisite count.").with_source(e)
                    })?;
                if expr_xmlid_exists.is_empty() && expr_label_exists.is_empty() && prereq_count == 2
                {
                    let inserted = tx
                        .query(
                            "INSERT INTO reporting_report_expression \
                             (line_id, label, engine, formula, date_scope, zero_division) \
                             VALUES ($1, 'margin_pct', 'formula', 'margin * 100 / untaxed', 'strict_range', 'empty') \
                             RETURNING id::int8",
                            &[&line_id],
                        )
                        .await?;
                    if let Some(erow) = inserted.first() {
                        let expr_id: i64 = erow.try_get(0).map_err(|e| {
                            Error::db("Could not read inserted expression id.").with_source(e)
                        })?;
                        tx.execute(
                            "INSERT INTO ir_model_data (module, name, model, res_id, noupdate) \
                             VALUES ('sale', $1, 'reporting.report.expression', $2, true) \
                             ON CONFLICT (module, name) DO NOTHING",
                            &[&expr_xmlid, &expr_id],
                        )
                        .await?;
                    }
                }
            }
        }
    }

    // Reconcile stored line and order margins to match `margin.rs` (including orders with zero
    // accountable lines and order currency decimal places).
    tx.execute(
        "UPDATE sale_order_line \
         SET margin = CASE \
             WHEN display_type IS NOT NULL OR COALESCE(is_downpayment, false) THEN 0 \
             ELSE ROUND(COALESCE(price_subtotal, 0) - COALESCE(purchase_price, 0) * COALESCE(product_uom_qty, 0), 2) \
         END",
        &[],
    )
    .await?;
    tx.execute(
        "UPDATE sale_order_line \
         SET margin_percent = CASE \
             WHEN display_type IS NOT NULL OR COALESCE(is_downpayment, false) OR COALESCE(price_subtotal, 0) <= 0 THEN 0 \
             ELSE ROUND((COALESCE(margin, 0) * 100) / price_subtotal, 2) \
         END",
        &[],
    )
    .await?;
    tx.execute(
        "UPDATE sale_order o \
         SET margin = ROUND(COALESCE(s.raw_margin, 0), COALESCE(c.decimal_places, 2)), \
             margin_percent = CASE \
                 WHEN COALESCE(o.amount_untaxed, 0) <= 0 THEN 0 \
                 ELSE ROUND((ROUND(COALESCE(s.raw_margin, 0), COALESCE(c.decimal_places, 2)) * 100) / o.amount_untaxed, 2) \
             END \
         FROM sale_order o2 \
         LEFT JOIN res_currency c ON c.id = o2.currency_id \
         LEFT JOIN ( \
             SELECT order_id, \
                    COALESCE(SUM(COALESCE(price_subtotal, 0) - COALESCE(purchase_price, 0) * COALESCE(product_uom_qty, 0)), 0) AS raw_margin \
             FROM sale_order_line \
             WHERE display_type IS NULL AND NOT COALESCE(is_downpayment, false) \
             GROUP BY order_id \
         ) s ON s.order_id = o2.id \
         WHERE o.id = o2.id",
        &[],
    )
    .await?;

    tx.invalidate_all().await?;
    Ok(())
}

#[cfg(test)]
mod currency_tests {
    use super::*;
    use erp_sdk::money::Increment;

    #[test]
    fn divides_by_the_stored_order_rate_and_rounds_each_order() -> Result<()> {
        let cents = Increment::from_places(2)?;
        // The order rate is units of order currency per unit of company currency.
        let rate = Decimal::new(88, 2);
        let convert = |amount| company_amount(amount, rate, &cents);
        assert_eq!(convert(Decimal::from(88))?, Decimal::from(100));
        assert_eq!(convert(Decimal::new(-11, 2))?, Decimal::new(-13, 2));
        let small = convert(Decimal::new(3, 2))?;
        assert_eq!(small + small, Decimal::new(6, 2));
        assert_eq!(convert(Decimal::new(6, 2))?, Decimal::new(7, 2));
        for invalid_rate in [Decimal::ZERO, Decimal::NEGATIVE_ONE] {
            assert!(company_amount(Decimal::ONE, invalid_rate, &cents).is_err());
        }
        Ok(())
    }
}
