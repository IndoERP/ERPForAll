// SPDX-License-Identifier: MIT OR Apache-2.0
//! Addon data attached atomically to a newly authenticated session.
use std::time::Duration;

use erp_fields::{Error, Result, Value};
use erp_orm::{Context, Orm, Transaction, UserId};
use serde_json::{Map, Value as Json};

use super::{AuthConfig, AuthInfo, AuthMethod, SessionId, UserContext};

const METHOD: &str = "_after_session_login";
const MAX_KEYS: usize = 64;

/// Information exposed to the private session-login hook. Pass this value through
/// the method chain so all installed addons contribute to the same session.
pub struct SessionLogin {
    uid: UserId,
    login: String,
    method: AuthMethod,
    trusted: bool,
    data: Map<String, Json>,
    limit: usize,
}

impl SessionLogin {
    /// The authenticated user.
    pub fn uid(&self) -> UserId {
        self.uid
    }

    /// Canonical stored login, including when the submitted identifier was an alias.
    pub fn login(&self) -> &str {
        &self.login
    }

    /// How the user authenticated.
    pub fn method(&self) -> &AuthMethod {
        &self.method
    }

    /// Whether this session is marked as a trusted device.
    pub fn device_trusted(&self) -> bool {
        self.trusted
    }

    /// Read an addon value from the current hook chain.
    pub fn get(&self, key: &str) -> Option<&Json> {
        self.data.get(key)
    }

    /// Remove an addon value from this new session.
    pub fn remove(&mut self, key: &str) -> Option<Json> {
        self.data.remove(key)
    }

    /// Set an addon value.
    ///
    /// # Errors
    /// Refuses an invalid key or a change exceeding the key or serialized size limits.
    pub fn set(&mut self, key: &str, value: Json) -> Result<()> {
        let mut bytes = key.bytes();
        if matches!(key, "external_issuer" | "external_subject")
            || key.len() > 63
            || !bytes
                .next()
                .is_some_and(|b| b == b'_' || b.is_ascii_lowercase())
            || !bytes
                .all(|b| b == b'_' || b == b'.' || b.is_ascii_digit() || b.is_ascii_lowercase())
        {
            return Err(Error::validation("Invalid session data key"));
        }
        if self.data.len() >= MAX_KEYS && !self.data.contains_key(key) {
            return Err(Error::validation("Session data exceeds 64 keys"));
        }
        let old = self.data.insert(key.to_owned(), value);
        // A bounded writer prevents allocating an unbounded second copy of an addon value.
        let mut remaining = self.limit;
        struct Budget<'a>(&'a mut usize);
        impl std::io::Write for Budget<'_> {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                *self.0 = self
                    .0
                    .checked_sub(bytes.len())
                    .ok_or_else(|| std::io::Error::other("session data size limit"))?;
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        if serde_json::to_writer(Budget(&mut remaining), &self.data).is_err() {
            match old {
                Some(previous) => {
                    self.data.insert(key.to_owned(), previous);
                }
                None => {
                    self.data.remove(key);
                }
            }
            return Err(Error::validation(
                "Session data exceeds the configured byte limit",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Copy)]
pub(super) struct PostLoginPolicy {
    timeout: Duration,
    limit: usize,
}

impl PostLoginPolicy {
    pub(super) fn for_registry(orm: &Orm, config: &AuthConfig) -> Option<Self> {
        let id = orm.registry().model_id("res.users")?;
        (!orm.dispatch().is_base_method(id, METHOD)
            && !orm.dispatch().targets(id, METHOD).is_empty())
        .then_some(Self {
            timeout: config.session_login_deadline(),
            limit: config.session_data_bytes(),
        })
    }

    pub(super) async fn run(
        self,
        tx: &mut Transaction,
        id: &SessionId,
        context: &UserContext,
        login: &str,
        auth: &AuthInfo,
        token_plan: &super::token_plan::TokenPlan,
    ) -> Result<()> {
        let uid = context.uid();
        // Keep only a digest in memory while the hook runs. This covers revocation,
        // credential rehash, token-plan memberships and immutable session authority.
        let witness_sql = format!(
            "WITH protected AS (
            SELECT u.id, u.login, u.active, u.password, to_jsonb(s) - 'data' AS session,
                   s.data->'external_issuer' AS issuer, s.data->'external_subject' AS subject, {} AS token_fields
            FROM res_users u JOIN erp_session s ON s.user_id=u.id
            WHERE u.id=$1 AND u.active AND s.id=$2::text::uuid AND s.state='active'
        ) SELECT sha256(convert_to(to_jsonb(protected)::text,'UTF8')) FROM protected",
            token_plan.value_array("u")
        );
        let id = id.to_text();
        let witness = tx.query(&witness_sql, &[&uid.get(), &id]).await?;
        let witness = witness
            .first()
            .ok_or_else(|| Error::validation("New login session is unavailable"))?
            .try_get::<_, Vec<u8>>(0)
            .map_err(|error| Error::db(error.to_string()))?;
        let language = Value::str(context.lang());
        let timezone = context.tz().map_or(Value::False, Value::str);
        let env = Context::from_pairs([
            ("lang", language),
            ("tz", timezone),
            ("uid", Value::Int(uid.get())),
        ]);
        let input = SessionLogin {
            uid,
            login: login.to_owned(),
            method: auth.method().clone(),
            trusted: true,
            data: Map::new(),
            limit: self.limit,
        };
        let result: SessionLogin = tokio::time::timeout(
            self.timeout,
            tx.call_authenticated_hook(uid, env, METHOD, (input,)),
        )
        .await
        .map_err(|_| Error::validation("Session login hook exceeded its deadline"))??;
        if result.uid != uid
            || result.login != login
            || result.method != *auth.method()
            || result.limit != self.limit
            || !result.trusted
        {
            return Err(Error::validation(
                "Session login hook returned data for another login",
            ));
        }
        tx.flush().await?;
        let current = tx.query(&witness_sql, &[&uid.get(), &id]).await?;
        let current = current
            .first()
            .map(|row| row.try_get::<_, Vec<u8>>(0))
            .transpose()
            .map_err(|error| Error::db(error.to_string()))?;
        if current.as_ref() != Some(&witness) {
            return Err(Error::validation(
                "Session authority changed during login hook",
            ));
        }
        if !result.data.is_empty() {
            let encoded = serde_json::to_string(&result.data)
                .map_err(|e| Error::validation(e.to_string()))?;
            let updated = tx
                .execute(
                    "UPDATE erp_session SET data = $2::text::jsonb || data
                WHERE id = $1::text::uuid AND user_id = $3 AND state = 'active'",
                    &[&id, &encoded, &uid.get()],
                )
                .await?;
            if updated != 1 {
                return Err(Error::validation("Session changed during login hook"));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_invalid_or_oversized_changes_without_losing_previous_data() -> Result<()> {
        let mut login = SessionLogin {
            uid: UserId::new(2)?,
            login: "admin".into(),
            method: AuthMethod::PASSWORD,
            trusted: true,
            data: Map::new(),
            limit: 20,
        };
        login.set("x", Json::Bool(true))?;
        assert!(login.set("x", Json::String("X".repeat(21))).is_err());
        assert_eq!(login.get("x"), Some(&Json::Bool(true)));
        assert!(login.set("Invalid", Json::Null).is_err());
        assert_eq!(login.remove("x"), Some(Json::Bool(true)));
        Ok(())
    }
}
