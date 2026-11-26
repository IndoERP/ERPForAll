// SPDX-License-Identifier: MIT OR Apache-2.0
//! Addon `barcodes_gs1_nomenclature` for ERPForAll: GS1-128 Application Identifier parsing
//! and combined barcode splitting.

use std::collections::BTreeMap;

use erp_addon_barcodes::models::{BarcodeNomenclature, BarcodeRule};
use erp_sdk::Operation;
use erp_sdk::prelude::*;
use erp_sdk::serde_json;

erp_sdk::addon!(
    name = "barcodes_gs1_nomenclature",
    depends = ["base", "barcodes"]
);

#[extend(BarcodeNomenclature, addon = "barcodes_gs1_nomenclature")]
pub struct Gs1BarcodeNomenclature {
    /// Whether this nomenclature parses GS1-128 Application Identifiers.
    #[field(boolean, default = false, string = "Is GS1 Nomenclature")]
    pub is_gs1_nomenclature: bool,
    /// FNC1 group separator sequence used between variable-length GS1 elements.
    #[field(char, default = "\\x1D", string = "FNC1 Separator")]
    pub gs1_separator_fnc1: Option<Arc<str>>,
}

#[extend(BarcodeRule, addon = "barcodes_gs1_nomenclature")]
pub struct Gs1BarcodeRule {
    /// Related flag indicating whether the parent nomenclature uses GS1 rules.
    #[field(
        boolean,
        related = "barcode_nomenclature_id.is_gs1_nomenclature",
        string = "Is GS1 Nomenclature"
    )]
    pub is_gs1_nomenclature: bool,
    /// Semantic content type of the GS1 Application Identifier payload.
    #[field(
        selection,
        selection = [
            ("identifier", "Fixed Identifier"),
            ("numeric_identifier", "Numeric Identifier"),
            ("alpha", "Alphanumeric"),
            ("measure", "Measure"),
            ("date", "Date (YYMMDD)")
        ],
        default = "alpha",
        string = "GS1 Content Type"
    )]
    pub gs1_content_type: Option<Arc<str>>,
    /// Number of implied decimal places for GS1 measure values.
    #[field(integer, default = 0, string = "Decimal Places")]
    pub gs1_decimal_usage: i32,
}

/// One decoded GS1-128 element (Application Identifier + parsed value).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Gs1Element {
    /// Two-digit Application Identifier (for example `01`, `10`, `17`, `21`, `30`).
    pub ai: String,
    /// Business kind (`product`, `lot`, `serial`, `package`, `quantity`, `expiration_date`, `use_date`).
    pub kind: String,
    /// Parsed value string (ISO `YYYY-MM-DD` for date AIs, plain string/decimal otherwise).
    pub value: String,
}

/// Format a 6-digit GS1 date (`YYMMDD`) into an ISO date (`20YY-MM-DD`).
/// Day `00` maps to the last day of the month per GS1 General Specifications.
fn parse_gs1_date(raw: &str) -> Result<String> {
    if raw.len() != 6 || !raw.bytes().all(|b| b.is_ascii_digit()) {
        return Err(Error::validation(format!(
            "Invalid GS1 date '{raw}': expected 6 digits YYMMDD."
        )));
    }
    let yy: i32 = raw[0..2]
        .parse()
        .map_err(|_| Error::validation("Invalid GS1 year."))?;
    let mm: u32 = raw[2..4]
        .parse()
        .map_err(|_| Error::validation("Invalid GS1 month."))?;
    let mut dd: u32 = raw[4..6]
        .parse()
        .map_err(|_| Error::validation("Invalid GS1 day."))?;
    if !(1..=12).contains(&mm) {
        return Err(Error::validation(format!(
            "Invalid GS1 month '{mm:02}' in '{raw}'."
        )));
    }
    let year = 2000 + yy;
    let leap = (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0);
    let max_day = match mm {
        2 if leap => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    };
    if dd == 0 {
        dd = max_day;
    }
    if dd > max_day {
        return Err(Error::validation(format!(
            "Invalid GS1 day '{dd:02}' for {year}-{mm:02}."
        )));
    }
    Ok(format!("{year:04}-{mm:02}-{dd:02}"))
}

/// Parse a GS1-128 barcode string (either parenthesized human-readable `(01)...(10)...`
/// or raw FNC1 `\x1D`-delimited format) into structured elements.
///
/// # Errors
/// Returns `Validation` when the input exceeds 512 bytes, contains non-ASCII characters,
/// has malformed parentheses, or contains an unknown or invalid Application Identifier.
pub fn parse_gs1_elements(input: &str) -> Result<Vec<Gs1Element>> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    if trimmed.len() > 512 {
        return Err(Error::validation(
            "GS1 barcode exceeds maximum length of 512 bytes.",
        ));
    }
    if !trimmed.is_ascii() {
        return Err(Error::validation(
            "GS1 barcode must contain ASCII characters only.",
        ));
    }
    // Strip leading symbology identifier (`]C1`, `]e0`, `]d2`, `]Q3`) if present.
    let body = if trimmed.starts_with(']') {
        trimmed
            .get(3..)
            .ok_or_else(|| Error::validation("Truncated GS1 symbology identifier."))?
    } else {
        trimmed
    };

    if body.starts_with('(') {
        return parse_parenthesized_gs1(body);
    }
    parse_raw_gs1(body)
}

fn classify_ai(ai: &str, raw_value: &str) -> Result<Gs1Element> {
    let (kind, value) = match ai {
        "00" => ("package", raw_value.to_owned()),
        "01" | "02" => ("product", raw_value.to_owned()),
        "10" => ("lot", raw_value.to_owned()),
        "15" => ("use_date", parse_gs1_date(raw_value)?),
        "17" => ("expiration_date", parse_gs1_date(raw_value)?),
        "21" => ("serial", raw_value.to_owned()),
        "30" | "37" => {
            if raw_value.is_empty() || !raw_value.bytes().all(|b| b.is_ascii_digit()) {
                return Err(Error::validation(format!(
                    "Invalid GS1 quantity '{raw_value}' for AI ({ai})."
                )));
            }
            ("quantity", raw_value.to_owned())
        }
        other => {
            return Err(Error::validation(format!(
                "Unsupported GS1 Application Identifier '({other})'."
            )));
        }
    };
    Ok(Gs1Element {
        ai: ai.to_owned(),
        kind: kind.to_owned(),
        value,
    })
}

fn parse_parenthesized_gs1(body: &str) -> Result<Vec<Gs1Element>> {
    let mut elements = Vec::new();
    let mut rest = body;
    while !rest.is_empty() {
        let Some(after_open) = rest.strip_prefix('(') else {
            return Err(Error::validation(
                "Malformed parenthesized GS1 barcode: expected '('.",
            ));
        };
        let Some(close_idx) = after_open.find(')') else {
            return Err(Error::validation(
                "Malformed parenthesized GS1 barcode: missing ')'.",
            ));
        };
        let ai = &after_open[..close_idx];
        if ai.len() < 2 || ai.len() > 4 || !ai.bytes().all(|b| b.is_ascii_digit()) {
            return Err(Error::validation(format!(
                "Invalid GS1 Application Identifier '({ai})'."
            )));
        }
        let after_ai = &after_open[close_idx + 1..];
        let next_open = after_ai.find('(').unwrap_or(after_ai.len());
        let raw_val = after_ai[..next_open].trim_end_matches('\x1d');
        elements.push(classify_ai(ai, raw_val)?);
        rest = &after_ai[next_open..];
    }
    Ok(elements)
}

fn parse_raw_gs1(body: &str) -> Result<Vec<Gs1Element>> {
    let mut elements = Vec::new();
    let mut rest = body;
    while !rest.is_empty() {
        rest = rest.trim_start_matches('\x1d');
        if rest.is_empty() {
            break;
        }
        if rest.len() < 2 {
            return Err(Error::validation("Truncated GS1 Application Identifier."));
        }
        let ai = &rest[..2];
        let after_ai = &rest[2..];
        // Fixed-length 2-digit AIs per GS1 spec.
        let fixed_len = match ai {
            "00" => Some(18),
            "01" | "02" => Some(14),
            "11" | "13" | "15" | "17" => Some(6),
            _ => None,
        };
        let (val, next) = if let Some(len) = fixed_len {
            if after_ai.len() < len {
                return Err(Error::validation(format!(
                    "GS1 AI ({ai}) requires {len} characters."
                )));
            }
            (&after_ai[..len], &after_ai[len..])
        } else {
            let split = after_ai.find('\x1d').unwrap_or(after_ai.len());
            let next_start = if split < after_ai.len() {
                split + 1
            } else {
                split
            };
            (&after_ai[..split], &after_ai[next_start..])
        };
        elements.push(classify_ai(ai, val)?);
        rest = next;
    }
    Ok(elements)
}

/// Maximum raw byte length accepted by [`split_combined_barcode`] before splitting.
const MAX_COMBINED_BYTES: usize = 16 * 1024;

/// Split a combined barcode scan using the configured separator (non-empty chunks, bounded to 64 parts).
///
/// # Errors
/// Returns `Validation` when the separator is empty, input exceeds 16 KiB, or more than 64
/// combined parts are supplied.
pub fn split_combined_barcode(input: &str, separator: &str) -> Result<Vec<String>> {
    if separator.is_empty() {
        return Err(Error::validation("Barcode separator cannot be empty."));
    }
    if input.len() > MAX_COMBINED_BYTES {
        return Err(Error::validation(
            "Combined barcode exceeds maximum length of 16 KiB.",
        ));
    }
    let mut parts = Vec::new();
    for part in input
        .split(separator)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        if parts.len() >= 64 {
            return Err(Error::validation(
                "Combined barcode exceeds the limit of 64 segments.",
            ));
        }
        parts.push(part.to_owned());
    }
    Ok(parts)
}

#[methods(BarcodeNomenclature, addon = "barcodes_gs1_nomenclature")]
impl Gs1BarcodeNomenclature {
    /// Parse a GS1-128 barcode string into JSON elements `{ "ai", "kind", "value" }`.
    #[method]
    #[rpc(effect = read)]
    pub async fn parse_gs1_barcode(
        this: &RecordSet<BarcodeNomenclature>,
        tx: &mut Transaction,
        code: String,
    ) -> Result<String> {
        this.as_records().check_access(tx, Operation::Read).await?;
        let elements = parse_gs1_elements(&code)?;
        let out: Vec<serde_json::Value> = elements
            .into_iter()
            .map(|el| {
                serde_json::json!({
                    "ai": el.ai,
                    "kind": el.kind,
                    "value": el.value,
                })
            })
            .collect();
        serde_json::to_string(&out)
            .map_err(|e| Error::internal(format!("GS1 serialization failed: {e}")))
    }

    /// Split a combined barcode scan by `separator` (defaults to `,`) and parse each segment.
    #[method]
    #[rpc(effect = read)]
    pub async fn parse_combined_barcode(
        this: &RecordSet<BarcodeNomenclature>,
        tx: &mut Transaction,
        code: String,
        separator: Option<String>,
    ) -> Result<String> {
        this.as_records().check_access(tx, Operation::Read).await?;
        let sep = separator
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or(",");
        let segments = split_combined_barcode(&code, sep)?;
        let mut results: Vec<BTreeMap<String, serde_json::Value>> =
            Vec::with_capacity(segments.len());
        for segment in segments {
            let mut entry = BTreeMap::new();
            entry.insert("raw".into(), serde_json::Value::String(segment.clone()));
            let looks_like_gs1 = segment.starts_with('(')
                || segment.starts_with("]C1")
                || segment.starts_with("]d2")
                || segment.starts_with("]e0")
                || segment.starts_with("]Q3");
            if looks_like_gs1 {
                let gs1 = parse_gs1_elements(&segment)?;
                if !gs1.is_empty() {
                    let items: Vec<serde_json::Value> = gs1
                        .into_iter()
                        .map(|e| serde_json::json!({"ai": e.ai, "kind": e.kind, "value": e.value}))
                        .collect();
                    entry.insert("gs1".into(), serde_json::Value::Array(items));
                }
            }
            results.push(entry);
        }
        serde_json::to_string(&results)
            .map_err(|e| Error::internal(format!("Combined barcode serialization failed: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_parenthesized_gs1_128_gtin_lot_expiry_qty() {
        let elements = parse_gs1_elements("(01)01234567890128(10)LOT-2026-A(17)261231(30)12")
            .expect("valid GS1");
        assert_eq!(elements.len(), 4);
        assert_eq!(elements[0].kind, "product");
        assert_eq!(elements[0].value, "01234567890128");
        assert_eq!(elements[1].kind, "lot");
        assert_eq!(elements[1].value, "LOT-2026-A");
        assert_eq!(elements[2].kind, "expiration_date");
        assert_eq!(elements[2].value, "2026-12-31");
        assert_eq!(elements[3].kind, "quantity");
        assert_eq!(elements[3].value, "12");
    }

    #[test]
    fn parses_raw_fnc1_gs1_with_day_zero_end_of_month() {
        let elements =
            parse_gs1_elements("]C101012345678901281726020021SER-001").expect("valid raw GS1");
        assert_eq!(elements.len(), 3);
        assert_eq!(elements[0].value, "01234567890128");
        assert_eq!(elements[1].value, "2026-02-28");
        assert_eq!(elements[2].kind, "serial");
        assert_eq!(elements[2].value, "SER-001");
    }

    #[test]
    fn splits_combined_barcodes_with_configured_separator() {
        let parts = split_combined_barcode("PROD-01, LOT-01, 5", ",").expect("split");
        assert_eq!(parts, vec!["PROD-01", "LOT-01", "5"]);
    }
}
