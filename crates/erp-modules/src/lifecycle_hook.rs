//! Transactional admission shared by install, upgrade, uninstall and initialization.
//!
//! The host supplies these services with [`ModuleSystemBuilder::lifecycle_hook`](crate::ModuleSystemBuilder::lifecycle_hook).
//! They run after package-runtime admission, in registration order, while the module operation
//! still owns its transaction. For example, a host adapter can assemble its web addon's declared
//! assets and publish their manifest atomically with the changed addon set.

use crate::{AddonEntry, PackageChange, Result};
use erp_orm::{BoxFuture, Transaction};

/// The final effective addon set and changes of one operation, before it commits.
///
/// Only the module system constructs this context. Entries retain the immutable filesystem
/// snapshots or separately verified package maps used by the operation, in dependency/load
/// order. Filesystem consumers use [`AddonEntry::admitted_files`], never a later live path read.
/// The entries describe the final registry: removed
/// addons are absent; newly installed addons and unchanged dependencies are present. A source
/// entry is not authorization to expose its files: consumers still validate declarations,
/// paths, content and trust at their own boundary.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct LifecycleChange<'a> {
    change: PackageChange<'a>,
    active_addons: &'a [&'a AddonEntry],
}

impl<'a> LifecycleChange<'a> {
    pub(crate) fn new(change: PackageChange<'a>, active_addons: &'a [&'a AddonEntry]) -> Self {
        Self {
            change,
            active_addons,
        }
    }

    /// The modules installed, upgraded and uninstalled by this operation.
    pub fn change(&self) -> PackageChange<'a> {
        self.change
    }

    /// All addons of the final registry, in the module graph's dependency/load order.
    pub fn active_addons(&self) -> &'a [&'a AddonEntry] {
        self.active_addons
    }
}

/// A trusted host service that admits a changed addon set inside the operation's transaction.
///
/// Initialization and update operations call package-runtime admission first, then every
/// registered service in registration order. A failure prevents later services and rolls back
/// the whole operation. Plain startup loads do not call this hook: compatibility admission at
/// startup uses its own transaction, without implicitly installing or upgrading addons.
///
/// Implementations must keep durable effects in `tx`, and must not commit, roll back or replace
/// it. The module system checks the transaction's identity after each callback. External effects
/// cannot be undone by the module transaction and do not belong here.
pub trait LifecycleHook: Send + Sync {
    /// Admit the final addon set and write any associated rows before publication.
    ///
    /// # Errors
    /// Return an error naming the rejected addon/input or failed storage operation. The error is
    /// returned to the operation's caller; nothing from the operation is committed.
    fn before_commit<'a>(
        &'a self,
        tx: &'a mut Transaction,
        change: LifecycleChange<'a>,
    ) -> BoxFuture<'a, Result<()>>;

    /// Opt into confirmed uninstall by preparing every effect before the engine compares
    /// the fingerprint. Other lifecycle hooks retain the default refusal.
    fn uninstall_planner(&self) -> Option<&dyn UninstallPlanner> {
        None
    }
}

/// One public effect, included in the canonical uninstall fingerprint.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[non_exhaustive]
pub struct UninstallEffect {
    component: String,
    description: String,
    previous: Option<String>,
    next: Option<String>,
}

impl UninstallEffect {
    /// Describe an effect and the optional state identities it replaces and publishes.
    pub fn new(component: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            component: component.into(),
            description: description.into(),
            previous: None,
            next: None,
        }
    }

    /// Bind the effect to the previous and next durable identities.
    #[must_use]
    pub fn identities(mut self, previous: impl Into<String>, next: impl Into<String>) -> Self {
        self.previous = Some(previous.into());
        self.next = Some(next.into());
        self
    }
}

/// An effect prepared from locked database state and admitted addon bytes. Planning does not
/// write, invoke addon hooks or release transaction locks, even in a dry-run preview.
pub trait UninstallPlanner: Send + Sync {
    /// Prepare the exact effect that would execute on confirmation.
    ///
    /// # Errors
    /// A failure to describe or admit the complete effect refuses the operation.
    fn prepare_uninstall<'a>(
        &'a self,
        tx: &'a mut Transaction,
        change: LifecycleChange<'a>,
    ) -> BoxFuture<'a, Result<Box<dyn PreparedUninstall>>>;
}

/// A host effect admitted by the preview and confirmation planner.
pub trait PreparedUninstall: Send {
    /// Describe the exact transaction effect for the operator and the fingerprint.
    fn effect(&self) -> UninstallEffect;

    /// Apply the prepared effect in the same transaction after the fingerprint matched.
    ///
    /// # Errors
    /// An error rolls back the module operation.
    fn apply(self: Box<Self>, tx: &mut Transaction) -> BoxFuture<'_, Result<()>>;
}

impl PreparedUninstall for UninstallEffect {
    fn effect(&self) -> UninstallEffect {
        self.clone()
    }

    fn apply(self: Box<Self>, _tx: &mut Transaction) -> BoxFuture<'_, Result<()>> {
        Box::pin(async { Ok(()) })
    }
}
