//! H18 view-type publication for each ORM generation (plan WP3b).
//!
//! The module system attaches a [`PublishedViewTypes`] table to every ORM it builds, at the root
//! and at every stage, through the ORM's generation attachment factory. The factory's owned inputs
//! are the native contributions and the installed dependency closure of the module set being
//! composed; the factory runs against the final exposure table before the ORM escapes, so a
//! refused table never publishes a candidate and never replaces the previous generation's table.
//! Rebuilds inherit nothing: every composition passes a new factory for its own registry.

use crate::error::Error;
use crate::manifest::ModuleName;
use erp_fields::Result as OrmResult;
use erp_orm::rpc::Exposure;
use erp_orm::{GenerationAttachmentFactory, GenerationComposition};
use erp_registry::Registry;
use erp_views::{
    InstalledClosure, OwnerBoundViewTypes, PublishedViewTypes, ViewError, ViewTypeContribution,
};
use std::collections::BTreeSet;
use std::sync::Arc;

/// Owned, immutable inputs of one generation's view-type composition.
///
/// Opaque: build it with [`view_type_factory`]. It holds the native contributions (actual
/// descriptors, never owner strings) and the installed dependency closure.
pub struct ViewTypeInputs {
    contributions: Vec<ViewTypeContribution>,
    closure: InstalledClosure,
}

/// A factory that publishes the view types of `contributions` for `registry`.
///
/// Contributions of addons that are not installed in this generation are left out; one whose
/// descriptor merely has an installed addon's name is refused at composition. `closure` must
/// describe exactly the registry's addons.
pub fn view_type_factory(
    registry: &Arc<Registry>,
    contributions: Vec<ViewTypeContribution>,
    closure: InstalledClosure,
) -> GenerationAttachmentFactory<PublishedViewTypes, ViewTypeInputs> {
    GenerationAttachmentFactory::new(
        Arc::clone(registry),
        Arc::new(ViewTypeInputs {
            contributions,
            closure,
        }),
        compose,
    )
}

fn refused(error: &ViewError) -> erp_fields::Error {
    erp_fields::Error::programming(format!("View-type publication refused: {error}"))
}

fn compose(
    context: &GenerationComposition<'_>,
    inputs: &ViewTypeInputs,
) -> OrmResult<Arc<PublishedViewTypes>> {
    let registry = context.registry();
    let installed: BTreeSet<&str> = registry.addons().iter().map(|addon| addon.name).collect();
    let present: Vec<ViewTypeContribution> = inputs
        .contributions
        .iter()
        .filter(|contribution| installed.contains(contribution.owner().name))
        .copied()
        .collect();
    let bound =
        OwnerBoundViewTypes::bind(Arc::clone(registry), &present).map_err(|e| refused(&e))?;
    let exposure = context.exposure();
    let published = PublishedViewTypes::publish(bound, inputs.closure.clone(), |scope, method| {
        registry.models().iter().any(|spec| {
            spec.mro.iter().any(|decl| scope.contains(decl.addon))
                && matches!(
                    exposure.get(spec.id, method),
                    Some(Exposure::Exposed(exposed)) if scope.contains(exposed.effect_from().addon())
                )
        })
    })
    .map_err(|e| refused(&e))?;
    Ok(Arc::new(published))
}

/// Refuse H18 registrations that name a package addon as owner: the guest H18 contract has no published WIT
/// mapping in this slice, so the attempt is an explicit `Unsupported`, never accepted and ignored.
pub(crate) fn refuse_package_owners(
    contributions: &[ViewTypeContribution],
    is_package: impl Fn(&str) -> bool,
) -> Result<(), Error> {
    for contribution in contributions {
        let name = contribution.owner().name;
        if is_package(name) {
            return Err(Error::GuestViewTypesUnsupported {
                module: ModuleName::new(name)?,
            });
        }
    }
    Ok(())
}

/// A pair `(name, direct dependencies)` for [`InstalledClosure::new`].
pub(crate) fn closure_of(pairs: &[(&str, &[&str])]) -> Result<InstalledClosure, Error> {
    InstalledClosure::new(pairs).map_err(|e| Error::Orm(refused(&e)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use erp_registry::{AddonDescriptor, WEB_ADDON};

    static PACKAGE_OWNER: AddonDescriptor = AddonDescriptor {
        name: "pkg_addon",
        depends: &["web"],
        exact_only: false,
    };
    static NONE: [erp_views::ViewTypeDeclFn; 0] = [];

    /// addon; menu-contribution and fact declarations have no host entry point yet)
    #[test]
    fn a_package_owner_of_h18_registrations_names_the_completion_task() {
        let contributions = [
            ViewTypeContribution::new(&WEB_ADDON, &NONE),
            ViewTypeContribution::new(&PACKAGE_OWNER, &NONE),
        ];
        let error = refuse_package_owners(&contributions, |name| name == "pkg_addon").unwrap_err();
        assert!(matches!(
            &error,
            Error::GuestViewTypesUnsupported { module } if module.as_str() == "pkg_addon"
        ));
        assert_eq!(
            error.to_string(),
            "Module \"pkg_addon\": H18 view-type registrations are not supported for package addons (Unsupported: the guest H18 registration contract is deferred to a later release)"
        );
        refuse_package_owners(&contributions[..1], |name| name == "pkg_addon").unwrap();
    }
}
