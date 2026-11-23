//! User settings and durable preferences (`res.users.settings`).

use std::sync::Arc;

use erp_sdk::prelude::*;
use erp_sdk::serde_json;
use erp_sdk::{ErrorKind, ValueMap};

pub const MAX_HOMEMENU_CONFIG_BYTES: usize = 4096;
pub const MAX_HOMEMENU_APPS: usize = 256;
/// The most active companies a user may store: the bound of `cids` in the web client's address.
pub const MAX_ACTIVE_COMPANIES: usize = 64;
/// The keys `set_res_users_settings` accepts.
const SETTING_KEYS: &[&str] = &["homemenu_config", "active_company_ids"];

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
    /// The active companies the user last chose in the company switcher, current company first,
    /// stored as a JSON array of company ids. The web shell opened without `cids` in its address
    /// starts with them. Every write is checked against the owner's allowed companies, and every
    /// read keeps only the companies the reader is still allowed, in the stored order.
    #[field(char, size = 2048)]
    pub active_company_config: Option<Arc<str>>,
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

    /// The stored active companies must be a valid list of the owner's allowed companies, whichever
    /// path wrote them (the methods below or generic `create`/`write`, also by an administrator
    /// for another user).
    #[constrains("active_company_config", "user_id")]
    async fn _check_active_company_config(
        this: &RecordSet<ResUsersSettings>,
        tx: &mut Transaction,
    ) -> Result<()> {
        let rows = this.sudo(true);
        for row in rows.iter() {
            let Some(raw) = row.get(tx, ResUsersSettings::ACTIVE_COMPANY_CONFIG).await? else {
                continue;
            };
            let ids = parse_active_company_config(&raw)?;
            let owner = row
                .get(tx, ResUsersSettings::USER_ID)
                .await?
                .and_then(crate::group_graph::row_id)
                .ok_or_else(|| {
                    Error::validation("Active companies need the user they belong to.")
                })?;
            let allowed = owner_companies(tx, owner).await?;
            if ids.iter().any(|id| !allowed.contains(id)) {
                return Err(Error::validation(
                    "The active companies must be among the user's allowed companies.",
                ));
            }
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
        let stored = own.get(tx, ResUsersSettings::ACTIVE_COMPANY_CONFIG).await?;
        let companies = match stored {
            Some(raw) => still_allowed(tx, &parse_active_company_config(&raw)?),
            None => Vec::new(),
        };
        Ok(settings_map(config, &companies))
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
        let mut vals = this.vals();
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
            vals = vals.set(ResUsersSettings::HOMEMENU_CONFIG, config.clone());
        }
        let stored = own.get(tx, ResUsersSettings::ACTIVE_COMPANY_CONFIG).await?;
        let companies = match new_settings.get("active_company_ids") {
            // JSON `false` over RPC arrives as `Bool(false)`, `null` and in-process callers as `False`.
            Some(Value::False | Value::Bool(false)) => {
                vals = vals.set(ResUsersSettings::ACTIVE_COMPANY_CONFIG, None::<Arc<str>>);
                Vec::new()
            }
            Some(chosen) => {
                let ids = active_company_list(chosen)?;
                if still_allowed(tx, &ids) != ids {
                    return Err(Error::access(
                        "One of the selected companies does not exist or is not one of yours.",
                    ));
                }
                let raw = serde_json::to_string(&ids)
                    .map_err(|e| Error::programming(format!("active companies: {e}")))?;
                vals = vals.set(
                    ResUsersSettings::ACTIVE_COMPANY_CONFIG,
                    Some(Arc::from(raw.as_str())),
                );
                ids
            }
            None => match stored {
                Some(raw) => still_allowed(tx, &parse_active_company_config(&raw)?),
                None => Vec::new(),
            },
        };
        if !new_settings.is_empty() {
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
        Ok(settings_map(config, &companies))
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

fn settings_map(config: Option<Arc<str>>, companies: &[i64]) -> ValueMap {
    let mut out = ValueMap::new();
    out.insert(
        "homemenu_config".to_string(),
        config.map_or(Value::False, |c| Value::from(&*c)),
    );
    out.insert(
        "active_company_ids".to_string(),
        if companies.is_empty() {
            Value::False
        } else {
            Value::list(companies.iter().copied().map(Value::Int))
        },
    );
    out
}

/// `ids` without the companies the caller is no longer allowed (removed from the user, archived),
/// in the given order. The caller's companies come from the principal this transaction resolved.
fn still_allowed(tx: &Transaction, ids: &[i64]) -> Vec<i64> {
    let env = tx.env();
    let allowed = env.principal().resolution().all_companies();
    ids.iter()
        .copied()
        .filter(|id| allowed.iter().any(|company| company.get() == *id))
        .collect()
}

/// The allowed companies of `user` (`res_company_users_rel`), for the write check.
async fn owner_companies(tx: &mut Transaction, user: i64) -> Result<Vec<i64>> {
    let rows = tx
        .query(
            "SELECT cid::int8 FROM res_company_users_rel WHERE user_id = $1::int8",
            &[&user],
        )
        .await?;
    rows.iter()
        .map(|row| {
            row.try_get::<_, i64>(0)
                .map_err(|e| Error::db(format!("res.users.settings company check: {e}")))
        })
        .collect()
}

/// The `active_company_ids` argument of `set_res_users_settings`: a list of company ids.
fn active_company_list(value: &Value) -> Result<Vec<i64>> {
    let Value::List(items) = value else {
        return Err(Error::validation(
            "active_company_ids must be a list of company ids or False",
        ));
    };
    let ids = items
        .iter()
        .map(|item| match item {
            Value::Int(id) => Ok(*id),
            _ => Err(Error::validation(
                "active_company_ids must be a list of company ids or False",
            )),
        })
        .collect::<Result<Vec<_>>>()?;
    validate_active_companies(&ids)?;
    Ok(ids)
}

/// Parses a stored `active_company_config`: a JSON array of company ids, current first.
pub fn parse_active_company_config(raw: &str) -> Result<Vec<i64>> {
    let parsed: serde_json::Value = serde_json::from_str(raw)
        .map_err(|e| Error::validation(format!("Invalid JSON in active companies: {e}")))?;
    let arr = parsed
        .as_array()
        .ok_or_else(|| Error::validation("The active companies must be a JSON array"))?;
    let ids = arr
        .iter()
        .map(|item| {
            item.as_i64()
                .ok_or_else(|| Error::validation("The active companies must be company ids"))
        })
        .collect::<Result<Vec<_>>>()?;
    validate_active_companies(&ids)?;
    Ok(ids)
}

/// A non-empty list of at most [`MAX_ACTIVE_COMPANIES`] distinct positive ids.
fn validate_active_companies(ids: &[i64]) -> Result<()> {
    if ids.is_empty() {
        return Err(Error::validation(
            "Choose at least one active company (False clears the choice).",
        ));
    }
    if ids.len() > MAX_ACTIVE_COMPANIES {
        return Err(Error::validation(format!(
            "At most {MAX_ACTIVE_COMPANIES} active companies can be stored."
        )));
    }
    if ids.iter().any(|id| *id <= 0) {
        return Err(Error::validation(
            "The active companies must be positive company ids.",
        ));
    }
    let mut seen = std::collections::BTreeSet::new();
    if !ids.iter().all(|id| seen.insert(*id)) {
        return Err(Error::validation("A company is listed twice."));
    }
    Ok(())
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
    fn active_company_config_keeps_the_stored_order() {
        assert_eq!(parse_active_company_config("[3, 1, 2]").unwrap(), [3, 1, 2]);
    }

    #[test]
    fn active_company_config_refuses_malformed_lists() {
        let too_many = serde_json::to_string(&(1..=65).collect::<Vec<i64>>()).unwrap();
        for bad in [
            "",
            "{}",
            "[]",
            "[0]",
            "[-1]",
            "[1, 1]",
            "[\"1\"]",
            "[1.5]",
            too_many.as_str(),
        ] {
            let error = parse_active_company_config(bad).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::Validation, "{bad}");
        }
    }

    #[test]
    fn active_company_argument_must_be_a_list_of_ids() {
        assert_eq!(
            active_company_list(&Value::list([Value::Int(2), Value::Int(1)])).unwrap(),
            [2, 1]
        );
        for bad in [
            Value::Int(1),
            Value::from("[1]"),
            Value::list([Value::from("1")]),
            Value::list(std::iter::empty::<Value>()),
        ] {
            assert_eq!(
                active_company_list(&bad).unwrap_err().kind(),
                ErrorKind::Validation
            );
        }
    }

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
