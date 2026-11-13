//! Bounded administrator reset; no database connection is held while hashing.
use super::*;
use std::collections::{BTreeMap, BTreeSet};

/// One selected user's replacement password (blank means skip).
/// Secret text is private, zeroized on drop and omitted from Debug.
///
/// ```
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// use erp_security::auth::PasswordReset;
/// let target = erp_orm::UserId::new(7)?;
/// let replacement = PasswordReset::new(target, "example replacement")?;
/// assert!(!format!("{replacement:?}").contains("example replacement"));
/// # Ok(())
/// # }
/// ```
#[non_exhaustive]
pub struct PasswordReset {
    user: UserId,
    password: Zeroizing<String>,
    submitted_bytes: usize,
}
impl fmt::Debug for PasswordReset {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PasswordReset")
            .field("user", &self.user)
            .finish_non_exhaustive()
    }
}
impl PasswordReset {
    /// Validate the default credential-byte limit and normalize the public change input.
    /// Empty/whitespace-only administrator lines intentionally skip mutation.
    /// # Errors
    /// Validation when the input exceeds 1024 bytes; the service also enforces its configured limit.
    pub fn new(user: UserId, password: &str) -> Result<Self> {
        if password.len() > AuthConfig::DEFAULT.credential_bytes() {
            return Err(too_long());
        }
        Ok(Self {
            user,
            password: Zeroizing::new(password.trim().to_owned()),
            submitted_bytes: password.len(),
        })
    }

    /// The user whose password this line replaces.
    #[must_use]
    pub fn user(&self) -> UserId {
        self.user
    }
    /// Whether the line is blank (or whitespace only) and so changes nothing.
    #[must_use]
    pub fn is_blank(&self) -> bool {
        self.password.is_empty()
    }
}
fn too_long() -> Error {
    Error::validation("The new password exceeds the configured credential byte limit.")
}
fn manager_required() -> Error {
    Error::access("Password resets require access administration authority.")
}
const END_RESET: store::EndStatement = store::EndStatement::new(
    "DELETE FROM erp_session WHERE id = ANY($1::text[]::uuid[]) AND id <> $2::text::uuid RETURNING id::text, user_id::int8",
);

impl Auth {
    /// Reset at most 100 distinct selected users after fresh acting-user identity confirmation.
    /// Blank lines are skipped. All hashes/session-end hooks commit together. If selected,
    /// the actor retains only the presented session; ordinary password writes still revoke all.
    /// No general res.users write permission is granted by this narrow operation.
    /// Every target but the inactive superuser row must be visible to the actor under `res.users`'
    /// company restriction with all of the actor's companies, and a target holding
    /// `base.group_system` needs an actor holding it (Amendment 1); both are checked in
    /// the preflight and again after the final locks.
    /// # Errors
    /// UserError without HTTP authority; Access for a non-manager; Validation for duplicate,
    /// oversized batches/passwords or excess stored sessions; Missing for a missing target and,
    /// with the same message, for a target outside the actor's company restriction; Access
    /// "Only a Settings administrator can reset the password of a Settings administrator." for a
    /// Settings administrator target reset by an actor who is not one; session, hash, hook,
    /// concurrency and database failures propagate without partial commit.
    pub async fn reset_passwords(
        &self,
        secret: Option<&SessionSecret>,
        changes: &[PasswordReset],
    ) -> Result<IdentityCheck> {
        self.reset_passwords_authorized(secret, changes, &|_| Ok(()))
            .await
    }

    /// Administrator reset retaining an additional refusal-only gate through the final transaction.
    /// # Errors
    /// As reset_passwords, plus the gate refusal and a classified unknown-commit error.
    pub async fn reset_passwords_authorized(
        &self,
        secret: Option<&SessionSecret>,
        changes: &[PasswordReset],
        authorize: &PasswordAuthorization<'_>,
    ) -> Result<IdentityCheck> {
        let secret = request_secret(secret)?;
        if changes.is_empty() || changes.len() > 100 {
            return Err(Error::validation(
                "A password reset requires between 1 and 100 selected users.",
            ));
        }
        let mut selected = BTreeSet::new();
        // the targets alone: the actor joins `selected` for the final locks only
        let targets: Vec<UserId> = changes.iter().map(|change| change.user).collect();
        for change in changes {
            if !selected.insert(change.user.get()) {
                return Err(Error::validation(
                    "A password reset may name each user only once.",
                ));
            }
            if change.submitted_bytes > self.config.credential_bytes() {
                return Err(too_long());
            }
        }
        let _admitted = admit(
            &self.admission,
            self.pool.config().queue_wait(),
            &self.counters,
        )
        .await
        .map_err(Error::from)?;
        let (actor, check) = self.identity_user(secret).await?;
        // Authority is checked before expensive work and again after final lock waits.
        let mut preflight = self.orm.begin_opts(read_committed()).await?;
        let authorized = async {
            let authority = self.check_session_in(&mut preflight, secret).await?;
            if !authority.has_groups("base.group_erp_manager,base.group_system")? {
                return Err(manager_required());
            }
            if check == IdentityCheck::Verified {
                let ids: Vec<i64> = selected.iter().copied().collect();
                let rows = preflight
                    .query(
                        "SELECT id FROM res_users WHERE id = ANY($1::int8[])",
                        &[&ids],
                    )
                    .await?;
                if rows.len() != ids.len() {
                    return Err(missing_reset_target());
                }
                authority
                    .check_reset_targets(&mut preflight, &targets)
                    .await?;
            }
            Ok(())
        }
        .await;
        match authorized {
            Ok(()) => preflight.rollback().await?,
            Err(error) => return Err(abandon(&mut preflight, error).await),
        }
        drop(preflight);
        if check == IdentityCheck::Required {
            return Ok(check);
        }
        let settings = self.settings_current().await?;
        let params = *settings.settings.hash_params();
        let mut ids = Vec::new();
        let mut hashes = Zeroizing::new(Vec::<String>::new());
        for change in changes {
            if change.password.is_empty() {
                continue;
            } // specified administrator blank-line skip
            let plain = Zeroizing::new(change.password.as_bytes().to_vec());
            hashes.push(
                self.registration
                    .run(params.memory_kib(), move || password::hash(&plain, &params))
                    .await??,
            );
            ids.push(change.user.get());
        }
        selected.insert(actor.uid.get());
        let selected: Vec<i64> = selected.into_iter().collect();
        let mut tx = self.orm.begin_opts(read_committed()).await?;
        let outcome = async {
            let users = tx.query("SELECT id FROM res_users WHERE id = ANY($1::int8[]) ORDER BY id FOR UPDATE", &[&selected]).await?;
            if users.len() != selected.len() { return Err(missing_reset_target()); }
            let (session, _, actor_extra) = self.lock_identity(&mut tx, secret, &actor).await?;
            let authority = self.check_session_in(&mut tx, secret).await?;
            if !authority.has_groups("base.group_erp_manager,base.group_system")? { return Err(manager_required()); }
            if recent(&mut tx, session.id()).await? == IdentityCheck::Required { return Ok(IdentityCheck::Required); }
            // User locks conflict with login insertion; each lateral probe admits at most 101 rows.
            let sessions = tx.query("SELECT s.id::text, s.user_id::int8 FROM unnest($1::int8[]) u(id) CROSS JOIN LATERAL (SELECT id, user_id FROM erp_session WHERE user_id = u.id ORDER BY id LIMIT 101 FOR UPDATE) s ORDER BY s.user_id, s.id", &[&ids]).await?;
            let mut counts = BTreeMap::<i64, usize>::new();
            let mut session_ids = Vec::<String>::with_capacity(sessions.len());
            for row in &sessions {
                let uid: i64 = col(row, 1)?;
                let count = counts.entry(uid).or_default();
                *count = count.checked_add(1).ok_or_else(|| Error::validation("Password reset session count overflow."))?;
                if *count > IDENTITY_SESSION_LIMIT { return Err(Error::validation("Identity operations support at most 100 stored sessions per user; end excess sessions and retry.")); }
                session_ids.push(col(row, 0)?);
            }
            // Recheck the cookie, wall-clock expiry and policy after every target-session wait.
            self.lock_identity(&mut tx, secret, &actor).await?;
            let final_authority = self.check_session_in(&mut tx, secret).await?;
            authorize(final_authority.principal())?;
            if !final_authority.has_groups("base.group_erp_manager,base.group_system")? { return Err(manager_required()); }
            if recent(&mut tx, session.id()).await? == IdentityCheck::Required { return Ok(IdentityCheck::Required); }
            // after the final locks and authority rechecks: company scope, then the Settings
            // administrator rule, then the write (a refusal writes nothing)
            final_authority.check_reset_targets(&mut tx, &targets).await?;
            if let Some(pos) = ids.iter().position(|uid| *uid == actor.uid.get()) {
                let current = settings::load(&mut tx).await?;
                let (kid, key) = current.keys.issuing();
                let extra = self.token_plan.bind(&actor_extra)?;
                let token = session::token(key.bytes(), &session.id(), &TokenFields { user: actor.uid, login: &actor.login, password: &hashes[pos], active: true, extra: &extra })?;
                tx.execute("UPDATE erp_session SET token = $1, token_kid = $2 WHERE id = $3::text::uuid", &[&token.as_slice(), &kid, &session.id().to_text()]).await?;
            }
            if !ids.is_empty() {
                tx.execute("UPDATE res_users u SET password = v.password FROM unnest($1::int8[], $2::text[]) v(id,password) WHERE u.id = v.id", &[&ids, &hashes.as_slice()]).await?;
                store::end_with(&mut tx, END_RESET, &[&session_ids, &session.id().to_text()], EndReason::CredentialChanged).await?;
            }
            Ok(IdentityCheck::Verified)
        }.await;
        match outcome {
            Ok(value) => {
                identity_commit(&mut tx).await?;
                Ok(value)
            }
            Err(error) => Err(abandon(&mut tx, error).await),
        }
    }
}
