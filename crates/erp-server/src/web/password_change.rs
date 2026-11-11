//! Browser-only adapter for fixed password operations; Auth owns sensitive transactions.
use super::*;
use erp_fields::{Error, Value, ValueMap};
use erp_orm::{Orm, RawVals, Transaction};
use erp_rpc::{PasswordCall, PasswordOperation as Op, PasswordReply as Reply, RpcFailure};
use erp_security::auth::{IdentityCheck, PasswordReset};
use zeroize::Zeroizing;

type Result<T> = std::result::Result<T, RpcFailure>;
fn invalid(message: &str) -> RpcFailure {
    Error::validation(message).into()
}
fn domain(error: Error) -> RpcFailure {
    if matches!(
        error
            .source()
            .and_then(|e| e.downcast_ref::<erp_orm::CommitError>()),
        Some(erp_orm::CommitError::Unknown(_))
    ) {
        return unknown();
    }
    error.into()
}
fn unknown() -> RpcFailure {
    erp_rpc::InfraError::CommitUnknown {
        entry: "password operation".into(),
        keyed: false,
    }
    .into()
}
async fn commit(tx: &mut Transaction) -> Result<()> {
    match tx.commit_pipelined(None).await {
        Ok(()) => Ok(()),
        Err(erp_orm::CommitError::Unknown(_)) => Err(unknown()),
        Err(other) => Err(other.into_error().into()),
    }
}
async fn recent(tx: &mut Transaction, id: &str) -> Result<bool> {
    let row=tx.query("SELECT identity_checked_at IS NOT NULL AND identity_checked_at <= clock_timestamp() AND identity_checked_at > clock_timestamp() - interval '10 minutes' FROM erp_session WHERE id = $1::text::uuid", &[&id]).await?;
    let row = row.first().ok_or_else(|| {
        Error::new(
            erp_fields::ErrorKind::SessionExpired,
            "user is not connected",
        )
    })?;
    row.try_get(0).map_err(|e| {
        Error::db("Invalid identity timestamp result.")
            .with_source(e)
            .into()
    })
}
fn one(ids: &[i64]) -> Result<i64> {
    match ids {
        [id] => Ok(*id),
        _ => Err(invalid("A password operation requires exactly one wizard.")),
    }
}
async fn owned(tx: &mut Transaction, model: &str, id: i64, actor: i64) -> Result<()> {
    let records = tx.env().records(model)?.browse([id])?;
    let rows = records.read(tx, &["create_uid"]).await?;
    let owner = rows.first().and_then(|r| r.get("create_uid"));
    // read() may name the many2one; use raw SQL below only after creator-rule read admission.
    if owner.is_none() {
        return Err(
            Error::access("Password wizard is unavailable or belongs to another user.").into(),
        );
    }
    let sql = match model {
        "change.password.own" => "SELECT create_uid::bigint FROM change_password_own WHERE id = $1",
        "change.password.wizard" => {
            "SELECT create_uid::bigint FROM change_password_wizard WHERE id = $1"
        }
        "change.password.user" => {
            "SELECT create_uid::bigint FROM change_password_user WHERE id = $1"
        }
        _ => return Err(Error::programming("Unknown password wizard kind.").into()),
    };
    let rows = tx.query(sql, &[&id]).await?;
    if rows.len() != 1
        || rows[0]
            .try_get::<_, i64>(0)
            .map_err(|e| Error::db("Invalid wizard owner.").with_source(e))?
            != actor
    {
        return Err(
            Error::access("Password wizard is unavailable or belongs to another user.").into(),
        );
    }
    Ok(())
}
async fn manager(auth: &Auth, tx: &mut Transaction, secret: &SessionSecret) -> Result<()> {
    if !auth
        .check_session_in(tx, secret)
        .await?
        .has_groups("base.group_erp_manager,base.group_system")?
    {
        return Err(
            Error::access("Password resets require access administration authority.").into(),
        );
    }
    Ok(())
}
fn changes(values: Vec<ValueMap>, lines: &[(i64, i64)]) -> Result<Vec<PasswordReset>> {
    if values.len() > 100 {
        return Err(invalid(
            "A password reset requires between 1 and 100 selected users.",
        ));
    }
    let mut result = Vec::with_capacity(values.len());
    let mut seen = std::collections::BTreeSet::new();
    for mut value in values {
        if value.len() != 2 {
            return Err(invalid(
                "Each password reset change requires line_id and new_passwd.",
            ));
        }
        let line = match value.shift_remove("line_id") {
            Some(Value::Int(id)) if id > 0 => id,
            _ => {
                return Err(invalid(
                    "Password reset line_id must be a positive integer.",
                ));
            }
        };
        if !seen.insert(line) {
            return Err(invalid("A password reset may name each line only once."));
        }
        let user = lines
            .iter()
            .find_map(|(id, uid)| (*id == line).then_some(*uid))
            .ok_or_else(|| Error::access("Password reset line does not belong to this wizard."))?;
        let password = match value.shift_remove("new_passwd") {
            Some(Value::Str(text)) => Zeroizing::new(text.to_string()),
            _ => return Err(invalid("Password reset new_passwd must be text.")),
        };
        result.push(PasswordReset::new(
            erp_orm::UserId::new(user).map_err(Error::from)?,
            &password,
        )?);
    }
    if result.is_empty() {
        return Err(invalid(
            "A password reset requires between 1 and 100 selected users.",
        ));
    }
    Ok(result)
}

// Only the request-local single-line string is owned here; it is wiped on every exit.
enum ResetInput {
    Many(Vec<ValueMap>),
    Line {
        id: i64,
        password: Zeroizing<String>,
    },
}

pub(super) async fn execute(
    credential: &BrowserCredential,
    orm: &Orm,
    mut call: PasswordCall<'_>,
) -> Result<Reply> {
    if !Arc::ptr_eq(orm.registry(), credential.auth.orm().registry()) {
        return Err(Error::programming(
            "RPC credential belongs to a different registry generation",
        )
        .into());
    }
    let auth = &credential.auth;
    let secret = &credential.secret;
    let begun = auth
        .begin_session(
            secret,
            CredentialOptions::new().context(call.context().clone()),
        )
        .await?;
    let actor = begun.session.user().get();
    let session_id = begun.session.id().to_text();
    let mut tx = begun.tx;
    let authority = auth.check_session_in(&mut tx, secret).await?;
    call.authorize(authority.principal())?;
    match call.operation() {
        Op::OpenOwn | Op::OpenReset => {
            let own = call.operation() == Op::OpenOwn || call.ids() == [actor];
            if own && call.ids() != [actor] {
                return Err(Error::access(
                    "Only the current user's own password can be changed here.",
                )
                .into());
            }
            if !own {
                manager(auth, &mut tx, secret).await?;
            }
            if !recent(&mut tx, &session_id).await? {
                tx.rollback().await?;
                return Ok(Reply::required());
            }
            if own {
                let wizard = tx
                    .env()
                    .records("change.password.own")?
                    .create(&mut tx, vec![RawVals::new()])
                    .await?;
                let reply = Reply::own(one(&wizard.ids())?)?;
                commit(&mut tx).await?;
                return Ok(reply);
            }
            if call.ids().is_empty() || call.ids().len() > 100 {
                return Err(invalid(
                    "A password reset requires between 1 and 100 selected users.",
                ));
            }
            let unique: std::collections::BTreeSet<_> = call.ids().iter().copied().collect();
            if unique.len() != call.ids().len() {
                return Err(invalid("A password reset may name each user only once."));
            }
            let selected = call.ids().to_vec();
            let users=tx.query("SELECT id::bigint, login FROM res_users WHERE id = ANY($1::int8[]) ORDER BY id", &[&selected]).await?;
            if users.len() != selected.len() {
                return Err(Error::new(
                    erp_fields::ErrorKind::Missing,
                    "A selected password reset user no longer exists.",
                )
                .into());
            }
            let wizard = tx
                .env()
                .records("change.password.wizard")?
                .create(&mut tx, vec![RawVals::new()])
                .await?;
            let id = one(&wizard.ids())?;
            let reply = Reply::reset(id)?;
            let mut vals = Vec::with_capacity(users.len());
            for user in &users {
                let uid: i64 = user
                    .try_get(0)
                    .map_err(|e| Error::db("Invalid selected user.").with_source(e))?;
                let login: String = user
                    .try_get(1)
                    .map_err(|e| Error::db("Invalid selected login.").with_source(e))?;
                vals.push(
                    RawVals::new()
                        .set_cell("wizard_id", erp_fields::Cell::Id(id))
                        .set_cell("user_id", erp_fields::Cell::Id(uid))
                        .set_cell("user_login", erp_fields::Cell::Str(login.into())),
                );
            }
            tx.env()
                .records("change.password.user")?
                .create(&mut tx, vals)
                .await?;
            commit(&mut tx).await?;
            Ok(reply)
        }
        Op::ConfirmIdentity => {
            let password = Zeroizing::new(call.take::<String>("password")?);
            tx.rollback().await?;
            drop(tx);
            auth.confirm_identity_authorized(
                secret,
                &password,
                erp_security::auth::ClientAddr::Known(credential.client),
                &|p| call.authorize(p),
            )
            .await
            .map_err(domain)?;
            Ok(Reply::closed())
        }
        Op::LegacyChange | Op::ChangeOwn => {
            let legacy = call.operation() == Op::LegacyChange;
            if !legacy {
                owned(&mut tx, "change.password.own", one(call.ids())?, actor).await?;
            }
            let (old, password) = if legacy {
                (
                    Some(Zeroizing::new(call.take::<String>("old_passwd")?)),
                    Zeroizing::new(call.take::<String>("new_passwd")?),
                )
            } else {
                let password = Zeroizing::new(call.take::<String>("new_password")?);
                let confirmation = Zeroizing::new(call.take::<String>("confirm_password")?);
                if *password != *confirmation {
                    return Err(invalid(
                        "The new password and its confirmation must be identical.",
                    ));
                }
                (None, password)
            };
            // Bound original bytes before trimming so whitespace cannot bypass admission.
            if password.len() > auth.config().credential_bytes().min(1024) {
                return Err(invalid(
                    "The new password exceeds the configured credential byte limit.",
                ));
            }
            tx.rollback().await?;
            drop(tx);
            if let Some(old) = old {
                auth.confirm_identity_authorized(
                    secret,
                    &old,
                    erp_security::auth::ClientAddr::Known(credential.client),
                    &|p| call.authorize(p),
                )
                .await
                .map_err(domain)?;
            }
            let checked = auth
                .change_password_authorized(Some(secret), password.trim(), &|p| call.authorize(p))
                .await
                .map_err(domain)?;
            Ok(match checked {
                IdentityCheck::Required => Reply::required(),
                IdentityCheck::Verified if legacy => Reply::changed(),
                IdentityCheck::Verified => Reply::closed(),
            })
        }
        Op::ResetWizard | Op::ResetLine => {
            manager(auth, &mut tx, secret).await?;
            let id = one(call.ids())?;
            let (wizard, input) = if call.operation() == Op::ResetLine {
                owned(&mut tx, "change.password.user", id, actor).await?;
                let row = tx
                    .query(
                        "SELECT wizard_id::bigint FROM change_password_user WHERE id = $1",
                        &[&id],
                    )
                    .await?;
                let wizard: i64 = row
                    .first()
                    .ok_or_else(|| invalid("Password reset line is unavailable."))?
                    .try_get(0)
                    .map_err(|e| Error::db("Invalid password wizard.").with_source(e))?;
                let password = Zeroizing::new(call.take::<String>("new_passwd")?);
                (wizard, ResetInput::Line { id, password })
            } else {
                (id, ResetInput::Many(call.take::<Vec<ValueMap>>("changes")?))
            };
            owned(&mut tx, "change.password.wizard", wizard, actor).await?;
            let rows=tx.query("SELECT id::bigint, user_id::bigint FROM change_password_user WHERE wizard_id = $1 ORDER BY id LIMIT 101", &[&wizard]).await?;
            if rows.len() > 100 {
                return Err(invalid(
                    "A password reset requires between 1 and 100 selected users.",
                ));
            }
            let lines = rows
                .iter()
                .map(|row| {
                    Ok((
                        row.try_get(0)
                            .map_err(|e| Error::db("Invalid password line.").with_source(e))?,
                        row.try_get(1)
                            .map_err(|e| Error::db("Invalid password target.").with_source(e))?,
                    ))
                })
                .collect::<std::result::Result<Vec<(i64, i64)>, Error>>()?;
            let changes = match input {
                ResetInput::Many(values) => changes(values, &lines)?,
                ResetInput::Line { id, password } => {
                    let user = lines
                        .iter()
                        .find_map(|(line, user)| (*line == id).then_some(*user))
                        .ok_or_else(|| {
                            Error::access("Password reset line does not belong to this wizard.")
                        })?;
                    vec![PasswordReset::new(
                        erp_orm::UserId::new(user).map_err(Error::from)?,
                        &password,
                    )?]
                }
            };
            tx.rollback().await?;
            drop(tx);
            let check = auth
                .reset_passwords_authorized(Some(secret), &changes, &|p| call.authorize(p))
                .await
                .map_err(domain)?;
            Ok(match check {
                IdentityCheck::Required => Reply::required(),
                IdentityCheck::Verified => Reply::closed(),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn commit_classification_survives_the_auth_domain_error_adapter() {
        let failure = domain(
            Error::db("Password operation commit outcome is unknown.").with_source(
                erp_orm::CommitError::Unknown(Error::db("test-only lost answer")),
            ),
        );
        assert!(matches!(
            failure,
            RpcFailure::Infra(erp_rpc::InfraError::CommitUnknown { keyed: false, .. })
        ));
        let failure = domain(Error::validation("test-only definite rollback"));
        match failure {
            RpcFailure::Domain(error) => assert_eq!(
                (error.kind(), error.message()),
                (
                    erp_fields::ErrorKind::Validation,
                    "test-only definite rollback"
                )
            ),
            other => panic!("wrong rollback classification: {other:?}"),
        }
    }
}
