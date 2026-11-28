// SPDX-License-Identifier: MIT OR Apache-2.0
//! Bind supported best-seller questions to a verified selection before the provider can widen it.
use erp_fields::{DateTime, Error, Result};
use erp_sdk::ai::{ToolCall, ToolCallId};
use serde_json::{Value as Json, json};

#[derive(Clone, Copy)]
enum Period {
    All,
    Relative(i16),
    Year(i16),
}

pub(super) struct Ranking {
    limit: usize,
    period: Period,
    table: bool,
    indonesian: bool,
}

impl Ranking {
    pub(super) fn parse(question: &str) -> Result<Option<Self>> {
        let lower = question.to_lowercase();
        let words: Vec<_> = lower
            .split(|c: char| !c.is_alphanumeric())
            .filter(|word| !word.is_empty())
            .collect();
        let indonesian = words.contains(&"produk");
        let product = indonesian || words.iter().any(|w| matches!(*w, "product" | "products"));
        let ranking = words.iter().any(|w| {
            matches!(
                *w,
                "terlaris" | "tertinggi" | "terbanyak" | "top" | "bestselling"
            )
        }) || words
            .windows(2)
            .any(|w| matches!(w, ["best", "selling"] | ["paling", "laris" | "laku"]));
        // A request to save, publish or change this analysis belongs to the business action dispatcher.
        let action = words.iter().any(|w| {
            matches!(
                *w,
                "save" | "simpan" | "kirim" | "send" | "email" | "create" | "update" | "ubah"
            )
        });
        if !product || !ranking || action {
            return Ok(None);
        }
        let clarify = || {
            Error::validation(if indonesian {
                "Peringkat ini menghitung jumlah unit pada pesanan penjualan terkonfirmasi. Sebutkan jumlah produk dan satu tahun kalender, atau minta peringkat sepanjang waktu."
            } else {
                "This ranking counts units on confirmed sales orders. Specify the number of products and one calendar year, or request all time."
            })
        };
        if lower.split_whitespace().any(|w| {
            w.strip_prefix('-')
                .is_some_and(|n| n.starts_with(|c: char| c.is_ascii_digit()))
        }) {
            return Err(clarify());
        }
        let mut limit = None;
        let mut period = None;
        let mut table = false;
        let mut rest = words.as_slice();
        while !rest.is_empty() {
            let (n, range) = match rest {
                ["tahun", "ini", ..] | ["this", "year", ..] => (2, Some(Period::Relative(0))),
                ["this", "calendar", "year", ..] => (3, Some(Period::Relative(0))),
                ["tahun", "lalu", ..] | ["last", "year", ..] => (2, Some(Period::Relative(-1))),
                ["all", "time", ..] | ["sepanjang", "waktu", ..] => (2, Some(Period::All)),
                ["tahun" | "in" | "year", y, ..]
                    if y.len() == 4 && y.bytes().all(|b| b.is_ascii_digit()) =>
                {
                    (2, Some(Period::Year(y.parse().map_err(|_| clarify())?)))
                }
                _ => (0, None),
            };
            if let Some(range) = range {
                if period.replace(range).is_some() {
                    return Err(clarify());
                }
                rest = &rest[n..];
                continue;
            }
            let layout = match rest {
                ["in" | "as", "a", "table", ..] | ["dalam", "bentuk", "tabel", ..] => 3,
                ["dalam", "tabel", ..] => 2,
                _ => 0,
            };
            if layout > 0 {
                table = true;
                rest = &rest[layout..];
                continue;
            }
            let word = rest[0];
            if let Some(count) = match word {
                "satu" | "one" => Some(1),
                "dua" | "two" => Some(2),
                "tiga" | "three" => Some(3),
                "empat" | "four" => Some(4),
                "lima" | "five" => Some(5),
                "sepuluh" | "ten" => Some(10),
                n if n.bytes().all(|b| b.is_ascii_digit()) => {
                    Some(n.parse().map_err(|_| clarify())?)
                }
                _ => None,
            } {
                if count == 0 || limit.replace(count).is_some_and(|old| old != count) {
                    return Err(clarify());
                }
            } else if matches!(word, "table" | "tabel") {
                table = true;
            } else if let Some(length) = ranking_phrase(rest) {
                rest = &rest[length..];
                continue;
            } else {
                // Never answer an unimplemented qualifier with a broader selection.
                return Err(clarify());
            }
            rest = &rest[1..];
        }
        Ok(Some(Self {
            limit: limit.ok_or_else(clarify)?,
            period: period.unwrap_or(Period::All),
            table,
            indonesian,
        }))
    }

    pub(super) fn scoped_year(&self) -> bool {
        !matches!(self.period, Period::All)
    }

    pub(super) fn query(&self, zone: &str, now: DateTime) -> Result<ToolCall> {
        let mut domain = vec![
            json!(["state", "=", "sale"]),
            json!(["product_id", "!=", false]),
            json!(["is_downpayment", "=", false]),
        ];
        let year = match self.period {
            Period::All => None,
            Period::Year(year) => Some(year),
            Period::Relative(offset) => {
                let current = erp_fields::Date::context_today_at(zone, now)?.ymd().0;
                Some(
                    i16::try_from(current)
                        .map_err(|_| Error::validation("Calendar year out of range."))?
                        + offset,
                )
            }
        };
        if let Some(year) = year {
            let start = |year| {
                let civil = jiff::civil::DateTime::new(year, 1, 1, 0, 0, 0, 0)?;
                let timestamp = civil.in_tz(zone)?.timestamp();
                Ok::<_, jiff::Error>(DateTime::from_micros(timestamp.as_microsecond()).to_string())
            };
            let invalid =
                |_| Error::validation("The calendar year could not be resolved in your time zone.");
            domain.push(json!(["date_order", ">=", start(year).map_err(invalid)?]));
            domain.push(json!([
                "date_order",
                "<",
                start(year + 1).map_err(invalid)?
            ]));
        }
        aggregate_call(
            "best_sellers",
            json!({
                "model": "sale.order.line",
                "domain": domain,
                "groupby": ["product_id"],
                "aggregates": ["product_uom_qty:sum"],
                "order": "product_uom_qty:sum desc",
                "limit": self.limit,
            }),
        )
    }

    pub(super) fn check_limit(&self, max: usize) -> Result<()> {
        if self.limit > max {
            let reason = if self.indonesian {
                format!("Batas peringkat dan pencarian harga saat ini adalah {max} produk.")
            } else {
                format!("The current limit for ranking and price lookup is {max} products.")
            };
            return Err(Error::validation(reason));
        }
        Ok(())
    }

    /// Check units across the complete domain, before ordering products or taking the top N.
    pub(super) fn unit_query(&self, call: &ToolCall) -> Result<ToolCall> {
        let mut args = call.arguments().clone();
        args["groupby"] = json!(["product_uom_id"]);
        args["limit"] = json!(1);
        aggregate_call("ranking_units", args)
    }

    pub(super) fn verified_query(
        &self,
        base: &ToolCall,
        units: &Json,
        unit_id: i64,
    ) -> Result<ToolCall> {
        let malformed =
            || Error::invariant("The unit-of-measure check returned an invalid result.");
        let groups = units["groups"].as_array().ok_or_else(malformed)?;
        let truncated = units["truncated"].as_bool().ok_or_else(malformed)?;
        let units_only = match groups.as_slice() {
            [] => true,
            [group] => {
                let unit = &group["product_uom_id"];
                unit.as_i64().or_else(|| unit.get(0)?.as_i64()) == Some(unit_id)
            }
            _ => false,
        };
        if truncated || !units_only {
            return Err(Error::user(if self.indonesian {
                "Pesanan dalam periode ini memakai satuan selain Unit. Jumlah dalam kemasan, berat, atau satuan lain perlu dinormalisasi sebelum produk dapat diperingkat berdasarkan unit."
            } else {
                "These orders contain a unit of measure other than Units. Package, weight, or other quantities need to be normalized before products can be ranked by units."
            }));
        }
        // Keep later reads, saved analyses and source links on the unit we actually verified.
        let mut args = base.arguments().clone();
        args["domain"]
            .as_array_mut()
            .ok_or_else(malformed)?
            .push(json!(["product_uom_id", "=", unit_id]));
        aggregate_call("best_sellers", args)
    }

    pub(super) fn view(&self) -> &'static str {
        if self.table { "table" } else { "chart" }
    }
    pub(super) fn title(&self) -> &'static str {
        if self.indonesian {
            "Produk terlaris menurut kuantitas yang dipesan (satuan Unit)"
        } else {
            "Best-selling products by ordered quantity (Units)"
        }
    }
    pub(super) fn summary(&self, empty: bool) -> &'static str {
        match (self.indonesian, empty) {
            (true, true) => {
                "Tidak ada produk dengan satuan Unit pada pesanan penjualan terkonfirmasi untuk periode ini."
            }
            (false, true) => {
                "No products with the Units measure matched confirmed sales orders in this period."
            }
            (true, false) => {
                "Canvas mengurutkan produk menurut kuantitas pada pesanan penjualan terkonfirmasi. Satuan yang digunakan pada baris pesanan sudah diverifikasi sebagai Unit."
            }
            (false, false) => {
                "The Canvas ranks products by quantity on confirmed sales orders. Every matching order line was verified to use the Units measure."
            }
        }
    }
}

fn aggregate_call(id: &str, args: Json) -> Result<ToolCall> {
    let id = ToolCallId::new(id).map_err(|e| Error::programming(e.to_string()))?;
    ToolCall::new(id, "aggregate_records".into(), args)
        .map_err(|e| Error::programming(e.to_string()))
}

// Consume meaningful clauses as units. For example, "by units sold" is supported;
// consuming "by" and "orders" independently would misinterpret a ranking by order count.
fn ranking_phrase(words: &[&str]) -> Option<usize> {
    const PHRASES: &[&[&str]] = &[
        &["berdasarkan", "jumlah", "unit", "yang", "terjual"],
        &["berdasarkan", "jumlah", "unit", "terjual"],
        &["berdasarkan", "jumlah", "unit"],
        &["dengan", "jumlah", "unit", "terjual", "tertinggi"],
        &["dengan", "penjualan", "tertinggi"],
        &["dengan", "jumlah", "penjualan", "tertinggi"],
        &["dari", "baris", "pesanan", "penjualan", "terkonfirmasi"],
        &["dari", "pesanan", "penjualan", "terkonfirmasi"],
        &[
            "dari",
            "pesanan",
            "penjualan",
            "yang",
            "sudah",
            "dikonfirmasi",
        ],
        &["paling", "laris"],
        &["paling", "laku"],
        &["kelompokkan", "per", "produk"],
        &["urutkan", "dari", "terbanyak"],
        &["dan", "batasi"],
        &["by", "units", "sold"],
        &["by", "quantity", "sold"],
        &["by", "quantity"],
        &["by", "units"],
        &["on", "confirmed", "sales", "orders"],
        &["from", "confirmed", "sales", "orders"],
        &["best", "selling"],
    ];
    if let Some(phrase) = PHRASES.iter().find(|phrase| words.starts_with(phrase)) {
        return Some(phrase.len());
    }
    match words[0] {
        "coba" | "tolong" | "tampilkan" | "tunjukkan" | "berikan" | "buatkan" | "grafik"
        | "produk" | "terlaris" | "please" | "show" | "me" | "list" | "chart" | "plot" | "the"
        | "top" | "bestselling" | "products" | "product" => Some(1),
        _ => None,
    }
}

#[cfg(test)]
fn plan(question: &str, zone: &str, now: DateTime) -> Result<Option<ToolCall>> {
    Ranking::parse(question)?
        .map(|r| r.query(zone, now))
        .transpose()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn bestselling_units_exclude_drafts_and_bind_the_requested_local_year() {
        let now = DateTime::parse_odoo("2025-12-31 18:00:00").unwrap();
        for question in [
            "Coba tampilkan 3 produk dengan penjualan tertinggi tahun ini",
            "Tampilkan grafik 3 produk terlaris berdasarkan jumlah unit yang terjual dari pesanan penjualan terkonfirmasi tahun ini.",
            "Show the top 3 best-selling products by units sold this year.",
        ] {
            let call = plan(question, "Asia/Jakarta", now)
                .unwrap()
                .expect(question);
            let args = call.arguments();
            assert_eq!(call.name(), "aggregate_records");
            assert_eq!(args["model"], "sale.order.line");
            assert_eq!(args["groupby"], json!(["product_id"]));
            assert_eq!(args["aggregates"], json!(["product_uom_qty:sum"]));
            assert_eq!(args["order"], "product_uom_qty:sum desc");
            assert_eq!(args["limit"], 3);
            let domain = args["domain"].as_array().unwrap();
            for predicate in [
                json!(["state", "=", "sale"]),
                json!(["product_id", "!=", false]),
                json!(["is_downpayment", "=", false]),
                json!(["date_order", ">=", "2025-12-31 17:00:00"]),
                json!(["date_order", "<", "2026-12-31 17:00:00"]),
            ] {
                assert!(domain.contains(&predicate), "{question}: {predicate}");
            }
        }
    }

    #[test]
    fn units_must_match_across_the_full_selection_before_ranking() {
        let request = Ranking::parse("3 produk terlaris").unwrap().unwrap();
        let call = request.query("UTC", DateTime::now()).unwrap();
        let probe = request.unit_query(&call).unwrap();
        assert_eq!(probe.arguments()["domain"], call.arguments()["domain"]);
        assert_eq!(probe.arguments()["groupby"], json!(["product_uom_id"]));
        assert_eq!(probe.arguments()["limit"], 1);
        let unit = json!({"groups":[{"product_uom_id":[10,"Units"]}],"truncated":false});
        let ranked = request.verified_query(&call, &unit, 10).unwrap();
        assert!(
            ranked.arguments()["domain"]
                .as_array()
                .unwrap()
                .contains(&json!(["product_uom_id", "=", 10]))
        );
        for bad in [
            json!({"groups":[{"product_uom_id":[10,"Units"]}],"truncated":true}),
            json!({"groups":[{"product_uom_id":[11,"Dozen"]}],"truncated":false}),
            json!({"groups":[{"product_uom_id":false}],"truncated":false}),
        ] {
            assert!(request.verified_query(&call, &bad, 10).is_err());
        }
        assert!(request.title().contains("satuan Unit"));
        let english = Ranking::parse("top 3 products").unwrap().unwrap();
        assert!(english.title().contains("Units"));
        assert!(english.summary(false).contains("Units"));
        assert!(request.check_limit(2).is_err());
        assert!(request.check_limit(3).is_ok());
        for q in [
            "Show the top 3 products in a table",
            "Tampilkan 3 produk terlaris dalam tabel",
        ] {
            assert_eq!(Ranking::parse(q).unwrap().unwrap().view(), "table");
        }
    }

    #[test]
    fn all_time_ranking_and_unrelated_questions() {
        let now = DateTime::now();
        let call = plan("3 produk terlaris", "UTC", now).unwrap().unwrap();
        assert!(
            !call.arguments()["domain"]
                .to_string()
                .contains("date_order")
        );
        assert!(
            plan("Tampilkan harganya juga.", "UTC", now)
                .unwrap()
                .is_none()
        );
        assert!(
            plan("Chart all sales by currency", "UTC", now)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn explicit_and_previous_year_use_half_open_local_dates() {
        let now = DateTime::parse_odoo("2026-01-01 02:00:00").unwrap();
        for question in [
            "Three best-selling products in 2024",
            "Tampilkan tiga produk terlaris tahun lalu",
        ] {
            let query = plan(question, "America/New_York", now).unwrap().unwrap();
            let domain = query.arguments()["domain"].as_array().unwrap();
            assert!(domain.contains(&json!(["date_order", ">=", "2024-01-01 05:00:00"])));
            assert!(domain.contains(&json!(["date_order", "<", "2025-01-01 05:00:00"])));
        }
        assert!(
            plan("Save the top three products as a dashboard", "UTC", now)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn never_drop_unrecognized_ranking_constraints() {
        for question in [
            "3 produk terlaris minggu lalu",
            "Top 3 products by profit",
            "Tampilkan 3 produk terlaris di Bandung",
            "Top 0 best selling products",
            "Top -3 products by units sold",
            "Top 3 products by sales orders",
            "Top 3 products last year this year",
            "3 produk terlaris berdasarkan nilai penjualan",
            "Top 3 products for customer North",
            "Top 3 products in Q4 2024",
        ] {
            assert!(
                plan(question, "UTC", DateTime::now()).is_err(),
                "{question}"
            );
        }
    }
}
