//!   A getter checks read access on the model and reads one row; a setter creates or
//!   writes through the ORM (so create and write access apply) and returns the previous value.
//! - `database.secret` is visible only in superuser mode: an `#[access_domain]` override adds
//!   `key != 'database.secret'` to every operation of every other binding, and the typed API
//!   refuses the key, so no RPC call, export, search or read returns it (
//!   stricter than its "system group", design note §14.6).
//! - Renaming or deleting a default key is refused; `init()` creates the missing
//!   defaults, `database.secret` from the OS CSPRNG.
//! - Every create, write and delete invalidates the `config_parameter` cache, which tells the
//!   authentication settings cache of every process to reload (`cache:config_parameter`).
//! - A value written to a setting of authentication (`keys::ALL`) is checked with the parser the
//!   settings are read with, before the row is written: a value that would make every login fail
//!   is refused with a `Validation` error naming the key and the type (round 1 decision 5).

use erp_sdk::access::{AccessCtx, AccessSuper, ElevationKind, Operation};
use erp_sdk::auth::{CACHE_NAME, DatabaseSecret, keys, random_uuid_text, validate_setting_in};
use erp_sdk::prelude::*;

pub const DEFAULT_KEYS: [&str; 6] = [
    keys::SECRET,
    "database.uuid",
    "database.create_date",
    "web.base.url",
    keys::COOLDOWN_AFTER,
    keys::COOLDOWN_DURATION,
];

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

/// Refuse `database.secret` outside superuser mode.
fn guard_secret(env: &Env, key: &str) -> Result<()> {
    if key == keys::SECRET && !env.su() {
        return Err(Error::access(
            "ir.config_parameter database.secret is readable and writable only in superuser \
             mode, never over RPC",
        ));
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

/// Check the settings of authentication among `vals_list` before they are created (a hashing
/// memory value also against the database's hashing pool, security review round 2 M1).
fn check_created_settings(tx: &Transaction, vals_list: &[RawVals]) -> Result<()> {
    let env = tx.env();
    for vals in vals_list {
        if let Some(Some(key)) = written(vals, "key")? {
            refuse_attachment_location(key)?;
            validate_setting_in(&env, key, written(vals, "value")?.flatten())?;
        }
    }
    Ok(())
}

/// Check the settings of authentication `vals` would leave on the records of `this` (a new value,
/// or a key renamed onto a setting with the record's value), before the write.
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
    for rec in this.iter() {
        // the first read prefetches the set
        let key: Option<Arc<str>> = match new_key {
            Some(key) => key.map(Arc::from),
            None => rec.get(tx, IrConfigParameter::KEY).await?,
        };
        if let Some(key) = &key {
            refuse_attachment_location(key)?;
        }
        let Some(key) = key.filter(|k| keys::ALL.contains(&&**k)) else {
            continue;
        };
        let value: Option<Arc<str>> = match new_value {
            Some(value) => value.map(Arc::from),
            None => rec.get(tx, IrConfigParameter::VALUE).await?,
        };
        validate_setting_in(&tx.env(), &key, value.as_deref())?;
    }
    Ok(())
}

/// The keys of `this` that are default keys, sorted.
async fn default_keys_of(
    this: &RecordSet<IrConfigParameter>,
    tx: &mut Transaction,
) -> Result<Vec<String>> {
    let mut found = Vec::new();
    for rec in this.iter() {
        // the first read prefetches the key of every record of the set
        if let Some(key) = rec.get(tx, IrConfigParameter::KEY).await?
            && DEFAULT_KEYS.contains(&&*key)
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
    /// checked first.
    #[method(override)]
    async fn create(
        this: &RecordSet<IrConfigParameter>,
        tx: &mut Transaction,
        vals_list: Vec<RawVals>,
        sup: Super,
    ) -> Result<Records> {
        check_created_settings(tx, &vals_list)?;
        let created: Records = sup.call(tx, this.as_records(), (vals_list,)).await?;
        tx.invalidate_ormcache(CACHE_NAME);
        Ok(created)
    }

    /// Default keys cannot be renamed; settings of authentication are checked.
    #[method(override)]
    async fn write(
        this: &RecordSet<IrConfigParameter>,
        tx: &mut Transaction,
        vals: RawVals,
        sup: Super,
    ) -> Result<()> {
        if vals.contains("key") {
            let illegal = default_keys_of(this, tx).await?;
            if !illegal.is_empty() {
                return Err(Error::validation(format!(
                    "You cannot rename config parameters with keys {}",
                    illegal.join(", ")
                )));
            }
        }
        check_written_settings(this, tx, &vals).await?;
        sup.call(tx, this.as_records(), (vals,)).await?;
        tx.invalidate_ormcache(CACHE_NAME);
        Ok(())
    }

    /// Default keys cannot be deleted.
    #[method(override)]
    async fn unlink(
        this: &RecordSet<IrConfigParameter>,
        tx: &mut Transaction,
        sup: Super,
    ) -> Result<()> {
        if let Some(key) = default_keys_of(this, tx).await?.first() {
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

    /// `database.secret` is hidden from every binding without superuser mode.
    #[access_domain]
    fn _access_domain(ctx: &AccessCtx, op: Operation, sup: AccessSuper) -> Result<Domain> {
        let base = sup.call(ctx, op)?;
        if ctx.elevation() == ElevationKind::Full {
            return Ok(base);
        }
        Ok(base & Domain::condition("key", Operator::Ne, keys::SECRET)?)
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
