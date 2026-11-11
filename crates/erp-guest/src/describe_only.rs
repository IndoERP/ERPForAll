//! A component that exports `describe()` and refuses the rest with a typed error.
//!
//! `erp-sdk`'s `export_component!()` exports [`DescribeOnly`]: the addon's contributions are
//! discoverable while running them belongs to. Every other export
//! returns a `boundary-error` of kind `unsupported` that names that slice, never an empty answer.

use std::marker::PhantomData;

use crate::bindings::exports::erpforall::addon::addon_guest::Guest;
use crate::error::error;
use crate::{
    BoundaryError, BoundaryErrorKind, CallContext, ColumnVector, OnchangeResult, RecordBatch,
    RecordId, ScalarValue,
};

/// The slice that adds dispatch of the exports [`DescribeOnly`] refuses.
pub const DISPATCH_SLICE: &str = "a later release";

/// Where a component's `describe()` text comes from.
pub trait DescribeSource {
    /// The JSON text `describe()` returns.
    fn describe() -> String;
}

/// The `unsupported` error for `operation`, naming [`DISPATCH_SLICE`].
pub fn dispatch_unsupported(operation: &str) -> BoundaryError {
    error(
        BoundaryErrorKind::Unsupported,
        format!("{operation} is not implemented for WASM addons yet; {DISPATCH_SLICE} adds it"),
    )
}

/// An `addon-guest` implementation that answers `describe` from `D` and refuses everything else.
pub struct DescribeOnly<D>(PhantomData<fn() -> D>);

impl<D: DescribeSource> Guest for DescribeOnly<D> {
    fn describe() -> String {
        D::describe()
    }

    fn invoke_method(
        _ctx: CallContext,
        model: String,
        method: String,
        _ids: Vec<RecordId>,
        _args_json: String,
    ) -> Result<String, BoundaryError> {
        Err(dispatch_unsupported(&format!(
            "invoke-method '{method}' on '{model}'"
        )))
    }

    fn compute_fields(
        _ctx: CallContext,
        batch: RecordBatch,
        _fields: Vec<String>,
    ) -> Result<Vec<ColumnVector>, BoundaryError> {
        Err(dispatch_unsupported(&format!(
            "compute-fields on '{}'",
            batch.model
        )))
    }

    fn validate_constraints(
        _ctx: CallContext,
        batch: RecordBatch,
        _constraints: Vec<String>,
    ) -> Result<(), BoundaryError> {
        Err(dispatch_unsupported(&format!(
            "validate-constraints on '{}'",
            batch.model
        )))
    }

    fn onchange(
        _ctx: CallContext,
        model: String,
        _values: Vec<(String, ScalarValue)>,
        _changed_fields: Vec<String>,
    ) -> Result<OnchangeResult, BoundaryError> {
        Err(dispatch_unsupported(&format!("onchange on '{model}'")))
    }
}
