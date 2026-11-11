//! A component that exports `describe()` and runs the methods the addon registered
//! (`invoke-method`).
//!
//! [`Dispatching`] is what `erp-sdk`'s `export_component!()` exports. The addon's macros register one
//! [`GuestMethodFn`] per `#[method]` / `#[method(override)]`; `invoke-method` finds it by
//! `(model, method)` and runs it. The three other exports (`compute-fields`, `validate-constraints`,
//! `onchange`) still refuse with a typed `unsupported` error, as [`crate::DescribeOnly`] does.

use std::future::Future;
use std::marker::PhantomData;
use std::pin::pin;
use std::task::{Context, Poll, Waker};

use crate::bindings::exports::erpforall::addon::addon_guest::Guest;
use crate::describe_only::dispatch_unsupported;
use crate::error::error;
use crate::{
    BoundaryError, BoundaryErrorKind, CallContext, ColumnVector, OnchangeResult, RecordBatch,
    RecordId, ScalarValue,
};

/// One registered method: runs with the call's context, the ids of the records and the arguments
/// (JSON text), and answers JSON text.
pub type GuestMethodFn = fn(&CallContext, Vec<RecordId>, &str) -> Result<String, BoundaryError>;

/// What a component's macros collected: its description and its methods.
pub trait GuestSource {
    /// The JSON text `describe()` returns.
    fn describe() -> String;

    /// The function registered for `method` of `model`, if the addon (or an addon it was linked
    /// with) contributes one. Exactly one registered function must answer for a pair; the
    /// implementation reports a duplicate as an error.
    fn method(model: &str, method: &str) -> Result<Option<GuestMethodFn>, BoundaryError>;
}

/// An `addon-guest` implementation that answers `describe` and `invoke-method` from `S`.
pub struct Dispatching<S>(PhantomData<fn() -> S>);

impl<S: GuestSource> Guest for Dispatching<S> {
    fn describe() -> String {
        S::describe()
    }

    fn invoke_method(
        ctx: CallContext,
        model: String,
        method: String,
        ids: Vec<RecordId>,
        args_json: String,
    ) -> Result<String, BoundaryError> {
        match S::method(&model, &method)? {
            Some(run) => run(&ctx, ids, &args_json),
            None => Err(error(
                BoundaryErrorKind::NotFound,
                format!("this addon registers no method '{method}' on model '{model}'"),
            )),
        }
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

/// Runs a future that only waits on host calls. Every `orm-host` import is a plain synchronous
/// call, so a method's future never has to suspend; a future that does (it awaited something that
/// is not ready) is a bug in the addon and is reported, never spun on.
pub fn block_on<F: Future>(future: F) -> Result<F::Output, BoundaryError> {
    let mut future = pin!(future);
    let mut cx = Context::from_waker(Waker::noop());
    match future.as_mut().poll(&mut cx) {
        Poll::Ready(value) => Ok(value),
        Poll::Pending => Err(error(
            BoundaryErrorKind::Unsupported,
            "a method suspended on something other than a host call; guest code cannot wait on \
             timers, channels or I/O",
        )),
    }
}
