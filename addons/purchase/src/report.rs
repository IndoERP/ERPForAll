// SPDX-License-Identifier: MIT OR Apache-2.0
//! Purchase Report provider (`purchase.purchase_report`): vendor -> purchase orders, read through
//! the ORM as the caller (record rules and field groups apply), one company and one period,
//! confirmed orders only (requests for quotation and cancelled orders are left out), amounts in
//! the company currency at each order's stored rate, orders paged per vendor. An expected arrival before the caller's
//! today with quantities still to receive carries the danger tone.
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
use erp_sdk::{BoxFuture, Load, SearchOptions};

pub(crate) struct PurchaseReport;
const SPEC: ProviderSpec = ProviderSpec::new("purchase.purchase_report", 3);
const ORDER: &str = "purchase.order";
/// The states the report leaves out: requests for quotation and cancelled orders (the same
/// condition as "Purchases by Product").
const EXCLUDED_STATES: [&str; 3] = ["draft", "sent", "cancel"];

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
            "Purchase Report amount is not exact decimal: {other:?}"
        ))),
    }
}

fn group_id(value: Option<&GroupValue>) -> Option<i64> {
    value.and_then(GroupValue::record_id).map(|id| id.get())
}

fn get(row: &Value, field: &str) -> Result<Value> {
    row.dict()
        .ok_or_else(|| Error::internal("Purchase Report row is not a batch handle."))?
        .get(field)
}

fn int_field(row: &Value, field: &str) -> Result<i64> {
    match get(row, field)? {
        Value::Int(n) => Ok(n),
        other => Err(Error::internal(format!(
            "Purchase Report {field} is not an id: {other:?}"
        ))),
    }
}

fn dec_field(row: &Value, field: &str) -> Result<Decimal> {
    match get(row, field)? {
        Value::Decimal(n) => Ok(n),
        Value::Int(n) => Ok(Decimal::from(n)),
        Value::False => Ok(Decimal::ZERO),
        other => Err(Error::internal(format!(
            "Purchase Report {field} is not decimal: {other:?}"
        ))),
    }
}

fn text_field(row: &Value, field: &str) -> Result<String> {
    match get(row, field)? {
        Value::Str(s) => Ok(s.to_string()),
        Value::False => Ok(String::new()),
        other => Err(Error::internal(format!(
            "Purchase Report {field} is not text: {other:?}"
        ))),
    }
}

/// The local date of a datetime field in `tz`; `None` when unset.
fn local_date(row: &Value, field: &str, tz: &str) -> Result<Option<Date>> {
    match get(row, field)? {
        Value::DateTime(dt) => Ok(Some(Date::context_today_at(tz, dt)?)),
        Value::False => Ok(None),
        other => Err(Error::internal(format!(
            "Purchase Report {field} is not a datetime: {other:?}"
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
        return Err(invalid("Select one company for the Purchase Report."));
    };
    if options.column_groups.len() != 1 {
        return Err(invalid("Use a single period for the Purchase Report."));
    }
    let from = options
        .period
        .from
        .ok_or_else(|| invalid("The Purchase Report requires a date range with a start date."))?;
    Ok(Scope {
        company: *company,
        from,
        to: options.period.to,
    })
}

/// The orders of the report (company, period, confirmed), as a JSON domain.
fn orders_json(s: &Scope) -> Json {
    json!([
        ["company_id", "=", s.company],
        ["state", "not in", EXCLUDED_STATES],
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

fn lines_domain(orders: Json) -> Result<Domain> {
    cond(json!([["order_id", "any", orders]]))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Col {
    Date,
    Planned,
    Ordered,
    Received,
    Billed,
    Untaxed,
    Total,
}

impl Col {
    fn parse(label: &str, kind: CellKind) -> Result<Self> {
        Ok(match (label, kind) {
            ("date_order", CellKind::Date) => Self::Date,
            ("date_planned", CellKind::Date) => Self::Planned,
            ("ordered", CellKind::Decimal) => Self::Ordered,
            ("received", CellKind::Decimal) => Self::Received,
            ("billed", CellKind::Decimal) => Self::Billed,
            ("untaxed", CellKind::Monetary) => Self::Untaxed,
            ("total", CellKind::Monetary) => Self::Total,
            _ => {
                return Err(invalid(format!(
                    "The Purchase Report has no column {label} of kind {}. Use date_order, date_planned (date), ordered, received, billed (decimal), untaxed or total (monetary).",
                    kind.as_str()
                )));
            }
        })
    }
    fn amount(self) -> bool {
        !matches!(self, Self::Date | Self::Planned)
    }
}

#[derive(Clone, Copy, Default)]
struct Sums {
    ordered: Decimal,
    received: Decimal,
    billed: Decimal,
    untaxed: Decimal,
    total: Decimal,
}

impl Sums {
    fn add(&mut self, other: Sums) -> Result<()> {
        let add = |a: Decimal, b| {
            a.checked_add(b)
                .ok_or_else(|| invalid("A Purchase Report total is out of range."))
        };
        self.ordered = add(self.ordered, other.ordered)?;
        self.received = add(self.received, other.received)?;
        self.billed = add(self.billed, other.billed)?;
        self.untaxed = add(self.untaxed, other.untaxed)?;
        self.total = add(self.total, other.total)?;
        Ok(())
    }
}

/// Round each purchase order before adding its amounts to its vendor and report totals.
fn converted_amounts(
    untaxed: Decimal,
    total: Decimal,
    rate: Decimal,
    increment: &Increment,
    id: i64,
) -> Result<Sums> {
    if rate <= Decimal::ZERO {
        return Err(invalid(format!(
            "Purchase order #{id} has a missing or non-positive stored currency rate."
        )));
    }
    let convert =
        |amount| ExactRatio::quotient(amount, rate)?.quantize(increment, CURRENCY_ROUNDING_MODE);
    Ok(Sums {
        untaxed: convert(untaxed)?,
        total: convert(total)?,
        ..Sums::default()
    })
}

struct Layout {
    cols: Vec<Col>,
    scope: Scope,
    currency: CurrencyRounding,
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
            .ok_or_else(|| invalid("The selected Purchase Report company is not visible."))?;
        let currency = match get(&row, "currency_id")? {
            Value::Int(id) if id > 0 => id,
            _ => return Err(invalid("The Purchase Report company has no currency.")),
        };
        let id = RecId::from_real(currency)
            .ok_or_else(|| invalid("The Purchase Report company currency is invalid."))?;
        let currency = RoundingTable::load(&env, cx.tx(), &[id])
            .await?
            .get(id)?
            .clone();
        Ok(Self {
            cols,
            scope,
            currency,
        })
    }

    fn amounts(&self, row: &Value) -> Result<Sums> {
        let untaxed = dec_field(row, "amount_untaxed")?;
        let total = dec_field(row, "amount_total")?;
        if int_field(row, "currency_id")? == self.currency.id() {
            return Ok(Sums {
                untaxed,
                total,
                ..Sums::default()
            });
        }
        converted_amounts(
            untaxed,
            total,
            dec_field(row, "currency_rate")?,
            self.currency.increment(),
            int_field(row, "id")?,
        )
    }

    fn cells(
        &self,
        report: &ReportDef,
        dates: Option<(Date, Option<Date>, bool)>,
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
                let qty = |v: Option<Decimal>| {
                    ReportCell::number(CellKind::Decimal, v, def.scale).with_drillable(drillable)
                };
                let cell = match col {
                    Col::Date => dates.map_or_else(ReportCell::empty, |d| ReportCell::date(d.0)),
                    Col::Planned => match dates {
                        Some((_, Some(planned), overdue)) => {
                            ReportCell::date(planned).with_danger(overdue)
                        }
                        _ => ReportCell::empty(),
                    },
                    Col::Ordered => qty(sums.map(|s| s.ordered)),
                    Col::Received => qty(sums.map(|s| s.received)),
                    Col::Billed => qty(sums.map(|s| s.billed)),
                    Col::Untaxed => money(sums.map(|s| s.untaxed)),
                    Col::Total => money(sums.map(|s| s.total)),
                };
                if def.blank_if_zero {
                    cell.blank_if_zero()
                } else {
                    cell
                }
            })
            .collect()
    }
}

const LINE_AGGREGATES: [&str; 3] = ["product_qty:sum", "qty_received:sum", "qty_invoiced:sum"];

fn set_quantities(sums: &mut Sums, row: &[GroupValue]) -> Result<()> {
    sums.ordered = num(row.get(1))?;
    sums.received = num(row.get(2))?;
    sums.billed = num(row.get(3))?;
    Ok(())
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
        return Err(invalid("This line is not part of this Purchase Report."));
    }
    let bad = || invalid("This is not a Purchase Report line.");
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

async fn top_lines(
    cx: &mut ReportCx<'_>,
    report: &ReportDef,
    options: &Options,
) -> Result<Expansion> {
    let layout = Layout::load(cx, report, options).await?;
    let env = cx.env().clone();
    let limit = cx.bounds().max_screen_lines;
    let orders = env.records(ORDER)?;
    let domain = cond(orders_json(&layout.scope))?;
    let groups = orders
        ._read_group(
            cx.tx(),
            ReadGroup::new(
                domain.clone() & cond(json!([["currency_id", "=", layout.currency.id()]]))?,
            )
            .groupby(["partner_id"])
            .aggregates(["amount_untaxed:sum", "amount_total:sum"])
            .limit(limit as u64 + 1),
        )
        .await?;
    if groups.len() > limit {
        return Err(invalid(
            "The Purchase Report has more vendors than reporting.max_screen_lines allows; narrow the period.",
        ));
    }
    let mut by_partner: BTreeMap<Option<i64>, Sums> = BTreeMap::new();
    for row in groups.rows() {
        let sums = by_partner.entry(group_id(row.first())).or_default();
        sums.untaxed = num(row.get(1))?;
        sums.total = num(row.get(2))?;
    }
    // Domestic orders aggregate in SQL. Foreign orders need rounding before aggregation.
    let foreign = domain
        & cond(json!([
            "|",
            ["currency_id", "!=", layout.currency.id()],
            ["currency_id", "=", false]
        ]))?;
    let mut after = None;
    let paging = SearchOptions::new().order("id asc").limit(512);
    loop {
        let page = orders
            .search_after(cx.tx(), &foreign, &paging, after.as_ref())
            .await?;
        let rows = page
            .records
            .read_rows(
                cx.tx(),
                &[
                    "id",
                    "partner_id",
                    "currency_id",
                    "currency_rate",
                    "amount_untaxed",
                    "amount_total",
                ],
                Load::Raw,
            )
            .await?;
        for row in rows.iter().map(Value::Row) {
            let vendor = match get(&row, "partner_id")? {
                Value::Int(id) => Some(id),
                Value::False => None,
                _ => return Err(Error::internal("Invalid vendor in the Purchase Report.")),
            };
            by_partner
                .entry(vendor)
                .or_default()
                .add(layout.amounts(&row)?)?;
            if by_partner.len() > limit {
                return Err(invalid(
                    "The Purchase Report has more vendors than reporting.max_screen_lines allows; narrow the period.",
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
        .records("purchase.order.line")?
        ._read_group(
            cx.tx(),
            ReadGroup::new(lines_domain(orders_json(&layout.scope))?)
                .groupby(["partner_id"])
                .aggregates(LINE_AGGREGATES),
        )
        .await?;
    for row in qty_groups.rows() {
        if let Some(sums) = by_partner.get_mut(&group_id(row.first())) {
            set_quantities(sums, &row)?;
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
                    .unwrap_or_else(|| format!("Vendor #{p}")),
                None => "No vendor".to_owned(),
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
            layout.cells(report, None, Some(sums), true),
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
        "Total Purchases",
        LineKind::Total,
        layout.cells(report, None, Some(grand), false),
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
        return Err(invalid("This Purchase Report line has no children."));
    };
    let end = page
        .offset
        .checked_add(page.limit)
        .filter(|n| *n <= cx.bounds().max_screen_lines as u64)
        .ok_or_else(|| {
            invalid("Purchase Report paging exceeds reporting.max_screen_lines; narrow the period.")
        })?;
    let layout = Layout::load(cx, report, options).await?;
    let env = cx.env().clone();
    let today = env.context_today()?;
    let tz = env.tz().to_owned();
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
    let rows = found
        .read_rows(
            cx.tx(),
            &[
                "id",
                "name",
                "date_order",
                "date_planned",
                "currency_id",
                "currency_rate",
                "amount_untaxed",
                "amount_total",
            ],
            Load::Raw,
        )
        .await?;
    let ids: Vec<i64> = rows
        .iter()
        .map(|r| int_field(&Value::Row(r), "id"))
        .collect::<Result<_>>()?;
    let mut quantities: BTreeMap<i64, Sums> = BTreeMap::new();
    if !ids.is_empty() {
        let groups = env
            .records("purchase.order.line")?
            ._read_group(
                cx.tx(),
                ReadGroup::new(cond(json!([["order_id", "in", ids]]))?)
                    .groupby(["order_id"])
                    .aggregates(LINE_AGGREGATES),
            )
            .await?;
        for row in groups.rows() {
            if let Some(oid) = group_id(row.first()) {
                set_quantities(quantities.entry(oid).or_default(), &row)?;
            }
        }
    }
    let mut lines = Vec::with_capacity(ids.len() + 1);
    for row in rows.iter().map(Value::Row) {
        let oid = int_field(&row, "id")?;
        let date = local_date(&row, "date_order", &tz)?
            .ok_or_else(|| Error::internal(format!("Purchase order {oid} has no order date.")))?;
        let planned = local_date(&row, "date_planned", &tz)?;
        let mut sums = quantities.get(&oid).copied().unwrap_or_default();
        sums.add(layout.amounts(&row)?)?;
        let overdue = planned.is_some_and(|d| d < today) && sums.received < sums.ordered;
        let mut line = Line::new(
            id.child(Segment::group("order_id", ORDER, &oid.to_string())),
            text_field(&row, "name")?,
            LineKind::Detail,
            layout.cells(report, Some((date, planned, overdue)), Some(sums), true),
        );
        line.caret = vec!["open"];
        lines.push(line);
    }
    if count > end {
        let mut more = Line::new(
            id.child(Segment::more()),
            "",
            LineKind::More,
            layout.cells(report, None, None, false),
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
            .ok_or_else(|| invalid("The Purchase Report has no such column."))?;
        if !col.amount() {
            return Err(invalid("This Purchase Report cell has no amount to drill."));
        }
    }
    match (located, target) {
        (Located::Partner(Some(pid)), DrillTarget::Item(item)) if item == "partner" => {
            Ok(form_action("Vendor", "res.partner", pid))
        }
        (Located::Partner(partner), DrillTarget::Cell(_)) => {
            list_action("Purchase Orders", &partner_orders(&layout.scope, partner)?)
        }
        (Located::Partner(partner), DrillTarget::Item(item)) if item == "open" => {
            list_action("Purchase Orders", &partner_orders(&layout.scope, partner)?)
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
                return Err(invalid("This order is not in the Purchase Report scope."));
            }
            Ok(form_action("Purchase Order", ORDER, oid))
        }
        (_, DrillTarget::Item(item)) => Err(invalid(format!(
            "This Purchase Report line has no action {item}."
        ))),
        _ => Err(invalid("This Purchase Report action is not supported.")),
    }
}

impl ReportProvider for PurchaseReport {
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
                Some(key) => Err(invalid(format!("The Purchase Report has no option {key}."))),
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

erp_addon_reporting::provider_declarations!(PurchaseReport);

/// The report definitions are `noupdate` data: a database that installed "Purchases by Product"
/// before it left out requests for quotation and cancelled orders gets the condition here, dated
/// by the order date like the Purchase Report.
#[erp_sdk::migration(addon = "purchase", version = "1.8", stage = post)]
async fn confirmed_orders_by_product(
    tx: &mut Transaction,
    _from: &erp_sdk::migrations::SemverVersion,
) -> Result<()> {
    tx.execute(
        "UPDATE reporting_report_expression e
         SET date_field = 'date_order',
             domain = '[[\"product_id\",\"!=\",false],[\"state\",\"not in\",[\"draft\",\"sent\",\"cancel\"]]]'
         FROM ir_model_data d
         WHERE d.module = 'purchase' AND d.model = 'reporting.report.expression' AND d.res_id = e.id
           AND d.name IN ('pur_prod_ord', 'pur_prod_rec', 'pur_prod_bil', 'pur_prod_unt')",
        &[],
    )
    .await?;
    Ok(())
}

#[cfg(test)]
mod currency_tests {
    use super::*;

    #[test]
    fn vendor_total_adds_individually_rounded_orders_at_their_own_rates() -> Result<()> {
        let cents = Increment::from_places(2)?;
        let mut vendor = Sums {
            untaxed: Decimal::from(10),
            total: Decimal::from(11),
            ..Sums::default()
        };
        for (rate, expected) in [(80, 13), (88, 11)] {
            let fx = converted_amounts(
                Decimal::new(-10, 2),
                Decimal::new(3, 2),
                Decimal::new(rate, 2),
                &cents,
                9,
            )?;
            assert_eq!(fx.untaxed, Decimal::new(-expected, 2));
            vendor.add(fx)?;
        }
        assert_eq!(vendor.untaxed, Decimal::new(976, 2));
        assert_eq!(vendor.total, Decimal::new(1107, 2));
        for rate in [Decimal::ZERO, Decimal::NEGATIVE_ONE] {
            let error = converted_amounts(Decimal::ZERO, Decimal::ONE, rate, &cents, 91).err();
            assert!(error.is_some_and(|e| e.to_string().contains("order #91")));
        }
        Ok(())
    }
}
