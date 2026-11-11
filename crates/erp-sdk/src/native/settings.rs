//! Native settings declarations and link-time contributions.
pub use erp_registry::settings::*;
/// A contribution is bound to the actual addon descriptor emitted in its crate.
#[derive(Debug)]
#[non_exhaustive]
pub struct Contribution {
    /// Native addon identity, including its dependency declarations.
    pub addon: &'static crate::AddonDescriptor,
    /// Pure validated declarations, called when the installed registry is inspected.
    pub declarations: fn() -> crate::Result<Vec<SettingDecl>>,
}
impl Contribution {
    /// Bind a declaration factory to a native addon descriptor.
    pub const fn new(
        addon: &'static crate::AddonDescriptor,
        declarations: fn() -> crate::Result<Vec<SettingDecl>>,
    ) -> Self {
        Self {
            addon,
            declarations,
        }
    }
}
/// Installed composition consumes these entries; merely linking does not install an addon.
#[linkme::distributed_slice]
pub static CONTRIBUTIONS: [Contribution];
/// Register this addon's pure settings factory using its generated `ADDON` identity.
#[macro_export]
#[allow(
    clippy::crate_in_macro_def,
    reason = "the invoking addon crate owns the generated ADDON descriptor"
)]
macro_rules! setting_declarations {
    ($factory:path) => {
        #[$crate::__private::linkme::distributed_slice($crate::settings::CONTRIBUTIONS)]
        #[linkme(crate = $crate::__private::linkme)]
        static SETTINGS_CONTRIBUTION: $crate::settings::Contribution =
            $crate::settings::Contribution::new(&crate::ADDON, $factory);
    };
}
