// SPDX-License-Identifier: MIT OR Apache-2.0
use super::*;
use crate::auth::{
    AuthMethod, Mfa,
    browser::SignedPrefs,
    external::{Claims, ExternalProvider},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use erp_orm::OperationIdentity;
use sha2::{Digest, Sha256};

const REGISTER_PROVIDER: &str =
    "INSERT INTO erp_external_provider (issuer) VALUES ($1) ON CONFLICT DO NOTHING";
const LOCK_PROVIDER: &str = "SELECT enabled FROM erp_external_provider WHERE issuer = $1 FOR SHARE";
const NEUTRALIZED: &str = "SELECT EXISTS(SELECT 1 FROM ir_config_parameter
    WHERE key = 'database.neutralized' AND value = 'true')";
const DENIED: &str = "External sign-in was refused";

fn refused() -> Error {
    Error::new(ErrorKind::AccessDenied, DENIED)
}

fn random_token() -> Result<String> {
    let mut bytes = Zeroizing::new([0; 32]);
    getrandom::fill(&mut *bytes)
        .map_err(|_| Error::programming("External login entropy is unavailable"))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes.as_slice()))
}

async fn lock_provider(tx: &mut Transaction, issuer: &str) -> Result<()> {
    let rows = tx.query(LOCK_PROVIDER, &[&issuer]).await?;
    if rows.first().map(|row| col::<bool>(row, 0)).transpose()? != Some(true)
        || database_neutralized(tx).await?
    {
        return Err(refused());
    }
    Ok(())
}

async fn database_neutralized(tx: &mut Transaction) -> Result<bool> {
    let rows = tx.query(NEUTRALIZED, &[]).await?;
    col(rows.first().ok_or_else(refused)?, 0)
}

struct Intent {
    nonce: String,
    verifier: Zeroizing<String>,
    destination: String,
    link_session: Option<String>,
}

impl Auth {
    /// Create a cookie-bound, single-use OIDC challenge, optionally linking the currently signed-in user.
    ///
    /// # Errors
    /// Refusal for a disabled provider, an invalid link session, or excessive start attempts.
    pub async fn begin_external(
        &self,
        provider: &ExternalProvider,
        prefs: &SignedPrefs,
        redirect_uri: &str,
        destination: &str,
        link: Option<&SessionSecret>,
        client: ClientAddr,
    ) -> Result<String> {
        if destination.len() > 2048
            || !destination.starts_with('/')
            || destination.starts_with("//")
        {
            return Err(refused());
        }
        if let ClientAddr::Known(addr) = client
            && !self.prefilter.admit(&throttle::address_key(addr))
        {
            return Err(refused());
        }
        let link_session = if let Some(secret) = link {
            let mut begun = self.begin_session(secret, CredentialOptions::new()).await?;
            let id = begun.session.id().to_text();
            begun.tx.rollback().await?;
            Some(id)
        } else {
            None
        };
        let state = random_token()?;
        let nonce = random_token()?;
        let verifier = Zeroizing::new(random_token()?);
        let digest = Sha256::digest(state.as_bytes());
        let digest = digest.as_slice();
        let browser = prefs.binding().as_slice();
        let issuer = provider.issuer();
        let mut tx = self.orm.begin_opts(read_committed()).await?;
        let result = async {
            tx.execute(REGISTER_PROVIDER, &[&issuer]).await?;
            lock_provider(&mut tx, issuer).await?;
            tx.execute("DELETE FROM erp_external_login_intent WHERE state IN (
                SELECT state FROM erp_external_login_intent WHERE expires_at <= now()
                ORDER BY expires_at LIMIT 64 FOR UPDATE SKIP LOCKED)", &[]).await?;
            let created = tx.execute("INSERT INTO erp_external_login_intent
                (state, issuer, browser, nonce, verifier, destination, link_session)
                SELECT $1, $2, $3, $4, $5, $6, $7::text::uuid
                WHERE (SELECT count(*) FROM erp_external_login_intent WHERE browser = $3 AND expires_at > now()) < 8",
                &[&digest, &issuer, &browser, &nonce, &verifier.as_str(), &destination, &link_session]).await?;
            if created != 1 { return Err(refused()); }
            Ok(())
        }.await;
        if let Err(error) = result {
            return Err(abandon(&mut tx, error).await);
        }
        tx.commit().await?;
        Ok(provider.authorize(redirect_uri, &state, &nonce, &verifier))
    }

    /// Complete an OIDC login using the same repository session tokens and user context as passwords.
    /// The authorization code is exchanged only after the browser's challenge has been consumed.
    ///
    /// # Errors
    /// Refusal for invalid callbacks, disabled providers, unlinked or revoked identities, or unmet MFA.
    /// Database and session issuance failures propagate without exposing provider credentials.
    #[allow(clippy::too_many_arguments)]
    pub async fn complete_external(
        &self,
        provider: &ExternalProvider,
        prefs: &SignedPrefs,
        redirect_uri: &str,
        state: &str,
        code: &str,
        native_session: Option<&SessionSecret>,
        client: ClientAddr,
        agent: Option<&str>,
    ) -> Result<(Login, String)> {
        let intent = self.take_external_intent(provider, prefs, state).await?;
        let claims = provider
            .exchange(redirect_uri, code, &intent.nonce, &intent.verifier)
            .await?;
        let mut tx = self
            .orm
            .begin_opts(repeatable_read().operation_identity(OperationIdentity::generate()?))
            .await?;
        let result = async {
            lock_provider(&mut tx, provider.issuer()).await?;
            let link = match intent.link_session.as_deref() {
                Some(expected) => {
                    let session = native_session.ok_or_else(refused)?;
                    let authorization = self.check_session_in(&mut tx, session).await?;
                    let id = authorization.session_id().to_text();
                    if id != expected {
                        return Err(refused());
                    }
                    let rows = tx
                        .query(
                            "SELECT user_id FROM erp_session WHERE id = $1::text::uuid
                        AND state = 'active' AND expires_at > now() FOR SHARE",
                            &[&id],
                        )
                        .await?;
                    Some(
                        UserId::new(col(rows.first().ok_or_else(refused)?, 0)?)
                            .map_err(Error::from)?,
                    )
                }
                None => None,
            };
            let uid = find_or_link_external(&mut tx, provider, &claims, link).await?;
            self.issue_external(&mut tx, provider, &claims.sub, uid, client, agent)
                .await
        }
        .await;
        match result {
            Ok(login) => {
                tx.commit().await?;
                Ok((login, intent.destination))
            }
            Err(error) => Err(abandon(&mut tx, error).await),
        }
    }

    async fn take_external_intent(
        &self,
        provider: &ExternalProvider,
        prefs: &SignedPrefs,
        state: &str,
    ) -> Result<Intent> {
        if state.len() != 43
            || !state
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        {
            return Err(refused());
        }
        let digest = Sha256::digest(state.as_bytes());
        let digest = digest.as_slice();
        let browser = prefs.binding().as_slice();
        let issuer = provider.issuer();
        let mut tx = self.orm.begin_opts(read_committed()).await?;
        let result = async {
            lock_provider(&mut tx, issuer).await?;
            let rows = tx.query("DELETE FROM erp_external_login_intent WHERE state=$1 AND issuer=$2 AND browser=$3
                AND expires_at > now() RETURNING nonce, verifier, destination, link_session::text", &[&digest, &issuer, &browser]).await?;
            let row = rows.first().ok_or_else(refused)?;
            Ok(Intent { nonce: col(row, 0)?, verifier: Zeroizing::new(col(row, 1)?), destination: col(row, 2)?, link_session: col(row, 3)? })
        }.await;
        match result {
            Ok(intent) => {
                tx.commit().await?;
                Ok(intent)
            }
            Err(error) => Err(abandon(&mut tx, error).await),
        }
    }

    async fn issue_external(
        &self,
        tx: &mut Transaction,
        provider: &ExternalProvider,
        subject: &str,
        uid: UserId,
        client: ClientAddr,
        agent: Option<&str>,
    ) -> Result<Login> {
        let cached = settings::load(tx).await?;
        let rows = tx.query(&self.identity_lock_sql, &[&uid.get()]).await?;
        let row = rows.first().ok_or_else(refused)?;
        let active: bool = col(row, 2)?;
        if !active || uid == SUPERUSER_ID {
            return Err(refused());
        }
        let login: Zeroizing<String> = Zeroizing::new(col(row, 0)?);
        let password: Option<Zeroizing<String>> =
            col::<Option<String>>(row, 1)?.map(Zeroizing::new);
        let current = self.token_plan.read_values(row, 3)?;
        let extra = self.token_plan.bind(&current)?;
        let (kid, key) = cached.keys.issuing();
        let secret = SessionSecret::generate()?;
        let id = SessionId::generate()?;
        let token = session::token(
            key.bytes(),
            &id,
            &TokenFields {
                user: uid,
                login: &login,
                password: password.as_deref().map_or("", String::as_str),
                active,
                extra: &extra,
            },
        )?;
        let lifetime = cached.settings.max_lifetime();
        let context = super::super::context::user_context(tx, uid)
            .await?
            .ok_or_else(refused)?;
        let address = match client {
            ClientAddr::Known(addr) => Some(addr.to_string()),
            ClientAddr::InProcess(_) => None,
        };
        let agent = agent.map(session::user_agent);
        let issuer = provider.issuer();
        tx.execute("INSERT INTO erp_session (id,user_id,sid_digest,token,token_kid,state,expires_at,
                first_addr,first_agent,last_addr,last_agent,context,data)
            VALUES ($1::text::uuid,$2::int8,$3,$4,$5,'active', now()+make_interval(secs=>$6::float8),
                $7::text::inet,$8,$7::text::inet,$8,jsonb_build_object('lang',$9::text,'tz',$10::text,'uid',$2::int8),
                jsonb_build_object('external_issuer',$11::text,'external_subject',$12::text))",
            &[&id.to_text(), &uid.get(), &secret.digest().as_slice(), &token.as_slice(), &kid,
                &lifetime.as_secs_f64(), &address, &agent, &context.lang(), &context.tz(), &issuer, &subject]).await?;
        tx.execute(
            "INSERT INTO res_users_log (create_uid,create_date,write_uid,write_date)
            VALUES ($1, now() AT TIME ZONE 'UTC', $1, now() AT TIME ZONE 'UTC')",
            &[&uid.get()],
        )
        .await?;
        let older = tx
            .query(
                "SELECT id::text FROM erp_session WHERE user_id=$1 AND id<>$2::text::uuid
            ORDER BY created_at DESC,id DESC OFFSET $3::int8",
                &[
                    &uid.get(),
                    &id.to_text(),
                    &(i64::from(SESSIONS_PER_USER) - 1),
                ],
            )
            .await?;
        let ended = older
            .iter()
            .map(|row| SessionId::parse(col::<&str>(row, 0)?))
            .collect::<Result<Vec<_>>>()?;
        store::end_sessions(tx, &ended, EndReason::OverCap).await?;
        let auth = AuthInfo::new(uid, AuthMethod::OIDC, Mfa::Skip);
        if let Some(policy) = self.post_login {
            policy
                .run(tx, &id, &context, &login, &auth, &self.token_plan)
                .await?;
        }
        Ok(Login {
            session: NewSession {
                secret,
                id,
                lifetime,
            },
            auth,
            context,
        })
    }

    /// Revoke the caller's linked provider identity and all sessions issued from that provider for them.
    ///
    /// # Errors
    /// A refused native session or an error in the revocation transaction.
    pub async fn revoke_external(
        &self,
        provider: &ExternalProvider,
        secret: &SessionSecret,
    ) -> Result<u64> {
        let mut tx = self.orm.begin(SUPERUSER_ID).await?;
        let result = async {
            let identity = self.check_session_in(&mut tx, secret).await?;
            let issuer = provider.issuer();
            let rows = tx
                .query(
                    "SELECT user_id FROM erp_session WHERE id=$1::text::uuid
                AND state='active' AND expires_at>now() FOR SHARE",
                    &[&identity.session_id().to_text()],
                )
                .await?;
            let uid: i64 = col(rows.first().ok_or_else(refused)?, 0)?;
            let changed = tx
                .execute(
                    "UPDATE erp_external_identity SET revoked_at=now()
                WHERE issuer=$1 AND user_id=$2 AND revoked_at IS NULL",
                    &[&issuer, &uid],
                )
                .await?;
            end_provider_sessions(&mut tx, issuer, Some(uid)).await?;
            Ok(changed)
        }
        .await;
        match result {
            Ok(changed) => {
                tx.commit().await?;
                Ok(changed)
            }
            Err(error) => Err(abandon(&mut tx, error).await),
        }
    }

    /// Change provider availability with the current authority of a settings administrator.
    /// Disabling the provider ends all sessions it issued and prevents challenge completion.
    ///
    /// # Errors
    /// A missing or insufficient native session or an error in the policy transaction.
    pub async fn set_external_enabled(
        &self,
        provider: &ExternalProvider,
        secret: &SessionSecret,
        value: bool,
    ) -> Result<()> {
        let mut tx = self.orm.begin(SUPERUSER_ID).await?;
        let result = async {
            let identity = self.check_session_in(&mut tx, secret).await?;
            if !identity.has_groups("base.group_system")? {
                return Err(refused());
            }
            if value && database_neutralized(&mut tx).await? {
                return Err(refused());
            }
            let issuer = provider.issuer();
            tx.execute(
                "INSERT INTO erp_external_provider (issuer,enabled) VALUES ($1,$2)
                ON CONFLICT (issuer) DO UPDATE SET enabled=EXCLUDED.enabled",
                &[&issuer, &value],
            )
            .await?;
            if !value {
                tx.execute(
                    "DELETE FROM erp_external_login_intent WHERE issuer=$1",
                    &[&issuer],
                )
                .await?;
                end_provider_sessions(&mut tx, issuer, None).await?;
            }
            Ok(())
        }
        .await;
        match result {
            Ok(()) => tx.commit().await,
            Err(error) => Err(abandon(&mut tx, error).await),
        }
    }

    /// Whether the named provider may appear on the browser login screen.
    ///
    /// # Errors
    /// A database admission or query failure.
    pub async fn external_enabled(&self, provider: &ExternalProvider) -> Result<bool> {
        let mut tx = self
            .orm
            .begin_opts(read_committed().read_only("external identity availability"))
            .await?;
        let rows = tx
            .query(
                "SELECT enabled FROM erp_external_provider WHERE issuer=$1",
                &[&provider.issuer()],
            )
            .await?;
        let enabled = rows.first().map(|r| col(r, 0)).transpose()?.unwrap_or(true)
            && !database_neutralized(&mut tx).await?;
        tx.rollback().await?;
        Ok(enabled)
    }
}

async fn end_provider_sessions(tx: &mut Transaction, issuer: &str, uid: Option<i64>) -> Result<()> {
    let rows = tx
        .query(
            "SELECT id::text FROM erp_session WHERE data->>'external_issuer'=$1
        AND ($2::int8 IS NULL OR user_id=$2)",
            &[&issuer, &uid],
        )
        .await?;
    let ended = rows
        .iter()
        .map(|row| SessionId::parse(col::<&str>(row, 0)?))
        .collect::<Result<Vec<_>>>()?;
    store::end_sessions(tx, &ended, EndReason::Revoked).await?;
    Ok(())
}

async fn find_or_link_external(
    tx: &mut Transaction,
    provider: &ExternalProvider,
    claims: &Claims,
    link: Option<UserId>,
) -> Result<UserId> {
    let issuer = provider.issuer();
    let subject = &claims.sub;
    let rows = tx
        .query(
            "SELECT user_id, revoked_at IS NOT NULL FROM erp_external_identity
        WHERE issuer=$1 AND subject=$2 FOR UPDATE",
            &[&issuer, &subject],
        )
        .await?;
    if let Some(row) = rows.first() {
        let uid = UserId::new(col(row, 0)?).map_err(Error::from)?;
        let revoked: bool = col(row, 1)?;
        if link.is_some_and(|user| user != uid) || (link.is_none() && revoked) {
            return Err(refused());
        }
        if revoked {
            tx.execute(
                "UPDATE erp_external_identity SET revoked_at=NULL WHERE issuer=$1 AND subject=$2",
                &[&issuer, &subject],
            )
            .await?;
        }
        return Ok(uid);
    }
    let uid = match link {
        Some(uid) => uid,
        None => provision_external(tx, provider, claims).await?,
    };
    tx.execute(
        "INSERT INTO erp_external_identity (issuer,subject,user_id) VALUES ($1,$2,$3)",
        &[&issuer, &subject, &uid.get()],
    )
    .await?;
    Ok(uid)
}

async fn provision_external(
    tx: &mut Transaction,
    provider: &ExternalProvider,
    claims: &Claims,
) -> Result<UserId> {
    let rule = provider.provisioning().ok_or_else(refused)?;
    let email = claims
        .email
        .as_deref()
        .filter(|email| {
            email.len() <= 254
                && email.is_ascii()
                && !email
                    .bytes()
                    .any(|b| b.is_ascii_control() || b.is_ascii_whitespace())
                && email.split_once('@').is_some_and(|(user, domain)| {
                    !user.is_empty() && domain.eq_ignore_ascii_case(&rule.email_domain)
                })
        })
        .filter(|_| claims.email_verified == Some(true))
        .ok_or_else(refused)?;
    let rows = tx
        .query(
            "SELECT d.res_id FROM ir_model_data d JOIN res_groups g ON g.id=d.res_id
        WHERE d.module='base' AND d.name='group_portal' AND d.model='res.groups'",
            &[],
        )
        .await?;
    let portal: i64 = col(
        rows.first()
            .ok_or_else(|| Error::programming("Portal group is missing for external enrollment"))?,
        0,
    )?;
    let mut digest = Sha256::new();
    digest.update(provider.issuer().as_bytes());
    digest.update([0]);
    digest.update(claims.sub.as_bytes());
    let login = format!("oidc_{}", URL_SAFE_NO_PAD.encode(digest.finalize()));
    let ids = |id| {
        erp_fields::Value::list([erp_fields::Value::list([
            erp_fields::Value::Int(6),
            erp_fields::Value::Int(0),
            erp_fields::Value::list([erp_fields::Value::Int(id)]),
        ])])
    };
    let values = std::collections::BTreeMap::from([
        ("name".to_owned(), erp_fields::Value::str(email)),
        ("login".to_owned(), erp_fields::Value::str(login)),
        ("email".to_owned(), erp_fields::Value::str(email)),
        (
            "company_id".to_owned(),
            erp_fields::Value::Int(rule.company_id),
        ),
        ("company_ids".to_owned(), ids(rule.company_id)),
        ("group_ids".to_owned(), ids(portal)),
    ]);
    let env = tx.env();
    let users = env
        .records("res.users")?
        .create_values(tx, &[values])
        .await?;
    tx.flush().await?;
    let id = users
        .ids()
        .first()
        .copied()
        .ok_or_else(|| Error::programming("External enrollment did not create a user"))?;
    UserId::new(id).map_err(Error::from)
}
