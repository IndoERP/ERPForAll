//! The smallest addon component: it exports `addon-guest` through `export_addon!`, using only this
//! crate. CI builds it for `wasm32-wasip2` and checks the component it produces against the
//! contract (`crates/erp-wasm-host/tests/guest_component.rs`). It describes no models; its one
//! compute (`title_length` from `title`) exercises the typed column path, and its one method
//! (`count_all`) the `orm-host` import.

// The export glue `export_addon!` expands to is the canonical ABI's unsafe entry points.
#![allow(unsafe_code)]

use erp_guest::bindings::exports::erpforall::addon::addon_guest::Guest;
use erp_guest::host;
use erp_guest::{
    BoundaryError, BoundaryErrorKind, CallContext, ColumnData, ColumnStore, ColumnVector,
    OnchangeResult, RecordBatch, RecordId, ScalarValue, error,
};

struct MinimalGuest;

impl Guest for MinimalGuest {
    fn describe() -> String {
        r#"{"models":[]}"#.to_owned()
    }

    fn invoke_method(
        _ctx: CallContext,
        model: String,
        method: String,
        _ids: Vec<RecordId>,
        _args_json: String,
    ) -> Result<String, BoundaryError> {
        // `count_all` asks the host, so the component imports `orm-host` (unused imports are
        // dropped at link time).
        if method != "count_all" {
            return Err(error(
                BoundaryErrorKind::NotFound,
                format!("minimal guest has no method '{method}' on '{model}'"),
            ));
        }
        let ids = host::search(&model, "[]", None, None, None)?;
        Ok(ids.len().to_string())
    }

    fn compute_fields(
        ctx: CallContext,
        batch: RecordBatch,
        fields: Vec<String>,
    ) -> Result<Vec<ColumnVector>, BoundaryError> {
        let store = ColumnStore::new(batch, ctx.cache_epoch)?;
        let mut out = Vec::with_capacity(fields.len());
        for field in fields {
            if field != "title_length" {
                return Err(error(
                    BoundaryErrorKind::NotFound,
                    format!("minimal guest computes no field '{field}'"),
                ));
            }
            let titles = store.strings("title")?;
            let lengths = store
                .rows()
                .map(|row| i64::try_from(titles[row.index()].chars().count()).ok())
                .collect();
            out.push(ColumnVector {
                field,
                data: ColumnData::ColOptInt(lengths),
            });
        }
        Ok(out)
    }

    fn validate_constraints(
        _ctx: CallContext,
        _batch: RecordBatch,
        constraints: Vec<String>,
    ) -> Result<(), BoundaryError> {
        match constraints.first() {
            None => Ok(()),
            Some(name) => Err(error(
                BoundaryErrorKind::NotFound,
                format!("minimal guest has no constraint '{name}'"),
            )),
        }
    }

    fn onchange(
        _ctx: CallContext,
        model: String,
        _values: Vec<(String, ScalarValue)>,
        _changed_fields: Vec<String>,
    ) -> Result<OnchangeResult, BoundaryError> {
        Err(error(
            BoundaryErrorKind::NotFound,
            format!("minimal guest has no onchange on '{model}'"),
        ))
    }
}

erp_guest::bindings::export_addon!(MinimalGuest);
