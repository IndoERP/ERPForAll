//! User settings and durable preferences (`res.users.settings`).

use std::sync::Arc;

use erp_sdk::prelude::*;
use erp_sdk::serde_json;
use erp_sdk::{ErrorKind, ValueMap};

pub const MAX_HOMEMENU_CONFIG_BYTES: usize = 4096;
pub const MAX_HOMEMENU_APPS: usize = 256;
/// The only key `set_res_users_settings` accepts.
const SETTING_KEYS: &[&str] = &["homemenu_config"];

/// Model for durable per-user UI preferences and settings.
#[model(
    name = "res.users.settings",
    description = "User Settings",
    company_access = unrestricted(
        reason = "user settings belong to a user across all allowed companies"
    ),
    constraints = [("_user_uniq", "UNIQUE (user_id)", "A user has one settings record.")]
)]
pub struct ResUsersSettings {
    /// The user who owns these settings. Only administrators may read or set it through the
    /// generic API: the record rule filters the rows, not the values written, so without this a
    /// user could hand a row to someone else. The methods below set it in superuser mode.
    #[field(
        many2one,
        groups = "base.group_system",
        comodel = "res.users",
        required,
        ondelete = "cascade",
        index = true
    )]
    pub user_id: Option<RecId>,
    /// Home menu application ordering preference, stored as a JSON string of ordered root menu IDs.
    #[field(char, size = 4096)]
    pub homemenu_config: Option<Arc<str>>,
}

#[methods(ResUsersSettings, addon = "base")]
impl ResUsersSettings {
    /// The stored ordering must be valid whichever path wrote it (the methods below or generic
    /// `create`/`write`).
    #[constrains("homemenu_config")]
    async fn _check_homemenu_config(
        this: &RecordSet<ResUsersSettings>,
        tx: &mut Transaction,
    ) -> Result<()> {
        for raw in this
            .mapped(tx, ResUsersSettings::HOMEMENU_CONFIG)
            .await?
            .into_iter()
            .flatten()
        {
            validate_homemenu_config(&raw)?;
        }
        Ok(())
    }

    /// Read the caller's user settings.
    #[method]
    #[rpc(effect = read, model)]
    async fn get_res_users_settings(
        this: &RecordSet<ResUsersSettings>,
        tx: &mut Transaction,
    ) -> Result<ValueMap> {
        refuse_public(tx).await?;
        let own = own_row(this, tx).await?;
        let config = own.get(tx, ResUsersSettings::HOMEMENU_CONFIG).await?;
        Ok(config_map(config))
    }

    /// Update the caller's user settings.
    #[method]
    #[rpc(effect = write, model)]
    async fn set_res_users_settings(
        this: &RecordSet<ResUsersSettings>,
        tx: &mut Transaction,
        new_settings: ValueMap,
    ) -> Result<ValueMap> {
        refuse_public(tx).await?;
        if let Some(key) = new_settings
            .keys()
            .find(|key| !SETTING_KEYS.contains(&key.as_str()))
        {
            return Err(Error::validation(format!(
                "Unknown user setting '{key}'; the only setting is homemenu_config."
            )));
        }
        let own = own_row(this, tx).await?;
        let mut config = own.get(tx, ResUsersSettings::HOMEMENU_CONFIG).await?;
        if let Some(config_val) = new_settings.get("homemenu_config") {
            config = match config_val {
                Value::False => None,
                Value::Str(s) => {
                    validate_homemenu_config(s)?;
                    Some(Arc::from(&**s))
                }
                _ => {
                    return Err(Error::validation(
                        "homemenu_config must be a string or False",
                    ));
                }
            };
            let vals = this
                .vals()
                .set(ResUsersSettings::HOMEMENU_CONFIG, config.clone());
            if own.ids().is_empty() {
                let uid = RecId::from_real(tx.env().uid())
                    .ok_or_else(|| Error::programming("the caller has no real user id"))?;
                this.sudo(true)
                    .create(tx, vec![vals.set(ResUsersSettings::USER_ID, Some(uid))])
                    .await
                    .map_err(first_save_race)?;
            } else {
                own.write(tx, vals).await?;
            }
        }
        Ok(config_map(config))
    }
}

/// Two first saves of one user race on `_user_uniq`: the loser cannot see the winner's row in its
/// snapshot, so it fails as a serialization failure (`40001`), which `Orm::run` retries as a new
/// attempt that finds the row and writes it (the convention of `sequence.rs`).
fn first_save_race(error: Error) -> Error {
    if error.sqlstate() == Some("23505")
        && error.constraint() == Some("res_users_settings_user_uniq")
    {
        return Error::new(
            ErrorKind::Concurrency,
            "Another save of this user's settings is in progress; retry.",
        )
        .with_sqlstate(Some("40001".to_owned()))
        .with_source(error);
    }
    error
}

/// Every anonymous visitor shares one public user: a row of it would be shared too.
async fn refuse_public(tx: &mut Transaction) -> Result<()> {
    let env = tx.env();
    if !env.su() && tx.has_group(&env, "base.group_public").await? {
        return Err(Error::access("Public users have no user settings."));
    }
    Ok(())
}

/// The caller's own settings row (empty when none yet). The user comes from the environment,
/// never from the arguments; `sudo` only keeps users outside `base.group_user` working, as the
/// row is selected by that user id alone. Generic `read`/`write`/`create` stay under the
/// `ir.access` rows.
async fn own_row(
    this: &RecordSet<ResUsersSettings>,
    tx: &mut Transaction,
) -> Result<RecordSet<ResUsersSettings>> {
    let uid = tx.env().uid();
    this.sudo(true)
        .search(tx, &Domain::condition("user_id", Operator::Eq, uid)?)
        .await
}

fn config_map(config: Option<Arc<str>>) -> ValueMap {
    let mut out = ValueMap::new();
    out.insert(
        "homemenu_config".to_string(),
        config.map_or(Value::False, |c| Value::from(&*c)),
    );
    out
}

/// Validates that a raw `homemenu_config` string is a valid, bounded JSON array of IDs.
pub fn validate_homemenu_config(raw: &str) -> Result<()> {
    if raw.len() > MAX_HOMEMENU_CONFIG_BYTES {
        return Err(Error::validation(format!(
            "homemenu_config exceeds the {MAX_HOMEMENU_CONFIG_BYTES}-byte limit"
        )));
    }
    let parsed: serde_json::Value = serde_json::from_str(raw)
        .map_err(|e| Error::validation(format!("Invalid JSON in homemenu_config: {e}")))?;
    let arr = parsed
        .as_array()
        .ok_or_else(|| Error::validation("homemenu_config must decode to a JSON array"))?;
    if arr.len() > MAX_HOMEMENU_APPS {
        return Err(Error::validation(format!(
            "homemenu_config exceeds the {MAX_HOMEMENU_APPS}-app limit"
        )));
    }
    for item in arr {
        match item {
            serde_json::Value::Number(n) if n.as_i64().is_some_and(|id| id > 0) => {}
            serde_json::Value::String(s) if !s.is_empty() && s.len() <= 128 => {}
            _ => {
                return Err(Error::validation(
                    "homemenu_config elements must be positive integer menu IDs or valid string \
                     identifiers",
                ));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_homemenu_config_valid_numbers() {
        assert!(validate_homemenu_config("[10, 20, 30]").is_ok());
    }

    #[test]
    fn test_validate_homemenu_config_valid_strings() {
        assert!(validate_homemenu_config("[\"base.menu_root\", \"mail.menu_root\"]").is_ok());
    }

    #[test]
    fn test_validate_homemenu_config_rejects_invalid_json() {
        assert!(validate_homemenu_config("[10, 20,").is_err());
    }

    #[test]
    fn test_validate_homemenu_config_rejects_non_array() {
        assert!(validate_homemenu_config("{\"app\": 10}").is_err());
        assert!(validate_homemenu_config("123").is_err());
        assert!(validate_homemenu_config("\"string\"").is_err());
    }

    #[test]
    fn test_validate_homemenu_config_rejects_negative_or_zero_ids() {
        assert!(validate_homemenu_config("[0]").is_err());
        assert!(validate_homemenu_config("[-5]").is_err());
    }

    #[test]
    fn test_validate_homemenu_config_rejects_oversized_payload() {
        let large_str = "x".repeat(MAX_HOMEMENU_CONFIG_BYTES + 1);
        assert!(validate_homemenu_config(&large_str).is_err());
    }

    #[test]
    fn test_validate_homemenu_config_rejects_too_many_apps() {
        let apps: Vec<usize> = (1..=257).collect();
        let serialized = serde_json::to_string(&apps).unwrap();
        assert!(validate_homemenu_config(&serialized).is_err());
    }
}
