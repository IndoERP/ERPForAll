//! Owner/registry admission before mandatory H18 schema and callback publication.
use crate::{ViewError, ViewResult, ViewTypeDecl, ViewTypeDeclFn, validate_view_types};
use erp_registry::{AddonDescriptor, Registry};
use std::sync::Arc;

const MAX_REGISTRATIONS: usize = 256;

/// View declarations contributed by an actual native addon descriptor.
///
/// This is unvalidated input to composition. A declared owner string alone conveys nothing.
#[derive(Debug, Clone, Copy)]
pub struct ViewTypeContribution {
    owner: &'static AddonDescriptor,
    declarations: &'static [ViewTypeDeclFn],
}
impl ViewTypeContribution {
    /// The actual native descriptor that contributes these declarations.
    pub const fn owner(&self) -> &'static AddonDescriptor {
        self.owner
    }
    /// Contribute declarations; binding checks owner identity against the installed registry.
    pub const fn new(
        owner: &'static AddonDescriptor,
        declarations: &'static [ViewTypeDeclFn],
    ) -> Self {
        Self {
            owner,
            declarations,
        }
    }
}

/// Registry/owner-bound intermediate, deliberately without a serving/publication API.
///
/// Schemas, checked callbacks, dependency-scoped selections and caller authorization must still
/// be composed before these declarations can become a published H18 service.
#[derive(Debug)]
pub struct OwnerBoundViewTypes {
    registry: Arc<Registry>,
    declarations: Vec<(&'static AddonDescriptor, ViewTypeDecl)>,
}
impl OwnerBoundViewTypes {
    /// Bind canonical builtins and native owners; no equal-name descriptor substitution.
    pub fn bind(
        registry: Arc<Registry>,
        contributions: &[ViewTypeContribution],
    ) -> ViewResult<Self> {
        if contributions.len() > MAX_REGISTRATIONS {
            return Err(invalid("Too many native view-type contributions"));
        }
        let count = contributions
            .iter()
            .try_fold(0usize, |count, contribution| {
                count
                    .checked_add(contribution.declarations.len())
                    .filter(|count| *count <= MAX_REGISTRATIONS)
            })
            .ok_or_else(|| invalid("Too many view-type registrations"))?;
        let mut declarations = Vec::with_capacity(count);
        for contribution in contributions {
            let installed = registry
                .addons()
                .iter()
                .copied()
                .find(|addon| addon.name == contribution.owner.name)
                .ok_or_else(|| invalid("View-type contributing owner is not installed"))?;
            if !std::ptr::eq(installed, contribution.owner) {
                return Err(invalid(
                    "View-type contributing owner descriptor differs from the actual installed descriptor",
                ));
            }
            for declare in contribution.declarations {
                let declaration = declare()?;
                if declaration.addon() != contribution.owner.name {
                    return Err(invalid(
                        "View-type declared owner differs from its contributing descriptor",
                    ));
                }
                declarations.push((contribution.owner, declaration));
            }
        }
        let types: Vec<&ViewTypeDecl> = declarations
            .iter()
            .map(|(_, declaration)| declaration)
            .collect();
        validate_view_types(&types)?;
        Ok(Self {
            registry,
            declarations,
        })
    }
    /// Refuse use against another exact registry, including one with equal contents.
    pub fn require_registry(&self, registry: &Arc<Registry>) -> ViewResult<()> {
        if Arc::ptr_eq(&self.registry, registry) {
            Ok(())
        } else {
            Err(invalid(
                "View-type registry identity changed; compose a new generation before serving",
            ))
        }
    }
    /// The registry and owner-bound declarations, for publication in this crate.
    pub(crate) fn into_parts(
        self,
    ) -> (Arc<Registry>, Vec<(&'static AddonDescriptor, ViewTypeDecl)>) {
        (self.registry, self.declarations)
    }
    /// Owner-bound declarations for the subsequent mandatory schema/callback composition phase.
    pub fn declarations(
        &self,
    ) -> impl Iterator<Item = (&'static AddonDescriptor, &ViewTypeDecl)> + '_ {
        self.declarations
            .iter()
            .map(|(owner, declaration)| (*owner, declaration))
    }
}
fn invalid(message: &str) -> ViewError {
    ViewError::InvalidDescription(message.to_owned())
}
