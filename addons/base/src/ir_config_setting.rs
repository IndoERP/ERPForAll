//! Declared settings, durable exact-scope CAS and transactional audit.
//!
//! Only this service writes storage; ordinary ORM grants cover the facade and audit reads.
//! There is no settings value cache: every node reads committed PostgreSQL state.
use erp_sdk::prelude::*;
use erp_sdk::serde_json::{self, Value as Json};
use erp_sdk::settings::{
    LinkTarget, PanelItemDecl, PanelItemKind, PanelItemValue, PanelResolver, ScopeKind,
    SettingDecl, SettingKind, SettingsRegistry,
};
use erp_sdk::views::wire::{self, SettingsScope, SettingsValues};
use std::collections::{BTreeMap, BTreeSet};

fn denied(key: &str) -> Error {
    Error::access(format!("Setting {key} is unavailable or access is denied."))
}
const CONFLICT: &str = "Settings changed; reload before saving.";
const MAX_BATCH: usize = 256;
const MAX_STORED_VALUE_BYTES: usize = 400_000;

// the Settings writer already owns the global scope lock and records the audit.
// Its parameter write must not recursively advance that same revision.
#[derive(Default)]
struct ParameterBridge(bool);

pub(crate) struct ParameterEdit {
    scope_id: i64,
    revision: u64,
    // the managed attachment size before the edit, when the edit touches it
    old_max: Option<u64>,
    // each touched declared parameter with its stored text before the edit
    old_declared: Vec<(String, Option<String>)>,
}

/// The stored text of each of `keys` (`None` for no row or NULL).
async fn parameter_texts(
    tx: &mut Transaction,
    keys: &[String],
) -> Result<Vec<(String, Option<String>)>> {
    if keys.is_empty() {
        return Ok(Vec::new());
    }
    let model = tx.env().records("ir.config_parameter")?.model_id();
    tx.flush_model(model).await?;
    let rows = tx
        .query(
            "SELECT key::text, value::text FROM ir_config_parameter WHERE key=ANY($1::text[])",
            &[&keys],
        )
        .await?;
    let mut found = BTreeMap::new();
    for row in &rows {
        let key: String = row.try_get(0).map_err(|_| corrupt())?;
        let value: Option<String> = row.try_get(1).map_err(|_| corrupt())?;
        found.insert(key, value);
    }
    Ok(keys
        .iter()
        .map(|key| (key.clone(), found.get(key).cloned().flatten()))
        .collect())
}

/// Serialise a parameter write on the global scope row (for the managed
/// attachment size): `managed` when it touches `base.attachment_max_bytes`, `declared` the
/// declared parameters it touches. `None` inside the Settings writer, which owns the lock.
pub(crate) async fn begin_parameter_edit(
    tx: &mut Transaction,
    managed: bool,
    declared: &[String],
) -> Result<Option<ParameterEdit>> {
    if tx.extension::<ParameterBridge>().0 || (!managed && declared.is_empty()) {
        return Ok(None);
    }
    tx.execute("INSERT INTO ir_config_setting_scope(identity,revision) VALUES ('global','0') ON CONFLICT(identity) DO NOTHING", &[]).await.map_err(scope_lock_error)?;
    let rows = tx.query("SELECT id::int8,revision FROM ir_config_setting_scope WHERE identity='global' FOR UPDATE", &[]).await.map_err(scope_lock_error)?;
    let row = rows.first().ok_or_else(corrupt)?;
    let current: String = row.try_get(1).map_err(|_| corrupt())?;
    let old_max = if managed {
        Some(crate::attachments::settings::max_file_bytes(tx).await?)
    } else {
        None
    };
    Ok(Some(ParameterEdit {
        scope_id: row.try_get(0).map_err(|_| corrupt())?,
        revision: revision(&current)?,
        old_max,
        old_declared: parameter_texts(tx, declared).await?,
    }))
}

fn audit_actor(env: &Env) -> Result<String> {
    match env.principal().actor() {
        Some(erp_sdk::Actor::ApiKey { id, .. }) => Ok(format!("api_key:{}", id.get())),
        Some(erp_sdk::Actor::Impersonator(principal)) => {
            Ok(format!("user:{}", principal.subject().get()))
        }
        Some(erp_sdk::Actor::Link { id }) => Ok(format!("link:{}", id.get())),
        None => Ok(format!("user:{}", env.uid())),
        Some(_) => Err(Error::validation(
            "Unsupported audit actor; requires a later release actor mapping.",
        )),
    }
}

/// The audit text of a declared secret: the row records that it changed, never the value.
const SECRET_AUDIT_TEXT: &str = "(secret)";

/// Advance the global revision and write one audit row per parameter whose effective value the
/// edit changed; nothing when none did (default data, a no-op, an emptied value equal to the
/// default).
pub(crate) async fn finish_parameter_edit(
    tx: &mut Transaction,
    edit: Option<ParameterEdit>,
) -> Result<()> {
    let Some(edit) = edit else {
        return Ok(());
    };
    let mut changes: Vec<(String, String, String)> = Vec::new();
    if let Some(old) = edit.old_max {
        let new = crate::attachments::settings::max_file_bytes(tx).await?;
        if old != new {
            changes.push((
                crate::attachments::settings::MAX_FILE_KEY.to_owned(),
                old.to_string(),
                new.to_string(),
            ));
        }
    }
    if !edit.old_declared.is_empty() {
        let params = erp_sdk::params::registry(&tx.env())?;
        let keys: Vec<String> = edit
            .old_declared
            .iter()
            .map(|(key, _)| key.clone())
            .collect();
        let now = parameter_texts(tx, &keys).await?;
        for ((key, old), (_, new)) in edit.old_declared.iter().zip(now) {
            let decl = params
                .get(key)
                .ok_or_else(|| Error::programming(format!("Parameter {key} is not declared.")))?;
            // the write path checked the new text; an old text written around the model is named so
            let old = decl
                .effective_text(old.as_deref())
                .unwrap_or_else(|_| "(invalid)".to_owned());
            let new = decl.effective_text(new.as_deref())?;
            if old != new {
                let (old, new) = if decl.is_secret() {
                    (SECRET_AUDIT_TEXT.to_owned(), SECRET_AUDIT_TEXT.to_owned())
                } else {
                    (old, new)
                };
                changes.push((key.clone(), old, new));
            }
        }
    }
    if changes.is_empty() {
        return Ok(());
    }
    let next = edit
        .revision
        .checked_add(1)
        .ok_or_else(|| Error::validation("Settings revision exhausted."))?
        .to_string();
    let env = tx.env();
    let actor = audit_actor(&env)?;
    let subject = env.uid().to_string();
    for (key, old, new) in &changes {
        tx.execute("INSERT INTO ir_config_setting_audit(scope_id,actor,subject_uid,setting_key,old_value,new_value,revision,create_date) VALUES ($1,$2,$3,$4,$5,$6,$7,CURRENT_TIMESTAMP AT TIME ZONE 'UTC')", &[&edit.scope_id,&actor,&subject,key,old,new,&next]).await?;
    }
    tx.execute(
        "UPDATE ir_config_setting_scope SET revision=$2 WHERE id=$1",
        &[&edit.scope_id, &next],
    )
    .await?;
    tx.invalidate_ormcache("declared_settings");
    Ok(())
}

/// Public settings service facade; storage lives in models with no user write grants.
#[model(name = "ir.config.setting", description = "Settings")]
pub struct IrConfigSetting {}

/// Private scoped scalar storage. Direct CRUD has no user grants.
#[model(name = "ir.config.setting.value", description = "Declared Setting Value", company_access = parent(edge = "scope_id"), constraints = [
    ("_scope_key_unique", "UNIQUE (scope_id, setting_key)", "Setting keys must be unique in their scope.")
])]
pub struct IrConfigSettingValue {
    /// Durable exact scope.
    #[field(
        many2one,
        comodel = "ir.config.setting.scope",
        required = true,
        ondelete = "restrict"
    )]
    pub scope_id: Option<RecId>,
    /// Registered addon-qualified key.
    #[field(char, required = true)]
    pub setting_key: Option<Arc<str>>,
    /// JSON scalar text, always checked against the active declaration on read.
    #[field(text, required = true)]
    pub value_text: Option<Arc<str>>,
}
/// One durable writer lock and monotonic revision per exact scope.
#[model(name = "ir.config.setting.scope", description = "Settings Scope", company_access = direct(field = "company_id", branch = "exact", empty = "allow"), constraints = [
    ("_identity_unique", "UNIQUE (identity)", "Settings scope must be unique."),
    ("_identity_company", "CHECK ((company_id IS NULL AND identity = 'global') OR (company_id IS NOT NULL AND identity = 'company:' || company_id::text))", "Settings scope identity must match its company."),
    ("_revision_canonical", "CHECK (revision ~ '^(0|[1-9][0-9]{0,19})$')", "Settings revision must be canonical.")
])]
pub struct IrConfigSettingScope {
    /// Unique exact identity; no nullable uniqueness ambiguity.
    #[field(char, required = true)]
    pub identity: Option<Arc<str>>,
    /// Actual company foreign key; global scope is NULL.
    #[field(many2one, comodel = "res.company", ondelete = "restrict")]
    pub company_id: Option<RecId>,
    /// Checked u64, serialized as canonical decimal.
    #[field(char, required = true, default = "0")]
    pub revision: Option<Arc<str>>,
}
/// Append-only service audit. No user has ORM create/write/unlink grants.
#[model(name = "ir.config.setting.audit", description = "Settings Audit", company_access = parent(edge = "scope_id"))]
pub struct IrConfigSettingAudit {
    /// Scope changed.
    #[field(
        many2one,
        comodel = "ir.config.setting.scope",
        required = true,
        ondelete = "restrict"
    )]
    pub scope_id: Option<RecId>,
    /// Actor identity, retained independently of later user deletion.
    #[field(char, required = true)]
    pub actor: Option<Arc<str>>,
    /// Subject whose current access policy authorized the write.
    #[field(char, required = true)]
    pub subject_uid: Option<Arc<str>>,
    /// Declaration identity.
    #[field(char, required = true)]
    pub setting_key: Option<Arc<str>>,
    /// Previous typed value, including the default on first write.
    #[field(text, required = true)]
    pub old_value: Option<Arc<str>>,
    /// New typed value.
    #[field(text, required = true)]
    pub new_value: Option<Arc<str>>,
    /// Accepted scope revision.
    #[field(char, required = true)]
    pub revision: Option<Arc<str>>,
}

fn invalid() -> Error {
    Error::validation("Invalid settings request.")
}
fn corrupt() -> Error {
    Error::validation("Stored setting is invalid for its declaration.")
}
// Repeatable-read cannot observe a scope inserted/updated after its snapshot. PostgreSQL
// reports that specific durable-lock race as 40001; it is the same stale CAS observation.
fn scope_lock_error(error: Error) -> Error {
    if error.kind() == erp_sdk::ErrorKind::Concurrency && error.sqlstate() == Some("40001") {
        Error::conflict(CONFLICT).with_source(error)
    } else {
        error
    }
}

fn revision(text: &str) -> Result<u64> {
    let value = text.parse::<u64>().map_err(|_| invalid())?;
    if value.to_string() != text {
        return Err(invalid());
    }
    Ok(value)
}
fn identity(scope: &SettingsScope) -> Result<(String, Option<i64>, ScopeKind)> {
    match scope {
        SettingsScope::Global {} => Ok(("global".into(), None, ScopeKind::Global)),
        SettingsScope::Company { company_id } => {
            let id = i64::try_from(u64::from(*company_id)).map_err(|_| invalid())?;
            Ok((format!("company:{id}"), Some(id), ScopeKind::Company))
        }
        _ => Err(invalid()),
    }
}
async fn authorize_scope(env: &Env, tx: &mut Transaction, scope: &SettingsScope) -> Result<()> {
    if let (_, Some(company), _) = identity(scope)?
        && !tx
            .companies(env)
            .await?
            .iter()
            .any(|allowed| allowed.get() == company)
    {
        return Err(Error::access("Choose a company available to your account."));
    }
    // Reuse normal ORM access (including its current-principal resolution) on the facade.
    env.records("ir.config.setting")?
        .check_access(tx, erp_sdk::Operation::Read)
        .await
}
/// Validate all installed native declarations and panel items before publishing a lifecycle generation.
///
/// This is also called by the model's existing precommit `_register_hook` lifecycle seam.
/// # Errors
/// Invalid ownership/dependency closure, duplicate keys, unavailable groups, unresolved item links
/// ("Setting item references an unavailable action or menu.") or invalid declarations, including
/// the declared technical parameters (`Parameter <key> ...`).
pub async fn validate_declarations(env: &Env, tx: &mut Transaction) -> Result<()> {
    let published = published(env, tx).await?;
    // An addon's own shipped toggle that the database makes unavailable is logged, not refused (a Groups-form
    // change must not stop module loads).
    for (key, row) in &published.toggles.groups {
        if let Err((_, reason)) = row {
            tracing::warn!(target: "erp::settings", key = %key, "Settings group row unavailable: {reason}");
        }
    }
    // One key space with the declared technical parameters.
    crate::system_parameters::published(env).map(drop)
}

fn installed_addons(env: &Env) -> BTreeSet<String> {
    // The lifecycle registry contains only installed dependency-resolved addons.
    env.registry()
        .addons()
        .iter()
        .map(|a| a.name.into())
        .collect()
}
// A contribution must come from the very descriptor the lifecycle registry installed.
fn same_descriptor(env: &Env, addon: &erp_sdk::AddonDescriptor) -> Result<()> {
    let actual = env
        .registry()
        .addons()
        .iter()
        .find(|a| a.name == addon.name)
        .ok_or_else(invalid)?;
    if actual.depends != addon.depends {
        return Err(invalid());
    }
    Ok(())
}
// A group may only come from the contributing addon's transitive dependencies.
fn collect_groups(
    env: &Env,
    addon: &str,
    groups: &[String],
    references: &mut BTreeSet<String>,
) -> Result<()> {
    for group in groups {
        let owner = group.split_once('.').ok_or_else(invalid)?.0;
        if !dependency_contains(env, addon, owner) {
            return Err(invalid());
        }
        references.insert(group.clone());
    }
    Ok(())
}
type SettingContributions = Vec<(String, Vec<SettingDecl>)>;
fn setting_contributions(
    env: &Env,
    installed: &BTreeSet<String>,
    references: &mut BTreeSet<String>,
) -> Result<SettingContributions> {
    let mut contributions = Vec::new();
    for entry in erp_sdk::settings::CONTRIBUTIONS {
        if !installed.contains(entry.addon.name) {
            continue;
        }
        same_descriptor(env, entry.addon)?;
        let declarations = (entry.declarations)()?;
        for decl in &declarations {
            if !dependency_contains(env, entry.addon.name, decl.target_app()) {
                return Err(invalid());
            }
            collect_groups(env, entry.addon.name, decl.read_groups(), references)?;
            collect_groups(env, entry.addon.name, decl.write_groups(), references)?;
        }
        contributions.push((entry.addon.name.to_owned(), declarations));
    }
    Ok(contributions)
}
async fn resolve_groups(
    tx: &mut Transaction,
    references: BTreeSet<String>,
) -> Result<BTreeSet<String>> {
    // Resolve references even in superuser mode; a typo must fail rather than grant access.
    let mut resolved = BTreeSet::new();
    for group in references {
        let (module, name) = group.split_once('.').ok_or_else(invalid)?;
        let rows = tx.query("SELECT d.res_id FROM ir_model_data d JOIN res_groups g ON g.id=d.res_id WHERE d.module=$1 AND d.name=$2 AND d.model='res.groups'", &[&module, &name]).await?;
        if rows.len() != 1 {
            return Err(Error::validation(
                "Setting policy references an unavailable group.",
            ));
        }
        resolved.insert(group);
    }
    Ok(resolved)
}

type GroupContributions = Vec<(String, Vec<erp_sdk::settings::SettingGroupDecl>)>;
fn group_contributions(env: &Env, installed: &BTreeSet<String>) -> Result<GroupContributions> {
    let mut groups = Vec::new();
    for entry in erp_sdk::settings::GROUP_CONTRIBUTIONS {
        if installed.contains(entry.addon.name) {
            same_descriptor(env, entry.addon)?;
            groups.push((entry.addon.name.to_owned(), (entry.declarations)()?));
        }
    }
    Ok(groups)
}
async fn registry(env: &Env, tx: &mut Transaction) -> Result<SettingsRegistry> {
    let installed = installed_addons(env);
    let mut references = BTreeSet::new();
    let contributions = setting_contributions(env, &installed, &mut references)?;
    let resolved = resolve_groups(tx, references).await?;
    let registry =
        SettingsRegistry::build(contributions, &installed, |group| resolved.contains(group))?
            .with_groups(group_contributions(env, &installed)?, &installed)?;
    crate::jobs_settings::require(&registry)?;
    crate::approval_settings::require(&registry)?;
    crate::report_settings::require(&registry)?;
    Ok(registry)
}

fn unsupported_item() -> Error {
    Error::validation("Unsupported setting item kind; requires a versioned settings extension.")
}
const UNAVAILABLE_LINK: &str = "Setting item references an unavailable action or menu.";
// The two link models and the statement that finds identities of each; the SQL is fixed text.
const ACTION_LINKS: &str = "SELECT d.module, d.name, d.res_id::int8 FROM ir_model_data d JOIN ir_act_window a ON a.id=d.res_id WHERE d.model='ir.actions.act_window' AND (d.module, d.name) IN (SELECT * FROM unnest($1::text[], $2::text[]))";
const MENU_LINKS: &str = "SELECT d.module, d.name, d.res_id::int8 FROM ir_model_data d JOIN ir_ui_menu m ON m.id=d.res_id WHERE d.model='ir.ui.menu' AND (d.module, d.name) IN (SELECT * FROM unnest($1::text[], $2::text[]))";

/// Link identities resolved to their rows: `(is_menu, xmlid) -> id` when exactly one row exists.
#[derive(Debug, Default)]
struct LinkRows(BTreeMap<(bool, String), i64>);
impl LinkRows {
    fn id(&self, target: &LinkTarget) -> Option<i64> {
        self.0
            .get(&(
                matches!(target, LinkTarget::Menu { .. }),
                target.xmlid().to_owned(),
            ))
            .copied()
    }
}
async fn link_rows(tx: &mut Transaction, targets: &[&LinkTarget]) -> Result<LinkRows> {
    let mut found: BTreeMap<(bool, String), Vec<i64>> = BTreeMap::new();
    for (menu, sql) in [(false, ACTION_LINKS), (true, MENU_LINKS)] {
        let (mut modules, mut names) = (Vec::new(), Vec::new());
        for target in targets {
            if matches!(target, LinkTarget::Menu { .. }) != menu {
                continue;
            }
            let (module, name) = target.xmlid().split_once('.').ok_or_else(invalid)?;
            modules.push(module.to_owned());
            names.push(name.to_owned());
        }
        if modules.is_empty() {
            continue;
        }
        for row in tx.query(sql, &[&modules, &names]).await? {
            let unreadable = |_| Error::internal("Unreadable external identity row.");
            let module: String = row.try_get(0).map_err(unreadable)?;
            let name: String = row.try_get(1).map_err(unreadable)?;
            let id: i64 = row.try_get(2).map_err(unreadable)?;
            found
                .entry((menu, format!("{module}.{name}")))
                .or_default()
                .push(id);
        }
    }
    let mut rows = BTreeMap::new();
    for (identity, ids) in found {
        // An identity naming several rows is as unavailable as one naming none.
        if let [id] = ids.as_slice() {
            rows.insert(identity, *id);
        }
    }
    Ok(LinkRows(rows))
}
// Exactly one row of the target's model, owned by an addon in the contributor's dependency closure.
fn link_available(env: &Env, rows: &LinkRows, owner: &str, target: &LinkTarget) -> Option<i64> {
    let module = target.xmlid().split_once('.')?.0;
    if !dependency_contains(env, owner, module) {
        return None;
    }
    rows.id(target)
}
/// Resolve a link target of the contributing `addon` to its `ir.actions.act_window` or `ir.ui.menu` row
/// id, exactly as registry publication does.
/// # Errors
/// `Validation` "Setting item references an unavailable action or menu." when no single row of the target's
/// model has that identity or its module is outside the addon's dependency closure; database errors.
// Public only so the integration tests can check the resolution rule directly.
#[doc(hidden)]
pub async fn resolve_link(
    env: &Env,
    tx: &mut Transaction,
    addon: &str,
    target: &LinkTarget,
) -> Result<i64> {
    let rows = link_rows(tx, &[target]).await?;
    link_available(env, &rows, addon, target).ok_or_else(|| Error::validation(UNAVAILABLE_LINK))
}

/// The settings registry with its panel items, each item's resolver, link row ids, module states and toggle groups.
struct Published {
    registry: SettingsRegistry,
    resolvers: BTreeMap<String, PanelResolver>,
    links: LinkRows,
    // The recorded `ir_module_module` state of each module toggle's addon that has a row.
    modules: BTreeMap<String, String>,
    // The install route's catalogue, read only when a module toggle's addon is not installed.
    catalogue: Option<erp_sdk::settings::AddonCatalogue>,
    toggles: ToggleGroups,
}
async fn published(env: &Env, tx: &mut Transaction) -> Result<Published> {
    let installed = installed_addons(env);
    let mut references = BTreeSet::new();
    let contributions = setting_contributions(env, &installed, &mut references)?;
    let mut items = Vec::new();
    let mut resolvers = BTreeMap::new();
    for entry in erp_sdk::settings::PANEL_CONTRIBUTIONS {
        if !installed.contains(entry.addon.name) {
            continue;
        }
        same_descriptor(env, entry.addon)?;
        let declarations = (entry.declarations)()?;
        for item in &declarations {
            if !dependency_contains(env, entry.addon.name, item.target_app()) {
                return Err(invalid());
            }
            collect_groups(env, entry.addon.name, item.read_groups(), &mut references)?;
            // A toggled group, like a read group, must come from the contributor's dependency closure; its row and
            // what it implies are checked once the registry is built (`ToggleGroups::load`).
            if let PanelItemKind::Group { group, .. } = item.kind()
                && !dependency_contains(
                    env,
                    entry.addon.name,
                    group.split_once('.').ok_or_else(invalid)?.0,
                )
            {
                return Err(invalid());
            }
            // Its own addon and what that addon needs can never be uninstalled from its own page.
            if let PanelItemKind::Module { addon, .. } = item.kind()
                && dependency_contains(env, entry.addon.name, addon)
            {
                return Err(Error::validation(format!(
                    "Setting item {} toggles the addon {addon}, which {} itself needs.",
                    item.key(),
                    entry.addon.name
                )));
            }
            if resolvers
                .insert(item.key().to_owned(), entry.resolve)
                .is_some()
            {
                return Err(invalid());
            }
        }
        items.push((entry.addon.name.to_owned(), declarations));
    }
    // base's own items are a link-time registration; a binary that dropped it must not publish a
    // Settings page silently missing them.
    if installed.contains("base") {
        for key in crate::settings_general::KEYS {
            if !resolvers.contains_key(key) {
                return Err(Error::programming(format!(
                    "The settings item {key} of base is not linked into this binary (owner crate \
                     erp-addon-base); its panel registration was dropped by the linker."
                )));
            }
        }
    }
    let mut targets = Vec::new();
    for item in items.iter().flat_map(|(_, declarations)| declarations) {
        match item.kind() {
            PanelItemKind::Info | PanelItemKind::Module { .. } | PanelItemKind::Group { .. } => {}
            PanelItemKind::Link(target) => targets.push(target),
            _ => return Err(unsupported_item()),
        }
    }
    let links = link_rows(tx, &targets).await?;
    let resolved = resolve_groups(tx, references).await?;
    let registry =
        SettingsRegistry::build(contributions, &installed, |group| resolved.contains(group))?
            .with_groups(group_contributions(env, &installed)?, &installed)?
            .with_items(
                items,
                &installed,
                |group| resolved.contains(group),
                |owner, target| link_available(env, &links, owner, target).is_some(),
            )?;
    crate::jobs_settings::require(&registry)?;
    crate::approval_settings::require(&registry)?;
    crate::report_settings::require(&registry)?;
    let modules = module_states(tx, &registry).await?;
    let catalogue = addon_catalogue(&registry, &modules).await?;
    let toggles = ToggleGroups::load(env, tx, &registry).await?;
    Ok(Published {
        registry,
        resolvers,
        links,
        modules,
        catalogue,
        toggles,
    })
}
// The recorded lifecycle state of each module toggle's addon, as the Addons directory reads it.
async fn module_states(
    tx: &mut Transaction,
    registry: &SettingsRegistry,
) -> Result<BTreeMap<String, String>> {
    let addons: Vec<String> = registry
        .items()
        .filter_map(|item| match item.kind() {
            PanelItemKind::Module { addon, .. } => Some(addon.clone()),
            _ => None,
        })
        .collect();
    if addons.is_empty() {
        return Ok(BTreeMap::new());
    }
    let mut states = BTreeMap::new();
    for row in tx
        .query(
            "SELECT name::text, state::text FROM ir_module_module WHERE name = ANY($1)",
            &[&addons],
        )
        .await?
    {
        let unreadable = |_| Error::internal("Unreadable module state row.");
        states.insert(
            row.try_get(0).map_err(unreadable)?,
            row.try_get(1).map_err(unreadable)?,
        );
    }
    Ok(states)
}
// Whether the database records `addon` as installed (`Some(true)`), not installed (`Some(false)`), or between
// states (`None`: an install, upgrade or removal is under way).
fn recorded_state(modules: &BTreeMap<String, String>, addon: &str) -> Option<bool> {
    match modules.get(addon).map(String::as_str) {
        Some("installed") => Some(true),
        None | Some("uninstalled" | "uninstallable") => Some(false),
        Some(_) => None,
    }
}
// A module toggle whose addon is not installed must name an addon of the install route's catalogue. A process
// that binds no catalogue serves no install route; its descriptions refuse such a row instead (`describe_item`).
async fn addon_catalogue(
    registry: &SettingsRegistry,
    modules: &BTreeMap<String, String>,
) -> Result<Option<erp_sdk::settings::AddonCatalogue>> {
    let toggles: Vec<(&str, &str)> = registry
        .items()
        .filter_map(|item| match item.kind() {
            PanelItemKind::Module { addon, .. }
                if recorded_state(modules, addon) == Some(false) =>
            {
                Some((item.key(), addon.as_str()))
            }
            _ => None,
        })
        .collect();
    if toggles.is_empty() {
        return Ok(None);
    }
    let catalogue = erp_sdk::settings::addon_catalogue().await?;
    if let Some(catalogue) = &catalogue
        && let Some((key, addon)) = toggles.iter().find(|(_, addon)| !catalogue.knows(addon))
    {
        return Err(Error::validation(format!(
            "Setting item {key} toggles the addon {addon}, which is neither installed nor in the addon directory."
        )));
    }
    Ok(catalogue)
}
fn dependency_contains(env: &Env, source: &str, target: &str) -> bool {
    let mut pending = vec![source];
    let mut seen = BTreeSet::new();
    while let Some(name) = pending.pop() {
        if name == target {
            return true;
        }
        if !seen.insert(name) {
            continue;
        }
        if let Some(addon) = env.registry().addons().iter().find(|a| a.name == name) {
            pending.extend(addon.depends.iter().copied());
        }
    }
    false
}
async fn permits(env: &Env, tx: &mut Transaction, groups: &[String]) -> Result<bool> {
    if env.su() {
        return Ok(true);
    }
    let mut allowed = false;
    for group in groups {
        allowed |= tx.has_group(env, group).await?;
    }
    Ok(allowed)
}
async fn declared<'a>(
    registry: &'a SettingsRegistry,
    env: &Env,
    tx: &mut Transaction,
    scope: ScopeKind,
    key: &str,
    write: bool,
) -> Result<&'a SettingDecl> {
    // V5: evaluate a fixed, catalogue-derived policy set before looking up the
    // requested key. Unknown, hidden, wrong-scope and panel-item keys all do this same
    // authorization work and reach one refusal. No key-existence branch skips group reads.
    let groups: BTreeSet<_> = registry
        .iter()
        .flat_map(|decl| decl.read_groups().iter().chain(decl.write_groups()))
        .collect();
    let mut membership = BTreeMap::new();
    for group in groups {
        let allowed = if env.su() {
            true
        } else {
            tx.has_group(env, group).await?
        };
        membership.insert(group.as_str(), allowed);
    }
    let declaration = registry.get(key);
    let permitted = declaration.is_some_and(|decl| {
        let read = decl
            .read_groups()
            .iter()
            .any(|group| membership.get(group.as_str()) == Some(&true));
        let can_write = decl
            .write_groups()
            .iter()
            .any(|group| membership.get(group.as_str()) == Some(&true));
        decl.scope() == scope && read && (!write || can_write)
    });
    declaration.filter(|_| permitted).ok_or_else(|| denied(key))
}
fn scalar(value: &Value) -> Result<wire::SettingValue> {
    match value {
        Value::Bool(v) => Ok(wire::SettingValue::Boolean(*v)),
        Value::Str(v) => Ok(wire::SettingValue::String(v.to_string())),
        Value::Int(v) => Ok(wire::SettingValue::Integer(*v)),
        Value::Float(v) if v.is_finite() => Ok(wire::SettingValue::Float(*v)),
        _ => Err(invalid()),
    }
}
/// A value that is no boolean, text or finite number (null, list, object, beyond 64 bits). Only the key is named.
fn unsupported_value(key: &str) -> Error {
    if key.len() > 128 {
        return invalid();
    }
    Error::validation(format!("{key} takes a boolean, a text or a finite number."))
}
fn from_json(value: Json) -> Result<Value> {
    match value {
        Json::Bool(v) => Ok(Value::Bool(v)),
        Json::String(v) => Ok(Value::str(v)),
        Json::Number(v) => {
            if let Some(v) = v.as_i64() {
                Ok(Value::Int(v))
            } else if v.is_f64() {
                v.as_f64()
                    .filter(|v| v.is_finite())
                    .map(Value::Float)
                    .ok_or_else(invalid)
            } else {
                Err(invalid())
            }
        }
        _ => Err(invalid()),
    }
}
fn json(value: &Value) -> Result<Json> {
    match value {
        Value::Bool(v) => Ok(Json::Bool(*v)),
        Value::Str(v) => Ok(Json::String(v.to_string())),
        Value::Int(v) => Ok(Json::from(*v)),
        Value::Float(v) if v.is_finite() => Ok(Json::from(*v)),
        Value::Json(v) => Ok((**v).clone()),
        Value::Map(v) => v
            .iter()
            .map(|(k, v)| Ok((k.to_string(), json(v)?)))
            .collect::<Result<serde_json::Map<String, Json>>>()
            .map(Json::Object),
        Value::List(v) => v
            .iter()
            .map(json)
            .collect::<Result<Vec<_>>>()
            .map(Json::Array),
        _ => Err(invalid()),
    }
}
fn stored_error(key: &str) -> Error {
    Error::validation(format!("Stored setting {key} is invalid."))
}
fn stored(decl: &SettingDecl, text: &str) -> Result<Value> {
    if text.len() > MAX_STORED_VALUE_BYTES {
        return Err(corrupt());
    }
    let value =
        from_json(serde_json::from_str(text).map_err(|_| corrupt())?).map_err(|_| corrupt())?;
    decl.validate(&value).map_err(|_| corrupt())?;
    Ok(value)
}
fn batch(keys: &[String], allow_empty: bool) -> Result<()> {
    if keys.len() > MAX_BATCH || !allow_empty && keys.is_empty() {
        return Err(invalid());
    }
    let mut seen = BTreeSet::new();
    if keys.iter().any(|key| {
        key.is_empty() || key.len() > 128 || key.chars().any(char::is_control) || !seen.insert(key)
    }) {
        return Err(invalid());
    }
    Ok(())
}
/// Describe permitted declarations only, sorted by key, then the permitted information, link and toggle rows of
/// the scope. No policy identities are returned: a group toggle carries its group's external id and state, never
/// row ids.
///
/// Item values are computed by the declaring addon's resolver with the caller's own environment, never
/// elevated, each inside a savepoint. A resolver refused with an `Access` error leaves its item out.
/// # Errors
/// Unauthorized scope, invalid declaration registry, storage or access errors; a resolver's non-access
/// error; `Programming` when a resolver returns a value of the wrong kind for its item.
pub async fn describe(
    env: &Env,
    tx: &mut Transaction,
    scope: SettingsScope,
) -> Result<wire::SettingsDescription> {
    authorize_scope(env, tx, &scope).await?;
    let (_, _, kind) = identity(&scope)?;
    let published = published(env, tx).await?;
    let registry = &published.registry;
    let mut result = Vec::new();
    for decl in registry.iter().filter(|decl| decl.scope() == kind) {
        if !permits(env, tx, decl.read_groups()).await? {
            continue;
        }
        let (kind, choices) = match decl.kind() {
            SettingKind::Boolean => (wire::SettingKind::Boolean, None),
            SettingKind::String { .. } => (wire::SettingKind::String, None),
            SettingKind::Integer { .. } => (wire::SettingKind::Integer, None),
            SettingKind::Float { .. } => (wire::SettingKind::Float, None),
            SettingKind::Selection { choices } => {
                (wire::SettingKind::Selection, Some(choices.clone()))
            }
            _ => {
                return Err(Error::validation(
                    "Unsupported setting kind; requires a versioned settings extension.",
                ));
            }
        };
        result.push(
            wire::SettingDescription::new(
                decl.key().into(),
                decl.label().into(),
                kind,
                scalar(decl.default_value())?,
                choices,
                permits(env, tx, decl.write_groups()).await?,
                decl.target_app().into(),
                decl.block_title().into(),
                decl.display_sequence(),
            )
            .with_help(decl.help_text().map(str::to_owned)),
        );
    }
    // Defence in depth: the scope check (`authorize_scope` -> `check_access`, whose model check reads
    // `env.companies()` first) already refuses a bad company context; this keeps that true if a later scope
    // kind skips it, because inside a resolver the same `Access` error would only leave items out. The
    // superuser's empty company list passes.
    tx.companies(env).await?;
    let mut items = Vec::new();
    for item in registry.items().filter(|item| item.shows_in(kind)) {
        if !permits(env, tx, item.read_groups()).await? {
            continue;
        }
        if let Some(row) = describe_item(env, tx, &scope, &published, item).await? {
            items.push(row);
        }
    }
    Ok(
        wire::SettingsDescription::new(env.registry().generation().to_string(), scope, result)
            .with_items(items),
    )
}
fn wrong_kind(key: &str) -> Error {
    Error::programming(format!(
        "Setting item {key} resolved to a value of the wrong kind."
    ))
}
fn unrepresentable() -> Error {
    Error::internal("Setting item identity is not a positive JSON-safe id.")
}
fn wire_id(id: i64) -> Result<wire::WireId> {
    let raw = u64::try_from(id).map_err(|_| unrepresentable())?;
    wire::WireId::try_from(raw).map_err(|_| unrepresentable())
}
// Run the declaring addon's resolver with the caller's own environment inside a savepoint. `None` means
// the resolver was refused with an access error and the item is left out for this caller.
async fn resolve_item(
    env: &Env,
    tx: &mut Transaction,
    scope: &SettingsScope,
    published: &Published,
    key: &str,
) -> Result<Option<PanelItemValue>> {
    let resolver = *published
        .resolvers
        .get(key)
        .ok_or_else(|| Error::internal("Published setting item has no resolver."))?;
    match tx
        .savepoint_outcome(async |tx| resolver(env, tx, scope, key).await)
        .await
    {
        erp_sdk::SavepointOutcome::Committed(value) => Ok(Some(value)),
        erp_sdk::SavepointOutcome::RolledBack(error)
            if error.kind() == erp_sdk::ErrorKind::Access =>
        {
            Ok(None)
        }
        erp_sdk::SavepointOutcome::RolledBack(error)
        | erp_sdk::SavepointOutcome::SetupFailed(error) => Err(error),
        failed @ erp_sdk::SavepointOutcome::RollbackFailed { .. } => failed.into_result().map(Some),
    }
}
/// The rows of the groups the published group toggles name: the internal-user group with the groups it implies
/// (itself included, transitively), and each toggled group's id.
///
/// Loading refuses a declaration whose group is the internal-user, portal or public group, or is not a light
/// feature group, so turning it on for every internal user grants no administrative or other regular rights.
/// What an administrator can change in the Groups form (the group deleted, or made to imply a group that is not
/// light) only makes that row unavailable, with the reason; every other row and the page keep working.
#[derive(Default)]
struct ToggleGroups {
    internal: Option<(i64, BTreeSet<i64>)>,
    // Each toggled group's id, or the refusal that makes its row unavailable (with the group's id when the group
    // exists, so that switching an implied one off stays possible).
    groups: BTreeMap<String, std::result::Result<i64, (Option<i64>, String)>>,
}
impl ToggleGroups {
    async fn load(env: &Env, tx: &mut Transaction, registry: &SettingsRegistry) -> Result<Self> {
        let toggled: Vec<(&str, &str)> = registry
            .items()
            .filter_map(|item| match item.kind() {
                PanelItemKind::Group { group, .. } => Some((item.key(), group.as_str())),
                _ => None,
            })
            .collect();
        if toggled.is_empty() {
            return Ok(Self::default());
        }
        let mut xmlids = vec![crate::group_graph::GROUP_USER];
        xmlids.extend(toggled.iter().map(|(_, group)| *group));
        let ids = crate::group_graph::groups_by_xmlid(tx, &xmlids).await?;
        let internal = ids.first().copied().flatten().ok_or_else(|| {
            Error::programming("The internal-user group base.group_user has no row.")
        })?;
        let light_xmlids: Vec<String> = env
            .records("res.groups")?
            .call::<(), Vec<String>>(tx, "_get_light_group_xmlids", ())
            .await?;
        let light_refs: Vec<&str> = light_xmlids.iter().map(String::as_str).collect();
        let light: BTreeSet<i64> = crate::group_graph::groups_by_xmlid(tx, &light_refs)
            .await?
            .into_iter()
            .flatten()
            .collect();
        let graph = crate::group_graph::Graph::load(tx).await?;
        const NOT_LIGHT: &str = "it is or implies a group that is not a light feature group";
        let mut groups = BTreeMap::new();
        for ((key, group), id) in toggled.into_iter().zip(ids.into_iter().skip(1)) {
            let refused =
                |why: &str| format!("Setting item {key} cannot toggle the group {group}: {why}.");
            // What the addon declares: publication refuses it.
            if group == crate::group_graph::GROUP_USER {
                return Err(Error::validation(refused(
                    "it is the internal-user group itself",
                )));
            }
            if [
                crate::group_graph::GROUP_PORTAL,
                crate::group_graph::GROUP_PUBLIC,
            ]
            .contains(&group)
            {
                return Err(Error::validation(refused(
                    "it is the portal or public group",
                )));
            }
            if [
                "base.group_everyone",
                "base.group_no_one",
                "base.default_user_group",
            ]
            .contains(&group)
            {
                return Err(Error::validation(refused("it is a built-in base group")));
            }
            if !light_xmlids.iter().any(|light| light == group) {
                return Err(Error::validation(refused(if id.is_none() {
                    "it does not exist"
                } else {
                    NOT_LIGHT
                })));
            }
            // What the database holds now: only this row is unavailable.
            let row = match id {
                None => Err((None, refused("it does not exist"))),
                Some(id) if id == internal => {
                    Err((None, refused("it is the internal-user group itself")))
                }
                Some(id)
                    if graph
                        .up([id])
                        .iter()
                        .any(|implied| !light.contains(implied)) =>
                {
                    let mut reason = refused(NOT_LIGHT);
                    if graph.up([internal]).contains(&id) {
                        reason.push_str(" It is on for every internal user.");
                    }
                    Err((Some(id), reason))
                }
                Some(id) => Ok(id),
            };
            groups.insert(group.to_owned(), row);
        }
        Ok(Self {
            internal: Some((internal, graph.up([internal]))),
            groups,
        })
    }
    /// The toggled group's id and the internal-user group's id with what it implies; `Validation` with the reason
    /// when the row is unavailable.
    /// Switching a group off is allowed for an unavailable row whose group exists: removing the implication grants
    /// nothing.
    fn rows(&self, group: &str, enabled: bool) -> Result<(i64, i64, &BTreeSet<i64>)> {
        match (self.groups.get(group), &self.internal) {
            (Some(Ok(id)), Some((internal, implied))) => Ok((*id, *internal, implied)),
            (Some(Err((Some(id), _))), Some((internal, implied))) if !enabled => {
                Ok((*id, *internal, implied))
            }
            (Some(Err((_, reason))), _) => Err(Error::validation(reason.clone())),
            _ => Err(Error::invariant(format!(
                "The group toggle of {group} was not loaded with the published registry."
            ))),
        }
    }
    /// The row's state, for an unavailable row why, and whether its group is on for every internal user
    /// (an unavailable row that is on can still be switched off).
    fn state(&self, group: &str) -> Result<(wire::GroupToggleState, Option<String>, bool)> {
        if let Some(Err((id, reason))) = self.groups.get(group) {
            let on = match (id, &self.internal) {
                (Some(id), Some((_, implied))) => implied.contains(id),
                _ => false,
            };
            return Ok((
                wire::GroupToggleState::NotAvailable,
                Some(reason.clone()),
                on,
            ));
        }
        let (id, _, implied) = self.rows(group, true)?;
        Ok((toggle_state(implied.contains(&id)), None, false))
    }
}
async fn describe_item(
    env: &Env,
    tx: &mut Transaction,
    scope: &SettingsScope,
    published: &Published,
    item: &PanelItemDecl,
) -> Result<Option<wire::SettingsItem>> {
    let key = item.key();
    match item.kind() {
        PanelItemKind::Info => {
            let Some(value) = resolve_item(env, tx, scope, published, key).await? else {
                return Ok(None);
            };
            let PanelItemValue::Info {
                value,
                label,
                detail,
                ..
            } = value
            else {
                return Err(wrong_kind(key));
            };
            Ok(Some(wire::SettingsItem::Info {
                key: key.into(),
                // Specified fallback, not a default for a wrong value: an information value without its own
                // label shows the declared one.
                label: label.unwrap_or_else(|| item.label().into()),
                app: item.target_app().into(),
                block: item.block_title().into(),
                sequence: item.display_sequence(),
                value: scalar(&value)?,
                detail,
            }))
        }
        PanelItemKind::Link(target) => {
            let id = wire_id(
                published
                    .links
                    .id(target)
                    .ok_or_else(|| Error::internal("Published setting item link has no row."))?,
            )?;
            let target = match target {
                LinkTarget::Action { record: false, .. } => {
                    wire::SettingsLinkTarget::Action { id, res_id: None }
                }
                LinkTarget::Action { record: true, .. } => {
                    let Some(value) = resolve_item(env, tx, scope, published, key).await? else {
                        return Ok(None);
                    };
                    let PanelItemValue::Record { id: record, .. } = value else {
                        return Err(wrong_kind(key));
                    };
                    wire::SettingsLinkTarget::Action {
                        id,
                        res_id: Some(wire_id(record.get())?),
                    }
                }
                LinkTarget::Menu { .. } => wire::SettingsLinkTarget::Menu { id },
                _ => return Err(unsupported_item()),
            };
            Ok(Some(wire::SettingsItem::Link {
                key: key.into(),
                label: item.label().into(),
                app: item.target_app().into(),
                block: item.block_title().into(),
                sequence: item.display_sequence(),
                target,
            }))
        }
        PanelItemKind::Module { addon, .. } => {
            // Installed or not as the database records it; between states, the same answer as the Addons directory.
            let installed: BTreeSet<String> = match recorded_state(&published.modules, addon) {
                Some(true) => BTreeSet::from([addon.clone()]),
                Some(false) => BTreeSet::new(),
                None => {
                    return Err(Error::new(
                        erp_sdk::ErrorKind::Concurrency,
                        "Addon catalogue is in a transitional state; retry after lifecycle recovery",
                    ));
                }
            };
            let state = match erp_sdk::settings::ModuleToggleState::resolve(
                addon,
                &installed,
                published.catalogue.as_ref(),
            )
            .ok_or_else(|| {
                Error::programming(format!(
                    "Setting item {key} toggles the addon {addon}, but this process has no addon catalogue \
                     that names it."
                ))
            })? {
                erp_sdk::settings::ModuleToggleState::Installed => {
                    wire::ModuleToggleState::Installed
                }
                erp_sdk::settings::ModuleToggleState::NotInstalled => {
                    wire::ModuleToggleState::NotInstalled
                }
                erp_sdk::settings::ModuleToggleState::NotAvailable => {
                    wire::ModuleToggleState::NotAvailable
                }
                _ => return Err(unsupported_item()),
            };
            Ok(Some(wire::SettingsItem::Module {
                key: key.into(),
                label: item.label().into(),
                app: item.target_app().into(),
                block: item.block_title().into(),
                sequence: item.display_sequence(),
                addon: addon.clone(),
                state,
            }))
        }
        PanelItemKind::Group { group, .. } => {
            let (state, reason, on_for_all) = published.toggles.state(group)?;
            Ok(Some(wire::SettingsItem::Group {
                key: key.into(),
                label: item.label().into(),
                app: item.target_app().into(),
                block: item.block_title().into(),
                sequence: item.display_sequence(),
                group: group.clone(),
                state,
                reason,
                on_for_all,
            }))
        }
        _ => Err(unsupported_item()),
    }
}
/// Read typed values and exact scope revision in one PostgreSQL snapshot.
/// # Errors
/// Duplicate/unknown/disallowed keys, unauthorized companies, corrupt stored values.
pub async fn read_values(
    env: &Env,
    tx: &mut Transaction,
    scope: SettingsScope,
    keys: Vec<String>,
) -> Result<SettingsValues> {
    batch(&keys, true)?;
    let requested = keys.clone();
    read_values_requested(env, tx, scope, keys, &requested).await
}
// A group may include a sibling that the caller cannot read. Its identity is not request
// data: preserve the denial kind, but report only an original requested key (V5).
fn conceal_expanded(error: Error, key: &str, requested: &[String]) -> Error {
    if error.kind() == erp_sdk::ErrorKind::Access && !requested.iter().any(|input| input == key) {
        match requested.first() {
            Some(input) => denied(input),
            None => invalid(),
        }
    } else {
        error
    }
}
async fn read_values_requested(
    env: &Env,
    tx: &mut Transaction,
    scope: SettingsScope,
    keys: Vec<String>,
    requested: &[String],
) -> Result<SettingsValues> {
    batch(&keys, true)?;
    authorize_scope(env, tx, &scope).await?;
    let (identity, _, kind) = identity(&scope)?;
    let registry = registry(env, tx).await?;
    for key in &keys {
        declared(&registry, env, tx, kind, key, false)
            .await
            .map_err(|error| conceal_expanded(error, key, requested))?;
    }
    let expanded = registry.expanded_keys(&keys)?;
    for key in &expanded {
        declared(&registry, env, tx, kind, key, false)
            .await
            .map_err(|error| conceal_expanded(error, key, requested))?;
    }
    let StoredSnapshot {
        revision: current,
        values: complete,
        mut sources,
        ..
    } = load_values(&registry, tx, &identity, expanded, &[]).await?;
    let mut result = Vec::new();
    for key in keys {
        let value = complete.get(&key).ok_or_else(corrupt)?;
        let source = sources.remove(&key).ok_or_else(corrupt)?;
        result.push(wire::SettingEntry::new(key, scalar(value)?, source));
    }
    Ok(SettingsValues::new(scope, current, result))
}
/// Native runtime policy read: no request principal or package dispatch, one value/revision snapshot.
/// The caller owns a separately pooled, read-only transaction on the tenant primary.
pub(crate) async fn read_job_policy(
    tx: &mut Transaction,
) -> Result<(u64, BTreeMap<String, Value>)> {
    let env = tx.env();
    let registry = registry(&env, tx).await?;
    let keys: Vec<String> = crate::jobs_settings::shared()
        .map(crate::jobs_settings::key)
        .collect();
    let keys = registry.expanded_keys(&keys)?;
    let snapshot = load_values(&registry, tx, "global", keys, &[]).await?;
    Ok((revision(&snapshot.revision)?, snapshot.values))
}

struct StoredSnapshot {
    revision: String,
    values: BTreeMap<String, Value>,
    sources: BTreeMap<String, wire::ValueSource>,
    repaired: BTreeSet<String>,
}

/// Read one company setting used as a native business-field default.
///
/// The declaration is provided by trusted addon code. The caller of this function owns
/// the business-model access check. The setting's company must belong to the caller's
/// active scope. This is a read-only lookup, including when the scope has no saved values.
///
/// # Errors
///
/// The caller may not use this company, the declaration is not company-scoped, or a
/// stored value is corrupt or outside the declaration's domain.
pub async fn company_field_default(
    env: &Env,
    tx: &mut Transaction,
    company: i64,
    declaration: &SettingDecl,
) -> Result<Value> {
    let identity = authorized_company_identity(env, tx, company).await?;
    if declaration.scope() != ScopeKind::Company {
        return Err(Error::programming(
            "A company field default requires a company setting.",
        ));
    }
    let key = declaration.key();
    let rows = tx.query(
        &format!(
            "SELECT CASE WHEN octet_length(v.value_text)<={MAX_STORED_VALUE_BYTES} THEN v.value_text END \
             FROM ir_config_setting_value v JOIN ir_config_setting_scope s ON s.id=v.scope_id \
             WHERE s.identity=$1 AND v.setting_key=$2"
        ),
        &[&identity, &key],
    ).await?;
    let Some(row) = rows.first() else {
        return Ok(declaration.default_value().clone());
    };
    let text: String = row.try_get(0).map_err(|_| stored_error(key))?;
    stored(declaration, &text).map_err(|_| stored_error(key))
}

async fn authorized_company_identity(
    env: &Env,
    tx: &mut Transaction,
    company: i64,
) -> Result<String> {
    let id = u64::try_from(company).map_err(|_| invalid())?;
    let scope = SettingsScope::Company {
        company_id: wire::WireId::try_from(id).map_err(|_| invalid())?,
    };
    authorize_scope(env, tx, &scope).await?;
    Ok(identity(&scope)?.0)
}

/// Order a company field creation and changes to the setting that supplies its default.
///
/// Both code paths touch the existing company lifecycle fence before reading records that
/// depend on each other. A concurrent transaction with an older repeatable-read snapshot
/// must retry. This admission does not create an empty Settings scope or modify a company.
///
/// # Errors
///
/// The fence is unavailable, or the caller's transaction must retry a concurrent change.
pub async fn fence_company_field_defaults(tx: &mut Transaction) -> Result<()> {
    crate::company_lifecycle::fence(tx).await
}

async fn load_values(
    registry: &SettingsRegistry,
    tx: &mut Transaction,
    identity: &str,
    expanded: Vec<String>,
    replacements: &[(String, Value)],
) -> Result<StoredSnapshot> {
    // Read revision and values in a single statement; READ COMMITTED cannot mix revisions.
    // Keep a present key when withholding oversized/corrupt text. Its NULL projection below
    // must decode as the existing corruption error, never as an absent/default setting.
    let sql = format!(
        "SELECT COALESCE(s.revision,'0'),v.setting_key,v.value_text FROM (SELECT 1) anchor LEFT JOIN ir_config_setting_scope s ON s.identity=$1 LEFT JOIN LATERAL (SELECT setting_key, CASE WHEN octet_length(value_text)<={MAX_STORED_VALUE_BYTES} THEN value_text END AS value_text FROM ir_config_setting_value WHERE scope_id=s.id AND setting_key=ANY($2) AND setting_key<>$3 UNION ALL SELECT key, CASE WHEN octet_length(value)<=32 THEN value ELSE 'invalid' END FROM ir_config_parameter WHERE $1='global' AND key=$3 AND key=ANY($2) AND value IS NOT NULL AND value<>'') v ON TRUE"
    );
    let rows = tx
        .query(
            &sql,
            &[
                &identity,
                &expanded,
                &crate::attachments::settings::MAX_FILE_KEY,
            ],
        )
        .await?;
    let mut current = "0".to_owned();
    let mut values = BTreeMap::new();
    for row in rows {
        current = row.try_get(0).map_err(|_| corrupt())?;
        revision(&current).map_err(|_| corrupt())?;
        let key: Option<String> = row.try_get(1).map_err(|_| corrupt())?;
        if let Some(key) = key {
            let text: Option<String> = row.try_get(2).map_err(|_| stored_error(&key))?;
            values.insert(key, text);
        }
    }
    let mut complete = BTreeMap::new();
    let mut sources = BTreeMap::new();
    let mut repaired = BTreeSet::new();
    for key in expanded {
        let decl = registry.get(&key).ok_or_else(corrupt)?;
        let (value, source) = match values.get(&key) {
            Some(text) => {
                let value = text.as_deref().and_then(|text| stored(decl, text).ok());
                let value = match value {
                    Some(value) => value,
                    None => {
                        let (_, replacement) = replacements
                            .iter()
                            .find(|(changed, _)| changed == &key)
                            .ok_or_else(|| stored_error(&key))?;
                        repaired.insert(key.clone());
                        replacement.clone()
                    }
                };
                (value, wire::ValueSource::Stored)
            }
            None => (decl.default_value().clone(), wire::ValueSource::Default),
        };
        complete.insert(key.clone(), value);
        sources.insert(key, source);
    }
    if replacements.is_empty() {
        registry.validate_groups(&complete)?;
    }
    Ok(StoredSnapshot {
        revision: current,
        values: complete,
        sources,
        repaired,
    })
}

/// Atomically validate, lock, compare and write a batch with audit in the caller's transaction.
/// # Errors
/// Invalid or unauthorized batches, stale revisions, overflow and database failures; no prefix persists.
pub async fn write_values(
    env: &Env,
    tx: &mut Transaction,
    scope: SettingsScope,
    expected_revision: String,
    mut changes: Vec<(String, Value)>,
) -> Result<SettingsValues> {
    let expected = revision(&expected_revision)?;
    let keys: Vec<String> = changes.iter().map(|(key, _)| key.clone()).collect();
    batch(&keys, false)?;
    authorize_scope(env, tx, &scope).await?;
    env.records("ir.config.setting")?
        .check_access(tx, erp_sdk::Operation::Write)
        .await?;
    let (identity, company, kind) = identity(&scope)?;
    let registry = registry(env, tx).await?;
    for (key, value) in &mut changes {
        let decl = declared(&registry, env, tx, kind, key, true).await?;
        // JSON has one number type: a whole number sent to a float setting is that float.
        if let (SettingKind::Float { .. }, Value::Int(whole)) = (decl.kind(), &*value) {
            #[allow(clippy::cast_precision_loss)]
            let float = *whole as f64;
            *value = Value::Float(float);
        }
        // `validate` refuses a wrong kind or a value outside the bounds, naming the key.
        decl.validate(value)?;
    }
    // Savepoint guarantees callers cannot catch an error and commit a successful prefix.
    tx.savepoint(async |tx| {
        tx.execute("INSERT INTO ir_config_setting_scope(identity,company_id,revision) VALUES ($1,$2,'0') ON CONFLICT(identity) DO NOTHING", &[&identity,&company]).await.map_err(scope_lock_error)?;
        let row = tx.query("SELECT id::int8, revision FROM ir_config_setting_scope WHERE identity=$1 FOR UPDATE", &[&identity]).await.map_err(scope_lock_error)?;
        let row = row.first().ok_or_else(corrupt)?;
        let scope_id: i64 = row.try_get(0).map_err(|_| corrupt())?;
        let current: String = row.try_get(1).map_err(|_| corrupt())?;
        if revision(&current).map_err(|_| corrupt())? != expected { return Err(Error::conflict(CONFLICT)); }
        let next = expected.checked_add(1).ok_or_else(|| Error::validation("Settings revision exhausted."))?.to_string();
        // Access policy is refreshed again after waiting for the durable scope lock.
        authorize_scope(env, tx, &scope).await?;
        env.records("ir.config.setting")?.check_access(tx, erp_sdk::Operation::Write).await?;
        for (key, _) in &changes { declared(&registry, env, tx, kind, key, true).await?; }
        let expanded = registry.expanded_keys(&keys)?;
        batch(&expanded, true)?;
        for key in &expanded {
            declared(&registry, env, tx, kind, key, false).await
                .map_err(|error| conceal_expanded(error, key, &keys))?;
        }
        let snapshot = load_values(&registry, tx, &identity, expanded, &changes).await?;
        let mut proposed = snapshot.values.clone();
        for (key, value) in &changes { proposed.insert(key.clone(), value.clone()); }
        registry.validate_groups(&proposed)?;
        for (key, value) in &changes {
            if key == crate::approval_settings::MAX_APPROVERS_KEY {
                // Intrinsic declaration validation above already checked the integer and range.
                let Value::Int(limit) = value else { return Err(invalid()); };
                crate::approval_settings::check_active(env, tx, *limit).await?;
            }
        }
        let subject = env.uid().to_string();
        let actor = audit_actor(env)?;
        let mut result = Vec::new();
        let mut effective_changes = erp_sdk::ValueMap::new();
        for (key,value) in &changes {
            let old_text = if snapshot.repaired.contains(key) {
                "[invalid stored value]".to_owned()
            } else {
                let old_value = snapshot.values.get(key).ok_or_else(corrupt)?;
                serde_json::to_string(&scalar(old_value)?).map_err(|_| corrupt())?
            };
            let text = serde_json::to_string(&scalar(value)?).map_err(|_| invalid())?;
            if old_text != text {
                effective_changes.insert(key.clone(), value.clone());
            }
            if key == crate::attachments::settings::MAX_FILE_KEY {
                let params = env.records("ir.config_parameter")?;
                tx.extension::<ParameterBridge>().0 = true;
                let written: Result<String> = params.call(tx, "set_str", (key.clone(), Some(text.clone()))).await;
                tx.extension::<ParameterBridge>().0 = false;
                written?;
            } else {
                tx.execute("INSERT INTO ir_config_setting_value(scope_id,setting_key,value_text) VALUES ($1,$2,$3) ON CONFLICT(scope_id,setting_key) DO UPDATE SET value_text=EXCLUDED.value_text", &[&scope_id, &key, &text]).await?;
            }
            tx.execute("INSERT INTO ir_config_setting_audit(scope_id,actor,setting_key,old_value,new_value,revision,subject_uid,create_date) VALUES ($1,$2,$3,$4,$5,$6,$7,CURRENT_TIMESTAMP AT TIME ZONE 'UTC')", &[&scope_id,&actor,&key,&old_text,&text,&next,&subject]).await?;
            result.push(wire::SettingEntry::new(key.clone(), scalar(value)?, wire::ValueSource::Stored));
        }
        tx.execute("UPDATE ir_config_setting_scope SET revision=$2 WHERE id=$1", &[&scope_id,&next]).await?;
        tx.invalidate_ormcache("declared_settings");
        if !effective_changes.is_empty() {
            env.records("ir.config.setting")?
                .call::<_, ()>(tx, "_apply_value_changes", (company, effective_changes))
                .await?;
        }
        if changes.iter().any(|(key, _)| key.starts_with("base.jobs.")) {
            let (_, values) = read_job_policy(tx).await?;
            let settings = crate::jobs_settings::settings(&values)?;
            tx.job_settings_changed(&settings).await?;
        }
        Ok(SettingsValues::new(scope, next, result))
    }).await
}

fn toggle_state(enabled: bool) -> wire::GroupToggleState {
    if enabled {
        wire::GroupToggleState::Enabled
    } else {
        wire::GroupToggleState::Disabled
    }
}

/// Turn the feature group of the published group toggle `key` on or off for every internal user, in the caller's
/// transaction, with one Settings audit row in the global scope; returns the new state.
///
/// Only Settings administrators (`base.group_system`) may call it. The declaration is checked again in this
/// transaction. On means the internal-user group implies the feature group; off removes that implication and is
/// refused, naming the groups, while another group the internal-user group implies still implies the feature
/// group (off would not be true). Members of other groups that imply the feature group, and users granted it
/// directly, keep it.
/// # Errors
/// `Access` for a caller without `base.group_system`; `Validation` for a key longer than 128 bytes or that is not
/// a group toggle, a declaration the rules refuse, a row the database made unavailable (its reason), or an off
/// that another implication would contradict; database failures.
pub async fn toggle_group(
    env: &Env,
    tx: &mut Transaction,
    key: &str,
    enabled: bool,
) -> Result<wire::GroupToggleState> {
    if !env.su() && !tx.has_group(env, "base.group_system").await? {
        return Err(Error::access(
            "Only Settings administrators may turn a feature on or off for every user.",
        ));
    }
    // Every declared key fits; a longer one is refused before it is repeated in any message.
    if key.len() > 128 || key.chars().any(char::is_control) {
        return Err(Error::validation(
            "A setting key is at most 128 bytes and has no control characters.",
        ));
    }
    tx.savepoint(async |tx| {
        // Publication may read the install catalogue; that happens before the scope lock is taken.
        let published = published(env, tx).await?;
        let not_toggle = || Error::validation(format!("Setting {key} is not a feature toggle."));
        let item = published.registry.item(key).ok_or_else(not_toggle)?;
        let PanelItemKind::Group { group, .. } = item.kind() else {
            return Err(not_toggle());
        };
        // The global scope lock serializes the change with Settings saves and other toggles.
        tx.execute("INSERT INTO ir_config_setting_scope(identity,company_id,revision) VALUES ('global',NULL,'0') ON CONFLICT(identity) DO NOTHING", &[]).await.map_err(scope_lock_error)?;
        let row = tx.query("SELECT id::int8, revision FROM ir_config_setting_scope WHERE identity='global' FOR UPDATE", &[]).await.map_err(scope_lock_error)?;
        let row = row.first().ok_or_else(corrupt)?;
        let scope_id: i64 = row.try_get(0).map_err(|_| corrupt())?;
        let current: String = row.try_get(1).map_err(|_| corrupt())?;
        // The group rows are read again under the lock: another toggle may have changed them meanwhile.
        let toggles = ToggleGroups::load(env, tx, &published.registry).await?;
        let (id, internal, implied) = toggles.rows(group, enabled)?;
        let was = implied.contains(&id);
        if was == enabled {
            return Ok(toggle_state(was));
        }
        let groups = env.records("res.groups")?.browse([internal])?;
        if enabled {
            groups.call::<_, ()>(tx, "_apply_group", (id,)).await?;
        } else {
            let graph = crate::group_graph::Graph::load(tx).await?;
            let others: Vec<i64> = graph
                .up([internal])
                .into_iter()
                .filter(|other| *other != internal && *other != id && graph.direct(*other).contains(&id))
                .collect();
            if !others.is_empty() {
                // By external identity, or by display name for a group created in the Groups form.
                let mut named: BTreeMap<i64, String> = BTreeMap::new();
                for row in tx
                    .query("SELECT res_id::int8, module || '.' || name FROM ir_model_data WHERE model = 'res.groups' AND res_id::int8 = ANY($1) ORDER BY 2", &[&others])
                    .await?
                {
                    let unreadable = |_| Error::internal("Unreadable group identity row.");
                    named.entry(row.try_get(0).map_err(unreadable)?).or_insert(row.try_get(1).map_err(unreadable)?);
                }
                let unnamed: Vec<i64> = others.iter().copied().filter(|other| !named.contains_key(other)).collect();
                if !unnamed.is_empty() {
                    named.extend(crate::group_graph::display_names(tx, &unnamed).await?);
                }
                let mut names: Vec<String> = named.into_values().collect();
                names.sort();
                return Err(Error::validation(format!(
                    "{} stays on for every internal user through the group {}; change that group instead.",
                    item.label(),
                    names.join(", ")
                )));
            }
            groups.call::<_, ()>(tx, "_remove_group", (id,)).await?;
        }
        let now = crate::group_graph::Graph::load(tx).await?.up([internal]).contains(&id);
        if now != enabled {
            return Err(Error::invariant(format!(
                "Setting {key}: the internal-user group still does not match the requested state."
            )));
        }
        let next = revision(&current).map_err(|_| corrupt())?.checked_add(1).ok_or_else(|| Error::validation("Settings revision exhausted."))?.to_string();
        let (old_text, new_text) = (was.to_string(), now.to_string());
        let subject = env.uid().to_string();
        let actor = audit_actor(env)?;
        tx.execute("INSERT INTO ir_config_setting_audit(scope_id,actor,setting_key,old_value,new_value,revision,subject_uid,create_date) VALUES ($1,$2,$3,$4,$5,$6,$7,CURRENT_TIMESTAMP AT TIME ZONE 'UTC')", &[&scope_id,&actor,&key,&old_text,&new_text,&next,&subject]).await?;
        tx.execute("UPDATE ir_config_setting_scope SET revision=$2 WHERE id=$1", &[&scope_id,&next]).await?;
        Ok(toggle_state(now))
    }).await
}

#[methods(IrConfigSetting)]
impl IrConfigSetting {
    /// Reject invalid declarations before the final registry generation commits or publishes.
    #[method(override, sealed)]
    async fn _register_hook(
        this: &RecordSet<IrConfigSetting>,
        tx: &mut Transaction,
        sup: Super,
    ) -> Result<()> {
        validate_declarations(this.env(), tx).await?;
        sup.call(tx, this.as_records(), ()).await
    }

    /// Native-only hook for effective changes after an authorized scoped save.
    /// `company` is the exact saved scope, or `None` for global settings. The scope lock
    /// and savepoint still cover the staged values, audit and consumer writes.
    #[method]
    async fn _apply_value_changes(
        _this: &RecordSet<IrConfigSetting>,
        _tx: &mut Transaction,
        _company: Option<i64>,
        _changes: erp_sdk::ValueMap,
    ) -> Result<()> {
        Ok(())
    }

    /// Authorized declaration metadata for the exact scope.
    #[method]
    #[rpc(effect = read, model)]
    async fn erp_describe(
        this: &RecordSet<IrConfigSetting>,
        tx: &mut Transaction,
        scope: Value,
    ) -> Result<Value> {
        let scope = serde_json::from_value(json(&scope)?).map_err(|_| invalid())?;
        Ok(Value::Json(Arc::new(
            serde_json::to_value(describe(this.env(), tx, scope).await?).map_err(|_| invalid())?,
        )))
    }
    /// Typed values and observed scope CAS revision.
    #[method]
    #[rpc(effect = read, model)]
    async fn erp_read_values(
        this: &RecordSet<IrConfigSetting>,
        tx: &mut Transaction,
        scope: Value,
        keys: Vec<String>,
    ) -> Result<Value> {
        let scope = serde_json::from_value(json(&scope)?).map_err(|_| invalid())?;
        Ok(Value::Json(Arc::new(
            serde_json::to_value(read_values(this.env(), tx, scope, keys).await?)
                .map_err(|_| invalid())?,
        )))
    }
    /// Turn a group toggle on or off for every internal user ([`toggle_group`]); returns its new state.
    #[method]
    #[rpc(effect = write, model)]
    async fn erp_toggle_group(
        this: &RecordSet<IrConfigSetting>,
        tx: &mut Transaction,
        key: String,
        enabled: bool,
    ) -> Result<Value> {
        let state = toggle_group(this.env(), tx, &key, enabled).await?;
        Ok(Value::Json(Arc::new(
            serde_json::json!({ "key": key, "state": serde_json::to_value(state).map_err(|_| invalid())? }),
        )))
    }
    /// Validated atomic audited write; stale revisions cannot overwrite another writer.
    #[method]
    #[rpc(effect = write, model)]
    async fn erp_write_values(
        this: &RecordSet<IrConfigSetting>,
        tx: &mut Transaction,
        scope: Value,
        expected_revision: String,
        changes: Vec<Value>,
    ) -> Result<Value> {
        if changes.len() > MAX_BATCH {
            return Err(invalid());
        }
        let scope = serde_json::from_value(json(&scope)?).map_err(|_| invalid())?;
        let mut parsed = Vec::new();
        for change in changes {
            let Json::Object(mut object) = json(&change)? else {
                return Err(invalid());
            };
            if object.len() != 2 {
                return Err(invalid());
            }
            let Some(Json::String(key)) = object.remove("key") else {
                return Err(invalid());
            };
            let value = from_json(object.remove("value").ok_or_else(invalid)?)
                .map_err(|_| unsupported_value(&key))?;
            parsed.push((key, value));
        }
        Ok(Value::Json(Arc::new(
            serde_json::to_value(
                write_values(this.env(), tx, scope, expected_revision, parsed).await?,
            )
            .map_err(|_| invalid())?,
        )))
    }
}

// Full ORM elevation does not bypass the service's CAS/audit contract. Native service SQL is
// deliberately narrow and never receives SQL text, scope ids or serialized storage from callers.
macro_rules! service_owned_storage {
    ($model:ident) => {
        #[methods($model)]
        impl $model {
            /// Direct ORM creation is forbidden; use the audited settings service.
            #[allow(
                unused_variables,
                reason = "override signature preserves the ORM method contract"
            )]
            #[method(override)]
            async fn create(
                this: &RecordSet<$model>,
                tx: &mut Transaction,
                vals_list: Vec<RawVals>,
                sup: Super,
            ) -> Result<Records> {
                Err(Error::access(
                    "Settings storage can only be changed by the audited Settings service.",
                ))
            }
            /// Direct ORM mutation is forbidden, including in superuser mode.
            #[allow(
                unused_variables,
                reason = "override signature preserves the ORM method contract"
            )]
            #[method(override)]
            async fn write(
                this: &RecordSet<$model>,
                tx: &mut Transaction,
                vals: RawVals,
                sup: Super,
            ) -> Result<()> {
                Err(Error::access(
                    "Settings storage can only be changed by the audited Settings service.",
                ))
            }
            /// Audit and revision identities cannot be removed through generic ORM deletion.
            #[allow(
                unused_variables,
                reason = "override signature preserves the ORM method contract"
            )]
            #[method(override)]
            async fn unlink(
                this: &RecordSet<$model>,
                tx: &mut Transaction,
                sup: Super,
            ) -> Result<()> {
                Err(Error::access(
                    "Settings storage can only be changed by the audited Settings service.",
                ))
            }
        }
    };
}
service_owned_storage!(IrConfigSettingValue);
service_owned_storage!(IrConfigSettingScope);
service_owned_storage!(IrConfigSettingAudit);
