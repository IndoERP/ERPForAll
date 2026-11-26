// SPDX-License-Identifier: MIT OR Apache-2.0
//! Fixed Asset & CIP History Sheet dynamic report provider (`account_reporting.asset_register`):
//! caller-scoped ORM reads grouped by Asset Class -> Fixed Assets & CIP projects with
//! Gross / CIP Cost, CIP Settled, Accumulated Depreciation, and Net Book Value.

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

pub(crate) struct AssetRegisterReport;
const SPEC: ProviderSpec = ProviderSpec::new("account_reporting.asset_register", 1);

fn invalid(msg: impl Into<String>) -> Error {
    Error::validation(msg.into())
}

fn cond(value: Json) -> Result<Domain> {
    Ok(Domain::from_json(&value)?)
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
        [seg]
            if seg.kind() == &SegmentKind::Group("category_id".to_owned())
                && seg.model() == "account.asset.category" =>
        {
            match seg.value() {
                "" => Ok(AssetLineTarget::Category(None)),
                v => Ok(AssetLineTarget::Category(Some(
                    v.parse::<i64>()
                        .map_err(|_| invalid("Invalid asset class id."))?,
                ))),
            }
        }
        [_, seg]
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
                if !matches!(
                    col.label.as_str(),
                    "asset_type" | "state" | "gross" | "settled" | "depreciation" | "nbv"
                ) {
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
            let limit = cx.bounds().max_screen_lines;

            let asset_rec = cx.env().records("account.asset")?;
            let found = asset_rec
                .search_with(
                    cx.tx(),
                    &cond(json!([["company_id", "=", company]]))?,
                    &SearchOptions::new().order("id").limit(limit as u64 + 1),
                )
                .await?;
            if found.len() > limit {
                return Err(invalid(format!(
                    "Asset & CIP Register exceeds reporting.max_screen_lines ({limit})."
                )));
            }
            let rows = found
                .read_rows(
                    cx.tx(),
                    &[
                        "id",
                        "category_id",
                        "asset_type",
                        "original_value",
                        "value_residual",
                        "cip_accumulated_amount",
                        "cip_settled_amount",
                        "cip_unsettled_amount",
                    ],
                    Load::Raw,
                )
                .await?;

            let mut by_cat = BTreeMap::<Option<i64>, AssetTotals>::new();
            let mut cat_ids = Vec::new();
            let mut grand = AssetTotals::default();

            for row in rows.iter().map(Value::Row) {
                let cat_id = opt_int_field(&row, "category_id")?;
                if let Some(cid) = cat_id
                    && !cat_ids.contains(&cid)
                {
                    cat_ids.push(cid);
                }
                let atype = text_field(&row, "asset_type")?;
                let (gross, settled, dep, nbv) = if atype == "cip" {
                    let acc = dec_field(&row, "cip_accumulated_amount")?;
                    let set = dec_field(&row, "cip_settled_amount")?;
                    let uns = dec_field(&row, "cip_unsettled_amount")?;
                    (acc, set, Decimal::ZERO, uns)
                } else {
                    let orig = dec_field(&row, "original_value")?;
                    let res = dec_field(&row, "value_residual")?;
                    (orig, Decimal::ZERO, (orig - res).max(Decimal::ZERO), res)
                };
                let entry = by_cat.entry(cat_id).or_default();
                entry.gross += gross;
                entry.settled += settled;
                entry.depreciation += dep;
                entry.nbv += nbv;

                grand.gross += gross;
                grand.settled += settled;
                grand.depreciation += dep;
                grand.nbv += nbv;
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
                    lid.clone(),
                    label,
                    LineKind::Group,
                    build_cells(report, currency, scale, &totals),
                );
                line.unfoldable = true;
                line.unfolded = options.unfold_all || options.unfolded.contains(&lid);
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
            let company = options.companies[0];
            let (currency, scale) = company_currency(cx, company).await?;

            let domain = match cat_id {
                Some(cid) => cond(json!([
                    ["company_id", "=", company],
                    ["category_id", "=", cid]
                ]))?,
                None => cond(json!([
                    ["company_id", "=", company],
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
                        "value_residual",
                        "cip_accumulated_amount",
                        "cip_settled_amount",
                        "cip_unsettled_amount",
                    ],
                    Load::Raw,
                )
                .await?;

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
                    AssetTotals {
                        kind_text: "CIP".to_owned(),
                        state_text: state,
                        gross: dec_field(&row, "cip_accumulated_amount")?,
                        settled: dec_field(&row, "cip_settled_amount")?,
                        depreciation: Decimal::ZERO,
                        nbv: dec_field(&row, "cip_unsettled_amount")?,
                    }
                } else {
                    let orig = dec_field(&row, "original_value")?;
                    let res = dec_field(&row, "value_residual")?;
                    AssetTotals {
                        kind_text: "Fixed Asset".to_owned(),
                        state_text: state,
                        gross: orig,
                        settled: Decimal::ZERO,
                        depreciation: (orig - res).max(Decimal::ZERO),
                        nbv: res,
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
        _cx: &'a mut ReportCx<'_>,
        report: &'a ReportDef,
        _options: &'a Options,
        line: &'a LineId,
        target: &'a DrillTarget,
    ) -> BoxFuture<'a, Result<Json>> {
        Box::pin(async move {
            match (locate(report, line)?, target) {
                (AssetLineTarget::Asset(aid), DrillTarget::Item(item)) if item == "open" => {
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
