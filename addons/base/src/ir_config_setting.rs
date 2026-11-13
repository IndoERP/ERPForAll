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

const DENIED: &str = "Setting is unavailable or access is denied.";
const CONFLICT: &str = "Settings changed; reload before saving.";
const MAX_BATCH: usize = 256;
const MAX_STORED_VALUE_BYTES: usize = 400_000;

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
        return Err(Error::access(
            "Access to unauthorized or invalid companies.",
        ));
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
/// ("Setting item references an unavailable action or menu.") or invalid declarations.
pub async fn validate_declarations(env: &Env, tx: &mut Transaction) -> Result<()> {
    published(env, tx).await.map(|_| ())
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

async fn registry(env: &Env, tx: &mut Transaction) -> Result<SettingsRegistry> {
    let installed = installed_addons(env);
    let mut references = BTreeSet::new();
    let contributions = setting_contributions(env, &installed, &mut references)?;
    let resolved = resolve_groups(tx, references).await?;
    SettingsRegistry::build(contributions, &installed, |group| resolved.contains(group))
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

/// The settings registry with its panel items, each item's resolver and its link row ids.
struct Published {
    registry: SettingsRegistry,
    resolvers: BTreeMap<String, PanelResolver>,
    links: LinkRows,
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
            PanelItemKind::Info => {}
            PanelItemKind::Link(target) => targets.push(target),
            _ => return Err(unsupported_item()),
        }
    }
    let links = link_rows(tx, &targets).await?;
    let resolved = resolve_groups(tx, references).await?;
    let registry =
        SettingsRegistry::build(contributions, &installed, |group| resolved.contains(group))?
            .with_items(
                items,
                &installed,
                |group| resolved.contains(group),
                |owner, target| link_available(env, &links, owner, target).is_some(),
            )?;
    Ok(Published {
        registry,
        resolvers,
        links,
    })
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
    let declaration = registry.get(key).ok_or_else(|| Error::access(DENIED))?;
    if declaration.scope() != scope
        || !permits(env, tx, declaration.read_groups()).await?
        || write && !permits(env, tx, declaration.write_groups()).await?
    {
        return Err(Error::access(DENIED));
    }
    Ok(declaration)
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
    if keys.iter().any(|key| key.len() > 128 || !seen.insert(key)) {
        return Err(invalid());
    }
    Ok(())
}
/// Describe permitted declarations only, sorted by key, then the permitted information and link rows of
/// the scope. No policy identities are returned.
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
        result.push(wire::SettingDescription::new(
            decl.key().into(),
            decl.label().into(),
            kind,
            scalar(decl.default_value())?,
            choices,
            permits(env, tx, decl.write_groups()).await?,
            decl.target_app().into(),
            decl.block_title().into(),
            decl.display_sequence(),
        ));
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
    authorize_scope(env, tx, &scope).await?;
    let (identity, _, kind) = identity(&scope)?;
    let registry = registry(env, tx).await?;
    for key in &keys {
        declared(&registry, env, tx, kind, key, false).await?;
    }
    // Read revision and values in a single statement; READ COMMITTED cannot mix revisions.
    // Keep a present key when withholding oversized/corrupt text. Its NULL projection below
    // must decode as the existing corruption error, never as an absent/default setting.
    let sql = format!(
        "SELECT s.revision, v.setting_key, CASE WHEN octet_length(v.value_text) <= {MAX_STORED_VALUE_BYTES} THEN v.value_text END FROM ir_config_setting_scope s LEFT JOIN ir_config_setting_value v ON v.scope_id=s.id AND v.setting_key=ANY($2) WHERE s.identity=$1"
    );
    let rows = tx.query(&sql, &[&identity, &keys]).await?;
    let mut current = "0".to_owned();
    let mut values = BTreeMap::new();
    for row in rows {
        current = row.try_get(0).map_err(|_| corrupt())?;
        revision(&current).map_err(|_| corrupt())?;
        let key: Option<String> = row.try_get(1).map_err(|_| corrupt())?;
        if let Some(key) = key {
            let text: String = row.try_get(2).map_err(|_| corrupt())?;
            values.insert(key, text);
        }
    }
    let mut result = Vec::new();
    for key in keys {
        let decl = registry.get(&key).ok_or_else(|| Error::access(DENIED))?;
        let (value, source) = match values.get(&key) {
            Some(text) => (stored(decl, text)?, wire::ValueSource::Stored),
            None => (decl.default_value().clone(), wire::ValueSource::Default),
        };
        result.push(wire::SettingEntry::new(key, scalar(&value)?, source));
    }
    Ok(SettingsValues::new(scope, current, result))
}
/// Atomically validate, lock, compare and write a batch with audit in the caller's transaction.
/// # Errors
/// Invalid or unauthorized batches, stale revisions, overflow and database failures; no prefix persists.
pub async fn write_values(
    env: &Env,
    tx: &mut Transaction,
    scope: SettingsScope,
    expected_revision: String,
    changes: Vec<(String, Value)>,
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
    for (key, value) in &changes {
        declared(&registry, env, tx, kind, key, true)
            .await?
            .validate(value)?;
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
        let old = read_values(env, tx, scope.clone(), keys.clone()).await?;
        let subject = env.uid().to_string();
        let actor = match env.principal().actor() {
            Some(erp_sdk::Actor::ApiKey { id, .. }) => format!("api_key:{}", id.get()),
            Some(erp_sdk::Actor::Impersonator(principal)) => format!("user:{}", principal.subject().get()),
            Some(erp_sdk::Actor::Link { id }) => format!("link:{}", id.get()),
            None => format!("user:{subject}"),
            Some(_) => return Err(Error::validation("Unsupported audit actor; requires a later release actor mapping.")),
        };
        let mut result = Vec::new();
        for ((key,value), previous) in changes.iter().zip(old.values) {
            let old_text = serde_json::to_string(&previous.value).map_err(|_| corrupt())?;
            let text = serde_json::to_string(&scalar(value)?).map_err(|_| invalid())?;
            tx.execute("INSERT INTO ir_config_setting_value(scope_id,setting_key,value_text) VALUES ($1,$2,$3) ON CONFLICT(scope_id,setting_key) DO UPDATE SET value_text=EXCLUDED.value_text", &[&scope_id, &key, &text]).await?;
            tx.execute("INSERT INTO ir_config_setting_audit(scope_id,actor,setting_key,old_value,new_value,revision,subject_uid,create_date) VALUES ($1,$2,$3,$4,$5,$6,$7,CURRENT_TIMESTAMP AT TIME ZONE 'UTC')", &[&scope_id,&actor,&key,&old_text,&text,&next,&subject]).await?;
            result.push(wire::SettingEntry::new(key.clone(), scalar(value)?, wire::ValueSource::Stored));
        }
        tx.execute("UPDATE ir_config_setting_scope SET revision=$2 WHERE id=$1", &[&scope_id,&next]).await?;
        tx.invalidate_ormcache("declared_settings");
        Ok(SettingsValues::new(scope, next, result))
    }).await
}

#[methods(IrConfigSetting)]
impl IrConfigSetting {
    /// Reject invalid declarations before the final registry generation commits or publishes.
    #[method(override)]
    async fn _register_hook(
        this: &RecordSet<IrConfigSetting>,
        tx: &mut Transaction,
        sup: Super,
    ) -> Result<()> {
        validate_declarations(this.env(), tx).await?;
        sup.call(tx, this.as_records(), ()).await
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
            let value = from_json(object.remove("value").ok_or_else(invalid)?)?;
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
