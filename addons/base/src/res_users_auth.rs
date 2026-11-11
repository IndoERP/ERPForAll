//! `res.users` authentication.
//!
//! - `password`: a non-stored field, always empty when read, whose inverse hashes the written
//!   value on the database's hashing pool (the one handed to its `Auth`) with the database's
//!   current parameters, stores the hash in the `password` column (one `UPDATE` per batch) and
//!   ends every session of the users whose password was set, with the session-end hooks (lead
//!   decision 6). An empty value, and one longer than [`PASSWORD_MAX_BYTES`], is refused. `init()`
//!   adds the column and replaces plaintext written by SQL with its hash; a value
//!   shaped like a hash that does not parse is refused, never hashed over.
//! - `login` is unique; the superuser cannot be activated and nobody can
//!   deactivate themselves ([`check_write`], called by the `write` override in
//!   [`crate::res_users`]).
//! - Archiving or deleting users ends their sessions in the same transaction, with the
//!   session-end hooks ([`end_sessions_of`]); so does renaming a user's `login` ([`renamed`],
//!   security review round 2 N3).
//! - `_check_credentials(credential, env)`: the base implementation verifies a password against
//!   the stored hash ([`erp_sdk::auth::check_password`]); addons that add a credential type
//!   override it and call `super` for the others.
//! - `context_get()`: the user's `lang`, `tz` and `uid`.
//! - `_get_session_token_fields()`: the user fields a session token covers (`id`, `login`,
//!   `password`, `active`; addons add scalar fields, H58 TOKEN).

use erp_sdk::auth::{
    AuthInfo, CheckEnv, Credential, EndReason, LoginCooldownContext, LoginCooldownPolicy,
    LoginDomain, LoginLookup, LoginOrder, SessionTokenFields, StoredHash, end_user_sessions,
    hash_all_with_params, load_hash_params, log_plaintext_replaced, user_context,
};
use erp_sdk::prelude::*;
use erp_sdk::{Cell, UserId, ValueMap};

pub const EMPTY_PASSWORD: &str = "Setting empty passwords is not allowed for security reasons!";

/// The longest password that can be set, bytes: the login's default `limits.credential_bytes`,
/// so a password that is set can be presented (a server configured with a lower
/// `limits.credential_bytes` refuses longer ones at login).
pub const PASSWORD_MAX_BYTES: usize = 1_024;

/// The text of a password over [`PASSWORD_MAX_BYTES`].
pub const LONG_PASSWORD: &str = "A password may have at most 1024 bytes.";

/// Adds the `password` column of `res_users` when it is missing. The catalog is read first:
/// `ADD COLUMN IF NOT EXISTS` takes ACCESS EXCLUSIVE on the table even when the column exists,
/// and would wait behind every open reader at each sync (database review round 2, N10).
pub const PASSWORD_COLUMN_DDL: &str = "\
DO $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_attribute
                    WHERE attrelid = 'res_users'::regclass AND attname = 'password'
                      AND NOT attisdropped) THEN
        ALTER TABLE res_users ADD COLUMN password varchar;
    END IF;
END
$$";

/// The superuser's id.
const SUPERUSER: i64 = 1;

/// The authentication columns and methods of `res.users`.
#[extend(
    ResUsers,
    constraints = [(
        "_login_key",
        "UNIQUE (login)",
        "You can not have two users with the same login!"
    )]
)]
pub struct ResUsersAuth {
    /// The password: written in clear, stored hashed, read empty.
    #[field(
        char,
        compute = "_compute_password",
        inverse = "_set_password",
        copy = false,
        secret = true,
        string = "Password"
    )]
    pub password: Option<Arc<str>>,
}

fn truthy(cell: &Cell) -> bool {
    match cell {
        Cell::Null => false,
        Cell::Bool(b) => *b,
        Cell::Int(n) => *n != 0,
        Cell::Id(n) => *n != 0,
        Cell::Str(s) => !s.is_empty(),
        _ => true,
    }
}

/// The rules of `write` on users before the write.
///
/// # Errors
///
/// `UserError` "You cannot activate the superuser." and "You cannot deactivate the user you're
/// currently logged in as.".
pub fn check_write(this: &Records, vals: &RawVals) -> Result<()> {
    let Some(active) = vals.get("active") else {
        return Ok(());
    };
    let ids = this.ids();
    if truthy(active) && ids.contains(&SUPERUSER) {
        return Err(Error::user("You cannot activate the superuser."));
    }
    if !truthy(active) && ids.contains(&this.env().uid()) {
        return Err(Error::user(
            "You cannot deactivate the user you're currently logged in as.",
        ));
    }
    Ok(())
}

/// Whether `vals` archives the records.
pub fn archives(vals: &RawVals) -> bool {
    vals.get("active").is_some_and(|cell| !truthy(cell))
}

/// End every session of `this` (archive or delete), with the session-end hooks, in `tx`.
///
/// # Errors
///
/// The deletion's and the hooks' errors.
pub async fn end_sessions_of(this: &Records, tx: &mut Transaction) -> Result<()> {
    let ids = this.ids();
    if ids.is_empty() {
        return Ok(());
    }
    end_user_sessions(tx, &ids, EndReason::Revoked).await?;
    Ok(())
}

/// The users of `this` whose `login` `vals` changes, read before the write (security review
/// round 2 N3): a session's token covers the login, so a rename ends the user's sessions now,
/// with the session-end hooks ([`end_sessions_of_renamed`]), as a password set does, not at their
/// next check. A write that keeps the login names nobody.
///
/// # Errors
///
/// The read's errors.
pub async fn renamed(this: &Records, tx: &mut Transaction, vals: &RawVals) -> Result<Vec<i64>> {
    let Some(new) = vals.get("login") else {
        return Ok(Vec::new());
    };
    let ids = this.ids();
    let old = this.sudo(true).cells(tx, "login").await?;
    Ok(ids
        .iter()
        .zip(&old)
        .filter(|(_, old)| *old != new)
        .map(|(id, _)| *id)
        .collect())
}

/// End every session of the users `renamed` named, with the session-end hooks, in `tx` (after the
/// write).
///
/// # Errors
///
/// The deletion's and the hooks' errors.
pub async fn end_sessions_of_renamed(tx: &mut Transaction, ids: &[i64]) -> Result<()> {
    if ids.is_empty() {
        return Ok(());
    }
    end_user_sessions(tx, ids, EndReason::CredentialChanged).await?;
    Ok(())
}

/// Every stored password value (`init` hashes those that are not a hash we parse).
const STORED_SQL: &str = "SELECT id::int8, password FROM res_users WHERE password IS NOT NULL AND password <> '' ORDER BY id";

const STORE_SQL: &str = "UPDATE res_users u SET password = v.p \
     FROM unnest($1::int8[], $2::text[]) AS v (id, p) WHERE u.id = v.id";

#[methods(ResUsers)]
impl ResUsersAuth {
    #[compute(depends = [])]
    fn _compute_password(batch: &mut Batch<'_, ResUsers>) -> Result<()> {
        for i in batch.indices() {
            batch.set(i, ResUsersAuth::PASSWORD, None::<Arc<str>>)?;
        }
        Ok(())
    }

    /// Hash the written passwords and store them (one statement for the settings, the hashes, one
    /// `UPDATE`), then end every session of those users with the session-end hooks (lead decision
    /// 6: eagerly, so their subscribers' rows go now, not at the sessions' next check). The inverse
    /// runs in the caller's transaction (its contract): the hashes run before any row of
    /// `res_users` is written here, on the database's hashing pool, as many at once as it has
    /// permits but one, at least one (`batch_width`: one permit stays free for logins; rust
    /// review round 2 S-1, database review round 2 S4, security delta N-e; claude2 ruling, lead to
    /// confirm), so a batch holds the transaction for about N ÷ (permits − 1) hash times (N on a
    /// pool of one or two permits; rust review round 3 RD-4).
    #[method]
    async fn _set_password(this: &RecordSet<ResUsers>, tx: &mut Transaction) -> Result<()> {
        if this.as_records().is_empty() {
            return Ok(());
        }
        let mut ids = Vec::with_capacity(this.as_records().len());
        let mut plains = Vec::with_capacity(ids.capacity());
        for rec in this.iter() {
            let plain = rec.get(tx, ResUsersAuth::PASSWORD).await?;
            let Some(plain) = plain.filter(|p| !p.is_empty()) else {
                return Err(Error::user(EMPTY_PASSWORD));
            };
            if plain.len() > PASSWORD_MAX_BYTES {
                return Err(Error::user(LONG_PASSWORD));
            }
            plains.push(plain);
            ids.push(rec.as_records().int()?);
        }
        let params = load_hash_params(tx).await?;
        let hashes = hash_all_with_params(&tx.env(), &params, &plains).await?;
        tx.execute(STORE_SQL, &[&ids, &hashes]).await?;
        end_user_sessions(tx, &ids, EndReason::CredentialChanged).await?;
        // the clear value written into the cache goes
        tx.invalidate_records(this.as_records(), Some(&["password"]))
            .await
    }

    /// Add the `password` column and hash the plaintext values written by SQL.
    /// A value that starts with `$` and does not parse is shaped like a hash (another scheme, or
    /// parameters outside our bounds): it is refused, never hashed over, since that would destroy
    /// a real credential for good (silent-failure review M3). Every replaced value is logged with
    /// the user id only.
    ///
    /// # Errors
    ///
    /// `Validation` naming the users whose stored value is hash-shaped but not parsed (all of
    /// them in one error, the first [`HASH_SHAPED_LISTED`] by id; silent-delta N-5).
    #[method(override)]
    async fn init(this: &RecordSet<ResUsers>, tx: &mut Transaction, sup: Super) -> Result<()> {
        sup.call(tx, this.as_records(), ()).await?;
        tx.batch(PASSWORD_COLUMN_DDL).await?;
        let rows = tx.query(STORED_SQL, &[]).await?;
        let mut plain_rows = Vec::new();
        let mut hash_shaped = Vec::new();
        for row in &rows {
            let id: i64 = row
                .try_get(0)
                .map_err(|e| Error::db(format!("res_users.id: {e}")))?;
            let stored: String = row
                .try_get(1)
                .map_err(|e| Error::db(format!("res_users.password of user {id}: {e}")))?;
            let stored = erp_sdk::auth::Zeroizing::new(stored);
            if StoredHash::parse(&stored).is_ok() {
                continue;
            }
            if stored.starts_with('$') {
                hash_shaped.push(id);
                continue;
            }
            plain_rows.push((id, stored));
        }
        if !hash_shaped.is_empty() {
            return Err(hash_shaped_error(&hash_shaped));
        }
        if plain_rows.is_empty() {
            return Ok(());
        }
        let params = load_hash_params(tx).await?;
        let (ids, plains): (Vec<i64>, Vec<_>) = plain_rows.into_iter().unzip();
        let hashes = hash_all_with_params(&tx.env(), &params, &plains).await?;
        tx.execute(STORE_SQL, &[&ids, &hashes]).await?;
        // logged once stored (silent-delta N-4); a later rollback of the caller's transaction
        // keeps the plaintext, and the next `init` logs it again
        for id in &ids {
            log_plaintext_replaced(*id);
        }
        Ok(())
    }

    #[method]
    async fn _check_credentials(
        this: &RecordSet<ResUsers>,
        tx: &mut Transaction,
        credential: Arc<Credential>,
        env: CheckEnv,
    ) -> Result<AuthInfo> {
        let uid = this.as_records().ensure_one()?.int()?;
        let uid = UserId::new(uid)?;
        erp_sdk::auth::check_password(tx, uid, &credential, &env).await
    }

    /// The extra login cooldown rules of an attempt: the base implementation asks for
    /// none, so only the core cooldown applies. An override returns a stricter policy (and calls
    /// `super`); it runs in a read-only transaction before any credential work, is told only the
    /// submitted login and client address, and can only tighten. Private: not an RPC method.
    #[method]
    async fn _on_login_cooldown(
        _this: &RecordSet<ResUsers>,
        _tx: &mut Transaction,
        _context: LoginCooldownContext,
    ) -> Result<LoginCooldownPolicy> {
        Ok(LoginCooldownPolicy::base())
    }

    /// The login selection domain (H58): the base implementation looks up by exact
    /// login match. An override returns a custom domain (and calls `super` or builds on typed
    /// scalar fields). Private: not an RPC method.
    #[method]
    async fn _get_login_domain(
        _this: &RecordSet<ResUsers>,
        _tx: &mut Transaction,
        lookup: LoginLookup,
    ) -> Result<LoginDomain> {
        LoginDomain::exact_login(lookup.as_str())
    }

    /// The login candidate selection ordering (H58): the base implementation orders
    /// by `id ASC`. An override returns an ordering and calls `super`. Private: not an RPC method.
    #[method]
    async fn _get_login_order(
        _this: &RecordSet<ResUsers>,
        _tx: &mut Transaction,
    ) -> Result<LoginOrder> {
        Ok(LoginOrder::base())
    }

    /// An override calls
    /// `super` and adds the names of further scalar user fields
    /// ([`SessionTokenFields::with_field`]); it cannot remove a base field. Resolved once when the
    /// authentication service of the registry is built, in a read-only transaction that is rolled
    /// back, where any query is refused. Private: not an RPC method.
    #[method]
    async fn _get_session_token_fields(
        _this: &RecordSet<ResUsers>,
        _tx: &mut Transaction,
    ) -> Result<SessionTokenFields> {
        Ok(SessionTokenFields::base())
    }

    #[method]
    #[rpc(effect = read, model)]
    async fn context_get(this: &RecordSet<ResUsers>, tx: &mut Transaction) -> Result<ValueMap> {
        let mut map = ValueMap::new();
        let Some(context) = user_context(tx, this.env().user_id()).await? else {
            return Ok(map);
        };
        map.insert("lang".into(), Value::str(context.lang()));
        map.insert("tz".into(), context.tz().map_or(Value::False, Value::str));
        map.insert("uid".into(), Value::Int(context.uid().get()));
        Ok(map)
    }
}

/// How many user ids [`hash_shaped_error`] lists.
const HASH_SHAPED_LISTED: usize = 20;

/// The refusal of `res.users.init` for stored values shaped like a hash it does not verify: the
/// first [`HASH_SHAPED_LISTED`] user ids (never the values) and how many more.
fn hash_shaped_error(ids: &[i64]) -> Error {
    let listed: Vec<String> = ids
        .iter()
        .take(HASH_SHAPED_LISTED)
        .map(i64::to_string)
        .collect();
    let more = ids.len().saturating_sub(HASH_SHAPED_LISTED);
    let more = if more == 0 {
        String::new()
    } else {
        format!(" and {more} more")
    };
    Error::validation(format!(
        "res.users {}{more}: {} stored password(s) shaped like a hash but not one this server \
         verifies (another scheme, or parameters outside the accepted bounds) are left in place: \
         set a new password for these users",
        listed.join(", "),
        ids.len()
    ))
}

#[cfg(test)]
mod tests {
    use super::hash_shaped_error;

    /// Silent-delta N-5: one error names every such user, bounded to the first 20.
    #[test]
    fn hash_shaped_users_are_listed_together() {
        let e = hash_shaped_error(&[3, 5]);
        assert_eq!(
            e.message(),
            "res.users 3, 5: 2 stored password(s) shaped like a hash but not one this server \
             verifies (another scheme, or parameters outside the accepted bounds) are left in \
             place: set a new password for these users"
        );
        let many: Vec<i64> = (1..=23).collect();
        let e = hash_shaped_error(&many);
        assert!(
            e.message().starts_with(
                "res.users 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20 \
                 and 3 more: 23 stored password(s)"
            ),
            "{e}"
        );
    }
}
