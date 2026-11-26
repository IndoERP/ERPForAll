// SPDX-License-Identifier: MIT OR Apache-2.0
//! Fixed Asset & CIP History Sheet dynamic report provider (`account_reporting.asset_register`):
//! caller-scoped ORM reads grouped by Asset Class -> Fixed Assets & CIP projects with
//! Gross / CIP Cost, CIP Settled, Accumulated Depreciation, and Net Book Value.

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

pub(crate) struct AssetRegisterReport;
const SPEC: ProviderSpec = ProviderSpec::new("account_reporting.asset_register", 1);

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
            "Asset Register aggregate is not exact decimal: {other:?}"
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
            "Asset Register group key is not text: {other:?}"
        ))),
    }
}

fn get(row: &Value, field: &str) -> Result<Value> {
    row.dict()
        .ok_or_else(|| Error::internal("Asset Register row is not a batch handle."))?
        .get(field)
}

fn int_field(row: &Value, field: &str) -> Result<i64> {
    match get(row, field)? {
        Value::Int(n) => Ok(n),
        other => Err(Error::internal(format!(
            "Asset Register {field} is not an integer: {other:?}"
        ))),
    }
}

fn opt_int_field(row: &Value, field: &str) -> Result<Option<i64>> {
    match get(row, field)? {
        Value::Int(n) if n > 0 => Ok(Some(n)),
        Value::False | Value::Int(_) => Ok(None),
        other => Err(Error::internal(format!(
            "Asset Register {field} is not an optional id: {other:?}"
        ))),
    }
}

fn dec_field(row: &Value, field: &str) -> Result<Decimal> {
    match get(row, field)? {
        Value::Decimal(d) => Ok(d),
        Value::Int(n) => Ok(Decimal::from(n)),
        Value::False => Ok(Decimal::ZERO),
        other => Err(Error::internal(format!(
            "Asset Register {field} is not exact decimal: {other:?}"
        ))),
    }
}

fn text_field(row: &Value, field: &str) -> Result<String> {
    match get(row, field)? {
        Value::Str(s) => Ok(s.to_string()),
        Value::False => Ok(String::new()),
        other => Err(Error::internal(format!(
            "Asset Register {field} is not text: {other:?}"
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

#[derive(Default, Clone)]
struct AssetTotals {
    kind_text: String,
    state_text: String,
    gross: Decimal,
    settled: Decimal,
    depreciation: Decimal,
    nbv: Decimal,
}

fn build_cells(report: &ReportDef, currency: i64, scale: u32, t: &AssetTotals) -> Vec<ReportCell> {
    let mut cells = Vec::with_capacity(report.columns.len());
    for col in &report.columns {
        let cell = match col.label.as_str() {
            "asset_type" => {
                if t.kind_text.is_empty() {
                    ReportCell::empty()
                } else {
                    ReportCell::text(t.kind_text.clone())
                }
            }
            "state" => {
                if t.state_text.is_empty() {
                    ReportCell::empty()
                } else {
                    ReportCell::text(t.state_text.clone())
                }
            }
            "gross" => ReportCell::monetary(Some(t.gross), scale, Some(currency)),
            "settled" => ReportCell::monetary(Some(t.settled), scale, Some(currency)),
            "depreciation" => ReportCell::monetary(Some(t.depreciation), scale, Some(currency)),
            "nbv" => ReportCell::monetary(Some(t.nbv), scale, Some(currency)),
            _ => ReportCell::empty(),
        };
        cells.push(if col.blank_if_zero {
            cell.blank_if_zero()
        } else {
            cell
        });
    }
    cells
}

enum AssetLineTarget {
    Category(Option<i64>),
    Asset(i64),
}

fn locate(report: &ReportDef, id: &LineId) -> Result<AssetLineTarget> {
    if id.report() != report.id {
        return Err(invalid("Line id does not belong to Asset & CIP Register."));
    }
    match id.segments() {
        [_, seg]
            if seg.kind() == &SegmentKind::Group("category_id".to_owned())
                && seg.model() == "account.asset.category" =>
        {
            match seg.value() {
                "" => Ok(AssetLineTarget::Category(None)),
                v => Ok(AssetLineTarget::Category(Some(
                    v.parse::<i64>()
                        .ok()
                        .filter(|id| *id > 0)
                        .ok_or_else(|| invalid("Invalid asset class id."))?,
                ))),
            }
        }
        [_, _, seg]
            if seg.kind() == &SegmentKind::Group("asset_id".to_owned())
                && seg.model() == "account.asset" =>
        {
            let aid = seg
                .value()
                .parse::<i64>()
                .ok()
                .filter(|v| *v > 0)
                .ok_or_else(|| invalid("Invalid asset id."))?;
            Ok(AssetLineTarget::Asset(aid))
        }
        _ => Err(invalid("Unsupported Asset & CIP Register line id.")),
    }
}

impl ReportProvider for AssetRegisterReport {
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
                    "Asset & CIP Register rows are ordered by asset class and code.",
                ));
            }
            if let Some(key) = options.provider.keys().next() {
                return Err(invalid(format!(
                    "Asset & CIP Register does not accept option '{key}'."
                )));
            }
            if options.companies.len() != 1 {
                return Err(invalid("Asset & CIP Register requires one company."));
            }
            if options.column_groups.len() != 1 {
                return Err(invalid(
                    "Asset & CIP Register does not support comparison periods.",
                ));
            }
            for col in &report.columns {
                let ok = match col.label.as_str() {
                    "asset_type" | "state" => col.kind == CellKind::Text,
                    "gross" | "settled" | "depreciation" | "nbv" => col.kind == CellKind::Monetary,
                    _ => false,
                };
                if !ok {
                    return Err(invalid(format!(
                        "Unknown Asset & CIP Register column '{}'.",
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
            let to = options.period.to;
            let limit = cx.bounds().max_screen_lines;

            // Aggregate non-draft assets acquired on or before `to` by (category, type, state)
            // so registers with >max_screen_lines assets remain fast and never fail unnecessarily.
            let asset_rec = cx.env().records("account.asset")?;
            let asset_domain = cond(json!([
                ["company_id", "=", company],
                ["state", "!=", "draft"],
                ["acquisition_date", "<=", day(to)]
            ]))?;
            let grouped = asset_rec
                ._read_group(
                    cx.tx(),
                    ReadGroup::new(asset_domain)
                        .groupby(["category_id", "asset_type", "state"])
                        .aggregates(["original_value:sum"]),
                )
                .await?;

            // Derive CIP accumulation & settlement strictly from stored `account.asset.cip.line`
            // rows dated <= `to` (never aggregating unstored computed fields on `account.asset`).
            let cip_groups = cx
                .env()
                .records("account.asset.cip.line")?
                ._read_group(
                    cx.tx(),
                    ReadGroup::new(cond(json!([
                        ["asset_id.company_id", "=", company],
                        ["asset_id.asset_type", "=", "cip"],
                        ["asset_id.state", "!=", "draft"],
                        ["asset_id.acquisition_date", "<=", day(to)],
                        ["date", "<=", day(to)]
                    ]))?)
                    .groupby(["asset_id"])
                    .aggregates(["amount:sum", "settled_amount:sum"]),
                )
                .await?;
            let mut cip_by_asset = BTreeMap::<i64, (Decimal, Decimal)>::new();
            for row in cip_groups.rows() {
                if let Some(aid) = gid(row.first()) {
                    let acc = gnum(row.get(1))?;
                    let set = gnum(row.get(2))?;
                    if !acc.is_zero() || !set.is_zero() {
                        let slot = cip_by_asset
                            .entry(aid)
                            .or_insert((Decimal::ZERO, Decimal::ZERO));
                        slot.0 += acc;
                        slot.1 += set;
                    }
                }
            }
            let mut cip_by_cat = BTreeMap::<Option<i64>, (Decimal, Decimal, Decimal)>::new();
            if !cip_by_asset.is_empty() {
                let cip_asset_ids: Vec<i64> = cip_by_asset.keys().copied().collect();
                let arows = asset_rec
                    .browse(cip_asset_ids)?
                    .read_rows(cx.tx(), &["id", "category_id"], Load::Raw)
                    .await?;
                for ar in arows.iter().map(Value::Row) {
                    let aid = int_field(&ar, "id")?;
                    let cat_id = opt_int_field(&ar, "category_id")?;
                    if let Some(&(acc, set)) = cip_by_asset.get(&aid) {
                        let uns = (acc - set).max(Decimal::ZERO);
                        let slot = cip_by_cat.entry(cat_id).or_insert((
                            Decimal::ZERO,
                            Decimal::ZERO,
                            Decimal::ZERO,
                        ));
                        slot.0 += acc;
                        slot.1 += set;
                        slot.2 += uns;
                    }
                }
            }

            // Derive Accumulated Depreciation strictly from posted depreciation lines <= `to`
            // (never `original_value - value_residual`, which misclassifies disposal write-offs as depreciation).
            let dep_groups = cx
                .env()
                .records("account.asset.depreciation.line")?
                ._read_group(
                    cx.tx(),
                    ReadGroup::new(cond(json!([
                        ["asset_id.company_id", "=", company],
                        ["asset_id.state", "!=", "draft"],
                        ["asset_id.acquisition_date", "<=", day(to)],
                        ["state", "=", "posted"],
                        ["depreciation_date", "<=", day(to)]
                    ]))?)
                    .groupby(["asset_id"])
                    .aggregates(["amount:sum"]),
                )
                .await?;
            let mut dep_by_asset = BTreeMap::<i64, Decimal>::new();
            for row in dep_groups.rows() {
                if let Some(aid) = gid(row.first()) {
                    let amt = gnum(row.get(1))?;
                    if !amt.is_zero() {
                        *dep_by_asset.entry(aid).or_insert(Decimal::ZERO) += amt;
                    }
                }
            }
            let mut dep_by_cat = BTreeMap::<Option<i64>, Decimal>::new();
            let mut active_dep_by_cat = BTreeMap::<Option<i64>, Decimal>::new();
            if !dep_by_asset.is_empty() {
                let dep_asset_ids: Vec<i64> = dep_by_asset.keys().copied().collect();
                let arows = asset_rec
                    .browse(dep_asset_ids)?
                    .read_rows(cx.tx(), &["id", "category_id", "state"], Load::Raw)
                    .await?;
                for ar in arows.iter().map(Value::Row) {
                    let aid = int_field(&ar, "id")?;
                    let cat_id = opt_int_field(&ar, "category_id")?;
                    let st = text_field(&ar, "state")?;
                    if let Some(&amt) = dep_by_asset.get(&aid) {
                        *dep_by_cat.entry(cat_id).or_insert(Decimal::ZERO) += amt;
                        if st != "disposed" {
                            *active_dep_by_cat.entry(cat_id).or_insert(Decimal::ZERO) += amt;
                        }
                    }
                }
            }

            let mut by_cat = BTreeMap::<Option<i64>, AssetTotals>::new();
            let mut active_orig_by_cat = BTreeMap::<Option<i64>, Decimal>::new();
            let mut cat_ids = BTreeSet::new();

            for row in grouped.rows() {
                let cat_id = gid(row.first());
                if let Some(cid) = cat_id {
                    cat_ids.insert(cid);
                }
                let atype = gtext(row.get(1))?;
                let state = gtext(row.get(2))?;
                let orig = gnum(row.get(3))?;

                let entry = by_cat.entry(cat_id).or_default();
                if atype != "cip" {
                    entry.gross += orig;
                    if state != "disposed" {
                        *active_orig_by_cat.entry(cat_id).or_insert(Decimal::ZERO) += orig;
                    }
                }
            }
            for (cat_id, (cip_acc, cip_set, cip_uns)) in cip_by_cat {
                if let Some(cid) = cat_id {
                    cat_ids.insert(cid);
                }
                let entry = by_cat.entry(cat_id).or_default();
                entry.gross += cip_acc;
                entry.settled += cip_set;
                entry.nbv += cip_uns;
            }
            if by_cat.len() > limit {
                return Err(invalid(format!(
                    "Asset & CIP Register exceeds reporting.max_screen_lines ({limit})."
                )));
            }

            let mut grand = AssetTotals::default();
            for (cat_id, totals) in &mut by_cat {
                let dep = dep_by_cat.get(cat_id).copied().unwrap_or(Decimal::ZERO);
                let active_orig = active_orig_by_cat
                    .get(cat_id)
                    .copied()
                    .unwrap_or(Decimal::ZERO);
                let active_dep = active_dep_by_cat
                    .get(cat_id)
                    .copied()
                    .unwrap_or(Decimal::ZERO);
                totals.depreciation = dep;
                totals.nbv += (active_orig - active_dep).max(Decimal::ZERO);

                grand.gross += totals.gross;
                grand.settled += totals.settled;
                grand.depreciation += totals.depreciation;
                grand.nbv += totals.nbv;
            }

            let mut cat_names = BTreeMap::<i64, String>::new();
            if !cat_ids.is_empty() {
                let crows = cx
                    .env()
                    .records("account.asset.category")?
                    .browse(cat_ids)?
                    .read_rows(cx.tx(), &["id", "code", "name"], Load::Raw)
                    .await?;
                for cr in crows.iter().map(Value::Row) {
                    let cid = int_field(&cr, "id")?;
                    let code = text_field(&cr, "code")?;
                    let name = text_field(&cr, "name")?;
                    let label = if code.is_empty() {
                        name
                    } else {
                        format!("{code} {name}")
                    };
                    cat_names.insert(cid, label);
                }
            }

            let root = LineId::root(report.id);
            let mut out = Vec::new();
            for (cat_id, totals) in by_cat {
                if options.hide_zero
                    && totals.gross.is_zero()
                    && totals.settled.is_zero()
                    && totals.depreciation.is_zero()
                    && totals.nbv.is_zero()
                {
                    continue;
                }
                let (label, seg_val) = match cat_id {
                    Some(cid) => (
                        cat_names
                            .get(&cid)
                            .cloned()
                            .unwrap_or_else(|| format!("Asset Class {cid}")),
                        cid.to_string(),
                    ),
                    None => ("Unclassified Assets".to_owned(), String::new()),
                };
                let lid = root.child(Segment::group(
                    "category_id",
                    "account.asset.category",
                    &seg_val,
                ));
                let mut line = Line::new(
                    lid,
                    label,
                    LineKind::Group,
                    build_cells(report, currency, scale, &totals),
                );
                line.unfoldable = true;
                out.push(line);
            }

            out.push(Line::new(
                root.child(Segment::total()),
                "Total Fixed Assets & CIP",
                LineKind::Total,
                build_cells(report, currency, scale, &grand),
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
            let AssetLineTarget::Category(cat_id) = locate(report, line)? else {
                return Err(invalid("Only Asset Class rows can be unfolded."));
            };
            let max_page = cx.bounds().max_screen_lines as u64;
            if page.limit == 0 || page.limit > max_page {
                return Err(invalid(format!(
                    "Page limit must be between 1 and {max_page}."
                )));
            }
            let company = options.companies[0];
            let to = options.period.to;
            let (currency, scale) = company_currency(cx, company).await?;

            let domain = match cat_id {
                Some(cid) => cond(json!([
                    ["company_id", "=", company],
                    ["state", "!=", "draft"],
                    ["acquisition_date", "<=", day(to)],
                    ["category_id", "=", cid]
                ]))?,
                None => cond(json!([
                    ["company_id", "=", company],
                    ["state", "!=", "draft"],
                    ["acquisition_date", "<=", day(to)],
                    ["category_id", "=", false]
                ]))?,
            };
            let rec = cx.env().records("account.asset")?;
            let total_count = rec.search_count(cx.tx(), &domain, None).await?;
            let found = rec
                .search_with(
                    cx.tx(),
                    &domain,
                    &SearchOptions::new()
                        .order("code, name, id")
                        .offset(page.offset)
                        .limit(page.limit),
                )
                .await?;
            let rows = found
                .read_rows(
                    cx.tx(),
                    &[
                        "id",
                        "code",
                        "name",
                        "asset_type",
                        "state",
                        "original_value",
                    ],
                    Load::Raw,
                )
                .await?;

            let page_ids: Vec<i64> = rows
                .iter()
                .map(Value::Row)
                .map(|r| int_field(&r, "id"))
                .collect::<Result<_>>()?;
            let mut dep_by_asset = BTreeMap::<i64, Decimal>::new();
            let mut cip_by_asset = BTreeMap::<i64, (Decimal, Decimal)>::new();
            if !page_ids.is_empty() {
                let dep_groups = cx
                    .env()
                    .records("account.asset.depreciation.line")?
                    ._read_group(
                        cx.tx(),
                        ReadGroup::new(cond(json!([
                            ["asset_id", "in", page_ids],
                            ["state", "=", "posted"],
                            ["depreciation_date", "<=", day(to)]
                        ]))?)
                        .groupby(["asset_id"])
                        .aggregates(["amount:sum"]),
                    )
                    .await?;
                for dr in dep_groups.rows() {
                    if let Some(aid) = gid(dr.first()) {
                        *dep_by_asset.entry(aid).or_insert(Decimal::ZERO) += gnum(dr.get(1))?;
                    }
                }
                let cip_groups = cx
                    .env()
                    .records("account.asset.cip.line")?
                    ._read_group(
                        cx.tx(),
                        ReadGroup::new(cond(json!([
                            ["asset_id", "in", page_ids],
                            ["date", "<=", day(to)]
                        ]))?)
                        .groupby(["asset_id"])
                        .aggregates(["amount:sum", "settled_amount:sum"]),
                    )
                    .await?;
                for cr in cip_groups.rows() {
                    if let Some(aid) = gid(cr.first()) {
                        let slot = cip_by_asset
                            .entry(aid)
                            .or_insert((Decimal::ZERO, Decimal::ZERO));
                        slot.0 += gnum(cr.get(1))?;
                        slot.1 += gnum(cr.get(2))?;
                    }
                }
            }

            let mut out = Vec::new();
            for row in rows.iter().map(Value::Row) {
                let aid = int_field(&row, "id")?;
                let code = text_field(&row, "code")?;
                let name = text_field(&row, "name")?;
                let atype = text_field(&row, "asset_type")?;
                let state = text_field(&row, "state")?;
                let label = if code.is_empty() {
                    name
                } else {
                    format!("{code} {name}")
                };
                let totals = if atype == "cip" {
                    let (acc, set) = cip_by_asset
                        .get(&aid)
                        .copied()
                        .unwrap_or((Decimal::ZERO, Decimal::ZERO));
                    AssetTotals {
                        kind_text: "CIP".to_owned(),
                        state_text: state,
                        gross: acc,
                        settled: set,
                        depreciation: Decimal::ZERO,
                        nbv: (acc - set).max(Decimal::ZERO),
                    }
                } else {
                    let orig = dec_field(&row, "original_value")?;
                    let dep = dep_by_asset.get(&aid).copied().unwrap_or(Decimal::ZERO);
                    let nbv = if state == "disposed" {
                        Decimal::ZERO
                    } else {
                        (orig - dep).max(Decimal::ZERO)
                    };
                    AssetTotals {
                        kind_text: "Fixed Asset".to_owned(),
                        state_text: state,
                        gross: orig,
                        settled: Decimal::ZERO,
                        depreciation: dep,
                        nbv,
                    }
                };
                let cid = line.child(Segment::group(
                    "asset_id",
                    "account.asset",
                    &aid.to_string(),
                ));
                let mut dline = Line::new(
                    cid,
                    label,
                    LineKind::Detail,
                    build_cells(report, currency, scale, &totals),
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
            match (locate(report, line)?, target) {
                (AssetLineTarget::Asset(aid), DrillTarget::Item(item)) if item == "open" => {
                    let company = options.companies[0];
                    let visible = cx
                        .env()
                        .records("account.asset")?
                        .search_count(
                            cx.tx(),
                            &cond(json!([["id", "=", aid], ["company_id", "=", company]]))?,
                            None,
                        )
                        .await?;
                    if visible == 0 {
                        return Err(invalid("Asset is not accessible in this company."));
                    }
                    Ok(json!({
                        "type": "ir.actions.act_window",
                        "name": "Asset / CIP",
                        "res_model": "account.asset",
                        "res_id": aid,
                        "target": "current",
                        "views": [[false, "form"]]
                    }))
                }
                _ => Err(invalid("Unsupported drill target on Asset & CIP Register.")),
            }
        })
    }
}

erp_addon_reporting::provider_declarations!(AssetRegisterReport);
