//!   A getter checks read access on the model and reads one row; a setter creates or
//!   writes through the ORM (so create and write access apply) and returns the previous value.
//! - Administrators (`base.group_system`) manage the model. Secret keys
//!   ([`SECRET_KEYS`] and every declared `secret` parameter) are visible only in superuser mode:
//!   an `#[access_domain]` override adds `key not in <secrets>` to every operation of every other
//!   binding, and the typed API refuses them, so no RPC call, search, domain on `value` or read
//!   returns one (for `database.secret`). A
//!   secret is written with `erp_replace_secret`, which returns nothing.
//! - Renaming or deleting a protected key (the default keys and every declared key) is refused;
//!   `init()` creates the missing defaults, `database.secret` from
//!   the OS CSPRNG.
//! - Every create, write and delete invalidates the `config_parameter` cache, which tells the
//!   authentication settings cache of every process to reload (`cache:config_parameter`).
//! - A value written to a setting of authentication (`keys::ALL`) is checked with the parser the
//!   settings are read with, before the row is written: a value that would make every login fail
//!   is refused with a `Validation` error naming the key and the type (round 1 decision 5).
//! - Every installed addon's declared parameters (`erp_sdk::system_parameters!`) are
//!   checked on create, write, rename and delete: kind and bounds (`<key> takes <bounds>.`), their
//!   validator and their group validators; an undeclared key under the prefix of an installed
//!   addon that declares parameters is refused. `_register_hook` creates the missing default rows.

use erp_sdk::access::{AccessCtx, AccessSuper, ElevationKind, Operation};
use erp_sdk::auth::{
    CACHE_NAME, DatabaseSecret, SessionPolicy, Zeroizing, keys, random_uuid_text,
    validate_setting_in,
};
use erp_sdk::prelude::*;
use erp_sdk::settings::ParameterRegistry;

pub const DEFAULT_KEYS: [&str; 6] = [
    keys::SECRET,
    "database.uuid",
    "database.create_date",
    "web.base.url",
    keys::COOLDOWN_AFTER,
    keys::COOLDOWN_DURATION,
];

/// A closed
/// list: keys an ERPForAll addon owns are declared with `ParameterDecl::secret` instead.
pub const SECRET_KEYS: [&str; 18] = [
    keys::SECRET,
    "database.uuid",
    "base_geolocalize.google_map_api_key",
    "cf.turnstile_secret_key",
    "cloud_storage_azure_client_secret",
    "discuss.klipy_api_key",
    "google_address_autocomplete.google_places_api_key",
    "google_gmail_client_secret",
    "google_maps.signed_static_api_secret",
    "iap_vies.client_token",
    "mail.google_translate_api_key",
    "mail.sfu_server_key",
    "mail.twilio_account_token",
    "mail.web_push_vapid_private_key",
    "microsoft_calendar_client_secret",
    "microsoft_outlook_client_secret",
    "recaptcha_private_key",
    "snailmail.webhook_signing_key",
];

/// Secrets created with the database: `erp_replace_secret` refuses them (`database.secret` is
/// rotated by `erp db rotate-secret`).
const IDENTITY_KEYS: [&str; 2] = [keys::SECRET, "database.uuid"];

/// Every secret key of `registry`: [`SECRET_KEYS`] and the installed parameters declared secret.
/// # Errors
/// The refusals of the parameter registry (`erp_sdk::params::registry_of`).
pub fn secret_keys(registry: &erp_sdk::Registry) -> Result<Vec<String>> {
    let declared = erp_sdk::params::registry_of(registry)?;
    Ok(SECRET_KEYS
        .iter()
        .map(|k| (*k).to_owned())
        .chain(
            declared
                .iter()
                .filter(|d| d.is_secret())
                .map(|d| d.key().to_owned()),
        )
        .collect())
}

/// The refusal of a secret key outside superuser mode.
fn secret_refusal(key: &str) -> Error {
    if key == keys::SECRET {
        return Error::access(
            "ir.config_parameter database.secret is readable and writable only in superuser \
             mode, never over RPC",
        );
    }
    Error::access(format!(
        "ir.config_parameter {key} is a secret: its value is never returned; replace it with \
         erp_replace_secret."
    ))
}

/// `ir.config_parameter`.
#[model(
    name = "ir.config_parameter",
    description = "System Parameter",
    rec_name = "key",
    order = "key",
    allow_sudo_commands = false,
    constraints = [("_key_uniq", "UNIQUE (key)", "Key must be unique.")]
)]
pub struct IrConfigParameter {
    /// The key.
    #[field(char, required = true)]
    pub key: Option<Arc<str>>,
    /// The value as text; empty means undefined. Integrators keep credentials here, so the audit
    /// digest never covers it (`secret`).
    #[field(text, secret = true)]
    pub value: Option<Arc<str>>,
}

/// The types of the typed API.
#[derive(Debug, Clone, Copy)]
enum Kind {
    Int,
    Float,
    Bool,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Self::Int => "int",
            Self::Float => "float",
            Self::Bool => "bool",
        }
    }
}

fn invalid(key: &str, _value: &str, kind: Kind) -> Error {
    Error::validation(format!(
        "ir.config_parameter {key} has a stored value that is not of type {}",
        kind.name()
    ))
}

fn parse_int(key: &str, value: &str) -> Result<i64> {
    value
        .trim()
        .parse::<i64>()
        .map_err(|_| invalid(key, value, Kind::Int))
}

fn parse_float(key: &str, value: &str) -> Result<f64> {
    match value.trim().parse::<f64>() {
        Ok(v) if v.is_finite() => Ok(v),
        _ => Err(invalid(key, value, Kind::Float)),
    }
}

fn parse_bool(key: &str, value: &str) -> Result<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" | "1" | "true" | "t" | "on" => Ok(true),
        "n" | "no" | "0" | "false" | "f" | "off" => Ok(false),
        _ => Err(invalid(key, value, Kind::Bool)),
    }
}

/// Refuse a secret key outside superuser mode.
fn guard_secret(env: &Env, key: &str) -> Result<()> {
    if !env.su() && secret_keys(env.registry())?.iter().any(|k| k == key) {
        return Err(secret_refusal(key));
    }
    Ok(())
}

/// The row of `key`: `(id, value)`, the value `None` when NULL or empty.
async fn row_of(
    this: &RecordSet<IrConfigParameter>,
    tx: &mut Transaction,
    key: &str,
) -> Result<Option<(i64, Option<String>)>> {
    tx.flush_model(this.as_records().model_id()).await?;
    let rows = tx
        .query(
            "SELECT id::int8, value FROM ir_config_parameter WHERE key = $1",
            &[&key],
        )
        .await?;
    match rows.as_slice() {
        [] => Ok(None),
        [row] => {
            let id: i64 = row
                .try_get(0)
                .map_err(|e| Error::db(format!("ir_config_parameter.id: {e}")))?;
            let value: Option<String> = row
                .try_get(1)
                .map_err(|e| Error::db(format!("ir_config_parameter.value: {e}")))?;
            Ok(Some((id, value.filter(|v| !v.is_empty()))))
        }
        more => Err(Error::programming(format!(
            "{} ir_config_parameter rows with key {key}; the key is unique",
            more.len()
        ))),
    }
}

/// A getter: read access on the model, the secret rule, one row.
async fn get_text(
    this: &RecordSet<IrConfigParameter>,
    tx: &mut Transaction,
    key: &str,
) -> Result<Option<String>> {
    guard_secret(this.env(), key)?;
    this.as_records().check_access(tx, Operation::Read).await?;
    Ok(row_of(this, tx, key).await?.and_then(|(_, v)| v))
}

/// A setter: authorize before inspecting or returning the previous value, even for a no-op.
/// Returning the previous value also requires read permission. Creation still goes through
/// the normal ORM create check when no row exists.
async fn set_text<T>(
    this: &RecordSet<IrConfigParameter>,
    tx: &mut Transaction,
    key: &str,
    value: Option<String>,
    parse: impl Fn(&str, &str) -> Result<T>,
) -> Result<Option<T>> {
    guard_secret(this.env(), key)?;
    this.as_records().check_access(tx, Operation::Write).await?;
    this.as_records().check_access(tx, Operation::Read).await?;
    let row = row_of(this, tx, key).await?;
    if let Some((id, _)) = &row {
        let previous = this.browse([*id])?;
        previous
            .as_records()
            .check_access(tx, Operation::Write)
            .await?;
        previous
            .as_records()
            .check_access(tx, Operation::Read)
            .await?;
    }
    let old = match row.as_ref().and_then(|(_, v)| v.as_deref()) {
        Some(text) => Some(parse(key, text)?),
        None => None,
    };
    let stored: Option<Arc<str>> = value.map(Arc::from);
    match row {
        None => {
            this.create(
                tx,
                vec![
                    this.vals()
                        .set(IrConfigParameter::KEY, key)
                        .set(IrConfigParameter::VALUE, stored),
                ],
            )
            .await?;
        }
        Some((id, previous)) => {
            if previous.as_deref() != stored.as_deref() {
                this.browse([id])?
                    .write(tx, this.vals().set(IrConfigParameter::VALUE, stored))
                    .await?;
            }
        }
    }
    Ok(old)
}

/// The text a vals cell writes to the `char`/`text` field `field`: `None` for NULL or `False`.
fn cell_text<'a>(field: &str, cell: &'a Cell) -> Result<Option<&'a str>> {
    match cell {
        Cell::Str(text) => Ok(Some(text)),
        Cell::Null | Cell::Bool(false) => Ok(None),
        other => Err(Error::validation(format!(
            "ir.config_parameter {field} must be text, not {other:?}"
        ))),
    }
}

/// The `field` text of `vals`, when written.
fn written<'a>(vals: &'a RawVals, field: &str) -> Result<Option<Option<&'a str>>> {
    vals.get(field)
        .map(|cell| cell_text(field, cell))
        .transpose()
}

const ATTACHMENT_LOCATION_KEY: &str = "ir_attachment.location";

fn refuse_attachment_location(key: &str) -> Result<()> {
    if key == ATTACHMENT_LOCATION_KEY {
        return Err(Error::validation(
            "ir_attachment.location cannot be set: attachment storage is server configuration \
             (attachments.write_store), not a database parameter.",
        ));
    }
    Ok(())
}

// Any parameter may be renamed into a policy key, so existing-row mutations share this lock.
async fn lock_parameter_changes(tx: &mut Transaction) -> Result<()> {
    tx.query(
        "SELECT pg_advisory_xact_lock(hashtextextended('erp.auth.session-policy', 0))",
        &[],
    )
    .await?;
    Ok(())
}

async fn parameter_rows_for_change(
    this: &RecordSet<IrConfigParameter>,
    tx: &mut Transaction,
) -> Result<Vec<(String, Zeroizing<Option<String>>)>> {
    this.as_records().check_access(tx, Operation::Read).await?;
    lock_parameter_changes(tx).await?;
    tx.flush_model(this.as_records().model_id()).await?;
    let rows = tx.query("SELECT key,value FROM ir_config_parameter WHERE id=ANY($1::int8[]) ORDER BY id FOR UPDATE", &[&this.as_records().ids()]).await?;
    rows.into_iter()
        .map(|row| {
            let key = row.try_get(0).map_err(|e| {
                Error::db("invalid parameter key during policy validation").with_source(e)
            })?;
            let value = row.try_get(1).map_err(|e| {
                Error::db("invalid parameter value during policy validation").with_source(e)
            })?;
            Ok((key, Zeroizing::new(value)))
        })
        .collect()
}

/// A declared key's text checked against its declaration; an undeclared key under the prefix of
/// an installed addon that declares parameters refused.
fn check_declared(params: &ParameterRegistry, key: &str, value: Option<&str>) -> Result<()> {
    match params.get(key) {
        Some(decl) => decl.check_text(value).map(drop),
        None => params.refuse_undeclared(key),
    }
}

/// Whether a change of `key` is checked together with related keys: the session policy, or a
/// member of a declared parameter group.
fn related(params: &ParameterRegistry, key: &str) -> bool {
    keys::SESSION_POLICY.contains(&key) || !params.group_members([key]).is_empty()
}

/// Serialize related parameter changes across nodes, including absent rows (A1).
/// Validate the proposed final state of the session policy and of every touched declared group
/// before mutating any parameter or cache generation.
async fn check_related_changes(
    tx: &mut Transaction,
    params: &ParameterRegistry,
    changes: Vec<(String, Option<String>)>,
) -> Result<()> {
    if changes.is_empty() {
        return Ok(());
    }
    tx.env()
        .records("ir.config_parameter")?
        .check_access(tx, Operation::Read)
        .await?;
    lock_parameter_changes(tx).await?;
    // A lock alone does not advance a REPEATABLE READ snapshot. Version the existing
    // protected default row without changing its value: overlapping old snapshots receive
    // PostgreSQL's serialization refusal instead of admitting write skew on absent keys.
    let fence = tx
        .query(
            "UPDATE ir_config_parameter SET value=value WHERE key='database.uuid' RETURNING id",
            &[],
        )
        .await?;
    if fence.len() != 1 {
        return Err(Error::programming(
            "related parameter checks require initialized database.uuid",
        ));
    }
    let model = tx
        .env()
        .registry()
        .model_id("ir.config_parameter")
        .ok_or_else(|| Error::programming("session policy requires ir.config_parameter"))?;
    tx.flush_model(model).await?;
    let session = changes
        .iter()
        .any(|(key, _)| keys::SESSION_POLICY.contains(&key.as_str()));
    let members = params.group_members(changes.iter().map(|(key, _)| key.as_str()));
    let mut wanted: Vec<String> = members.clone();
    if session {
        wanted.extend(keys::SESSION_POLICY.iter().map(|k| (*k).to_owned()));
    }
    let rows = tx
        .query(
            "SELECT key,value,id::int8 FROM ir_config_parameter WHERE key=ANY($1::text[]) ORDER BY key",
            &[&wanted],
        )
        .await?;
    let ids = rows
        .iter()
        .map(|row| {
            row.try_get::<_, i64>(2)
                .map_err(|e| Error::db("invalid related parameter row identity").with_source(e))
        })
        .collect::<Result<Vec<_>>>()?;
    tx.env()
        .records("ir.config_parameter")?
        .browse(ids)?
        .check_access(tx, Operation::Read)
        .await?;
    let mut proposed = std::collections::HashMap::new();
    for row in rows {
        let key: String = row
            .try_get(0)
            .map_err(|e| Error::db("invalid related parameter key").with_source(e))?;
        let value: Option<String> = row
            .try_get(1)
            .map_err(|e| Error::db("invalid related parameter value").with_source(e))?;
        proposed.insert(key, value);
    }
    let changed: std::collections::BTreeSet<String> =
        changes.iter().map(|(key, _)| key.clone()).collect();
    for (key, value) in changes {
        proposed.insert(key, value);
    }
    if session {
        SessionPolicy::from_rows(&proposed)?;
    }
    if !members.is_empty() {
        let mut values = std::collections::BTreeMap::new();
        for member in members {
            let decl = params.get(&member).ok_or_else(|| {
                Error::programming(format!("parameter group member {member} is not declared"))
            })?;
            let text = proposed.get(&member).and_then(|v| v.as_deref());
            // a proposed value is refused with the write sentence, a stored one as invalid
            let value = if changed.contains(&member) {
                decl.check_text(text)?
            } else {
                decl.read_text(text)?
            };
            values.insert(member, value);
        }
        params.validate_groups(&values)?;
    }
    Ok(())
}

/// Check the parameters among `vals_list` before they are created: base's own write-time checks
/// (a hashing memory value also against the database's hashing pool, security review round 2
/// M1), then the declarations of every installed addon.
async fn check_created_settings(tx: &mut Transaction, vals_list: &[RawVals]) -> Result<()> {
    let env = tx.env();
    let params = erp_sdk::params::registry(&env)?;
    let mut changes = Vec::new();
    for vals in vals_list {
        if let Some(Some(key)) = written(vals, "key")? {
            let value = written(vals, "value")?.flatten();
            refuse_attachment_location(key)?;
            validate_setting_in(&env, key, value)?;
            crate::attachments::settings::validate(key, value)?;
            crate::partition_upkeep_settings::validate(key, value)?;
            check_declared(&params, key, value)?;
            if related(&params, key) {
                changes.push((key.to_owned(), value.map(str::to_owned)));
            }
        }
    }
    check_related_changes(tx, &params, changes).await
}

/// Check the parameters `vals` would leave on the records of `this` (a new value, or a key
/// renamed onto a parameter with the record's value), before the write.
async fn check_written_settings(
    this: &RecordSet<IrConfigParameter>,
    tx: &mut Transaction,
    vals: &RawVals,
) -> Result<()> {
    let new_key = written(vals, "key")?;
    let new_value = written(vals, "value")?;
    if let Some(Some(key)) = new_key {
        refuse_attachment_location(key)?;
    }
    if new_key.is_none() && new_value.is_none() {
        return Ok(());
    }
    let params = erp_sdk::params::registry(&tx.env())?;
    let mut changes = Vec::new();
    for (old_key, stored_value) in parameter_rows_for_change(this, tx).await? {
        // renaming a related key away resets it to its default
        if related(&params, &old_key) && new_key.is_some_and(|key| key != Some(old_key.as_str())) {
            changes.push((old_key.clone(), None));
        }
        let Some(key) = new_key.unwrap_or(Some(old_key.as_str())) else {
            continue;
        };
        refuse_attachment_location(key)?;
        let declared = params.get(key).is_some();
        if !declared {
            params.refuse_undeclared(key)?;
        }
        if !declared
            && !keys::ALL.contains(&key)
            && !crate::attachments::settings::contains(key)
            && key != crate::partition_upkeep_settings::KEY
        {
            continue;
        }
        let value = new_value.unwrap_or(stored_value.as_deref());
        validate_setting_in(&tx.env(), key, value)?;
        crate::attachments::settings::validate(key, value)?;
        crate::partition_upkeep_settings::validate(key, value)?;
        check_declared(&params, key, value)?;
        if related(&params, key) {
            changes.push((key.to_owned(), value.map(str::to_owned)));
        }
    }
    check_related_changes(tx, &params, changes).await
}

/// Create a row holding the declared default for every declared key without one.
/// An existing row, emptied or shipped by a data file, keeps its value.
async fn create_default_rows(
    this: &RecordSet<IrConfigParameter>,
    tx: &mut Transaction,
) -> Result<()> {
    let params = crate::system_parameters::published(this.env())?;
    let declared: Vec<String> = params.iter().map(|d| d.key().to_owned()).collect();
    if declared.is_empty() {
        return Ok(());
    }
    tx.flush_model(this.as_records().model_id()).await?;
    let rows = tx
        .query(
            "SELECT ARRAY(SELECT key::text FROM ir_config_parameter WHERE key = ANY($1::text[]))",
            &[&declared],
        )
        .await?;
    let present: Vec<String> = rows
        .first()
        .ok_or_else(|| Error::programming("the declared keys statement returned no row"))?
        .try_get(0)
        .map_err(|e| Error::db(format!("ir_config_parameter keys: {e}")))?;
    let sudo = this.sudo(true);
    for decl in params.iter().filter(|d| {
        // an absent per-login threshold follows the per-address one (4x); a row would freeze it
        d.key() != keys::COOLDOWN_AFTER_LOGIN && !present.iter().any(|p| p == d.key())
    }) {
        let created = tx
            .savepoint(async |tx| {
                let vals = sudo
                    .vals()
                    .set(IrConfigParameter::KEY, decl.key())
                    .set(IrConfigParameter::VALUE, decl.default_text());
                sudo.create(tx, vec![vals]).await.map(drop)
            })
            .await;
        match created {
            Ok(()) => {}
            // another node's load created the same row first: the default row exists
            Err(e) if e.sqlstate() == Some("23505") => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// The keys of `this` that are protected, sorted: the default keys, the managed attachment size
/// and every declared parameter.
async fn protected_keys_of(
    this: &RecordSet<IrConfigParameter>,
    tx: &mut Transaction,
) -> Result<Vec<String>> {
    let declared = erp_sdk::params::registry(&tx.env())?;
    let mut found = Vec::new();
    for rec in this.iter() {
        // the first read prefetches the key of every record of the set
        if let Some(key) = rec.get(tx, IrConfigParameter::KEY).await?
            && (DEFAULT_KEYS.contains(&&*key)
                || &*key == crate::attachments::settings::MAX_FILE_KEY
                || declared.get(&key).is_some())
        {
            found.push(key.to_string());
        }
    }
    found.sort();
    found.dedup();
    Ok(found)
}

#[methods(IrConfigParameter)]
impl IrConfigParameter {
    /// The text of `key`, `default` (empty) when undefined.
    #[method]
    #[rpc(effect = read, model)]
    async fn get_str(
        this: &RecordSet<IrConfigParameter>,
        tx: &mut Transaction,
        key: String,
        default: Option<String>,
    ) -> Result<String> {
        Ok(get_text(this, tx, &key)
            .await?
            .unwrap_or_else(|| default.unwrap_or_default()))
    }

    /// The integer of `key`, `default` (0) when undefined.
    #[method]
    #[rpc(effect = read, model)]
    async fn get_int(
        this: &RecordSet<IrConfigParameter>,
        tx: &mut Transaction,
        key: String,
        default: Option<i64>,
    ) -> Result<i64> {
        match get_text(this, tx, &key).await? {
            Some(text) => parse_int(&key, &text),
            None => Ok(default.unwrap_or(0)),
        }
    }

    /// The number of `key`, `default` (0.0) when undefined.
    #[method]
    #[rpc(effect = read, model)]
    async fn get_float(
        this: &RecordSet<IrConfigParameter>,
        tx: &mut Transaction,
        key: String,
        default: Option<f64>,
    ) -> Result<f64> {
        match get_text(this, tx, &key).await? {
            Some(text) => parse_float(&key, &text),
            None => Ok(default.unwrap_or(0.0)),
        }
    }

    /// The flag of `key`, `default` (false) when undefined.
    #[method]
    #[rpc(effect = read, model)]
    async fn get_bool(
        this: &RecordSet<IrConfigParameter>,
        tx: &mut Transaction,
        key: String,
        default: Option<bool>,
    ) -> Result<bool> {
        match get_text(this, tx, &key).await? {
            Some(text) => parse_bool(&key, &text),
            None => Ok(default.unwrap_or(false)),
        }
    }

    /// Set `key` to `value` (`None`: undefined); returns the previous text, empty when undefined.
    #[method]
    #[rpc(effect = write, model)]
    async fn set_str(
        this: &RecordSet<IrConfigParameter>,
        tx: &mut Transaction,
        key: String,
        value: Option<String>,
    ) -> Result<String> {
        let old = set_text(this, tx, &key, value, |_, v| Ok(v.to_owned())).await?;
        Ok(old.unwrap_or_default())
    }

    /// Set `key` to `value`; returns the previous integer, 0 when undefined.
    #[method]
    #[rpc(effect = write, model)]
    async fn set_int(
        this: &RecordSet<IrConfigParameter>,
        tx: &mut Transaction,
        key: String,
        value: Option<i64>,
    ) -> Result<i64> {
        let text = value.map(|v| v.to_string());
        Ok(set_text(this, tx, &key, text, parse_int)
            .await?
            .unwrap_or(0))
    }

    /// Set `key` to `value`; returns the previous number, 0.0 when undefined.
    #[method]
    #[rpc(effect = write, model)]
    async fn set_float(
        this: &RecordSet<IrConfigParameter>,
        tx: &mut Transaction,
        key: String,
        value: Option<f64>,
    ) -> Result<f64> {
        let text = match value {
            Some(v) if !v.is_finite() => {
                return Err(Error::validation(format!(
                    "ir.config_parameter {key}: {v} is not a finite number"
                )));
            }
            // `{:?}` keeps the decimal point (`0.0`), as Python's `str(float)`
            Some(v) => Some(format!("{v:?}")),
            None => None,
        };
        Ok(set_text(this, tx, &key, text, parse_float)
            .await?
            .unwrap_or(0.0))
    }

    #[method]
    #[rpc(effect = write, model)]
    async fn set_bool(
        this: &RecordSet<IrConfigParameter>,
        tx: &mut Transaction,
        key: String,
        value: Option<bool>,
    ) -> Result<bool> {
        let text = value.map(|v| if v { "True" } else { "False" }.to_owned());
        Ok(set_text(this, tx, &key, text, parse_bool)
            .await?
            .unwrap_or(false))
    }

    /// Creating parameters changes what the settings caches hold; settings of authentication are
    /// checked first. A secret key is created only in superuser mode (`erp_replace_secret`).
    #[method(override)]
    async fn create(
        this: &RecordSet<IrConfigParameter>,
        tx: &mut Transaction,
        vals_list: Vec<RawVals>,
        sup: Super,
    ) -> Result<Records> {
        this.as_records()
            .check_access(tx, Operation::Create)
            .await?;
        // the base create fills a missing key or value from `default_<field>` after these checks
        let context = this.env().context();
        if context.contains("default_key") || context.contains("default_value") {
            return Err(Error::validation(
                "A system parameter takes its key and value from the record, never from a \
                 default in the context.",
            ));
        }
        for vals in &vals_list {
            if let Some(Some(key)) = written(vals, "key")? {
                guard_secret(this.env(), key)?;
            }
        }
        let mut managed = false;
        for vals in &vals_list {
            managed |=
                written(vals, "key")?.flatten() == Some(crate::attachments::settings::MAX_FILE_KEY);
        }
        let created: Records = if managed {
            tx.savepoint(async |tx| {
                // Match managed writes: acquire the settings scope before the auth-policy
                // lock, and roll back the entire mixed batch if either validator refuses it.
                let edit = crate::ir_config_setting::begin_parameter_edit(tx).await?;
                check_created_settings(tx, &vals_list).await?;
                let rows: Records = sup.call(tx, this.as_records(), (vals_list,)).await?;
                crate::ir_config_setting::finish_parameter_edit(tx, edit).await?;
                Ok(rows)
            })
            .await?
        } else {
            check_created_settings(tx, &vals_list).await?;
            sup.call(tx, this.as_records(), (vals_list,)).await?
        };
        tx.invalidate_ormcache(CACHE_NAME);
        Ok(created)
    }

    /// Protected keys cannot be renamed, nor can a key be renamed
    /// onto a secret outside superuser mode; settings of authentication are checked.
    #[method(override)]
    async fn write(
        this: &RecordSet<IrConfigParameter>,
        tx: &mut Transaction,
        vals: RawVals,
        sup: Super,
    ) -> Result<()> {
        this.as_records().check_access(tx, Operation::Write).await?;
        if let Some(Some(key)) = written(&vals, "key")? {
            guard_secret(this.env(), key)?;
        }
        if vals.contains("key") {
            let illegal = protected_keys_of(this, tx).await?;
            if !illegal.is_empty() {
                return Err(Error::validation(format!(
                    "You cannot rename config parameters with keys {}",
                    illegal.join(", ")
                )));
            }
        }
        let key = crate::attachments::settings::MAX_FILE_KEY;
        let renames_to_managed = written(&vals, "key")?.flatten() == Some(key);
        let ids = this.as_records().ids();
        let rows = tx.query("SELECT EXISTS(SELECT 1 FROM ir_config_parameter WHERE id=ANY($1::int8[]) AND key=$2)", &[&ids,&key]).await?;
        let row = rows
            .first()
            .ok_or_else(|| Error::db("Missing managed parameter result."))?;
        let managed: bool = row
            .try_get(0)
            .map_err(|e| Error::db(format!("managed parameter: {e}")))?;
        if managed || renames_to_managed {
            tx.savepoint(async |tx| {
                let edit = crate::ir_config_setting::begin_parameter_edit(tx).await?;
                check_written_settings(this, tx, &vals).await?;
                sup.call(tx, this.as_records(), (vals,)).await?;
                crate::ir_config_setting::finish_parameter_edit(tx, edit).await
            })
            .await?;
        } else {
            check_written_settings(this, tx, &vals).await?;
            sup.call(tx, this.as_records(), (vals,)).await?;
        }
        tx.invalidate_ormcache(CACHE_NAME);
        Ok(())
    }

    /// Protected keys cannot be deleted. Every related key (the
    /// session policy, a group member) is declared, so a delete never changes a related value.
    #[method(override)]
    async fn unlink(
        this: &RecordSet<IrConfigParameter>,
        tx: &mut Transaction,
        sup: Super,
    ) -> Result<()> {
        this.as_records()
            .check_access(tx, Operation::Unlink)
            .await?;
        if let Some(key) = protected_keys_of(this, tx).await?.first() {
            return Err(Error::validation(format!(
                "You cannot delete the {key} record."
            )));
        }
        sup.call(tx, this.as_records(), ()).await?;
        tx.invalidate_ormcache(CACHE_NAME);
        Ok(())
    }

    /// Create the missing defaults: `database.secret` (64 hexadecimal
    /// characters of 32 OS CSPRNG bytes), `database.uuid`, `database.create_date` (UTC),
    /// `base.login_cooldown_after` 10 and `base.login_cooldown_duration` 60. `web.base.url` is
    /// 's (the canonical origin).
    #[method(override)]
    async fn init(
        this: &RecordSet<IrConfigParameter>,
        tx: &mut Transaction,
        sup: Super,
    ) -> Result<()> {
        sup.call(tx, this.as_records(), ()).await?;
        let wanted: Vec<&str> = DEFAULT_KEYS
            .iter()
            .copied()
            .filter(|k| *k != "web.base.url")
            .collect();
        let rows = tx
            .query(
                "SELECT ARRAY(SELECT key::text FROM ir_config_parameter WHERE key = ANY($1::text[])), \
                        to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI:SS')",
                &[&wanted],
            )
            .await?;
        let row = rows
            .first()
            .ok_or_else(|| Error::programming("the defaults statement returned no row"))?;
        let present: Vec<String> = row
            .try_get(0)
            .map_err(|e| Error::db(format!("ir_config_parameter keys: {e}")))?;
        let now: String = row
            .try_get(1)
            .map_err(|e| Error::db(format!("the database clock: {e}")))?;
        let mut vals_list = Vec::new();
        for key in wanted {
            if present.iter().any(|p| p == key) {
                continue;
            }
            let value: String = match key {
                keys::SECRET => DatabaseSecret::generate_text()?.to_string(),
                "database.uuid" => random_uuid_text()?,
                "database.create_date" => now.clone(),
                keys::COOLDOWN_AFTER => "10".to_owned(),
                keys::COOLDOWN_DURATION => "60".to_owned(),
                other => {
                    return Err(Error::programming(format!(
                        "ir.config_parameter.init has no value for the default key {other}"
                    )));
                }
            };
            vals_list.push(
                this.vals()
                    .set(IrConfigParameter::KEY, key)
                    .set(IrConfigParameter::VALUE, value),
            );
        }
        if !vals_list.is_empty() {
            this.sudo(true).create(tx, vals_list).await?;
        }
        Ok(())
    }

    /// Every declared parameter gets its default row when the registry is published;
    /// the declarations are checked first.
    #[method(override)]
    async fn _register_hook(
        this: &RecordSet<IrConfigParameter>,
        tx: &mut Transaction,
        sup: Super,
    ) -> Result<()> {
        sup.call(tx, this.as_records(), ()).await?;
        create_default_rows(this, tx).await
    }

    /// Secret keys are hidden from every binding without superuser mode.
    #[access_domain]
    fn _access_domain(ctx: &AccessCtx, op: Operation, sup: AccessSuper) -> Result<Domain> {
        let base = sup.call(ctx, op)?;
        if ctx.elevation() == ElevationKind::Full {
            return Ok(base);
        }
        Ok(base & Domain::condition("key", Operator::NotIn, secret_keys(ctx.registry())?)?)
    }

    /// Replace the value of the secret parameter `key` (`None`: undefined) without reading it:
    /// the caller needs write and create access on the model (`base.group_system`); the row is
    /// then written elevated through the same checks as every other write. Returns nothing.
    #[method]
    #[rpc(effect = write, model)]
    async fn erp_replace_secret(
        this: &RecordSet<IrConfigParameter>,
        tx: &mut Transaction,
        key: String,
        value: Option<String>,
    ) -> Result<()> {
        this.as_records().check_access(tx, Operation::Write).await?;
        this.as_records()
            .check_access(tx, Operation::Create)
            .await?;
        if !secret_keys(this.env().registry())?.contains(&key) {
            return Err(Error::validation(format!(
                "Parameter {key} is not a secret; change it in System Parameters."
            )));
        }
        if IDENTITY_KEYS.contains(&key.as_str()) {
            let rotate = if key == keys::SECRET {
                " (rotate it with erp db rotate-secret)"
            } else {
                ""
            };
            return Err(Error::access(format!(
                "Parameter {key} is created with the database and cannot be replaced over \
                 RPC{rotate}."
            )));
        }
        let sudo = this.sudo(true);
        let stored: Option<Arc<str>> = value.map(Arc::from);
        match row_of(&sudo, tx, &key).await? {
            None => {
                sudo.create(
                    tx,
                    vec![
                        sudo.vals()
                            .set(IrConfigParameter::KEY, key.as_str())
                            .set(IrConfigParameter::VALUE, stored),
                    ],
                )
                .await?;
            }
            Some((id, _)) => {
                sudo.browse([id])?
                    .write(tx, sudo.vals().set(IrConfigParameter::VALUE, stored))
                    .await?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parsers_follow_reference_and_fail_loudly() {
        assert_eq!(parse_int("k", " 10 ").ok(), Some(10));
        assert_eq!(parse_float("k", "0.0").ok(), Some(0.0));
        assert_eq!(parse_bool("k", "True").ok(), Some(true));
        assert_eq!(parse_bool("k", "off").ok(), Some(false));
        let e = parse_int("bad_int", "invalid integer").unwrap_err();
        assert_eq!(e.kind(), erp_sdk::ErrorKind::Validation);
        assert_eq!(
            e.message(),
            "ir.config_parameter bad_int has a stored value that is not of type int"
        );
        assert!(parse_float("k", "nan").is_err());
        assert!(parse_bool("k", "maybe").is_err());
    }
}
