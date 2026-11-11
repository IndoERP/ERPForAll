//! The wire form of the core `create`, `write` and `unlink`.
//!
//! A WebAssembly addon overrides these three like a native addon does, and its `super()` reaches
//! the core implementation in `base`. The chain then crosses the native/WebAssembly seam in JSON,
//! so the core implementations need a [`MethodCodec`]; `#[method(wire)]` cannot be written on them
//! (they are not declared with `#[methods]`), so they are registered here and added by
//! [`crate::Dispatch::build_with`] (a `linkme` element in this crate would need `unsafe`, which the
//! workspace forbids).
//!
//! | Method | Arguments (JSON array, the argument tuple) | Result |
//! |---|---|---|
//! | `create` | `[[vals, ...]]` | the ids of the new records, `[id, ...]` |
//! | `write` | `[vals]` | `null` |
//! | `unlink` | `null` | `null` |
//!
//! `vals` is a JSON object, field name to value.
//! Decoding goes through the one converter the RPC surface uses
//! ([`Records::vals_from_values`]), so a value is checked against its field exactly as a value
//! from a client, and a malformed document is a `Validation` error naming the field.

use std::any::{Any, TypeId};

use erp_fields::convert::value_to_json;
use erp_fields::{Error, RecId, Result, Value, serde_json};

use crate::Transaction;
use crate::commands::Command;
use crate::dispatch::{BoxFuture, Chain, MethodCodec, MethodCodecImpl, MethodFn, invoke};
use crate::ir_default::json_to_value;
use crate::records::Records;
use crate::vals::RawVals;

/// Most records one `create` document may carry across the boundary. The ORM's own batch limits
/// apply after decoding; this bounds the work spent on a hostile document first.
pub(crate) const MAX_WIRE_BATCH: usize = 100_000;

/// Serialize an ABI1 data value without pretending a native scalar is an ordinary string.
/// Ordinary JSON documents retain their untyped JSON contract. This is not a field ACL check.
///
/// # Errors
/// `Programming` for a native or unknown value kind requiring a future ABI major;
/// errors from reading a result row or converting a legacy scalar are preserved.
#[doc(hidden)]
pub fn wasm_abi1_value_to_json(value: &Value) -> Result<serde_json::Value> {
    fn convert(value: &Value, depth: usize, remaining: &mut usize) -> Result<serde_json::Value> {
        use serde_json::Value as J;
        if depth > crate::wasm_abi1::MAX_DEPTH {
            return Err(Error::new(
                erp_fields::ErrorKind::ResourceExhausted,
                "WASM ABI1 value exceeds its size or recursion admission limit",
            ));
        }
        *remaining = remaining.checked_sub(1).ok_or_else(|| {
            Error::new(
                erp_fields::ErrorKind::ResourceExhausted,
                "WASM ABI1 value exceeds its size or recursion admission limit",
            )
        })?;
        let unsupported = |name: &str| {
            crate::wasm_abi1::native_error(format!(
                "native {name} values are not supported across frozen WASM ABI1; requires a future ABI major with typed native fields"
            ))
        };
        let next = depth.checked_add(1).ok_or_else(|| {
            Error::new(
                erp_fields::ErrorKind::ResourceExhausted,
                "WASM ABI1 value exceeds its size or recursion admission limit",
            )
        })?;
        Ok(match value {
            Value::Uuid(_) => return Err(unsupported("uuid")),
            Value::Xid8(_) => return Err(unsupported("xid8")),
            Value::TimestampTz(_) => return Err(unsupported("timestamptz")),
            Value::Float(value) if !value.is_finite() => return Err(Error::programming("a non-finite float cannot cross WASM ABI1")),
            Value::List(items) => J::Array(items.iter().map(|value| convert(value, next, remaining)).collect::<Result<_>>()?),
            Value::Map(map) => J::Object(map.iter().map(|(key, value)| Ok((key.clone(), convert(value, next, remaining)?))).collect::<Result<_>>()?),
            Value::Row(row) => J::Object(row.iter().map(|item| { let (key, value) = item?; Ok((key.to_owned(), convert(&value, next, remaining)?)) }).collect::<Result<_>>()?),
            Value::Rows(rows) => J::Array(rows.iter()?.map(|row| convert(&Value::Row(row), next, remaining)).collect::<Result<_>>()?),
            Value::False | Value::Bool(_) | Value::Int(_) | Value::Float(_) | Value::Decimal(_)
            | Value::Str(_) | Value::Bytes(_) | Value::Date(_) | Value::DateTime(_)
            | Value::Json(_) | Value::Range(_) => value_to_json(value)?,
            _ => return Err(crate::wasm_abi1::native_error("this value kind is not supported across frozen WASM ABI1; requires a future ABI major".to_owned())),
        })
    }
    let mut remaining = crate::wasm_abi1::MAX_VALUE_NODES;
    convert(value, 0, &mut remaining)
}

/// `vals` as the JSON object of the wire form.
///
/// # Errors
/// `Programming` when a value has no JSON form.
pub(crate) fn vals_to_json(vals: &RawVals) -> Result<serde_json::Value> {
    let mut map = serde_json::Map::new();
    for (name, cell) in vals.iter() {
        map.insert(
            name.to_owned(),
            wasm_abi1_value_to_json(&Value::from_cell(cell))?,
        );
    }
    for (name, commands) in vals.iter_commands() {
        let items = commands
            .iter()
            .map(|c: &Command| wasm_abi1_value_to_json(&c.to_value()))
            .collect::<Result<Vec<_>>>()?;
        map.insert(name.to_owned(), serde_json::Value::Array(items));
    }
    Ok(serde_json::Value::Object(map))
}

/// The values of the wire form `json`, converted with the fields of `records`' model.
///
/// # Errors
/// `Validation` when `json` is not an object or a value does not fit its field.
pub(crate) fn vals_from_json(records: &Records, json: &serde_json::Value) -> Result<RawVals> {
    let serde_json::Value::Object(map) = json else {
        return Err(Error::validation(format!(
            "the values of a write on '{}' must be a JSON object, got {json}",
            records.model_name()
        )));
    };
    let values: Vec<(String, Value)> = map
        .iter()
        .map(|(name, value)| (name.clone(), json_to_value(value)))
        .collect();
    records.vals_from_values(values.iter().map(|(name, value)| (name, value)))
}

fn bad_args(method: &str, error: &serde_json::Error) -> Error {
    Error::validation(format!(
        "arguments of '{method}' do not match its wire form: {error}"
    ))
}

fn ids_to_json(records: &Records) -> Result<String> {
    let ids: Vec<i64> = records.rec_ids().iter().map(|id| id.get()).collect();
    serde_json::to_string(&ids)
        .map_err(|e| Error::programming(format!("record ids are not serialisable: {e}")))
}

type CreateFn = MethodFn<(Vec<RawVals>,), Records>;
type WriteFn = MethodFn<(RawVals,), ()>;
type UnlinkFn = MethodFn<(), ()>;

struct CreateCodec;

impl MethodCodec for CreateCodec {
    fn signature(&self) -> TypeId {
        TypeId::of::<CreateFn>()
    }

    fn encode_args(&self, args: Box<dyn Any + Send>) -> Result<String> {
        let args = args
            .downcast::<(Vec<RawVals>,)>()
            .map_err(|_| Error::programming("create codec used with arguments of another type"))?;
        let list = args
            .0
            .iter()
            .map(vals_to_json)
            .collect::<Result<Vec<_>>>()?;
        serde_json::to_string(&(list,))
            .map_err(|e| Error::programming(format!("create arguments are not serialisable: {e}")))
    }

    fn decode_result(&self, records: &Records, json: &str) -> Result<Box<dyn Any + Send>> {
        let ids: Vec<i64> = serde_json::from_str(json).map_err(|e| {
            Error::validation(format!(
                "a WASM create returned something that is not a list of ids: {e}"
            ))
        })?;
        let ids = ids
            .into_iter()
            .map(|id| {
                RecId::from_real(id).ok_or_else(|| {
                    Error::validation(format!("a WASM create returned the record id {id}"))
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Box::new(records.browse_ids(ids)))
    }

    fn call_native<'a>(
        &'a self,
        chain: Chain,
        index: usize,
        tx: &'a mut Transaction,
        records: &'a Records,
        args_json: &'a str,
    ) -> BoxFuture<'a, Result<String>> {
        Box::pin(async move {
            let (list,): (Vec<serde_json::Value>,) =
                serde_json::from_str(args_json).map_err(|e| bad_args("create", &e))?;
            if list.len() > MAX_WIRE_BATCH {
                return Err(Error::new(
                    erp_fields::ErrorKind::ResourceExhausted,
                    format!(
                        "create received {} records across the WebAssembly boundary; at most \
                         {MAX_WIRE_BATCH} per call",
                        list.len()
                    ),
                ));
            }
            let vals_list = list
                .iter()
                .map(|doc| vals_from_json(records, doc))
                .collect::<Result<Vec<_>>>()?;
            let created =
                invoke::<(Vec<RawVals>,), Records>(chain, index, tx, records, (vals_list,)).await?;
            ids_to_json(&created)
        })
    }
}

struct WriteCodec;

impl MethodCodec for WriteCodec {
    fn signature(&self) -> TypeId {
        TypeId::of::<WriteFn>()
    }

    fn encode_args(&self, args: Box<dyn Any + Send>) -> Result<String> {
        let args = args
            .downcast::<(RawVals,)>()
            .map_err(|_| Error::programming("write codec used with arguments of another type"))?;
        serde_json::to_string(&(vals_to_json(&args.0)?,))
            .map_err(|e| Error::programming(format!("write arguments are not serialisable: {e}")))
    }

    fn decode_result(&self, _records: &Records, json: &str) -> Result<Box<dyn Any + Send>> {
        let unit: () = serde_json::from_str(json).map_err(|e| {
            Error::validation(format!("a WASM write returned something but null: {e}"))
        })?;
        Ok(Box::new(unit))
    }

    fn call_native<'a>(
        &'a self,
        chain: Chain,
        index: usize,
        tx: &'a mut Transaction,
        records: &'a Records,
        args_json: &'a str,
    ) -> BoxFuture<'a, Result<String>> {
        Box::pin(async move {
            let (doc,): (serde_json::Value,) =
                serde_json::from_str(args_json).map_err(|e| bad_args("write", &e))?;
            let vals = vals_from_json(records, &doc)?;
            invoke::<(RawVals,), ()>(chain, index, tx, records, (vals,)).await?;
            Ok("null".to_owned())
        })
    }
}

struct UnlinkCodec;

impl MethodCodec for UnlinkCodec {
    fn signature(&self) -> TypeId {
        TypeId::of::<UnlinkFn>()
    }

    fn encode_args(&self, args: Box<dyn Any + Send>) -> Result<String> {
        args.downcast::<()>()
            .map_err(|_| Error::programming("unlink codec used with arguments of another type"))?;
        Ok("null".to_owned())
    }

    fn decode_result(&self, _records: &Records, json: &str) -> Result<Box<dyn Any + Send>> {
        let unit: () = serde_json::from_str(json).map_err(|e| {
            Error::validation(format!("a WASM unlink returned something but null: {e}"))
        })?;
        Ok(Box::new(unit))
    }

    fn call_native<'a>(
        &'a self,
        chain: Chain,
        index: usize,
        tx: &'a mut Transaction,
        records: &'a Records,
        args_json: &'a str,
    ) -> BoxFuture<'a, Result<String>> {
        Box::pin(async move {
            let _: () = serde_json::from_str(args_json).map_err(|e| bad_args("unlink", &e))?;
            invoke::<(), ()>(chain, index, tx, records, ()).await?;
            Ok("null".to_owned())
        })
    }
}

static CREATE_CODEC: MethodCodecImpl = MethodCodecImpl {
    addon: "base",
    model: "base",
    method: "create",
    codec: &CreateCodec,
};
static WRITE_CODEC: MethodCodecImpl = MethodCodecImpl {
    addon: "base",
    model: "base",
    method: "write",
    codec: &WriteCodec,
};
static UNLINK_CODEC: MethodCodecImpl = MethodCodecImpl {
    addon: "base",
    model: "base",
    method: "unlink",
    codec: &UnlinkCodec,
};

/// The codecs of the core `create`, `write` and `unlink`.
pub(crate) static BUILTIN_CODECS: [&MethodCodecImpl; 3] =
    [&CREATE_CODEC, &WRITE_CODEC, &UNLINK_CODEC];

#[cfg(test)]
mod native_fields_tests {
    #![allow(clippy::unwrap_used, reason = "tests")]
    use super::*;
    use erp_fields::{TimestampTz, Uuid, Xid8};

    #[test]
    fn native_fields_abi1_json_refuses_nested_tagged_scalars_without_inferring_strings() {
        for (value, name) in [
            (Value::Uuid(Uuid::NIL), "uuid"),
            (Value::Xid8(Xid8::from_u64(u64::MAX)), "xid8"),
            (Value::TimestampTz(TimestampTz::UNIX_EPOCH), "timestamptz"),
        ] {
            let error = wasm_abi1_value_to_json(&Value::list([value])).unwrap_err();
            assert_eq!(error.kind(), erp_fields::ErrorKind::Programming);
            assert_eq!(
                error.message(),
                format!(
                    "native {name} values are not supported across frozen WASM ABI1; requires a future ABI major with typed native fields"
                )
            );
        }
        assert_eq!(
            wasm_abi1_value_to_json(&Value::str("18446744073709551615")).unwrap(),
            serde_json::json!("18446744073709551615")
        );
        assert_eq!(
            wasm_abi1_value_to_json(&Value::False).unwrap(),
            serde_json::json!(false)
        );
    }
    #[test]
    fn native_fields_abi1_value_guard_bounds_recursion_and_never_substitutes_null() {
        let mut nested = Value::False;
        for _ in 0..=crate::wasm_abi1::MAX_DEPTH {
            nested = Value::list([nested]);
        }
        let error = wasm_abi1_value_to_json(&nested).unwrap_err();
        assert_eq!(error.kind(), erp_fields::ErrorKind::ResourceExhausted);
        assert_eq!(
            error.message(),
            "WASM ABI1 value exceeds its size or recursion admission limit"
        );
        for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let error = wasm_abi1_value_to_json(&Value::Float(value)).unwrap_err();
            assert_eq!(error.kind(), erp_fields::ErrorKind::Programming);
            assert_eq!(error.message(), "a non-finite float cannot cross WASM ABI1");
        }
    }
}
