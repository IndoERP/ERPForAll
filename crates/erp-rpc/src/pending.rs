use std::collections::BTreeMap;

use erp_fields::{Error, Result};

/// (model, method, task); model `base` applies to every model.
pub(crate) const CORE_PENDING: &[(&str, &str, &str)] = &[
    ("base", "check_field_access", "a later release"),
    ("base", "get_base_url", "a later release"),
    ("base", "get_empty_list_help", "a later release"),
    ("base", "get_external_id", "a later release"),
    ("base", "get_field_translations", "a later release"),
    ("base", "get_formview_id", "a later release"),
    ("base", "get_metadata", "a later release"),
    ("base", "get_property_definition", "a later release"),
    ("base", "get_record_default_action", "a later release"),
    ("base", "get_view", "a later release"),
    ("base", "get_views", "a later release"),
    ("base", "has_field_access", "a later release"),
    ("base", "update_field_translations", "a later release"),
    ("base", "view_header_get", "a later release"),
    ("base", "web_override_translations", "a later release"),
    ("res.users", "action_create_passkey", "a later release"),
    ("res.users", "action_revoke_all_devices", "a later release"),
    ("res.users", "action_show_accesses", "a later release"),
    ("res.users", "action_show_groups", "a later release"),
    ("res.users", "action_totp_disable", "a later release"),
    ("res.users", "action_totp_enable_wizard", "a later release"),
    ("res.users", "api_key_wizard", "a later release"),
    ("res.users", "authenticate", "a later release"),
    ("res.users", "get_company_currency_id", "a later release"),
    ("res.users", "has_groups", "a later release"),
    ("res.users", "on_change_login", "a later release"),
    ("res.users", "onchange_parent_id", "a later release"),
    ("res.users", "revoke_all_devices", "a later release"),
    ("res.users", "switch_tour_enabled", "a later release"),
    ("res.users", "web_create_users", "a later release"),
];

/// Pending methods: (model, method) → the task that adds them.
///
/// ```
/// use erp_rpc::PendingMethods;
/// let pending = PendingMethods::core();
/// assert_eq!(pending.lookup("res.partner", "copy"), None);
/// assert_eq!(pending.lookup("res.partner", "search"), None);
/// ```
#[derive(Debug, Clone, Default)]
pub struct PendingMethods {
    entries: BTreeMap<(String, String), String>,
}

impl PendingMethods {
    /// No pending method.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Core's table.
    pub fn core() -> Self {
        let mut out = Self::empty();
        for (model, method, task) in CORE_PENDING {
            out.entries.insert(
                ((*model).to_owned(), (*method).to_owned()),
                (*task).to_owned(),
            );
        }
        out
    }

    /// Add an entry (an addon's own pending methods); `model` `base` applies to every model.
    ///
    /// # Errors
    ///
    /// `Programming` for an empty name or task, or an entry listed twice.
    pub fn add(&mut self, model: &str, method: &str, task: &str) -> Result<()> {
        if model.is_empty() || method.is_empty() || task.is_empty() {
            return Err(Error::programming(
                "a pending method needs a model, a method and a task",
            ));
        }
        let key = (model.to_owned(), method.to_owned());
        if self.entries.contains_key(&key) {
            return Err(Error::programming(format!(
                "pending method {model}.{method} is listed twice"
            )));
        }
        self.entries.insert(key, task.to_owned());
        Ok(())
    }

    /// The task of `model.method`, or of `base.method`.
    pub fn lookup(&self, model: &str, method: &str) -> Option<&str> {
        self.entries
            .get(&(model.to_owned(), method.to_owned()))
            .or_else(|| self.entries.get(&("base".to_owned(), method.to_owned())))
            .map(String::as_str)
    }

    /// Every entry, sorted: (model, method, task).
    pub fn entries(&self) -> impl Iterator<Item = (&str, &str, &str)> {
        self.entries
            .iter()
            .map(|((m, f), t)| (m.as_str(), f.as_str(), t.as_str()))
    }
}
