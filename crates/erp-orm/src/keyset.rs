//! Keyset pagination: [`Records::search_after`], [`Cursor`], [`Page`].
//!
//! A page is a search with a limit; the next page is the same search restricted to the rows whose
//! order key comes after the last record of the page (`erp_sql::keyset`). Deep pages cost what the
//! first page costs, where `OFFSET n` makes PostgreSQL read and discard `n` rows; and pages do not
//! shift when rows are inserted or deleted before the cursor. The order always ends with the `id`
//! tiebreaker, so the key of a record is unique.

use std::sync::Arc;

use erp_domain::Domain;
use erp_fields::convert::decimal_from_f64;
use erp_fields::{Cell, Decimal, Error, FieldType, RecId, Result, Value, serde_json};
use erp_registry::ModelSpec;
use erp_sql::keyset::{KeyKind, KeyTerm, KeyValue, keyset_pieces, keyset_terms};
use erp_sql::param::{Scalar, ScalarKind};

use crate::Transaction;
use crate::records::Records;
use crate::search::SearchOptions;

/// Where a keyset page ends: the order specification it belongs to and the key of the page's
/// last record, one value per order term with the `id` tiebreaker last (`False` for NULL), so a
/// client can carry it as a plain list next to the order. Selection terms carry the key, boolean
/// terms `true`/`false`.
///
/// ```
/// use erp_orm::Cursor;
/// use erp_fields::Value;
///
/// let cursor = Cursor::new("name desc, id", vec![Value::from("Dune"), Value::from(42)]);
/// assert_eq!(cursor.order(), "name desc, id");
/// assert_eq!(cursor.values().len(), 2);
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct Cursor {
    order: String,
    values: Vec<Value>,
}

impl Cursor {
    /// A cursor received from a client: `order` as [`Cursor::order`] returned it (the effective
    /// order: the model's order when the search gave none), `values` as [`Cursor::values`]
    /// returned them. Checked when used.
    pub fn new(order: impl Into<String>, values: Vec<Value>) -> Self {
        Self {
            order: order.into(),
            values,
        }
    }

    /// The effective order specification (the model's order when the search gave none).
    pub fn order(&self) -> &str {
        &self.order
    }

    /// The key of the last record, one value per order term, `id` last.
    pub fn values(&self) -> &[Value] {
        &self.values
    }

    /// The cursor as JSON for a client, lossless: `{"order": …, "values": […]}` with dates as
    /// `YYYY-MM-DD`, datetimes as `YYYY-MM-DD HH:MM:SS.ffffff` (microseconds kept, unlike the
    /// display format), decimals as strings, NULL as `false`. A non-finite float is an error (no
    /// cursor ever holds one).
    pub fn to_json(&self) -> Result<serde_json::Value> {
        let values = self
            .values
            .iter()
            .map(|v| {
                Ok(match v {
                    Value::False => serde_json::Value::Bool(false),
                    Value::Bool(b) => serde_json::Value::Bool(*b),
                    Value::Int(i) => serde_json::Value::from(*i),
                    Value::Float(f) => serde_json::Number::from_f64(*f)
                        .map(serde_json::Value::Number)
                        .ok_or_else(|| {
                            Error::validation(format!("keyset cursor value {f} is not finite"))
                        })?,
                    Value::Decimal(d) => serde_json::Value::String(d.to_string()),
                    Value::Str(s) => serde_json::Value::String(s.to_string()),
                    Value::Date(d) => serde_json::Value::String(d.to_string()),
                    Value::Uuid(v) => serde_json::Value::String(v.to_string()),
                    Value::Xid8(v) => serde_json::Value::String(v.to_string()),
                    Value::TimestampTz(v) => serde_json::Value::String(v.to_string()),
                    Value::DateTime(d) => {
                        let micro = d.micros().rem_euclid(1_000_000);
                        serde_json::Value::String(format!("{d}.{micro:06}"))
                    }
                    other => {
                        return Err(Error::programming(format!(
                            "keyset cursor value {other:?} has no JSON form"
                        )));
                    }
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(serde_json::json!({"order": self.order, "values": values}))
    }

    /// A cursor from the JSON of [`Cursor::to_json`] (values are checked against the order's
    /// fields when the cursor is used). Malformed input is a `Validation` error.
    pub fn from_json(json: &serde_json::Value) -> Result<Self> {
        let bad = |why: &str| Error::validation(format!("invalid keyset cursor: {why}"));
        let order = json
            .get("order")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| bad("no \"order\" string"))?;
        let values = json
            .get("values")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| bad("no \"values\" list"))?
            .iter()
            .map(|v| match v {
                serde_json::Value::Bool(false) | serde_json::Value::Null => Ok(Value::False),
                serde_json::Value::Bool(true) => Ok(Value::Bool(true)),
                serde_json::Value::Number(n) => match n.as_i64() {
                    Some(i) => Ok(Value::Int(i)),
                    None => n
                        .as_f64()
                        .filter(|f| f.is_finite())
                        .map(Value::Float)
                        .ok_or_else(|| bad("a number out of range")),
                },
                serde_json::Value::String(s) => Ok(Value::str(s.as_str())),
                _ => Err(bad("values must be scalars")),
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Self::new(order, values))
    }
}

/// `YYYY-MM-DD HH:MM:SS[.ffffff]` (the display format, plus up to six fraction digits).
fn parse_timestamp(text: &str) -> Option<erp_fields::DateTime> {
    let (head, fraction) = match text.split_once('.') {
        Some((h, f)) => (h, Some(f)),
        None => (text, None),
    };
    let base = erp_fields::DateTime::parse_odoo(head)?;
    let micro = match fraction {
        None => 0,
        Some(f) if !f.is_empty() && f.len() <= 6 && f.bytes().all(|b| b.is_ascii_digit()) => {
            format!("{f:0<6}").parse::<i64>().ok()?
        }
        Some(_) => return None,
    };
    Some(erp_fields::DateTime::from_micros(base.micros() + micro))
}

/// One page of a keyset pagination.
#[derive(Debug, Clone)]
pub struct Page {
    /// The records of the page, in order.
    pub records: Records,
    /// The cursor of the next page; `None` when this page is not full (nothing follows).
    pub next: Option<Cursor>,
}

fn bad_cursor(spec: &ModelSpec, why: impl std::fmt::Display) -> Error {
    Error::validation(format!("invalid keyset cursor for '{}': {why}", spec.name))
}

/// A cursor value as the key of `term`.
fn key_value(spec: &ModelSpec, term: &KeyTerm, value: &Value) -> Result<KeyValue> {
    let field = spec.field_by_id(term.field).name;
    let mismatch = || bad_cursor(spec, format!("{value:?} is not a value of '{field}'"));
    Ok(match (&term.kind, value) {
        (KeyKind::Bool, Value::Bool(b)) => KeyValue::Bool(*b),
        (KeyKind::Bool, Value::False) => KeyValue::Bool(false),
        // a term that cannot be NULL (the id, a required field) never has a NULL key
        (_, Value::False) if !term.nullable => {
            return Err(bad_cursor(
                spec,
                format!("'{field}' cannot be NULL, the cursor gives False"),
            ));
        }
        (_, Value::False) => KeyValue::Null,
        (KeyKind::Selection(_), Value::Str(key)) => KeyValue::Key(key.to_string()),
        (KeyKind::Column(kind), value) => KeyValue::Scalar(match (kind, value) {
            (ScalarKind::Int8, Value::Int(i)) => Scalar::Int8(*i),
            // NaN and infinities are refused at both ends
            (ScalarKind::Float8, Value::Float(f)) if f.is_finite() => Scalar::Float8(*f),
            (ScalarKind::Float8, Value::Int(i)) => Scalar::Float8(*i as f64),
            (ScalarKind::Numeric, Value::Decimal(d)) => Scalar::Numeric(*d),
            (ScalarKind::Numeric, Value::Int(i)) => Scalar::Numeric(Decimal::from(*i)),
            (ScalarKind::Numeric, Value::Float(f)) => {
                Scalar::Numeric(decimal_from_f64(*f).ok_or_else(mismatch)?)
            }
            // the JSON form of a cursor (Cursor::to_json) carries these as strings
            (ScalarKind::Numeric, Value::Str(s)) => {
                Scalar::Numeric(s.parse::<Decimal>().map_err(|_| mismatch())?)
            }
            (ScalarKind::Date, Value::Str(s)) if s.len() == 10 => {
                Scalar::Date(erp_fields::Date::parse_odoo(s).ok_or_else(mismatch)?)
            }
            (ScalarKind::Timestamp, Value::Str(s)) => {
                Scalar::Timestamp(parse_timestamp(s).ok_or_else(mismatch)?)
            }
            (ScalarKind::Uuid, Value::Uuid(v)) => Scalar::Uuid(*v),
            (ScalarKind::Xid8, Value::Xid8(v)) => Scalar::Xid8(*v),
            (ScalarKind::TimestampTz, Value::TimestampTz(v)) => Scalar::TimestampTz(*v),
            (ScalarKind::Uuid, Value::Str(s)) => {
                Scalar::Uuid(erp_fields::Uuid::parse(s).map_err(|_| mismatch())?)
            }
            (ScalarKind::Xid8, Value::Str(s)) => {
                Scalar::Xid8(erp_fields::Xid8::parse(s).map_err(|_| mismatch())?)
            }
            (ScalarKind::TimestampTz, Value::Str(s)) => {
                Scalar::TimestampTz(erp_fields::TimestampTz::parse(s).map_err(|_| mismatch())?)
            }
            (ScalarKind::Text, Value::Str(s)) => Scalar::Text(Arc::clone(s)),
            (ScalarKind::Date, Value::Date(d)) => Scalar::Date(*d),
            (ScalarKind::Timestamp, Value::DateTime(d)) => Scalar::Timestamp(*d),
            _ => return Err(mismatch()),
        }),
        _ => return Err(mismatch()),
    })
}

/// The cursor value of `term` for `id`, from the cache (the page's query fetched it).
fn cursor_value(tx: &Transaction, spec: &ModelSpec, term: &KeyTerm, id: RecId) -> Result<Value> {
    let field = spec.field_by_id(term.field);
    if matches!(field.ty, FieldType::Id) {
        let real = id.as_real().ok_or_else(|| {
            Error::programming(format!(
                "keyset: a search on '{}' returned the new record {id}",
                spec.name
            ))
        })?;
        return Ok(Value::Int(real.get()));
    }
    let cell = tx.cache.get(spec.id, term.field, id).ok_or_else(|| {
        Error::programming(format!(
            "keyset: {}.{} of the page's last record is not cached",
            spec.name, field.name
        ))
    })?;
    Ok(match (&term.kind, &cell) {
        (KeyKind::Bool, Cell::Null) => Value::Bool(false),
        // a NaN (only raw SQL can store one) cannot be carried by a cursor
        (_, Cell::Float(f)) if !f.is_finite() => {
            return Err(Error::validation(format!(
                "keyset pagination cannot continue after the value {f} of {}.{}",
                spec.name, field.name
            )));
        }
        _ => Value::from_cell(&cell),
    })
}

impl Records {
    /// Keyset pagination: up to `opts.limit` records matching `domain` in
    /// `opts.order` (the model's order when `None`), after the record `after` points at (from the
    /// start when `None`). The page's `next` cursor is set when the page is full.
    ///
    /// Costs one query per page whatever its depth (no `OFFSET`), and two for the one page where
    /// a nullable leading order term reaches its NULL rows (when they come last); the order
    /// columns are fetched with the ids (plus `opts.fetch`), so the cursor needs no other query. `opts.offset` must be
    /// 0 and `opts.limit` positive (`Validation` otherwise). Supported order terms: the model's
    /// stored columns and many2one fields by id (`partner_id.id`, or a comodel ordered by `id`);
    /// ordering through a comodel's order is refused with an error. A cursor of another order, or
    /// whose values do not fit the order's fields, is a `Validation` error.
    pub async fn search_after(
        &self,
        tx: &mut Transaction,
        domain: &Domain,
        opts: &SearchOptions,
        after: Option<&Cursor>,
    ) -> Result<Page> {
        let spec = self.spec();
        let limit = match opts.limit {
            Some(n) if n > 0 => n,
            _ => {
                return Err(Error::validation(format!(
                    "search_after on '{}' needs a positive limit",
                    spec.name
                )));
            }
        };
        if opts.offset != 0 {
            return Err(Error::validation(format!(
                "search_after on '{}' pages with a cursor; the offset must be 0",
                spec.name
            )));
        }
        // the effective order: no order and the model's order explicitly are the same cursor space
        let requested = opts.order.clone().unwrap_or_default();
        let order = if requested.trim().is_empty() {
            spec.order.trim().to_owned()
        } else {
            requested.trim().to_owned()
        };
        let registry = Arc::clone(tx.registry());
        let terms = keyset_terms(&registry, spec, &order)?;
        let values = match after {
            None => None,
            Some(cursor) => {
                if cursor.order.trim() != order {
                    return Err(bad_cursor(
                        spec,
                        format!("it belongs to the order '{}', not '{order}'", cursor.order),
                    ));
                }
                if cursor.values.len() != terms.len() {
                    return Err(bad_cursor(
                        spec,
                        format!(
                            "{} values for {} order terms",
                            cursor.values.len(),
                            terms.len()
                        ),
                    ));
                }
                Some(
                    terms
                        .iter()
                        .zip(&cursor.values)
                        .map(|(t, v)| key_value(spec, t, v))
                        .collect::<Result<Vec<_>>>()?,
                )
            }
        };
        // the policy generation and the subject's groups first: every field check below reads the
        // binding, so a membership or policy write of this transaction (or a fresh `with_user`) is
        // seen before the first mask is asked for
        if !self.env().su() {
            tx.prepare_access(self.env()).await?;
        }
        let mut columns = crate::search::checked_columns(tx, self.env(), spec, &opts.fetch)?;
        for term in &terms {
            let field = spec.field_by_id(term.field);
            if field.has_column() && !columns.contains(&term.field) {
                columns.push(term.field);
            }
        }
        let search = SearchOptions {
            offset: 0,
            limit: Some(limit),
            order: Some(order.clone()),
            active_test: opts.active_test,
            fetch: crate::search::Fetch::Ids,
        };
        let base = self.search_query(tx, domain, &search).await?;
        let ids = match &values {
            None => {
                let mut query = base;
                query.execute_columns(tx, &columns).await?
            }
            Some(values) => {
                let pieces = keyset_pieces(&terms, values, base.filter_params() + 1)?;
                let mut ids: Vec<RecId> = Vec::new();
                for (sql, params) in pieces {
                    let remaining = limit - ids.len() as u64;
                    if remaining == 0 {
                        break;
                    }
                    let mut query = base.clone();
                    query.restrict(&sql, params);
                    query.set_limit(remaining);
                    ids.extend(query.execute_columns(tx, &columns).await?);
                }
                ids
            }
        };
        let next = match ids.last() {
            Some(last) if ids.len() as u64 == limit => Some(Cursor {
                order,
                values: terms
                    .iter()
                    .map(|t| cursor_value(tx, spec, t, *last))
                    .collect::<Result<Vec<_>>>()?,
            }),
            _ => None,
        };
        Ok(Page {
            records: self.browse_ids(ids),
            next,
        })
    }
}

#[cfg(test)]
mod native_fields_tests {
    #![allow(clippy::unwrap_used, reason = "tests")]
    use super::*;
    use erp_fields::{TimestampTz, Uuid, Xid8};
    use erp_registry::{AddonDescriptor, FieldDecl, ModelDecl, RegistryBuilder};
    #[test]
    fn native_fields_cursors_validate_against_the_expected_native_kind() {
        static ADDON: AddonDescriptor = AddonDescriptor {
            name: "native_cursor",
            depends: &["base"],
            exact_only: false,
        };
        static MODEL: ModelDecl = ModelDecl::new("native_cursor", "native.cursor").fields(
            const {
                &[
                    FieldDecl::uuid("u"),
                    FieldDecl::xid8("x"),
                    FieldDecl::timestamptz("t"),
                ]
            },
        );
        let registry = RegistryBuilder::new([&ADDON])
            .linked(false)
            .with_decls([&MODEL])
            .build()
            .unwrap();
        let model = registry.model("native.cursor").unwrap();
        for (name, value, expected) in [
            ("u", Value::Uuid(Uuid::NIL), Scalar::Uuid(Uuid::NIL)),
            (
                "x",
                Value::Xid8(Xid8::from_u64(u64::MAX)),
                Scalar::Xid8(Xid8::from_u64(u64::MAX)),
            ),
            (
                "t",
                Value::TimestampTz(TimestampTz::parse("2024-03-01T01:00:00.123456+07:00").unwrap()),
                Scalar::TimestampTz(TimestampTz::parse("2024-02-29T18:00:00.123456Z").unwrap()),
            ),
        ] {
            let term = &keyset_terms(&registry, model, name).unwrap()[0];
            assert_eq!(
                key_value(model, term, &value).unwrap(),
                KeyValue::Scalar(expected.clone())
            );
            let cursor = Cursor::new(name, vec![value]);
            let json = cursor.to_json().unwrap();
            assert!(json["values"][0].is_string());
            let received = Cursor::from_json(&json).unwrap();
            assert_eq!(
                key_value(model, term, &received.values()[0]).unwrap(),
                KeyValue::Scalar(expected)
            );
            let error = key_value(model, term, &Value::Int(1)).unwrap_err();
            assert_eq!(error.kind(), erp_fields::ErrorKind::Validation);
        }
    }
}
