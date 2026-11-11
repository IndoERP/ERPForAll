//! SEC-041 typed identity gate and session-preserving password mutation.
use super::*;
mod reset;
/// A request-local policy check; it can only refuse, never grant session authority.
pub type PasswordAuthorization<'a> = dyn Fn(&erp_orm::Principal) -> Result<()> + Send + Sync + 'a;

async fn identity_commit(tx: &mut Transaction) -> Result<()> {
    match tx.commit_pipelined(None).await {
        Ok(()) => Ok(()),
        Err(erp_orm::CommitError::RolledBack(error)) => Err(error),
        Err(error @ erp_orm::CommitError::Unknown(_)) => {
            Err(Error::db("Password operation commit outcome is unknown.").with_source(error))
        }
        Err(other) => Err(other.into_error()),
    }
}
use erp_fields::{Value, ValueMap};
pub use reset::PasswordReset;

/// Result of an identity-gated operation. Exhaustive so callers must handle the wizard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentityCheck {
    /// The identity window is current (and a requested mutation committed).
    Verified,
    /// Confirm the current user's identity, then resubmit the operation.
    Required,
}

impl IdentityCheck {
    /// The typed window action for a required identity check; no action after success.
    /// The HTTP adapter supplies the wizard UI and resubmits its fixed operation afterwards.
    ///
    /// ```
    /// use erp_security::auth::IdentityCheck;
    /// assert!(IdentityCheck::Required.action().is_some());
    /// assert!(IdentityCheck::Verified.action().is_none());
    /// ```
    pub fn action(self) -> Option<ValueMap> {
        match self {
            Self::Verified => None,
            Self::Required => Some(ValueMap::from_iter([
                ("type".into(), Value::str("ir.actions.act_window")),
                ("res_model".into(), Value::str("res.users.identitycheck")),
                ("view_mode".into(), Value::str("form")),
                ("target".into(), Value::str("new")),
            ])),
        }
    }
}

const RECENT: &str = "SELECT identity_checked_at IS NOT NULL \
    AND identity_checked_at <= clock_timestamp() \
    AND identity_checked_at > clock_timestamp() - interval '10 minutes' \
    FROM erp_session WHERE id = $1::text::uuid";
// Independent hard operation bound: login's best-effort cleanup is not a storage invariant.
const IDENTITY_SESSION_LIMIT: usize = 100;
const IDENTITY_SESSION_PROBE: i64 = 101;

const END_OTHERS: store::EndStatement = store::EndStatement::new(
    "DELETE FROM erp_session WHERE id = ANY($1::text[]::uuid[]) AND id <> $2::text::uuid \
     RETURNING id::text, user_id::int8",
);

fn request_secret(secret: Option<&SessionSecret>) -> Result<&SessionSecret> {
    secret.ok_or_else(|| Error::user("This method can only be accessed over HTTP"))
}

async fn recent(tx: &mut Transaction, id: SessionId) -> Result<IdentityCheck> {
    let rows = tx.query(RECENT, &[&id.to_text()]).await?;
    let row = rows
        .first()
        .ok_or_else(|| session::session_expired(Refusal::NotFound))?;
    Ok(if col::<bool>(row, 0)? {
        IdentityCheck::Verified
    } else {
        IdentityCheck::Required
    })
}

/// A checked user snapshot kept across the connection-free hash work.
struct IdentityUser {
    uid: UserId,
    login: String,
    stored: Zeroizing<Option<String>>,
}

/// Successful verification carries its original rollout proof into the caller's final transaction.
struct IdentityVerdict {
    user: IdentityUser,
    feature: Option<AttemptFeature>,
    base_generation: Option<i64>,
    selection: Option<IdentitySelection>,
    _admitted: OwnedSemaphorePermit,
}

struct IdentitySelection {
    plan: LoginSelectionPlan,
    witness: SelectionWitness,
}

/// Protection always precedes the user/config/session locks of an identity operation.
async fn protect_identity_rollout(
    tx: &mut Transaction,
    feature: Option<&AttemptFeature>,
    base_generation: Option<i64>,
) -> Result<()> {
    if let Some(feature) = feature {
        return feature
            .member
            .binding
            .protect_in(tx, &feature.snapshot)
            .await
            .map_err(rollout_error);
    }
    if let Some(generation) = base_generation {
        let rows = tx
            .query(
                "SELECT required_version, enabled, generation FROM public.erp_node_feature_protect(
             'auth.login_principal', 1, NULL::uuid, NULL::bytea, $1, false)",
                &[&generation],
            )
            .await
            .map_err(rollout_error)?;
        let [row] = rows.as_slice() else {
            return Err(Error::programming(
                "feature protection helper returned no unique state",
            ));
        };
        if col::<i32>(row, 0)? != 1 || col::<bool>(row, 1)? || col::<i64>(row, 2)? != generation {
            return Err(Error::programming(
                "login selection rollout generation changed",
            ));
        }
    }
    // No proof exists only for the explicit selection regression fixture seam.
    Ok(())
}

impl Auth {
    /// Check the identity window of a presented HTTP session (SEC-041).
    ///
    /// ```no_run
    /// # async fn example(auth: &erp_security::auth::Auth, cookie: &erp_security::auth::SessionSecret) -> erp_fields::Result<()> {
    /// use erp_security::auth::IdentityCheck;
    /// match auth.check_identity(Some(cookie)).await? {
    ///     IdentityCheck::Verified => { /* resubmit the fixed sensitive operation */ }
    ///     IdentityCheck::Required => { /* render IdentityCheck::Required.action() */ }
    /// }
    /// # Ok(()) }
    /// ```
    ///
    /// # Errors
    /// `UserError` without an HTTP session, `SessionExpired` for an invalid cookie, or the
    /// database/settings errors. This observation alone is never mutation authority: each
    /// sensitive operation rechecks the window in its own transaction.
    pub async fn check_identity(&self, secret: Option<&SessionSecret>) -> Result<IdentityCheck> {
        let secret = request_secret(secret)?;
        let SessionBegin { mut tx, session } =
            self.begin_session(secret, CredentialOptions::new()).await?;
        let outcome = recent(&mut tx, session.id()).await;
        match outcome {
            Ok(check) => {
                tx.rollback().await?;
                Ok(check)
            }
            Err(e) => Err(abandon(&mut tx, e).await),
        }
    }

    async fn identity_user(&self, secret: &SessionSecret) -> Result<(IdentityUser, IdentityCheck)> {
        let SessionBegin { mut tx, session } =
            self.begin_session(secret, CredentialOptions::new()).await?;
        let outcome = async {
            let rows = tx
                .query(
                    "SELECT login, password FROM res_users WHERE id = $1",
                    &[&session.user().get()],
                )
                .await?;
            let row = rows.first().ok_or_else(credential::access_denied)?;
            let user = IdentityUser {
                uid: session.user(),
                login: col(row, 0)?,
                stored: Zeroizing::new(col(row, 1)?),
            };
            Ok((user, recent(&mut tx, session.id()).await?))
        }
        .await;
        match outcome {
            Ok(value) => {
                tx.rollback().await?;
                Ok(value)
            }
            Err(e) => Err(abandon(&mut tx, e).await),
        }
    }

    /// Verify the current user's password and persist a ten-minute identity confirmation.
    /// Admission precedes database work; shared cooldown and credential dispatch are the
    /// login's. Failed guesses are recorded even if the caller disconnects during verification.
    /// No additional session is created. Membership, generation and selection witness are retained
    /// across verification and protected before final user/session locks; a disabled or changed
    /// rollout cannot publish a new confirmation. The final check refuses a revoked cookie.
    /// See [`Auth::change_password`] for an example of confirming and resubmitting an operation.
    ///
    /// # Errors
    /// `AccessDenied` for refused credentials/cooldown, `SessionExpired` for a refused session,
    /// `Validation` above 100 stored sessions, admission overload, and database/settings/credential-hook
    /// errors. Secret input is bounded by `credential_bytes`; owned password copies are zeroized.
    pub async fn confirm_identity(
        &self,
        secret: &SessionSecret,
        password: &str,
        client: ClientAddr,
    ) -> Result<()> {
        self.confirm_identity_authorized(secret, password, client, &|_| Ok(()))
            .await
    }

    /// Confirm identity while retaining a request-local refusal gate through final authorization.
    /// The gate receives the current primary-transaction principal and cannot replace it.
    /// # Errors
    /// As confirm_identity, plus the gate refusal and a classified unknown-commit error.
    pub async fn confirm_identity_authorized(
        &self,
        secret: &SessionSecret,
        password: &str,
        client: ClientAddr,
        authorize: &PasswordAuthorization<'_>,
    ) -> Result<()> {
        self.screen("", password, client).map_err(Error::from)?;
        let admitted = admit(
            &self.admission,
            self.pool.config().queue_wait(),
            &self.counters,
        )
        .await
        .map_err(Error::from)?;
        let (user, _) = self.identity_user(secret).await?;
        if user.login.len() > self.config.credential_bytes() {
            return Err(credential::access_denied());
        }
        let cached = self.settings_current().await?;
        let feature = self.feature_attempt().await.map_err(Error::from)?;
        let principal_accounting = self.selection_overridden
            || feature
                .as_ref()
                .is_some_and(|feature| feature.snapshot.enabled());
        let expected_generation = feature
            .as_ref()
            .filter(|feature| !feature.snapshot.enabled())
            .map(|feature| feature.snapshot.generation());
        let keys = Keys::new(&cached.throttle, client, &user.login);
        let policy = self
            .cooldown_policy(&cached.settings, client, &user.login, &keys)
            .await
            .map_err(Error::from)?;
        let (attempt, phase_a, plan) = self
            .phase_a(principal_accounting, expected_generation)
            .run(&keys, &user.login, &cached.throttle)
            .await
            .map_err(Error::from)?;
        if let Some(refusal) = cooldown(&cached.settings, &policy, &attempt, &keys) {
            return Err(self.refuse_cooldown(phase_a, &keys, refusal).await.into());
        }
        let verdict = Verdict {
            orm: self.orm.clone(),
            pool: self.registration.clone(),
            counters: Arc::clone(&self.counters),
            cached,
            credential: Arc::new(Credential::password(&user.login, password)),
            interactive: true,
            attempt,
            keys,
            client,
            user_agent: None,
            sql: Arc::clone(&self.sql),
            has_languages: self.has_languages,
            checks_overridden: self.checks_overridden,
            cooldown_overridden: self.cooldown_overridden,
            principal_accounting,
            feature,
            limits: self.selection_limits,
            plan,
            cooldown_deadline: self.cooldown_deadline,
            policy,
            _admitted: admitted,
        };
        let login_key = verdict.keys.login;
        let expected_uid = user.uid;
        let task =
            tokio::spawn(async move { verdict.verify_identity(phase_a, expected_uid).await });
        let (reply, answer) = oneshot::channel();
        tokio::spawn(watch(task, Arc::clone(&self.counters), login_key, reply));
        let IdentityVerdict {
            user: verified,
            feature,
            base_generation,
            selection,
            _admitted,
        } = answer
            .await
            .map_err(|_| Error::programming("the identity verdict task ended without an answer"))?
            .map_err(Error::from)?;
        let mut tx = self.orm.begin_opts(read_committed()).await?;
        let outcome = async {
            protect_identity_rollout(&mut tx, feature.as_ref(), base_generation).await?;
            if let Some(selection) = &selection {
                tx.execute("LOCK TABLE res_users IN SHARE ROW EXCLUSIVE MODE", &[]).await?;
                let fresh = dispatch_selection(&mut tx, &verified.login, &self.selection_limits, self.orm.registry()).await?;
                let candidate = select_candidate(&mut tx, &fresh, &self.selection_limits).await?;
                let matches = fresh.domain.domain() == selection.plan.domain.domain()
                    && fresh.order == selection.plan.order
                    && candidate.is_some_and(|(uid, _, _, _, _, witness)| uid == verified.uid && witness == selection.witness);
                if !matches {
                    return Err(credential::access_denied());
                }
            }
            let (session, ..) = self.lock_identity(&mut tx, secret, &verified).await?;
            let authority = self.check_session_in(&mut tx, secret).await?;
            authorize(authority.principal())?;
            tx.execute("UPDATE erp_session SET identity_checked_at = clock_timestamp() WHERE id = $1::text::uuid", &[&session.id().to_text()]).await?;
            Ok(())
        }.await;
        match outcome {
            Ok(()) => identity_commit(&mut tx).await,
            Err(e) => Err(abandon(&mut tx, e).await),
        }
    }

    /// Change the password of the presented HTTP session after recent identity confirmation.
    /// Returns the wizard action outcome without mutation when confirmation is stale. Success
    /// preserves the cookie and stable session id, refreshes its token, and revokes other
    /// sessions with their hooks atomically. Ordinary password-field writes still revoke all.
    ///
    /// ```no_run
    /// # async fn example(auth: &erp_security::auth::Auth, cookie: &erp_security::auth::SessionSecret,
    /// # current_password: &str, new_password: &str, client: std::net::IpAddr) -> erp_fields::Result<()> {
    /// use erp_security::auth::{ClientAddr, IdentityCheck};
    /// auth.confirm_identity(cookie, current_password, ClientAddr::Known(client)).await?;
    /// match auth.change_password(Some(cookie), new_password).await? {
    ///     IdentityCheck::Verified => { /* the existing cookie remains valid */ }
    ///     IdentityCheck::Required => { /* confirmation expired; show the wizard again */ }
    /// }
    /// # Ok(()) }
    /// ```
    ///
    /// # Errors
    /// `UserError` for no HTTP session or an empty password; `Validation` for an oversized
    /// password or more than 100 stored sessions; session/admission/hash/hook/database errors.
    /// Concurrency errors require retrying this whole operation, including identity validation.
    /// No partial mutation commits.
    pub async fn change_password(
        &self,
        secret: Option<&SessionSecret>,
        password: &str,
    ) -> Result<IdentityCheck> {
        self.change_password_authorized(secret, password, &|_| Ok(()))
            .await
    }

    /// Change the own password with a request-local gate checked in the final transaction.
    /// # Errors
    /// As change_password, plus the gate refusal and a classified unknown-commit error.
    pub async fn change_password_authorized(
        &self,
        secret: Option<&SessionSecret>,
        password: &str,
        authorize: &PasswordAuthorization<'_>,
    ) -> Result<IdentityCheck> {
        let secret = request_secret(secret)?;
        if password.is_empty() {
            return Err(Error::user(
                "Setting empty passwords is not allowed for security reasons!",
            ));
        }
        if password.len() > self.config.credential_bytes() {
            return Err(Error::validation(
                "The new password exceeds the configured credential byte limit.",
            ));
        }
        let _admitted = admit(
            &self.admission,
            self.pool.config().queue_wait(),
            &self.counters,
        )
        .await
        .map_err(Error::from)?;
        let (user, check) = self.identity_user(secret).await?;
        if check == IdentityCheck::Required {
            return Ok(check);
        }
        let cached = self.settings_current().await?;
        let params = *cached.settings.hash_params();
        let plain = Zeroizing::new(password.as_bytes().to_vec());
        let hash = Zeroizing::new(
            self.registration
                .run(params.memory_kib(), move || password::hash(&plain, &params))
                .await??,
        );
        let mut tx = self.orm.begin_opts(read_committed()).await?;
        let outcome = async {
            let (session, session_ids, extra_values) =
                self.lock_identity(&mut tx, secret, &user).await?;
            let authority = self.check_session_in(&mut tx, secret).await?;
            authorize(authority.principal())?;
            if recent(&mut tx, session.id()).await? == IdentityCheck::Required {
                return Ok(IdentityCheck::Required);
            }
            let current = settings::load(&mut tx).await?;
            let (kid, key) = current.keys.issuing();
            // the plan's added fields, read under the user lock taken above
            let extra = self.token_plan.bind(&extra_values)?;
            let token = session::token(
                key.bytes(),
                &session.id(),
                &TokenFields {
                    user: user.uid,
                    login: &user.login,
                    password: &hash,
                    active: true,
                    extra: &extra,
                },
            )?;
            tx.execute(
                "UPDATE res_users SET password = $1 WHERE id = $2",
                &[&hash.as_str(), &user.uid.get()],
            )
            .await?;
            tx.execute(
                "UPDATE erp_session SET token = $1, token_kid = $2 WHERE id = $3::text::uuid",
                &[&token.as_slice(), &kid, &session.id().to_text()],
            )
            .await?;
            store::end_with(
                &mut tx,
                END_OTHERS,
                &[&session_ids, &session.id().to_text()],
                EndReason::CredentialChanged,
            )
            .await?;
            Ok(IdentityCheck::Verified)
        }
        .await;
        match outcome {
            Ok(check) => {
                identity_commit(&mut tx).await?;
                Ok(check)
            }
            Err(e) => Err(abandon(&mut tx, e).await),
        }
    }

    /// Fixed lock order shared with password writers and login cleanup. READ COMMITTED rechecks
    /// rows after waiting; credential comparison rejects stale hash work. The final session check
    /// validates the actual cookie, including predecessor grace and all expiry/token rules.
    async fn lock_identity(
        &self,
        tx: &mut Transaction,
        secret: &SessionSecret,
        expected: &IdentityUser,
    ) -> Result<(CheckedSession, Vec<String>, TokenValues)> {
        let rows = tx
            .query(&self.identity_lock_sql, &[&expected.uid.get()])
            .await?;
        let row = rows.first().ok_or_else(credential::access_denied)?;
        let login: &str = col(row, 0)?;
        let stored = Zeroizing::new(col::<Option<String>>(row, 1)?);
        if login != expected.login || *stored != *expected.stored || !col::<bool>(row, 2)? {
            return Err(credential::access_denied());
        }
        let extra_values = self.token_plan.read_values(row, 3)?;
        let keys = tx
            .query(
                "SELECT key FROM ir_config_parameter WHERE key = 'database.secret' FOR SHARE",
                &[],
            )
            .await?;
        if keys.len() != 1 {
            return Err(super::super::secret::SecretError::Missing.into());
        }
        // Probe one beyond the hard limit; never lock/materialize the whole stored set.
        // Login inserts hold the user row FOR SHARE, so our FOR UPDATE prevents phantoms
        // until commit. Keep the exact bounded ids for revocation rather than reselecting
        // an unbounded set. Expired and pending sessions count too.
        let sessions = tx
            .query(
                "SELECT id::text FROM erp_session WHERE user_id = $1 \
                 ORDER BY erp_session.id LIMIT $2 FOR UPDATE",
                &[&expected.uid.get(), &IDENTITY_SESSION_PROBE],
            )
            .await?;
        if sessions.len() > IDENTITY_SESSION_LIMIT {
            return Err(Error::validation(
                "Identity operations support at most 100 stored sessions per user; end excess sessions and retry.",
            ));
        }
        let session_ids = sessions
            .iter()
            .map(|row| col::<String>(row, 0))
            .collect::<Result<Vec<_>>>()?;
        let authorization = self.check_session_in(tx, secret).await?;
        if authorization.session.user() != expected.uid {
            return Err(credential::access_denied());
        }
        // The common check uses transaction-start now(). Locks may have waited since then:
        // recheck time-dependent authorization against the database clock after acquiring them.
        let current = settings::load(tx).await?;
        let times = tx
            .query(
                "SELECT expires_at > clock_timestamp(), \
             last_seen_at > clock_timestamp() - make_interval(secs => $2), \
             rotated_at >= clock_timestamp() - make_interval(secs => $3) \
             FROM erp_session WHERE id = $1::text::uuid",
                &[
                    &authorization.session.id().to_text(),
                    &current.settings.max_inactivity().as_secs_f64(),
                    &self.config.rotation_grace().as_secs_f64(),
                ],
            )
            .await?;
        let row = times
            .first()
            .ok_or_else(|| session::session_expired(Refusal::NotFound))?;
        if authorization.session.via_previous() && col::<Option<bool>>(row, 2)? != Some(true) {
            return Err(session::session_expired(Refusal::TooOld));
        }
        if !col::<bool>(row, 0)? || !col::<bool>(row, 1)? {
            return Err(session::session_expired(Refusal::Expired));
        }
        Ok((authorization.session, session_ids, extra_values))
    }
}

impl Verdict {
    async fn verify_identity(
        mut self,
        mut phase_a: PhaseA,
        expected: UserId,
    ) -> Result<IdentityVerdict, LoginError> {
        let auth = match self.verify(&mut phase_a).await {
            Ok(auth) => auth,
            Err(e) => return self.refuse(e, phase_a).await,
        };
        let Some(mut user) = self.attempt.user.take() else {
            return Err(self.abandon_phase_a(phase_a, LoginError::Denied).await);
        };
        if user.uid != expected {
            return Err(self.abandon_phase_a(phase_a, LoginError::Denied).await);
        }
        if auth.uid() != user.uid {
            let error = Error::programming(format!(
                "_check_credentials of user {} returned user {}",
                user.uid.get(),
                auth.uid().get(),
            ));
            return Err(self
                .abandon_phase_a(phase_a, LoginError::Failed(error))
                .await);
        }
        let selection = match (user.witness.take(), self.plan.take()) {
            (Some(witness), Some(plan)) => Some(IdentitySelection { witness, plan }),
            (None, None) => None,
            (Some(_), None) | (None, Some(_)) => {
                let error = Error::programming("missing identity selection plan or witness");
                return Err(self
                    .abandon_phase_a(phase_a, LoginError::Failed(error))
                    .await);
            }
        };
        end_phase_a(phase_a, true)
            .await
            .map_err(LoginError::Failed)?;
        let verified = IdentityUser {
            uid: expected,
            login: self.login().to_owned(),
            stored: Zeroizing::new(user.stored.as_deref().cloned()),
        };
        Ok(IdentityVerdict {
            user: verified,
            feature: self.feature,
            base_generation: self.attempt.base_generation,
            selection,
            _admitted: self._admitted,
        })
    }
}
