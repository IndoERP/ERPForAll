//! The seam between the module system and the runtime of package addons.
//!
//! A `.erpaddon` package carries no Rust the module system could link. What it declares (models
//! and fields, WebAssembly method layers) comes from a runtime the host program installs with
//! [`ModuleSystemBuilder::package_runtime`](crate::system::ModuleSystemBuilder::package_runtime).
//! The module system stays in charge of everything else: the catalog, dependency checks, schema
//! sync, the operation lock, the registry-change signal and the rollback. This trait has no
//! WebAssembly type in it; [`crate::wasm_lifecycle`] is the implementation for components.

use erp_orm::{BoxFuture, ComputeImpl, ConstraintImpl, Orm, Transaction, WasmMethodImpl};
use erp_registry::{ModelDef, Registry};

use crate::error::Result;
use crate::manifest::ModuleName;

/// The modules one operation installed, upgraded and uninstalled, as the runtime sees them just
/// before the operation commits.
#[derive(Debug, Clone, Copy)]
pub struct PackageChange<'a> {
    /// Modules installed by the operation.
    pub installed: &'a [ModuleName],
    /// Modules upgraded by the operation (dependents included).
    pub upgraded: &'a [ModuleName],
    /// Modules uninstalled by the operation (dependents included).
    pub uninstalled: &'a [ModuleName],
}

/// The computes and constraints of the package addons among a module set, as
/// implementations the ORM's compute graph takes.
#[derive(Debug, Default)]
pub struct PackageChecks {
    /// Compute implementations.
    pub computes: Vec<&'static ComputeImpl>,
    /// Constraint declarations.
    pub constraints: Vec<&'static ConstraintImpl>,
}

impl PackageChecks {
    /// Nothing declared.
    pub fn is_empty(&self) -> bool {
        self.computes.is_empty() && self.constraints.is_empty()
    }
}

/// What the module system asks of the runtime of package addons.
pub trait PackageRuntime: Send + Sync {
    /// The models and fields `module` (a package addon) declares, for the registry of a module
    /// set that contains it.
    ///
    /// # Errors
    /// The module is a package addon this runtime has not loaded: the error names the call that
    /// loads it. Never an empty list standing in for "unknown".
    fn model_defs(&self, module: &ModuleName) -> Result<Vec<ModelDef>>;

    /// The WebAssembly method layers of the package addons among `modules`, to give the ORM built
    /// for that set. `plain` is that set's ORM without them (its exposure table is what a guest's
    /// `call-method` may reach).
    ///
    /// # Errors
    /// As [`PackageRuntime::model_defs`].
    fn wasm_methods(&self, plain: &Orm, modules: &[ModuleName]) -> Result<Vec<WasmMethodImpl>>;

    /// The computes and constraints of the package addons among `modules`. The default declares
    /// none, so a runtime without them needs no change.
    ///
    /// # Errors
    /// As [`PackageRuntime::model_defs`]; also a name or a count the ORM refuses.
    fn checks(&self, _modules: &[ModuleName]) -> Result<PackageChecks> {
        Ok(PackageChecks::default())
    }

    /// Checks the typed model references (`model_ref!` with fields) of the package addons among
    /// `modules` against `registry`, the registry composed for that module set: every model and
    /// field they name must exist with a kind and comodel the Rust type can hold. The default
    /// checks nothing, so a runtime without references needs no change.
    ///
    /// # Errors
    /// A reference that does not match: the error names the addon, the model and the field.
    fn check_model_refs(&self, _registry: &Registry, _modules: &[ModuleName]) -> Result<()> {
        Ok(())
    }

    /// Called inside the operation's transaction, after the schema, the data and the registry
    /// signal, just before the commit. An error rolls the whole operation back, so this is where
    /// the runtime stores its rows (the package, the audit record) and refuses a change it did
    /// not expect.
    ///
    /// # Errors
    /// Any error refuses the operation.
    fn before_commit<'a>(
        &'a self,
        tx: &'a mut Transaction,
        change: PackageChange<'a>,
    ) -> BoxFuture<'a, Result<()>>;
}
