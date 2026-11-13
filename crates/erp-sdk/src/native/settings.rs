//! Native settings declarations and link-time contributions.
pub use erp_registry::settings::*;
/// The scope a panel resolver describes (re-exported from the settings wire types).
pub use erp_views::wire::SettingsScope;
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

/// Computes the value of one of an addon's panel items.
///
/// Called with the caller's own environment (never elevated), the request transaction (inside a savepoint), the
/// described scope and the item key. Returns [`PanelItemValue::info`] for an information row and
/// [`PanelItemValue::record`] for a record link. A key the addon did not declare is a programming error; an
/// `Access` error leaves the item out for this caller; any other error fails the description.
pub type PanelResolver = for<'a> fn(
    &'a crate::Env,
    &'a mut crate::Transaction,
    &'a crate::views::wire::SettingsScope,
    &'a str,
) -> crate::BoxFuture<'a, crate::Result<PanelItemValue>>;

/// An addon's information and link rows with the resolver that computes their values.
#[non_exhaustive]
pub struct PanelContribution {
    /// Native addon identity, including its dependency declarations.
    pub addon: &'static crate::AddonDescriptor,
    /// Pure validated declarations, called when the installed registry is published.
    pub declarations: fn() -> crate::Result<Vec<PanelItemDecl>>,
    /// Value resolver for the information rows and record links among `declarations`.
    pub resolve: PanelResolver,
}
impl std::fmt::Debug for PanelContribution {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PanelContribution")
            .field("addon", &self.addon.name)
            .finish_non_exhaustive()
    }
}
impl PanelContribution {
    /// Bind a declaration factory and its resolver to a native addon descriptor.
    pub const fn new(
        addon: &'static crate::AddonDescriptor,
        declarations: fn() -> crate::Result<Vec<PanelItemDecl>>,
        resolve: PanelResolver,
    ) -> Self {
        Self {
            addon,
            declarations,
            resolve,
        }
    }
}
/// Panel item contributions linked into the application; only installed addons' entries are published.
///
/// A link-time collection: an entry nothing references can be dropped by the linker. The consumer
/// (`erp_addon_base::ir_config_setting`) checks the entries it expects and refuses a registry that lacks one.
#[linkme::distributed_slice]
pub static PANEL_CONTRIBUTIONS: [PanelContribution];
/// Register this addon's panel items and their resolver using its generated `ADDON` identity.
///
/// ```rust,ignore
/// use erp_sdk::settings::{PanelItemDecl, PanelItemValue, SettingsScope};
/// fn items() -> erp_sdk::Result<Vec<PanelItemDecl>> {
///     Ok(vec![PanelItemDecl::info("my_addon.open_orders", "Open Orders")?.access(&["base.group_system"])?])
/// }
/// fn resolve<'a>(
///     env: &'a erp_sdk::Env,
///     tx: &'a mut erp_sdk::Transaction,
///     _scope: &'a SettingsScope,
///     key: &'a str,
/// ) -> erp_sdk::BoxFuture<'a, erp_sdk::Result<PanelItemValue>> {
///     Box::pin(async move {
///         if key != "my_addon.open_orders" {
///             return Err(erp_sdk::Error::programming(format!("my_addon does not declare {key}")));
///         }
///         // Counted with `env`: the caller's own rights, record rules included.
///         let open = env.records("my_addon.order")?.search_count(tx, &erp_sdk::Domain::TRUE, None).await?;
///         let open = i64::try_from(open).map_err(|_| erp_sdk::Error::internal("count does not fit"))?;
///         PanelItemValue::info(erp_sdk::Value::Int(open))
///     })
/// }
/// erp_sdk::settings_panel_items!(items, resolve);
/// ```
#[macro_export]
#[allow(
    clippy::crate_in_macro_def,
    reason = "the invoking addon crate owns the generated ADDON descriptor"
)]
macro_rules! settings_panel_items {
    ($factory:path, $resolver:path) => {
        #[$crate::__private::linkme::distributed_slice($crate::settings::PANEL_CONTRIBUTIONS)]
        #[linkme(crate = $crate::__private::linkme)]
        static SETTINGS_PANEL_CONTRIBUTION: $crate::settings::PanelContribution =
            $crate::settings::PanelContribution::new(&crate::ADDON, $factory, $resolver);
    };
}
