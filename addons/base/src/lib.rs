//! The `base` addon (layer L11): the core addon every database installs.
//!
//! creates the crate. It contributes through `erp-sdk`'s link-time slices, like every
//! native addon. brings `ir.default`, `ir.sequence`, the `ir.model` reflection,
//! `decimal.precision` and `res.company`; the policy sources of the access engine:
//! `ir.access`, groups and privileges ([`res_groups`]), the capability mapping
//! ([`ir_capability`]), the field-groups relation and the membership columns of `res.users`
//! ([`res_users`]); the rest. brings `res.currency`, `res.lang`, and
//! `res.company.currency_id`. brings money and currency services (H41:
//! `res.currency.rate`, rate lookup by date, exact conversion and rounding in [`money`], named
//! decimal precision lookup in [`decimal_precision`]) and the dated parameter table (H35:
//! [`dated_parameter`]). brings `ir.config_parameter`
//! ([`ir_config_parameter`]), passwords, `_check_credentials` and `context_get`
//! ([`res_users_auth`]) and `res.users.log` ([`res_users_log`]). brings the
//! approval policy revisions and their resolution ([`ir_approval_policy`]). The M1 fixture models of
//! the core `base` (`res.users`) move here. Until then [`ADDON`] stands for the
//! core `base` in a registry:
//!
//! ```
//! let registry = erp_sdk::RegistryBuilder::new([&erp_addon_base::ADDON]).build()?;
//! assert!(registry.model("res.users").is_some());
//! assert!(registry.model("res.groups").is_some());
//! assert!(registry.model("res.currency").is_some());
//! assert!(registry.model("res.lang").is_some());
//! # Ok::<(), erp_sdk::Error>(())
//! ```

mod approval_settings;
pub mod attachments;
mod company_lifecycle;
mod company_tree;
pub mod dated_parameter;
pub mod decimal_precision;
pub mod group_graph;
pub mod ir_access;
pub mod ir_actions;
pub mod ir_approval_policy;
pub mod ir_capability;
pub mod ir_config_parameter;
pub mod ir_config_setting;
pub mod ir_cron;
pub mod ir_default;
pub mod ir_filters;
pub mod ir_job;
pub mod ir_model;
pub mod ir_model_fields_groups;
pub mod ir_sequence;
pub mod ir_ui_menu;
pub mod ir_ui_view;
mod jobs_settings;
pub mod jobs_settings_source;
pub mod migrations;
pub mod money;
pub mod partition_upkeep_settings;
mod partner_address;
mod partner_address_budget;
mod partner_address_sync;
mod partner_email;
pub mod partner_hierarchy;
mod partner_labels;
pub mod password_change;
mod policy_sources;
pub mod res_company;
pub mod res_country;
pub mod res_currency;
pub mod res_currency_rate;
pub mod res_currency_rate_type;
pub mod res_groups;
pub mod res_lang;
pub mod res_partner;
pub mod res_users;
pub mod res_users_apikeys;
pub mod res_users_auth;
mod res_users_has_group;
pub mod res_users_log;
pub mod res_users_partner;
pub mod res_users_preferences;
pub mod res_users_settings;
pub mod server_action;
mod settings_general;
mod system_parameters;

pub use dated_parameter::IrDatedParameter;
pub use decimal_precision::*;
pub use ir_access::IrAccess;
pub use ir_actions::IrActionsActions;
pub use ir_approval_policy::IrApprovalPolicy;
pub use ir_capability::IrCapability;
pub use ir_config_parameter::IrConfigParameter;
pub use ir_default::*;
pub use ir_filters::IrFilters;
pub use ir_model::*;
pub use ir_model_fields_groups::IrModelFieldsGroups;
pub use ir_sequence::*;
pub use res_company::*;
pub use res_country::*;
pub use res_currency::*;
pub use res_currency_rate::ResCurrencyRate;
pub use res_currency_rate_type::{ResCurrencyRateType, SPOT_RATE_TYPE};
pub use res_groups::{ResGroups, ResGroupsPrivilege};
pub use res_lang::*;
pub use res_partner::{ResPartner, ResPartnerCategory, ResPartnerIndustry};
pub use res_users::{ResUsersAccess, Role};
pub use res_users_auth::ResUsersAuth;
pub use res_users_log::ResUsersLog;
pub use res_users_partner::ResUsersPartner;
pub use res_users_settings::ResUsersSettings;

/// Country, state, and country group usage, with partner address integration.
pub mod country_guide {}

/// Currency and language addon usage, with executable examples.
pub mod currencies_languages_guide {}

/// Money, currency rates, decimal precision and dated parameters, with executable examples.
pub mod money_currency_guide {}

/// Partner hierarchy integrity and host transaction retry usage.
pub mod partner_hierarchy_guide {}

/// The link-time slice crate, for [`server_action_runner!`]'s output. Not an API.
#[doc(hidden)]
pub use erp_sdk::__private::linkme as __linkme;

erp_sdk::addon!(name = "base");
