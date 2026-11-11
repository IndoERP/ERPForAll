//! Explicit ABI1 adversarial wire fixtures, distinct from portable business/model source.
//! Inclusion and registrations are unconditional; guest raw-host probes have native stubs.

use crate::TemporalJson;
use erp_sdk::prelude::*;
#[cfg(target_family = "wasm")]
use erp_sdk::serde_json;

// Explicit adversarial test mode; ordinary fixture calls stay passthrough. The
// actual guest inserts a native field name only after entry, exercising Super's
// own host admission rather than the initial call's preflight.
fn abi1_inject_native_on_super(tx: &Transaction) -> Result<bool> {
    #[cfg(target_family = "wasm")]
    {
        let context: serde_json::Value = serde_json::from_str(&tx.call_context().context_json)
            .map_err(|error| Error::programming(format!("invalid fixture context: {error}")))?;
        Ok(context.get("abi1_inject_native_on_super") == Some(&serde_json::Value::Bool(true)))
    }
    #[cfg(not(target_family = "wasm"))]
    {
        let _ = tx;
        Ok(false)
    }
}

#[methods(TemporalJson)]
impl TemporalJson {
    /// An adversarial ABI1 fixture: return the exact host boundary refusal as test data.
    /// The native body is never used by the real-component acceptance tests.
    #[method(wire)]
    async fn abi1_native_probe(
        this: &RecordSet<TemporalJson>,
        tx: &mut Transaction,
        operation: String,
        field: String,
        payload: String,
    ) -> Result<String> {
        #[cfg(target_family = "wasm")]
        {
            use erp_sdk::__private::bindings::erpforall::addon::{orm_host, types};
            let ids: Vec<i64> = this.rec_ids().iter().map(|id| id.get()).collect();
            let model = "test.wasm.temporal";
            let result = match operation.as_str() {
                "read" => orm_host::read_columns(model, &ids, &[field]).map(|_| ()),
                "call-create" => orm_host::call_method(model, "create", &ids, &payload).map(|_| ()),
                "call-write" => orm_host::call_method(model, "write", &ids, &payload).map(|_| ()),
                "write" | "create" | "columns" => {
                    let value: Option<String> =
                        serde_json::from_str(&payload).map_err(|error| {
                            Error::programming(format!("invalid probe payload: {error}"))
                        })?;
                    if operation == "columns" {
                        let column = types::ColumnVector {
                            field,
                            data: types::ColumnData::ColOptString(vec![value; ids.len()]),
                        };
                        orm_host::write_columns(model, &ids, &[column])
                    } else {
                        let scalar = match value {
                            Some(value) => types::ScalarValue::ValStr(value),
                            None => types::ScalarValue::ValNull,
                        };
                        let row = vec![(field, scalar)];
                        if operation == "create" {
                            orm_host::create_batch(model, &[row]).map(|_| ())
                        } else {
                            orm_host::write_batch(model, &ids, &row)
                        }
                    }
                }
                _ => return Err(Error::programming("unknown ABI1 native probe operation")),
            };
            let _ = tx;
            Ok(match result {
                Ok(()) => "ok".to_owned(),
                Err(error) => format!("{:?}:{}", error.kind, error.message),
            })
        }
        #[cfg(not(target_family = "wasm"))]
        {
            let _ = (this, tx, operation, field, payload);
            Err(Error::programming(
                "abi1_native_probe requires a real WebAssembly component",
            ))
        }
    }

    /// Raw guest-origin calls to a model whose method chains are entirely native.
    #[method(wire)]
    async fn abi1_native_child_probe(
        this: &RecordSet<TemporalJson>,
        tx: &mut Transaction,
        method: String,
        ids: Vec<i64>,
        payload: String,
    ) -> Result<String> {
        #[cfg(target_family = "wasm")]
        {
            use erp_sdk::__private::bindings::erpforall::addon::orm_host;
            let result = orm_host::call_method("test.wasm.native.child", &method, &ids, &payload);
            let _ = (this, tx);
            Ok(match result {
                Ok(_) => "ok".to_owned(),
                Err(error) => format!("{:?}:{}", error.kind, error.message),
            })
        }
        #[cfg(not(target_family = "wasm"))]
        {
            let _ = (this, tx, method, ids, payload);
            Err(Error::programming(
                "abi1_native_child_probe requires a real WebAssembly component",
            ))
        }
    }

    /// Pass create values through the JSON override seam without inferring field types.
    #[method(override)]
    async fn create(
        this: &RecordSet<TemporalJson>,
        tx: &mut Transaction,
        mut vals_list: Vec<RawVals>,
        sup: Super,
    ) -> Result<Records> {
        if abi1_inject_native_on_super(tx)? {
            for vals in &mut vals_list {
                vals.insert("native_uuid".into(), Cell::Null);
            }
        }
        sup.call(tx, this.as_records(), (vals_list,)).await
    }

    /// Pass write values through the JSON override seam without inferring field types.
    #[method(override)]
    async fn write(
        this: &RecordSet<TemporalJson>,
        tx: &mut Transaction,
        mut vals: RawVals,
        sup: Super,
    ) -> Result<()> {
        if abi1_inject_native_on_super(tx)? {
            vals.insert("native_uuid".into(), Cell::Null);
        }
        sup.call(tx, this.as_records(), (vals,)).await
    }
}
