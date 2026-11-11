//! Install, upgrade and uninstall hooks (MOD §4.9).
//!
//! An addon names its hooks in `manifest.toml` (`pre_init_hook`, `post_init_hook`,
//! `uninstall_hook`) and provides one [`HookImpl`] per named hook, linked into [`HOOKS`] (the
//! `#[hook]` attribute of `erp-sdk` emits it) or given to
//! [`crate::system::ModuleSystemBuilder::hooks`]. The module system checks the pairing when it is
//! built: a hook named in a manifest without an implementation, two implementations of one hook,
//! or an implementation no manifest names is an error naming the addon, the kind and the name.
//!
//! When hooks run:
//! - `pre_init`: install only, on the registry **without** the module (its models and tables do
//!   not exist yet): an untyped environment, raw SQL through the transaction;
//! - `post_init`: install only, after the module's tables, `init()`, data and demo;
//! - `uninstall`: before the module's tables and data are removed, in reverse load order, on the
//!   registry that still has the module.
//!
//! Every hook runs inside the operation's transaction as the superuser; an error aborts the whole
//! operation (nothing persists).
//!
//! A hook cannot end the transaction: `commit`, `rollback`, `commit_pipelined` and `switch_orm` on
//! it return a `Programming` error. A raw `COMMIT` / `ROLLBACK` / `BEGIN` statement cannot be
//! refused, but the operation checks right after the hook that its transaction is still the one it
//! began and fails with `OperationTransactionEnded` otherwise; whatever the hook committed stays.
//!
//! ```
//! use erp_modules::hooks::{HookImpl, HookKind};
//! use erp_orm::{BoxFuture, Transaction};
//!
//! fn fill_defaults(tx: &mut Transaction) -> BoxFuture<'_, erp_fields::Result<()>> {
//!     Box::pin(async move {
//!         tx.execute("UPDATE my_table SET flag = true WHERE flag IS NULL", &[]).await?;
//!         Ok(())
//!     })
//! }
//!
//! static FILL_DEFAULTS: HookImpl =
//!     HookImpl::new("my_addon", HookKind::PostInit, "fill_defaults", fill_defaults)
//!         .at(file!(), line!());
//! assert_eq!(FILL_DEFAULTS.kind().manifest_key(), "post_init_hook");
//! assert_eq!(FILL_DEFAULTS.name(), "fill_defaults");
//! ```

use erp_orm::{BoxFuture, Transaction};

/// A hook function: runs in the operation's transaction.
pub type HookFn = for<'a> fn(&'a mut Transaction) -> BoxFuture<'a, erp_fields::Result<()>>;

/// The three hook kinds (`post_load` is dropped: no runtime patching).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum HookKind {
    /// `pre_init_hook`.
    PreInit,
    /// `post_init_hook`.
    PostInit,
    /// `uninstall_hook`.
    Uninstall,
}

impl HookKind {
    /// The manifest key naming the hook.
    pub fn manifest_key(self) -> &'static str {
        match self {
            Self::PreInit => "pre_init_hook",
            Self::PostInit => "post_init_hook",
            Self::Uninstall => "uninstall_hook",
        }
    }
}

impl std::fmt::Display for HookKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.manifest_key())
    }
}

/// One hook implementation of one addon.
///
/// Build it in a `static` with the `const` constructor (above, and),
/// so a `linkme` element can hold it, and give its source location with
/// [`HookImpl::at`]`(file!(), line!())`: error messages about the hook point there. A `#[hook]`
/// attribute in `erp-sdk` will generate exactly this expression.
#[derive(Debug)]
pub struct HookImpl {
    pub(crate) addon: &'static str,
    pub(crate) kind: HookKind,
    pub(crate) name: &'static str,
    pub(crate) run: HookFn,
    location: Option<(&'static str, u32)>,
}

impl HookImpl {
    /// The hook `name` of kind `kind` named in the manifest of `addon`, implemented by `run`.
    pub const fn new(addon: &'static str, kind: HookKind, name: &'static str, run: HookFn) -> Self {
        Self {
            addon,
            kind,
            name,
            run,
            location: None,
        }
    }

    /// The source location of the implementation (`file!()`, `line!()`), named in errors.
    pub const fn at(mut self, file: &'static str, line: u32) -> Self {
        self.location = Some((file, line));
        self
    }

    /// The addon whose manifest names the hook.
    pub fn addon(&self) -> &'static str {
        self.addon
    }

    /// Which hook.
    pub fn kind(&self) -> HookKind {
        self.kind
    }

    /// The name used in the manifest.
    pub fn name(&self) -> &'static str {
        self.name
    }

    /// `file:line`, or a note that [`HookImpl::at`] was not called.
    pub(crate) fn location(&self) -> String {
        match self.location {
            Some((file, line)) => format!("{file}:{line}"),
            None => format!(
                "unknown location (HookImpl::at was not called for {}'s {})",
                self.addon, self.name
            ),
        }
    }
}

/// Hook implementations linked into the binary.
#[linkme::distributed_slice]
pub static HOOKS: [HookImpl];

/// Re-exported for macro output.
#[doc(hidden)]
pub use linkme;
