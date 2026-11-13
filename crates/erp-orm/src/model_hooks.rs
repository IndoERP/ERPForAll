//! `init()` and `_register_hook()`: per-model callbacks of the module system (MOD §4.9,
//! ).
//!
//! Both are defined on `base` as no-ops, so every model has them and an addon overrides them like
//! any method (`#[method(override)]`, calling `super()`). The module system calls
//! [`call_init`] on every model whose tables an install or upgrade step created or altered,
//! and [`call_register_hook`] once per model on the final registry of a load or an
//! operation. Neither is called by the ORM itself.

use std::any::Any;

use erp_fields::Result;

use crate::Transaction;
use crate::dispatch::{BoxFuture, ImplKind, MethodFn, MethodImpl, Super, call_method};

/// Signature of `init` and `_register_hook`: no arguments, no result.
pub type ModelHookFn = MethodFn<(), ()>;

/// Method name of the table callback.
pub const INIT: &str = "init";
/// Method name of the registry callback.
pub const REGISTER_HOOK: &str = "_register_hook";

fn noop<'a>(
    _tx: &'a mut Transaction,
    _records: &'a crate::Records,
    _args: (),
    _sup: Super<(), ()>,
) -> BoxFuture<'a, Result<()>> {
    Box::pin(async { Ok(()) })
}

fn noop_entry() -> &'static (dyn Any + Send + Sync) {
    static F: ModelHookFn = noop;
    &F
}

static BASE_INIT: MethodImpl = MethodImpl {
    addon: "base",
    model: "base",
    method: INIT,
    kind: ImplKind::Define,
    sig_name: std::any::type_name::<ModelHookFn>,
    entry: noop_entry,
    file: file!(),
    line: line!(),
};

static BASE_REGISTER_HOOK: MethodImpl = MethodImpl {
    addon: "base",
    model: "base",
    method: REGISTER_HOOK,
    kind: ImplKind::Define,
    sig_name: std::any::type_name::<ModelHookFn>,
    entry: noop_entry,
    file: file!(),
    line: line!(),
};

/// `base.init` and `base._register_hook`, registered by [`crate::Dispatch::build`].
pub(crate) static BUILTIN_IMPLS: [&MethodImpl; 2] = [&BASE_INIT, &BASE_REGISTER_HOOK];

/// Call `init()` on `model` (an empty recordset of the transaction's registry).
///
/// # Errors
/// `Access` unless the transaction acts as the `System` principal with full elevation (the
/// module system's own transaction). `Programming` when the model is not in the transaction's
/// registry; the error of an override.
pub async fn call_init(tx: &mut Transaction, model: &str) -> Result<()> {
    tx.require_system_full("model_hooks::call_init")?;
    let records = tx.env().records(model)?;
    call_method::<(), ()>(tx, &records, INIT, ()).await
}

/// Call `_register_hook()` on `model` (an empty recordset of the transaction's registry).
///
/// # Errors
/// As [`call_init`].
pub async fn call_register_hook(tx: &mut Transaction, model: &str) -> Result<()> {
    tx.require_system_full("model_hooks::call_register_hook")?;
    let records = tx.env().records(model)?;
    call_method::<(), ()>(tx, &records, REGISTER_HOOK, ()).await
}
