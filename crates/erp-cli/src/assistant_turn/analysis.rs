// SPDX-License-Identifier: MIT OR Apache-2.0
//! Compose quantitative outputs only from this turn's authorized, bounded reads.
use erp_addon_ai_base::canvas;
use erp_fields::{Error, Result};
use erp_sdk::ai::{LabelledData, ToolCall};
use serde_json::{Value as Json, json};
use std::collections::HashSet;

#[derive(Clone, Default)]
struct Read {
    data: Option<LabelledData>,
    domain: Json,
    failure: Option<String>,
}

impl Read {
    fn selects(&self, model: Option<&str>, domain: &Json) -> bool {
        // Both tools admit conjunctions only. Their order does not change the selection.
        fn conditions(domain: &Json) -> Option<Vec<String>> {
            let all = json!(["id", ">", 0]);
            let mut conditions: Vec<_> = domain
                .as_array()?
                .iter()
                .filter(|v| **v != all)
                .map(Json::to_string)
                .collect();
            conditions.sort_unstable();
            conditions.dedup();
            Some(conditions)
        }
        self.data.as_ref().is_some_and(|data| {
            data.source().model() == model
                && self.domain.is_array()
                && conditions(&self.domain) == conditions(domain)
        })
    }
}

fn chart_title(result: &Json) -> String {
    // Name the measure actually plotted. The source model is already shown in the provenance.
    let label = |field: &str| field.trim_end_matches("_id").replace('_', " ");
    let Some(measure) = result["aggregates"][0].as_str() else {
        return "Chart".into();
    };
    let mut title = match measure.split_once(':') {
        Some(("id", "count")) => "Record count".to_owned(),
        Some((field, operation)) => format!("{} ({operation})", label(field)),
        None => label(measure),
    };
    if let Some(first) = title.get_mut(..1) {
        first.make_ascii_uppercase();
    }
    let groups: Vec<_> = result["groupby"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Json::as_str)
        .map(|field| label(field).replace(':', " · "))
        .collect();
    if !groups.is_empty() {
        title.push_str(" by ");
        title.push_str(&groups.join(" / "));
    }
    title.truncate(title.floor_char_boundary(256));
    title
}

pub(super) struct Analysis {
    wants_chart: bool,
    skip_chart: bool,
    wants_details: bool,
    dimension_terms: HashSet<String>,
    // A later drill-down (e.g. by day) must not displace a requested grouping (e.g. by currency).
    preferred_group: Option<(usize, Read)>,
    aggregate: Read,
    search: Read,
    last_search: bool,
    pub(super) cards: Vec<Json>,
    bytes: usize,
}

impl Analysis {
    pub(super) fn new(question: &str) -> Self {
        let words: Vec<_> = question
            .split(|c: char| !c.is_alphanumeric())
            .filter(|word| !word.is_empty())
            .map(str::to_lowercase)
            .collect();
        let phrase = format!(" {} ", words.join(" "));
        let mentions = |choices: &[&str]| {
            choices
                .iter()
                .any(|choice| phrase.contains(&format!(" {choice} ")))
        };
        let compares_data = mentions(&[
            "trend",
            "trends",
            "tren",
            "bandingkan",
            "perbandingan",
            "compare",
            "comparison",
        ]) && mentions(&[
            "sales",
            "orders",
            "revenue",
            "purchases",
            "stock",
            "inventory",
            "profit",
            "costs",
            "pipeline",
            "penjualan",
            "omzet",
            "pendapatan",
            "pembelian",
            "stok",
            "laba",
            "biaya",
            "pesanan",
        ]);
        let skip_chart = mentions(&[
            "no chart",
            "without a chart",
            "tanpa grafik",
            "text only",
            "teks saja",
        ]);
        let wants_chart = !skip_chart
            && (compares_data
                || mentions(&[
                    "chart",
                    "graph",
                    "plot",
                    "grafik",
                    "grafikkan",
                    "grafiknya",
                    "grafikkanlah",
                    "gambarkan grafik",
                    "diagram batang",
                    "diagram garis",
                ]))
            && !mentions(&[
                "chart of accounts",
                "flow chart",
                "bagan akun",
                "flowchart",
                "hypothetical",
                "what if",
                "simulasi",
            ]);
        let wants_details = mentions(&[
            "underlying records",
            "records behind",
            "supporting records",
            "include the records",
            "include records",
            "records behind the trend",
            "record pendukung",
            "transaksi pendukung",
            "rincian transaksi",
            "detail transaksi",
            "data pendukung",
        ]);
        let dimension_terms = words
            .into_iter()
            .map(|term| match term.as_str() {
                "uang" | "currencies" => "currency".to_owned(),
                "monthly" | "bulan" | "bulanan" => "month".to_owned(),
                "daily" | "hari" | "harian" => "day".to_owned(),
                "yearly" | "tahun" | "tahunan" => "year".to_owned(),
                _ => term,
            })
            .collect();
        Self {
            wants_chart,
            skip_chart,
            wants_details,
            dimension_terms,
            preferred_group: None,
            aggregate: Read::default(),
            search: Read::default(),
            last_search: false,
            cards: Vec::new(),
            bytes: 0,
        }
    }

    pub(super) fn read(
        &mut self,
        call: &ToolCall,
        result: std::result::Result<&LabelledData, &Error>,
    ) {
        let slot = match call.name() {
            "aggregate_records" => {
                self.last_search = false;
                &mut self.aggregate
            }
            "search_records" => {
                self.last_search = true;
                &mut self.search
            }
            _ => return,
        };
        *slot = match result {
            Ok(source) => Read {
                data: Some(source.clone()),
                domain: call.arguments()["domain"].clone(),
                failure: None,
            },
            Err(error) => Read {
                failure: Some(match error.kind() {
                    erp_fields::ErrorKind::Access => {
                        "Access to the requested data was denied.".into()
                    }
                    erp_fields::ErrorKind::Validation | erp_fields::ErrorKind::UserError => {
                        error.message().to_owned()
                    }
                    _ => "The ERP query could not be completed.".into(),
                }),
                ..Read::default()
            },
        };
        if call.name() == "aggregate_records" && self.wants_chart {
            let Some(result) = self.aggregate.data.as_ref() else {
                self.preferred_group = None;
                return;
            };
            let score = result.value()["groupby"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Json::as_str)
                .filter(|field| {
                    let dimension = field.split_once(':').map_or(*field, |(_, grain)| grain);
                    dimension
                        .split('_')
                        .any(|word| word != "id" && self.dimension_terms.contains(word))
                })
                .count();
            if score > 0
                && self
                    .preferred_group
                    .as_ref()
                    .is_none_or(|(best, _)| score >= *best)
            {
                self.preferred_group = Some((score, self.aggregate.clone()));
            }
        }
    }

    fn chart_read(&self) -> &Read {
        self.preferred_group
            .as_ref()
            .map_or(&self.aggregate, |(_, read)| read)
    }

    fn details_match(&self) -> bool {
        if let Some(card) = self
            .cards
            .iter()
            .rev()
            .find(|card| card["canvas_type"] == "chart")
        {
            let data = &card["data"];
            return self
                .search
                .selects(data["source_model"].as_str(), &data["source_domain"]);
        }
        let primary = self.chart_read();
        primary
            .data
            .as_ref()
            .is_none_or(|data| self.search.selects(data.source().model(), &primary.domain))
    }

    pub(super) fn render(&mut self, call: &ToolCall) -> Result<()> {
        let kind = call.arguments()["canvas_type"].as_str();
        let source = match kind {
            Some("table") if self.wants_details => {
                if self.wants_chart
                    && self.aggregate.data.is_none()
                    && self.aggregate.failure.is_none()
                {
                    return Err(Error::validation(
                        "Run the chart query first so its supporting records can be matched.",
                    ));
                }
                if !self.details_match() {
                    return Err(Error::validation(
                        "Search the same ERP model with the chart filters for the supporting records.",
                    ));
                }
                self.search.data.as_ref()
            }
            Some("table") if self.last_search => self.search.data.as_ref(),
            _ => self.aggregate.data.as_ref(),
        };
        self.push(canvas::grounded_canvas_payload(call, source)?)
    }

    fn push(&mut self, payload: Json) -> Result<()> {
        let size = serde_json::to_vec(&payload)
            .map_err(|_| Error::invariant("Could not size the canvas."))?
            .len();
        if self.cards.len() >= 12 || self.bytes + size > 524_288 {
            return Err(Error::validation(
                "This turn is limited to 12 canvas outputs and 512 KiB combined. Narrow the data or start a follow-up.",
            ));
        }
        self.bytes += size;
        self.cards.push(payload);
        Ok(())
    }

    fn has(&self, kind: &str) -> bool {
        self.cards.iter().any(|card| card["canvas_type"] == kind)
    }

    pub(super) fn correction(&self) -> Option<&'static str> {
        let missing_chart = self.wants_chart
            && !self.has("chart")
            && self.aggregate.data.is_none()
            && self.aggregate.failure.is_none();
        let missing_records = self.wants_details
            && !self.has("table")
            && self.search.failure.is_none()
            && (self.search.data.is_none() || !self.details_match());
        match (missing_chart, missing_records) {
            (true, true) => Some(
                "Run aggregate_records for the requested chart and search_records for its supporting rows. Use the same model and filters for both.",
            ),
            (true, false) => Some(
                "Run aggregate_records for the requested chart, grouped by the requested dimension.",
            ),
            (false, true) => Some(
                "Run search_records for the supporting rows, using the chart's model and the same filters.",
            ),
            _ => None,
        }
    }

    /// Return a separate, visible reason for each requested output we could not build.
    pub(super) fn complete(&mut self) -> Vec<String> {
        let grouped = self.chart_read().data.as_ref().is_some_and(|d| {
            d.value()["groupby"]
                .as_array()
                .is_some_and(|g| !g.is_empty())
        });
        let many = self
            .search
            .data
            .as_ref()
            .is_some_and(|d| d.value()["records"].as_array().is_some_and(|r| r.len() > 1));
        let automatic = self.cards.is_empty();
        let chart = self.wants_chart || (!self.skip_chart && automatic && grouped);
        let catalog_prices = self
            .search
            .data
            .as_ref()
            .is_some_and(|data| data.value()["price_basis"].is_string());
        let table =
            self.wants_details || catalog_prices || (automatic && many && self.details_match());
        let mut notices = Vec::new();
        for (kind, needed, label) in [
            ("chart", chart, "Chart"),
            (
                "table",
                table,
                if catalog_prices {
                    "Product catalog prices"
                } else {
                    "Supporting records"
                },
            ),
        ] {
            if !needed || self.has(kind) {
                continue;
            }
            let read = if kind == "chart" {
                self.chart_read()
            } else {
                &self.search
            };
            let output = if let Some(data) = &read.data {
                if kind == "table" && !self.details_match() {
                    Err(Error::validation(
                        "The record search did not match the chart filters.",
                    ))
                } else {
                    let title = if kind == "chart" {
                        chart_title(data.value())
                    } else {
                        label.to_owned()
                    };
                    canvas::analytical_output(kind, &title, data)
                }
            } else {
                Err(Error::validation(
                    read.failure
                        .as_deref()
                        .unwrap_or("No matching ERP query returned data."),
                ))
            };
            if let Err(error) = output.and_then(|card| self.push(card)) {
                notices.push(format!("{label}: {}", error.message()));
            }
        }
        notices
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use erp_sdk::ai::{DataSource, ToolCallId, ToolName};

    fn read(analysis: &mut Analysis, tool: &str, model: &str, domain: Json, result: Json) {
        let ids = result["records"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|r| r["id"].as_i64().unwrap())
            .collect();
        let call = ToolCall::new(
            ToolCallId::new("read1").unwrap(),
            tool.into(),
            json!({"domain":domain}),
        )
        .unwrap();
        let source =
            DataSource::new(ToolName::new(tool).unwrap(), Some(model.into()), ids).unwrap();
        analysis.read(&call, Ok(&LabelledData::new(source, result)));
    }
    fn aggregate(analysis: &mut Analysis, groups: Json) {
        read(
            analysis,
            "aggregate_records",
            "sale.order",
            json!([["state", "=", "sale"]]),
            json!({"groups":groups,"groupby":["currency_id"],"aggregates":["amount_total:sum","id:count"],
                "query":{"model":"sale.order","domain":[["state","=","sale"]]},"truncated":false}),
        );
    }
    fn chart_and_details() -> Analysis {
        Analysis::new("Chart all sales by currency and include the records behind the trend")
    }
    #[test]
    fn markdown_final_after_two_reads_produces_chart_and_matching_order_rows() {
        let mut output = chart_and_details();
        assert!(output.correction().is_some());
        let rows: Vec<_>=(1..=17).map(|id|json!({"id":id,"currency":"USD","amount":if id==17 {"5047.92"} else {"3000.00"}})).collect();
        read(
            &mut output,
            "search_records",
            "sale.order",
            json!([["state", "=", "sale"], ["id", ">", 0]]),
            json!({"records":rows,"truncated":false}),
        );
        aggregate(
            &mut output,
            json!([{"currency_id":[1,"USD"],"amount_total:sum":"53047.92","id:count":17}]),
        );
        assert!(output.correction().is_none());
        assert!(output.complete().is_empty());
        assert_eq!(output.cards.len(), 2);
        assert_eq!(output.cards[0]["title"], "Amount total (sum) by currency");
        assert_eq!(
            output.cards[0]["data"]["series"],
            json!([{"label":"USD","value":53047.92}])
        );
        assert_eq!(output.cards[1]["data"]["records"], json!(rows));
        assert_eq!(
            output.cards[0]["data"]["records"][0]["amount_total:sum"],
            "53047.92"
        );
    }
    #[test]
    fn mismatched_detail_query_or_refusal_preserves_only_the_valid_chart() {
        let mut output = chart_and_details();
        aggregate(
            &mut output,
            json!([{"currency_id":"USD","amount_total:sum":23},{"currency_id":"EUR","amount_total:sum":25}]),
        );
        read(
            &mut output,
            "search_records",
            "sale.order",
            json!([["state", "=", "draft"]]),
            json!({"records":[{"id":4}]}),
        );
        assert!(output.correction().is_some());
        assert!(output.complete()[0].contains("filters"));
        assert_eq!(output.cards.len(), 1);
        assert_eq!(
            output.cards[0]["data"]["series"].as_array().unwrap().len(),
            2
        );
        let call = ToolCall::new(
            ToolCallId::new("failed").unwrap(),
            "search_records".into(),
            json!({}),
        )
        .unwrap();
        output.read(&call, Err(&Error::access("refused")));
        assert!(output.correction().is_none());
        assert!(output.complete()[0].contains("denied"));
        assert_eq!(output.cards.len(), 1);
    }
    #[test]
    fn simple_lookup_stays_in_chat_and_grouped_or_many_row_answers_get_a_canvas() {
        let mut output = Analysis::new("What is the status of SO001?");
        read(
            &mut output,
            "search_records",
            "sale.order",
            json!([]),
            json!({"records":[{"id":1}]}),
        );
        assert!(output.complete().is_empty());
        assert!(output.cards.is_empty());
        read(
            &mut output,
            "search_records",
            "sale.order",
            json!([]),
            json!({"records":[{"id":1},{"id":2}]}),
        );
        assert!(output.complete().is_empty());
        assert_eq!(output.cards[0]["canvas_type"], "table");
        let mut grouped = Analysis::new("Bandingkan omzet per mata uang");
        assert!(grouped.correction().is_some());
        aggregate(&mut grouped, json!([]));
        assert!(grouped.complete().is_empty());
        assert_eq!(grouped.cards[0]["data"]["series"], json!([]));
        let mut simple = Analysis::new("Ringkas tren penjualan tanpa grafik");
        aggregate(&mut simple, json!([]));
        assert!(simple.correction().is_none());
        assert!(simple.complete().is_empty());
        assert!(simple.cards.is_empty());
    }
    #[test]
    fn a_follow_up_drill_down_does_not_replace_the_requested_breakdown() {
        let mut output = chart_and_details();
        aggregate(
            &mut output,
            json!([{"currency_id":[1,"USD"],"amount_total:sum":"53047.92"}]),
        );
        read(
            &mut output,
            "aggregate_records",
            "sale.order",
            json!([["state", "=", "sale"]]),
            json!({"groups":[{"date_order:day":"2026-11-17","id:count":3}],"groupby":["date_order:day"],
            "aggregates":["id:count"],"query":{"domain":[["state","=","sale"]]}}),
        );
        read(
            &mut output,
            "search_records",
            "sale.order",
            json!([["state", "=", "sale"]]),
            json!({"records":[{"id":1}],"truncated":true}),
        );
        assert!(output.complete().is_empty());
        assert_eq!(
            output.cards[0]["data"]["series"],
            json!([{"label":"USD","value":53047.92}])
        );
        assert_eq!(output.cards[1]["data"]["truncated"], true);
    }

    #[test]
    fn a_missing_query_reports_each_requested_output_and_never_plots_chat_text() {
        let mut output = chart_and_details();
        assert_eq!(output.complete().len(), 2);
        assert!(output.cards.is_empty());
    }
    #[test]
    fn explicit_card_after_detail_read_uses_the_aggregate_without_duplication() {
        let mut output = chart_and_details();
        aggregate(
            &mut output,
            json!([{"currency_id":"USD","amount_total:sum":"0.00"}]),
        );
        read(
            &mut output,
            "search_records",
            "sale.order",
            json!([["state", "=", "sale"]]),
            json!({"records":[]}),
        );
        let call = ToolCall::new(
            ToolCallId::new("chart1").unwrap(),
            "render_canvas".into(),
            json!({"canvas_type":"chart","title":"Sales","description":"","data_json":"{}"}),
        )
        .unwrap();
        output.render(&call).unwrap();
        assert!(output.complete().is_empty());
        assert_eq!(output.cards.len(), 2);
        assert_eq!(output.cards[0]["title"], "Sales");
        assert_eq!(output.cards[1]["data"]["records"], json!([]));
    }
}
