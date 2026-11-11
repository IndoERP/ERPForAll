//! Authentication, base part: password hashes and
//! the hashing pool, `database.secret` and its purpose keys, the settings, the login throttle,
//! the `_check_credentials` types, sessions (check in the transaction's opening flight, rotation,
//! touch, end) with the session-end hook, and the [`Auth`] service that ties them together.
//!
//! Secrets never reach a log, an error message or `Debug` output: passwords are wiped when
//! dropped, `database.secret` is read only to derive keys, cookie secrets are stored as digests.
//!
//! Open for later credential types (TOTP, passkeys, LDAP, OIDC and SAML single sign-on, SCIM
//! provisioning): [`credential::Credential`] is `#[non_exhaustive]`,
//! `_check_credentials` is a dispatched method chain, sessions carry a `pending` state for a
//! second factor and a kid for future keys.

//! Selection execution is internal to the authentication service; it is unavailable
//! through the public login module too.
//!
//! ```compile_fail
//! use erp_security::auth::login::select_candidate;
//! ```
//!
//! ```compile_fail
//! use erp_security::auth::{LoginDomain, LoginOrder, LoginSelectionPlan};
//! let _ = LoginSelectionPlan::new(LoginDomain::exact_login("fixture").unwrap(), LoginOrder::base());
//! ```

pub mod apikey;
pub mod apikey_manage;
pub mod apikey_store;
mod apikey_verify;
pub mod browser;
pub mod context;
pub mod cooldown_policy;
pub mod credential;
mod dummy;
pub mod gc;
pub mod gc_schedule;
pub mod hooks;
pub mod login;
pub mod password;
pub mod pool;
pub mod retry;
pub mod schema;
pub mod secret;
pub mod session;
pub mod settings;
pub mod store;
pub mod throttle;
pub mod token_plan;
mod warn;

/// Integration regression seams; absent from default production builds.
#[cfg(feature = "test-support")]
#[doc(hidden)]
pub mod test_support;

pub use apikey_verify::{ApiKeyBegin, ApiKeyCooldown, ApiKeyRefusal, ApiKeyRequest};
pub use context::{UserContext, user_context};
pub use cooldown_policy::{LoginCooldownContext, LoginCooldownPolicy};
pub use credential::{
    ACCESS_DENIED, AuthInfo, AuthMethod, CheckEnv, Credential, Mfa, StoredPassword, access_denied,
    check_password,
};
/// How a session check opens the request's transaction (owned by the ORM, type design T4).
pub use erp_orm::CredentialOptions;
pub use gc::GcReport;
pub use hooks::{EndReason, EndedSession, SessionEnd, SessionEndFn, SessionEndHook};
pub use login::{
    Auth, AuthStats, COOLDOWN_MESSAGE, ClientAddr, IdentityCheck, Login, LoginDomain, LoginError,
    LoginLookup, LoginOrder, LoginOrderTerm, LoginRequest, LoginSelectionContext,
    LoginSelectionLimits, NewSession, OrderDirection, PasswordAuthorization, PasswordReset,
    SESSIONS_PER_USER, SelectionWitness, SessionBegin, SettingsView,
};
pub use password::{CostComparison, HashParams, PasswordError, Scheme, StoredHash};
pub use pool::{HashPool, HashPoolConfig, HashPoolError, HashPoolStats};
pub use retry::DeadlockRetries;
pub use schema::{check_orm, core_tables};
pub use session::{CheckedSession, Refusal, SessionId, SessionSecret, Touch};
pub use settings::{
    AuthConfig, AuthSettings, ConfigError, check_reservation, check_stored, load_hash_params,
    validate_setting, validate_setting_in,
};
pub use store::{end_sessions, end_user_sessions};
pub use throttle::{ThrottleHasher, ThrottleKey};
pub use token_plan::SessionTokenFields;
/// Wipes a secret when dropped (passwords read from the database, keys).
pub use zeroize::Zeroizing;

/// Hash `password` with `params` on the hashing pool of `env`'s database ([`pool::for_env`]: the
/// pool handed to that database's [`Auth`], else the process pool), so every hash of a database
/// shares one bound.
///
/// # Errors
///
/// The pool's errors (`Overloaded`, parameters needing more memory than the pool reserves); the
/// hasher's.
pub async fn hash_with_params(
    env: &erp_orm::Env,
    params: &HashParams,
    password: &str,
) -> erp_fields::Result<String> {
    let params = *params;
    let secret = zeroize::Zeroizing::new(password.as_bytes().to_vec());
    let hashed = pool::for_env(env)
        .run(params.memory_kib(), move || {
            password::hash(&secret, &params)
        })
        .await??;
    Ok(hashed)
}

/// Hash every one of `passwords` with `params` on the hashing pool of `env`'s database, as many
/// at once as the pool has permits but one (rust review round 2 S-1, database review round 2 S4;
/// the one left is for logins, security review round 2 delta N-e): a batch of N users takes about
/// N ÷ (permits − 1) hash times, not N, in the caller's transaction. The
/// hashes come in the order of `passwords`. The first error ends the batch; the hashes still in
/// flight are abandoned (their tasks aborted; a job already on a blocking thread finishes there).
///
/// # Errors
///
/// As [`hash_with_params`]; `Programming` when a hashing task panicked or was cancelled.
pub async fn hash_all_with_params<P: AsRef<str>>(
    env: &erp_orm::Env,
    params: &HashParams,
    passwords: &[P],
) -> erp_fields::Result<Vec<String>> {
    let pool = pool::for_env(env);
    let width = batch_width(pool.config().permits());
    let params = *params;
    let mut hashes: Vec<Option<String>> = vec![None; passwords.len()];
    let mut queued = passwords.iter().enumerate();
    let mut jobs = tokio::task::JoinSet::new();
    loop {
        while jobs.len() < width {
            let Some((index, plain)) = queued.next() else {
                break;
            };
            let secret = zeroize::Zeroizing::new(plain.as_ref().as_bytes().to_vec());
            let pool = pool.clone();
            jobs.spawn(async move {
                let made = pool
                    .run(params.memory_kib(), move || {
                        password::hash(&secret, &params)
                    })
                    .await;
                (index, made)
            });
        }
        let Some(joined) = jobs.join_next().await else {
            break;
        };
        let (index, made) = joined.map_err(|e| {
            erp_fields::Error::programming(format!("a password hashing task ended early: {e}"))
        })?;
        hashes[index] = Some(made??);
    }
    hashes
        .into_iter()
        .map(|hash| {
            hash.ok_or_else(|| {
                erp_fields::Error::programming("a password of the batch was not hashed")
            })
        })
        .collect()
}

/// How many hashes of a batch run at once on a pool of `permits` permits: all but one, so logins
/// keep a permit while an administrator's batch or `init` runs (security review round 2 delta
/// N-e), and at least one.
fn batch_width(permits: u32) -> usize {
    usize::try_from(permits.saturating_sub(1)).map_or(1, |w| w.max(1))
}

/// Hash `password` with the current settings of `tx`'s database on its hashing pool (setting a
/// password: one statement for the settings, one hash).
///
/// # Errors
///
/// `Validation` for a malformed hashing setting; the pool's errors; the statement's.
pub async fn hash_password(
    tx: &mut erp_orm::Transaction,
    password: &str,
) -> erp_fields::Result<String> {
    let params = settings::load_hash_params(tx).await?;
    hash_with_params(&tx.env(), &params, password).await
}

/// Log that `res.users.init` replaced the plaintext password SQL stored for `user` by its hash
/// (at `warn`, with the user id only, never the value; silent-failure review M3).
pub fn log_plaintext_replaced(user: i64) {
    tracing::warn!(
        target: "erp::auth",
        user,
        "res.users {user}: a plaintext password stored by SQL is replaced by its hash"
    );
}

#[cfg(test)]
mod tests {
    use super::batch_width;

    /// Security review round 2 delta N-e: a batch leaves one permit of the pool to logins, and
    /// still hashes on a pool of one permit.
    #[test]
    fn a_batch_leaves_a_permit_for_logins() {
        assert_eq!(
            [1, 2, 3, 8].map(batch_width),
            [1, 1, 2, 7],
            "permits - 1, at least 1"
        );
    }
}
