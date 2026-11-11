//! Publication of owner-bound view types for one registry generation (plan WP3b).
//!
//! [`OwnerBoundViewTypes`] only proves that each declaration belongs to the actual installed
//! descriptor that contributed it. Publication adds what a set of declarations needs before a
//! generation may serve them: the installed dependency closure ([`InstalledClosure`]), data-method
//! bindings resolved by the host, and the reconciliation of the `ir.ui.view.type` and
//! `ir.actions.act_window.view.view_mode` selection values. The result is immutable and bound to the
//! exact registry it was checked against; a selection value alone never authorizes serving.
//!
//! This module is pure: it has no database, ORM or request type. The native module system builds
//! the inputs and attaches the product to the ORM generation (`erp-modules`).

use crate::registry::{is_valid_view_type_key, reserved_owner};
use crate::view_type_decl::{located, shown};
use crate::{OwnerBoundViewTypes, ViewError, ViewResult, ViewTypeDecl};
use erp_registry::{AddonDescriptor, Registry};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

/// Most installed addons one closure may describe.
const MAX_INSTALLED: usize = 4096;

/// The selection fields whose view-type values are reconciled with the published keys.
const SELECTION_FIELDS: [(&str, &str); 2] = [
    ("ir.ui.view", "type"),
    ("ir.actions.act_window.view", "view_mode"),
];

/// The base template kind: reserved to `base`, not a client data-view renderer.
const TEMPLATE_KIND: &str = "qweb";

fn invalid(message: String) -> ViewError {
    ViewError::InvalidDescription(message)
}

/// The installed addons of one generation with the transitive dependency closure of each.
///
/// Built from the module system's selected modules and their manifest dependencies, not from the
/// registry's implicit descriptors; publication cross-checks it against the registry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledClosure {
    closures: BTreeMap<String, BTreeSet<String>>,
}

impl InstalledClosure {
    /// Build the closure of `(addon, direct dependencies)` pairs.
    ///
    /// # Errors
    /// An addon listed twice, more than 4,096 addons, or a dependency that is not itself listed.
    pub fn new(addons: &[(&str, &[&str])]) -> ViewResult<Self> {
        if addons.len() > MAX_INSTALLED {
            return Err(invalid(format!(
                "Too many installed addons: {} (at most {MAX_INSTALLED})",
                addons.len()
            )));
        }
        let mut direct: BTreeMap<&str, &[&str]> = BTreeMap::new();
        for (name, depends) in addons {
            if direct.insert(name, depends).is_some() {
                return Err(invalid(format!(
                    "Installed addon {} is listed twice",
                    shown(name)
                )));
            }
        }
        for (name, depends) in addons {
            if let Some(missing) = depends.iter().find(|dep| !direct.contains_key(**dep)) {
                return Err(invalid(format!(
                    "Installed addon {} depends on {}, which is not in the installed set",
                    shown(name),
                    shown(missing)
                )));
            }
        }
        let mut closures = BTreeMap::new();
        for (name, _) in addons {
            let mut seen: BTreeSet<String> = BTreeSet::new();
            let mut pending = vec![*name];
            while let Some(current) = pending.pop() {
                if seen.insert(current.to_owned()) {
                    pending.extend(direct[current].iter().copied());
                }
            }
            closures.insert((*name).to_owned(), seen);
        }
        Ok(Self { closures })
    }

    /// Whether `addon` is installed.
    pub fn contains(&self, addon: &str) -> bool {
        self.closures.contains_key(addon)
    }

    /// The installed addon names, sorted.
    pub fn names(&self) -> impl Iterator<Item = &str> + '_ {
        self.closures.keys().map(String::as_str)
    }

    /// `addon` and everything it depends on, transitively; `None` when `addon` is not installed.
    pub fn closure_of(&self, addon: &str) -> Option<&BTreeSet<String>> {
        self.closures.get(addon)
    }
}

/// The immutable H18 view-type table of one registry generation.
#[derive(Debug)]
pub struct PublishedViewTypes {
    registry: Arc<Registry>,
    closure: InstalledClosure,
    declarations: BTreeMap<String, (&'static AddonDescriptor, ViewTypeDecl)>,
}

impl PublishedViewTypes {
    /// Publish `bound` against its registry and the installed `closure`.
    ///
    /// `method_exposed(scope, name)` answers whether the data method `name` is exposed on a model
    /// the declaring addon's dependency closure `scope` declares; the host supplies it from the
    /// ORM's final exposure table, since this crate has no ORM.
    ///
    /// # Errors
    /// The installed set differs from the registry's addons; a declaration binds a data method the
    /// host cannot resolve; a selection value of `ir.ui.view.type` or
    /// `ir.actions.act_window.view.view_mode` is contributed by an addon that does not own the key,
    /// or is neither a reserved bare key nor namespaced.
    pub fn publish(
        bound: OwnerBoundViewTypes,
        closure: InstalledClosure,
        mut method_exposed: impl FnMut(&BTreeSet<String>, &str) -> bool,
    ) -> ViewResult<Self> {
        let (registry, bound) = bound.into_parts();
        for addon in registry.addons() {
            if !closure.contains(addon.name) {
                return Err(invalid(format!(
                    "Registry addon {} is not in the installed dependency closure",
                    shown(addon.name)
                )));
            }
        }
        let in_registry: BTreeSet<&str> = registry.addons().iter().map(|a| a.name).collect();
        if let Some(extra) = closure.names().find(|name| !in_registry.contains(name)) {
            return Err(invalid(format!(
                "Installed addon {} is not in the registry",
                shown(extra)
            )));
        }
        let mut declarations = BTreeMap::new();
        for (owner, decl) in bound {
            let scope = closure.closure_of(owner.name).ok_or_else(|| {
                invalid(format!(
                    "Addon {} is not in the installed set",
                    shown(owner.name)
                ))
            })?;
            for method in decl.methods() {
                if !method_exposed(scope, method) {
                    return Err(located(
                        &format!("binds the unknown data method {}", shown(method)),
                        decl.addon(),
                        decl.key(),
                    ));
                }
            }
            declarations.insert(decl.key().to_owned(), (owner, decl));
        }
        let published = Self {
            registry,
            closure,
            declarations,
        };
        published.reconcile_selections()?;
        Ok(published)
    }

    /// The addon that owns a published key, as its actual installed descriptor.
    pub fn owner(&self, key: &str) -> Option<&'static AddonDescriptor> {
        self.declarations.get(key).map(|(owner, _)| *owner)
    }

    /// A published declaration by key.
    pub fn find(&self, key: &str) -> Option<&ViewTypeDecl> {
        self.declarations.get(key).map(|(_, decl)| decl)
    }

    /// Every published `(owner, declaration)`, sorted by key.
    pub fn declarations(
        &self,
    ) -> impl Iterator<Item = (&'static AddonDescriptor, &ViewTypeDecl)> + '_ {
        self.declarations
            .values()
            .map(|(owner, decl)| (*owner, decl))
    }

    /// The published keys whose owner is in the dependency closure of `addon`, sorted; empty when
    /// `addon` is not installed.
    pub fn keys_visible_to(&self, addon: &str) -> Vec<&str> {
        let Some(scope) = self.closure.closure_of(addon) else {
            return Vec::new();
        };
        self.declarations
            .iter()
            .filter(|(_, (owner, _))| scope.contains(owner.name))
            .map(|(key, _)| key.as_str())
            .collect()
    }

    /// Check that a view record or action mode of `declaring_addon` may use `key`:
    /// the key's owner is in the addon's installed dependency closure (also for reserved keys that
    /// nothing registered) and the key is published. The framework builtins `list`, `form` and
    /// `search` need no dependency on `web` for record admission (lead clarification
    /// A20261103T192149Z-df3a1b); their serving paths still require a published declaration.
    /// The template kind `qweb` is not a data view type and always passes.
    ///
    /// # Errors
    /// Unknown addon, malformed or unknown bare key, owner outside the closure, or an unpublished key.
    pub fn require_key_for(&self, declaring_addon: &str, key: &str) -> ViewResult<()> {
        let scope = self.closure.closure_of(declaring_addon).ok_or_else(|| {
            invalid(format!(
                "Addon {} is not in the installed set",
                shown(declaring_addon)
            ))
        })?;
        if matches!(key, "list" | "form" | "search") || key == TEMPLATE_KIND {
            return Ok(());
        }
        let owner = key_owner(key)?;
        if !scope.contains(owner) {
            return Err(invalid(format!(
                "View-type key {} belongs to addon {}, which is not in the installed dependency closure of addon {}",
                shown(key),
                shown(owner),
                shown(declaring_addon)
            )));
        }
        if self.find(key).is_none() {
            return Err(invalid(format!(
                "View-type key {} is not registered in the published view types",
                shown(key)
            )));
        }
        Ok(())
    }

    /// Refuse use against another exact registry, including one with equal contents.
    pub fn require_registry(&self, registry: &Arc<Registry>) -> ViewResult<()> {
        if Arc::ptr_eq(&self.registry, registry) {
            Ok(())
        } else {
            Err(invalid(
                "View-type registry identity changed; compose a new generation before serving"
                    .to_owned(),
            ))
        }
    }

    /// Only the owner of a key contributes its `ir.ui.view.type` and `view_mode` selection value.
    /// The static values of `base` are the compatibility baseline and are not additions.
    fn reconcile_selections(&self) -> ViewResult<()> {
        for (model, field) in SELECTION_FIELDS {
            let Some(spec) = self.registry.model(model) else {
                continue;
            };
            for addon in self.closure.names() {
                for (name, values) in spec.selection_additions_from_addons(&[addon]) {
                    if name != field {
                        continue;
                    }
                    for value in values {
                        if value == TEMPLATE_KIND {
                            continue;
                        }
                        let owner = key_owner(value)?;
                        if owner != addon {
                            return Err(invalid(format!(
                                "Addon {} adds view-type key {} to {model}.{field}, but only its owner {} may contribute it",
                                shown(addon),
                                shown(value),
                                shown(owner)
                            )));
                        }
                        if self.find(value).is_none() {
                            return Err(invalid(format!(
                                "Addon {} adds view-type key {} to {model}.{field} without publishing its H18 declaration",
                                shown(addon),
                                shown(value)
                            )));
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

/// The addon that owns `key`: the namespace of `<addon>.<name>`, or the reserved bare-key table.
fn key_owner(key: &str) -> ViewResult<&str> {
    if !is_valid_view_type_key(key) {
        return Err(invalid(format!(
            "View-type key {} is not a valid view-type key",
            shown(key)
        )));
    }
    if let Some((addon, _)) = key.split_once('.') {
        return Ok(addon);
    }
    reserved_owner(key).ok_or_else(|| {
        invalid(format!(
            "View-type key {} is neither a reserved bare key nor in an addon namespace",
            shown(key)
        ))
    })
}
