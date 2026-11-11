//! What `res.users._check_credentials(credential, env)` takes and returns (H58,
//! , §14.4), and the base implementation's password check.
//!
//! `_check_credentials` is a dispatched method chain on `res.users`: the `base` addon defines it
//! with [`check_password`], and addons that add a credential type (TOTP, passkeys, LDAP, single
//! sign-on; and later) override it with `super`, handle their own type, and return an
//! [`AuthInfo`]. A credential type nobody handles is `AccessDenied`.

use std::borrow::Cow;
use std::fmt;

use erp_fields::{Error, ErrorKind, Result};
use erp_orm::{Transaction, UserId};
use zeroize::Zeroizing;

use super::dummy;
use super::password::{CostComparison, HashParams, StoredHash};
use super::pool::{HashPool, HashPoolError, PoolBinding};

pub const ACCESS_DENIED: &str = "Access Denied";

/// The `AccessDenied` error with [`ACCESS_DENIED`].
pub fn access_denied() -> Error {
    Error::new(ErrorKind::AccessDenied, ACCESS_DENIED)
}

/// A credential presented for a login.
#[non_exhaustive]
pub enum Credential {
    Password {
        /// The login as presented.
        login: String,
        /// The password (wiped when dropped).
        password: Zeroizing<String>,
    },
}

impl Credential {
    /// A password credential.
    pub fn password(login: impl Into<String>, password: impl Into<String>) -> Self {
        Self::Password {
            login: login.into(),
            password: Zeroizing::new(password.into()),
        }
    }

    pub fn kind(&self) -> &'static str {
        match self {
            Self::Password { .. } => "password",
        }
    }
}

impl fmt::Debug for Credential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            // never the login text, nor its length: users type passwords into the login field
            // (security review round 2 N1 and delta N-c)
            Self::Password { .. } => f
                .debug_struct("Password")
                .field("login", &"<redacted>")
                .field("password", &"<redacted>")
                .finish(),
        }
    }
}

/// Where the base implementation finds the user's stored hash.
#[derive(Clone)]
#[non_exhaustive]
pub enum StoredPassword {
    /// Read by the login statement already (`None`: the column is empty).
    Preloaded(Option<String>),
    /// Not read yet: the base implementation reads it (one statement).
    Unread,
}

impl Drop for StoredPassword {
    /// The preloaded hash is a copy of a credential: wipe it when the check is done (LD3).
    fn drop(&mut self) {
        if let Self::Preloaded(Some(hash)) = self {
            #[cfg(test)]
            let wiped_len = hash.len();
            zeroize::Zeroize::zeroize(hash);
            #[cfg(test)]
            wipe_probe::record(wiped_len, hash.is_empty());
        }
    }
}

/// Test seam for [`StoredPassword`]'s `Drop`: the drop records how many bytes it wiped and whether
/// the string was empty right after the wipe, on the dropping thread. It lets a test see that the
/// real `Drop` ran the wipe without reading freed memory (which would need `unsafe`).
#[cfg(test)]
pub(crate) mod wipe_probe {
    use std::cell::RefCell;

    thread_local! {
        static WIPES: RefCell<Vec<(usize, bool)>> = const { RefCell::new(Vec::new()) };
    }

    pub(super) fn record(len: usize, emptied: bool) {
        WIPES.with(|w| w.borrow_mut().push((len, emptied)));
    }

    /// The `(bytes wiped, emptied)` records of this thread since the last call.
    pub(crate) fn take() -> Vec<(usize, bool)> {
        WIPES.with(|w| std::mem::take(&mut *w.borrow_mut()))
    }
}

impl fmt::Debug for StoredPassword {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Preloaded(Some(_)) => f.write_str("Preloaded(<hash>)"),
            Self::Preloaded(None) => f.write_str("Preloaded(None)"),
            Self::Unread => f.write_str("Unread"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct CheckEnv {
    interactive: bool,
    stored: StoredPassword,
    params: HashParams,
    pool: &'static HashPool,
}

impl CheckEnv {
    pub fn new(
        interactive: bool,
        stored: StoredPassword,
        params: HashParams,
        pool: &'static HashPool,
    ) -> Self {
        Self {
            interactive,
            stored,
            params,
            pool,
        }
    }

    /// Whether a person is logging in interactively.
    pub fn interactive(&self) -> bool {
        self.interactive
    }

    /// The current hashing parameters.
    pub fn params(&self) -> &HashParams {
        &self.params
    }

    /// The hashing pool.
    pub fn pool(&self) -> &'static HashPool {
        self.pool
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AuthMethod(Cow<'static, str>);

impl AuthMethod {
    /// `password`.
    pub const PASSWORD: Self = Self(Cow::Borrowed("password"));

    /// A method name: 1 to 32 lowercase ASCII letters, digits and `_`.
    ///
    /// # Errors
    ///
    /// `Programming` for any other text.
    pub fn new(name: &str) -> Result<Self> {
        let valid = !name.is_empty()
            && name.len() <= 32
            && name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
        if valid {
            Ok(Self(Cow::Owned(name.to_owned())))
        } else {
            Err(Error::programming(format!(
                "'{}' is not an authentication method name (1 to 32 of a-z, 0-9, _)",
                super::password::bounded(name)
            )))
        }
    }

    /// The name.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Mfa {
    /// Let the second-factor addon decide.
    Default,
    /// The credential is strong enough on its own (a passkey).
    Skip,
    /// Ask a second factor whatever the user's settings.
    Enforce,
}

impl Mfa {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Skip => "skip",
            Self::Enforce => "enforce",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthInfo {
    uid: UserId,
    method: AuthMethod,
    mfa: Mfa,
    rehash: bool,
}

impl AuthInfo {
    /// The verified user, the method and the second-factor decision.
    pub fn new(uid: UserId, method: AuthMethod, mfa: Mfa) -> Self {
        Self {
            uid,
            method,
            mfa,
            rehash: false,
        }
    }

    /// The same, with the stored password to be rehashed after the login succeeds.
    pub fn with_rehash(mut self, rehash: bool) -> Self {
        self.rehash = rehash;
        self
    }

    /// The user.
    pub fn uid(&self) -> UserId {
        self.uid
    }

    /// The method.
    pub fn method(&self) -> &AuthMethod {
        &self.method
    }

    /// The second-factor decision.
    pub fn mfa(&self) -> Mfa {
        self.mfa
    }

    /// Whether the stored password should be rehashed.
    pub fn rehash(&self) -> bool {
        self.rehash
    }
}

/// An empty password,
/// an empty or unrecognised stored value and a wrong password are `AccessDenied` after one
/// verify (a dummy one when there is nothing to verify, so every refusal costs a hash; and a
/// second, dummy one at the current parameters when the stored hash is cheaper or of another
/// scheme, so no refusal costs less than the current parameters' work, lead decision 4: a floor,
/// so such a refusal may take longer than an unknown login's); an unrecognised stored value is
/// logged at `error` with the user id only. Once the stored hash was verified and did not match,
/// the answer is `AccessDenied` whatever happens to the floor's dummy (its failure is logged and
/// counted, [`super::pool::HashPoolStats::floor_skipped`]). Every dummy runs at the current
/// parameters within the pool's reservation, so the pool never refuses one. A stored hash that
/// needs more memory than a permit of the pool reserves (another node with a larger
/// `admission.hash_memory_kib` stored it) is never verified: it is logged at `error`, padded with a
/// dummy and refused with `AccessDenied` like a wrong password, which the login counts (security
/// review round 2 delta SD-1).
///
/// # Errors
///
/// `AccessDenied` ([`ACCESS_DENIED`]) as above and for a credential type other than `password`;
/// the pool's `Overloaded` (its wait expired before the verify; the login answers it
/// `AccessDenied` without counting it) and other pool errors; the read of the stored hash when it
/// was not preloaded.
pub async fn check_password(
    tx: &mut Transaction,
    uid: UserId,
    credential: &Credential,
    env: &CheckEnv,
) -> Result<AuthInfo> {
    let pool = super::pool::bind_env(&tx.env(), env.pool)?;
    match &env.stored {
        StoredPassword::Preloaded(stored) => {
            verify_stored(uid, credential, stored.as_deref(), &env.params, &pool).await
        }
        StoredPassword::Unread => {
            let stored = read_stored(tx, uid).await?;
            verify_stored(uid, credential, stored.as_deref(), &env.params, &pool).await
        }
    }
}

/// [`check_password`] against a stored value already read: no statement, no transaction (the
/// login's path when no addon overrides `_check_credentials`, design note §14.4 phase B).
///
/// # Errors
///
/// As [`check_password`].
pub(crate) async fn verify_stored(
    uid: UserId,
    credential: &Credential,
    stored: Option<&str>,
    params: &HashParams,
    pool: &PoolBinding,
) -> Result<AuthInfo> {
    let Credential::Password { password, .. } = credential;
    let parsed = match stored {
        None | Some("") => None,
        Some(text) => match StoredHash::parse(text) {
            Ok(parsed) => Some(parsed),
            Err(e) => {
                tracing::error!(
                    target: "erp::auth",
                    user = uid.get(),
                    "unrecognised password hash for user {}: {e}",
                    uid.get()
                );
                None
            }
        },
    };
    let (Some(parsed), false) = (parsed, password.is_empty()) else {
        dummy::verify(pool, params).await?;
        return Err(access_denied());
    };
    let memory = parsed.memory_kib();
    let rehash = parsed.needs_rehash(params);
    // the floor compares with what this node's dummy costs: the current parameters within its
    // reservation (security review round 2 delta SD-1)
    let floor_params = dummy::runnable(pool, params);
    let floor = !matches!(
        parsed.compare_cost(&floor_params),
        CostComparison::NotCheaper
    );
    let secret = Zeroizing::new(password.as_bytes().to_vec());
    let ok = match pool.run(memory, move || parsed.verify(&secret)).await {
        Ok(verified) => verified?,
        Err(HashPoolError::MemoryOverReservation {
            needed_kib,
            reserved_kib,
        }) => {
            // a configuration fault, not load (security review round 2 delta SD-1): another node
            // with a larger reservation stored this hash. The answer is an unknown login's: a
            // dummy for the time, then `AccessDenied`, which the login counts; a dummy that cannot
            // run returns its error as an unknown login's does, so under load the two are
            // answered and counted alike (round 3 R3-a)
            tracing::error!(
                target: "erp::auth",
                user = uid.get(),
                "the stored password hash of user {} needs {needed_kib} KiB, more than the \
                 {reserved_kib} KiB a hashing permit of this node reserves \
                 (admission.hash_memory_kib): this node refuses every login of the user, each \
                 counted as a failure once its padding ran, until admission.hash_memory_kib is the same on every node \
                 or the password is reset",
                uid.get()
            );
            pad_over_reservation(pool, params, uid).await?;
            return Err(access_denied());
        }
        Err(e) => return Err(e.into()),
    };
    if ok {
        return Ok(AuthInfo::new(uid, AuthMethod::PASSWORD, Mfa::Default).with_rehash(rehash));
    }
    // the verdict is decided: a wrong password. The floor's dummy is only padding, so its
    // failure never turns the denial into anything else (it would go uncounted; silent-failure
    // review M1): it is logged and counted, and the attempt is denied and counted
    if floor {
        pad(pool, params, uid).await;
    }
    Err(access_denied())
}

/// The dummy verify that pads a denial already decided (the cost floor): its failure is logged
/// and counted
/// ([`super::pool::HashPoolStats::floor_skipped`]), never returned. An expired wait is load; any
/// other failure (a panic, an OS fault) is a fault that keeps the padding off for this user
/// (silent-delta S-1).
async fn pad(pool: &PoolBinding, params: &HashParams, uid: UserId) {
    let Err(e) = dummy::verify(pool, params).await else {
        return;
    };
    pool.count_floor_skipped();
    if let Some(HashPoolError::Overloaded { .. }) = super::pool::pool_error(&e) {
        tracing::warn!(
            target: "erp::auth",
            user = uid.get(),
            "the denial cost floor's dummy verify could not get a permit for user {}: {e}",
            uid.get()
        );
    } else {
        pool.count_floor_failure();
        tracing::error!(
            target: "erp::auth",
            user = uid.get(),
            "the denial cost floor's dummy verify failed for user {}; denials of this user are \
             cheaper than the floor until this is fixed: {e}",
            uid.get()
        );
    }
}

/// The dummy verify that pads the refusal of a stored hash over this node's reservation
/// (security review round 2 delta SD-1). Unlike [`pad`], its failure is returned: the refusal is
/// then answered as an unknown login's whose dummy could not run (an expired wait is `Denied` and
/// not counted, anything else `Failed`), so the answer and the counting do not tell a known user
/// from an unknown login (round 3 R3-a). Counted in
/// [`super::pool::HashPoolStats::over_reservation_pad_skipped`] and logged at `warn` naming
/// `admission.hash_memory_kib`, not the cost floor (R3-b).
///
/// # Errors
///
/// The dummy verify's.
async fn pad_over_reservation(pool: &PoolBinding, params: &HashParams, uid: UserId) -> Result<()> {
    let Err(e) = dummy::verify(pool, params).await else {
        return Ok(());
    };
    pool.count_over_reservation_pad_skipped();
    tracing::warn!(
        target: "erp::auth",
        user = uid.get(),
        "the dummy verify that pads the refusal of user {}'s stored hash (over this node's \
         admission.hash_memory_kib) could not run; the attempt is answered as an unknown \
         login's: {e}",
        uid.get()
    );
    Err(e)
}

async fn read_stored(tx: &mut Transaction, uid: UserId) -> Result<Option<String>> {
    let rows = tx
        .query(
            "SELECT password FROM res_users WHERE id = $1",
            &[&uid.get()],
        )
        .await?;
    match rows.as_slice() {
        [] => Err(access_denied()),
        [row] => row.try_get::<_, Option<String>>(0).map_err(|e| {
            Error::db(format!("res_users.password of user {}: {e}", uid.get())).with_source(e)
        }),
        more => Err(Error::programming(format!(
            "{} res_users rows with id {}",
            more.len(),
            uid.get()
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Security review round 2 N1 and delta N-c: `Debug` of a credential prints neither the login
    /// (users type passwords into the login field) nor its length, nor the password.
    #[test]
    fn a_credential_debug_hides_the_login_and_its_length() {
        let credential = Credential::password("hunter2", "secret-pw");
        let text = format!("{credential:?}");
        assert_eq!(
            text,
            r#"Password { login: "<redacted>", password: "<redacted>" }"#
        );
    }
}
