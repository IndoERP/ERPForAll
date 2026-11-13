// SPDX-License-Identifier: MIT OR Apache-2.0
//! Pure company-map storage utilities.
//!
//! These functions perform no authorization or database access. The ORM must validate the
//! caller's company, field access, precision and HTML policy before using them. They never
//! accept a map as a scalar value. Exact decimals use JSON strings, not binary floats.

use std::sync::Arc;

use erp_fields::{Cell, Date, DateTime, Decimal, Error, FieldType, Result, Storage, Value};
use serde_json::{Map, Value as Json};

use crate::CompanyId;

/// Maximum entries in one stored company map; checked before cloning it.
pub const MAX_COMPANIES: usize = 65_536;
/// Maximum combined key and scalar payload bytes, conservatively charging 64 bytes per entry.
pub const MAX_MAP_BYTES: usize = 16 * 1024 * 1024;

fn invalid(ty: &FieldType) -> Error {
    Error::programming(format!(
        "invalid company-dependent {} storage value",
        ty.type_name()
    ))
}

/// Whether a scalar kind has a company-map codec. Other kinds must refuse at declaration.
pub fn supports(ty: &FieldType) -> bool {
    matches!(
        ty,
        FieldType::Char { .. }
            | FieldType::Text
            | FieldType::Html
            | FieldType::Boolean
            | FieldType::Integer
            | FieldType::Float { .. }
            | FieldType::Monetary { .. }
            | FieldType::Selection { .. }
            | FieldType::Many2one { .. }
            | FieldType::Date
            | FieldType::Datetime
    )
}

/// Encode an already admitted scalar. Null numeric/boolean cells normalize to their falsy value.
///
/// # Errors
/// Programming for unsupported kinds, cell shapes or noncanonical temporal values;
/// Validation for non-finite floats and oversized strings.
pub fn encode(ty: &FieldType, cell: &Cell) -> Result<Json> {
    if !supports(ty) || !cell.fits(ty.storage()) {
        return Err(invalid(ty));
    }
    match cell {
        Cell::Null => Ok(match ty.storage() {
            Storage::Bool => Json::Bool(false),
            Storage::Int => Json::from(0),
            Storage::Float => Json::from(0.0),
            Storage::Decimal => Json::String("0".into()),
            _ => Json::Null,
        }),
        Cell::Float(value) if !value.is_finite() => {
            Err(Error::validation("company-dependent float must be finite"))
        }
        Cell::Id(id) if *id <= 0 => Err(invalid(ty)),
        Cell::Decimal(value) => Ok(Json::String(value.normalize().to_string())),
        Cell::Date(date) => encode_temporal(ty, cell, date.to_string()),
        Cell::DateTime(datetime) => {
            let microseconds = datetime.time().3;
            let text = if microseconds == 0 {
                datetime.to_string()
            } else {
                format!("{datetime}.{microseconds:06}")
            };
            encode_temporal(ty, cell, text)
        }
        Cell::Str(value) if value.len() > MAX_MAP_BYTES => Err(Error::validation(
            "company-dependent scalar exceeds the map byte limit",
        )),
        _ => erp_fields::convert::value_to_json(&Value::from_cell(cell)),
    }
}

fn encode_temporal(ty: &FieldType, cell: &Cell, text: String) -> Result<Json> {
    let encoded = Json::String(text);
    // Refuse dates outside the canonical four-digit-year storage range.
    if decode(ty, &encoded)? != *cell {
        return Err(invalid(ty));
    }
    Ok(encoded)
}

/// Decode a persisted scalar without permissive dynamic-input coercions or precision loss.
///
/// # Errors
/// Programming for malformed storage; Validation for a scalar exceeding the byte limit.
/// Errors never include the raw stored value.
pub fn decode(ty: &FieldType, value: &Json) -> Result<Cell> {
    if !supports(ty) {
        return Err(invalid(ty));
    }
    if let Json::String(text) = value
        && text.len() > MAX_MAP_BYTES
    {
        return Err(Error::validation(
            "company-dependent scalar exceeds the map byte limit",
        ));
    }
    if value.is_null() {
        return Ok(Cell::Null);
    }
    let result = match (ty.storage(), value) {
        (Storage::Bool, Json::Bool(v)) => Some(Cell::Bool(*v)),
        (Storage::Int, Json::Number(v)) => v
            .as_i64()
            .and_then(|n| i32::try_from(n).ok())
            .map(Cell::Int),
        (Storage::Id, Json::Number(v)) => v.as_i64().filter(|v| *v > 0).map(Cell::Id),
        (Storage::Float, Json::Number(v)) => v.as_f64().filter(|v| v.is_finite()).map(Cell::Float),
        (Storage::Decimal, Json::String(v)) => Decimal::from_str_exact(v).ok().map(Cell::Decimal),
        // Integer JSON values are lossless legacy numeric inputs. Fractional JSON numbers
        // must be migrated through PostgreSQL text, never rounded through serde's f64.
        (Storage::Decimal, Json::Number(v)) => v.as_i64().map(|v| Cell::Decimal(v.into())),
        (Storage::Str, Json::String(v)) => Some(Cell::Str(Arc::from(v.as_str()))),
        (Storage::Date, Json::String(v)) => Date::parse_odoo(v)
            .filter(|date| date.to_string() == *v)
            .map(Cell::Date),
        (Storage::DateTime, Json::String(v)) => decode_datetime(v).map(Cell::DateTime),
        _ => None,
    };
    result.ok_or_else(|| invalid(ty))
}

fn decode_datetime(text: &str) -> Option<DateTime> {
    let (seconds, micros) = match text.len() {
        19 => (text, 0),
        26 if text.as_bytes()[19] == b'.' => {
            let fraction = text.get(20..)?;
            if !fraction.bytes().all(|byte| byte.is_ascii_digit()) {
                return None;
            }
            (text.get(..19)?, fraction.parse::<i64>().ok()?)
        }
        _ => return None,
    };
    let value = DateTime::parse_odoo(seconds)?;
    if value.to_string() != seconds {
        return None;
    }
    value
        .micros()
        .checked_add(micros)
        .map(DateTime::from_micros)
}

fn checked_map(cell: &Cell) -> Result<Option<&Map<String, Json>>> {
    let map = match cell {
        Cell::Null => return Ok(None),
        Cell::Json(value) => value
            .as_object()
            .ok_or_else(|| Error::programming("company-dependent storage must be a JSON object"))?,
        _ => {
            return Err(Error::programming(
                "company-dependent cache must hold a JSON map",
            ));
        }
    };
    if map.len() > MAX_COMPANIES {
        return Err(Error::validation(
            "company-dependent map exceeds the company limit",
        ));
    }
    let mut bytes = 0usize;
    for (key, value) in map {
        let id = key.parse::<i64>().map_err(|_| {
            Error::programming("company-dependent map has a noncanonical company key")
        })?;
        if id <= 0 || id.to_string() != *key {
            return Err(Error::programming(
                "company-dependent map has a noncanonical company key",
            ));
        }
        let payload = match value {
            Json::String(s) => s.len(),
            Json::Null | Json::Bool(_) | Json::Number(_) => 0,
            _ => {
                return Err(Error::programming(
                    "company-dependent map contains a non-scalar value",
                ));
            }
        };
        bytes = bytes
            .checked_add(key.len())
            .and_then(|n| n.checked_add(payload))
            .and_then(|n| n.checked_add(64))
            .ok_or_else(|| Error::validation("company-dependent map byte limit exceeded"))?;
        if bytes > MAX_MAP_BYTES {
            return Err(Error::validation(
                "company-dependent map byte limit exceeded",
            ));
        }
    }
    Ok(Some(map))
}

fn validate_map<'a>(ty: &FieldType, cell: &'a Cell) -> Result<Option<&'a Map<String, Json>>> {
    let map = checked_map(cell)?;
    if let Some(map) = map {
        for value in map.values() {
            decode(ty, value)?;
        }
    }
    Ok(map)
}

/// Schema admission uses the same complete map contract as runtime projection.
pub(crate) fn validate_storage(ty: &FieldType, cell: &Cell) -> Result<()> {
    validate_map(ty, cell).map(|_| ())
}

/// Project one company's effective scalar. Only an absent key inherits the fallback.
///
/// # Errors
/// Invalid map, scalar or fallback, including bounded-map admission failures.
pub fn project(ty: &FieldType, cell: &Cell, company: CompanyId, fallback: &Cell) -> Result<Cell> {
    let fallback = encode(ty, fallback)?;
    let map = validate_map(ty, cell)?;
    decode(
        ty,
        map.and_then(|m| m.get(&company.to_string()))
            .unwrap_or(&fallback),
    )
}

/// Set one company's admitted scalar, removing its override when it equals the fallback.
/// Other company keys are unchanged; an empty map becomes SQL NULL.
///
/// # Errors
/// Invalid scalar/map or a map exceeding the documented limits.
pub fn apply(
    ty: &FieldType,
    base: &Cell,
    company: CompanyId,
    value: &Cell,
    fallback: &Cell,
) -> Result<Cell> {
    let value = encode(ty, value)?;
    let default = encode(ty, fallback)?;
    let mut map = validate_map(ty, base)?.cloned().unwrap_or_default();
    if value == default {
        map.remove(&company.to_string());
    } else {
        map.insert(company.to_string(), value);
    }
    let cell = map_cell(map);
    checked_map(&cell)?;
    Ok(cell)
}

fn map_cell(map: Map<String, Json>) -> Cell {
    if map.is_empty() {
        Cell::Null
    } else {
        Cell::Json(Arc::new(Json::Object(map)))
    }
}

/// Produce a SQL company patch: `[value]` sets a key (including null), `[]` removes it.
/// This is distinct from translation deltas, whose null entries mean removal.
///
/// # Errors
/// Malformed or oversized source maps.
pub fn delta(before: &Cell, after: &Cell) -> Result<Cell> {
    let old = checked_map(before)?;
    let new = checked_map(after)?;
    let mut patch = Map::new();
    if let Some(old) = old {
        for key in old.keys() {
            if !new.is_some_and(|m| m.contains_key(key)) {
                patch.insert(key.clone(), Json::Array(Vec::new()));
            }
        }
    }
    if let Some(new) = new {
        for (key, value) in new {
            if old.and_then(|m| m.get(key)) != Some(value) {
                patch.insert(key.clone(), Json::Array(vec![value.clone()]));
            }
        }
    }
    Ok(Cell::Json(Arc::new(Json::Object(patch))))
}
