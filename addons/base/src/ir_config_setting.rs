//! Declared settings, durable exact-scope CAS and transactional audit.
//!
//! Only this service writes storage; ordinary ORM grants cover the facade and audit reads.
//! There is no settings value cache: every node reads committed PostgreSQL state.
use erp_sdk::prelude::*;
use erp_sdk::serde_json::{self, Value as Json};
use erp_sdk::settings::{ScopeKind, SettingDecl, SettingKind, SettingsRegistry};
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
/// Validate all installed native declarations before publishing a lifecycle generation.
///
/// This is also called by the model's existing precommit `_register_hook` lifecycle seam.
/// # Errors
/// Invalid ownership/dependency closure, duplicate keys, unavailable groups or invalid declarations.
pub async fn validate_declarations(env: &Env, tx: &mut Transaction) -> Result<()> {
    registry(env, tx).await.map(|_| ())
}

async fn registry(env: &Env, tx: &mut Transaction) -> Result<SettingsRegistry> {
    // The lifecycle registry contains only installed dependency-resolved addons.
    let installed: BTreeSet<String> = env
        .registry()
        .addons()
        .iter()
        .map(|a| a.name.into())
        .collect();
    let mut contributions = Vec::new();
    let mut references = BTreeSet::new();
    for entry in erp_sdk::settings::CONTRIBUTIONS {
        if !installed.contains(entry.addon.name) {
            continue;
        }
        let actual = env
            .registry()
            .addons()
            .iter()
            .find(|a| a.name == entry.addon.name)
            .ok_or_else(invalid)?;
        if actual.depends != entry.addon.depends {
            return Err(invalid());
        }
        let declarations = (entry.declarations)()?;
        for decl in &declarations {
            for group in decl.read_groups().iter().chain(decl.write_groups()) {
                // A group may only come from the contributing addon's transitive dependencies.
                let owner = group.split_once('.').ok_or_else(invalid)?.0;
                if !dependency_contains(env, entry.addon.name, owner) {
                    return Err(invalid());
                }
                references.insert(group.clone());
            }
        }
        contributions.push((entry.addon.name.to_owned(), declarations));
    }
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
    SettingsRegistry::build(contributions, &installed, |group| resolved.contains(group))
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
/// Describe permitted declarations only, sorted by key. No policy identities are returned.
/// # Errors
/// Unauthorized scope, invalid declaration registry, storage or access errors.
pub async fn describe(
    env: &Env,
    tx: &mut Transaction,
    scope: SettingsScope,
) -> Result<wire::SettingsDescription> {
    authorize_scope(env, tx, &scope).await?;
    let (_, _, kind) = identity(&scope)?;
    let registry = registry(env, tx).await?;
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
        ));
    }
    Ok(wire::SettingsDescription::new(
        env.registry().generation().to_string(),
        scope,
        result,
    ))
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
