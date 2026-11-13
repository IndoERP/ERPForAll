//! `erp auth emergency-revoke`: end API keys or sessions on
//! the owner connection, with the revocation's own audit row in the same transaction.
//!
//! The command refuses before any deletion when the audit store cannot take the row on the owner
//! connection, and [`erp_security::auth::emergency::revoke`] refuses when the connection does not
//! own the credential tables. It prints ids and counts only, never a key secret, prefix or name.

use erp_fields::{Error, ErrorKind};
use erp_orm::audit::EmergencyReason;
use erp_orm::{
    ApiKeyId, BeginOptions, CommitError, Isolation, OperationIdentity, Orm, SUPERUSER_ID,
    Transaction, UserId,
};
use erp_security::auth::SessionId;
use erp_security::auth::emergency::{self, Revoked, Target, revoke};

use crate::access::pool_with_size;
use crate::args::EmergencyRevokeArgs;
use crate::error::{CliError, Result};
use crate::human::{config, runtime};

fn invalid(arg: &str, reason: &str) -> CliError {
    CliError::InvalidInput {
        arg: arg.to_owned(),
        reason: reason.to_owned(),
    }
}

/// What the arguments ask for, validated.
enum Request {
    Keys(Vec<ApiKeyId>),
    Sessions(Vec<SessionId>),
    User {
        user: UserId,
        keys: bool,
        sessions: bool,
    },
}

fn request(args: &EmergencyRevokeArgs) -> Result<Request> {
    // This handler also accepts programmatically assembled arguments; clap is
    // an additional boundary, not the authority for selecting a revocation target.
    if (args.keys || args.sessions)
        && (args.user.is_none() || !args.key.is_empty() || !args.session.is_empty())
    {
        return Err(invalid(
            "--keys/--sessions",
            "scope flags require --user and cannot accompany --key or --session",
        ));
    }
    let targets = usize::from(!args.key.is_empty())
        + usize::from(!args.session.is_empty())
        + usize::from(args.user.is_some());
    if targets != 1 {
        return Err(invalid(
            "target",
            "name exactly one of --key, --session or --user",
        ));
    }
    if args.user.is_some() && !args.keys && !args.sessions {
        return Err(invalid("--user", "choose --keys, --sessions or both"));
    }
    if !args.key.is_empty() {
        let ids = args
            .key
            .iter()
            .map(|id| ApiKeyId::new(*id).map_err(|_| invalid("--key", "not a key id")))
            .collect::<Result<Vec<_>>>()?;
        return Ok(Request::Keys(ids));
    }
    if !args.session.is_empty() {
        let ids = args
            .session
            .iter()
            .map(|id| SessionId::parse(id).map_err(|_| invalid("--session", "not a session UUID")))
            .collect::<Result<Vec<_>>>()?;
        return Ok(Request::Sessions(ids));
    }
    match args.user {
        Some(user) => Ok(Request::User {
            user: UserId::new(user).map_err(|_| invalid("--user", "not a user id"))?,
            keys: args.keys,
            sessions: args.sessions,
        }),
        None => Err(invalid(
            "target",
            "name --key, --session or --user (the argument parser requires one)",
        )),
    }
}

/// `erp auth emergency-revoke`: run it and return what to print.
pub(crate) fn emergency_revoke(args: &EmergencyRevokeArgs) -> Result<String> {
    runtime()?.block_on(emergency_revoke_async(args))
}

async fn emergency_revoke_async(args: &EmergencyRevokeArgs) -> Result<String> {
    let request = request(args)?;
    let reason = args.reason.reason();
    let config = config(&args.config)?;
    let orm = Orm::builder(
        crate::access::registry()?,
        pool_with_size(config.owner_connection(), 1)?,
    )
    .require_core_tables(erp_security::auth::core_tables())
    .require_core_tables(erp_addon_base::partner_hierarchy::core_tables())
    .build()?;
    // Before any deletion: the owner connection must be able to write the row (a store that is
    // not installed, has another shape or has no partition day left blocks the owner too).
    let state = orm.check_audit_store().await?;
    emergency::require_audit_store(&state)?;
    let unlinked = preflight(&orm).await?;
    let mut report = Report::new(config.database().to_string(), reason);
    match request {
        Request::Keys(ids) => {
            let revoked = run_chunk(&orm, &Target::keys(ids)?, reason, &report).await?;
            report.add(&revoked);
        }
        Request::Sessions(ids) => {
            let revoked = run_chunk(&orm, &Target::sessions(ids)?, reason, &report).await?;
            report.add(&revoked);
        }
        Request::User {
            user,
            keys,
            sessions,
        } => {
            let mut first = true;
            loop {
                // the sessions end with the first chunk; later chunks only continue the keys
                let target = Target::user(user, keys, sessions && first)?;
                let revoked = run_chunk(&orm, &target, reason, &report).await?;
                report.add(&revoked);
                first = false;
                if !revoked.key_chunk_was_full() {
                    break;
                }
            }
        }
    }
    Ok(report.finish(&unlinked))
}

/// One transaction: the deletions and their audit row, committed together or not at all.
async fn run_chunk(
    orm: &Orm,
    target: &Target,
    reason: EmergencyReason,
    report: &Report,
) -> Result<Revoked> {
    let options = BeginOptions::new(SUPERUSER_ID)
        .isolation(Isolation::ReadCommitted)
        .on_primary()
        .operation_identity(OperationIdentity::generate()?);
    let mut tx = orm
        .begin_opts(options)
        .await
        .map_err(|e| report.after_commits(e))?;
    let revoked = match revoke(&mut tx, target, reason).await {
        Ok(revoked) => revoked,
        Err(error) => {
            let error = with_rollback(error, tx.rollback().await);
            return Err(report.after_commits(error).into());
        }
    };
    match tx.commit_pipelined(None).await {
        Ok(()) => Ok(revoked),
        // nothing was committed, for certain (this includes the audit row's own refusal)
        Err(CommitError::RolledBack(error)) => Err(report.after_commits(error).into()),
        // `CommitError` is non-exhaustive: any other outcome is treated as unknown, the safe side
        Err(other) => Err(report
            .after_commits(unknown_outcome(other.into_error()))
            .into()),
    }
}

/// The error of a failed run, with the rollback's own error added when the rollback failed too.
/// The run's error stays the primary one (its kind is kept).
fn with_rollback(error: Error, rollback: erp_fields::Result<()>) -> Error {
    match rollback {
        Ok(()) => error,
        Err(rollback) => Error::new(
            error.kind(),
            format!(
                "{} (and the rollback failed: {})",
                error.message(),
                rollback.message()
            ),
        )
        .with_source(error),
    }
}

/// `COMMIT` was sent and no answer arrived: it may or may not have been applied.
fn unknown_outcome(error: Error) -> Error {
    Error::new(
        error.kind(),
        format!(
            "{}; the commit outcome is unknown: check the keys and sessions before running the \
             command again",
            error.message()
        ),
    )
}

/// Before any revocation: the connection must own the credential tables (the revoke call checks
/// again inside its own transaction), and the installed addons this binary's registry does not
/// link are listed, because their session-end hooks cannot run from this command (decision D6).
async fn preflight(orm: &Orm) -> Result<Vec<String>> {
    let mut tx = orm
        .begin_opts(
            BeginOptions::new(SUPERUSER_ID)
                .isolation(Isolation::ReadCommitted)
                .on_primary(),
        )
        .await?;
    let probed = async {
        emergency::require_owner(&mut tx).await?;
        installed_addons(&mut tx).await
    }
    .await;
    let probed = match probed {
        Ok(names) => names,
        Err(error) => return Err(with_rollback(error, tx.rollback().await).into()),
    };
    tx.rollback().await?;
    let mut linked: Vec<&str> = Vec::new();
    for addon in orm.registry().addons() {
        linked.push(addon.name);
    }
    Ok(probed
        .into_iter()
        .filter(|name| !linked.contains(&name.as_str()))
        .collect())
}

async fn installed_addons(tx: &mut Transaction) -> erp_fields::Result<Vec<String>> {
    let present = tx
        .query(
            "SELECT to_regclass('public.ir_module_module') IS NOT NULL",
            &[],
        )
        .await?;
    let present: bool = present
        .first()
        .ok_or_else(|| failed_error("the module table probe returned no row"))?
        .try_get(0)
        .map_err(|e| failed_error(&format!("the module table probe: {e}")))?;
    if !present {
        return Ok(Vec::new());
    }
    let rows = tx
        .query(
            "SELECT name FROM ir_module_module WHERE state = 'installed' ORDER BY name",
            &[],
        )
        .await?;
    rows.iter()
        .map(|row| {
            row.try_get::<_, String>(0)
                .map_err(|e| failed_error(&format!("ir_module_module.name: {e}")))
        })
        .collect()
}

fn failed_error(message: &str) -> Error {
    Error::new(ErrorKind::Internal, format!("a later release: {message}"))
}

/// What the run did so far; printed at the end, and quoted in the error of a later chunk.
struct Report {
    database: String,
    reason: EmergencyReason,
    chunks: usize,
    keys: Vec<ApiKeyId>,
    sessions: Vec<SessionId>,
}

impl Report {
    fn new(database: String, reason: EmergencyReason) -> Self {
        Self {
            database,
            reason,
            chunks: 0,
            keys: Vec::new(),
            sessions: Vec::new(),
        }
    }

    fn add(&mut self, revoked: &Revoked) {
        if !revoked.keys().is_empty() || !revoked.sessions().is_empty() {
            self.chunks += 1;
        }
        self.keys.extend_from_slice(revoked.keys());
        self.sessions.extend_from_slice(revoked.sessions());
    }

    /// The error of a chunk, saying how much earlier chunks already committed.
    fn after_commits(&self, error: Error) -> Error {
        if self.chunks == 0 {
            return error;
        }
        Error::new(
            error.kind(),
            format!(
                "{}; {} earlier chunk(s) were already committed and audited: {} key(s), \
                 {} session(s)",
                error.message(),
                self.chunks,
                self.keys.len(),
                self.sessions.len()
            ),
        )
    }

    fn finish(&self, unlinked: &[String]) -> String {
        let mut lines = vec![format!(
            "Emergency revocation for database {} (reason: {}).",
            self.database,
            self.reason.as_str()
        )];
        if self.keys.is_empty() && self.sessions.is_empty() {
            lines.push("0 revoked: the target had no API keys or sessions to end.".to_owned());
            return lines.join("\n");
        }
        lines.push(format!(
            "Revoked {} API key(s) and {} session(s) in {} transaction(s), each with its own \
             audit row (origin cli).",
            self.keys.len(),
            self.sessions.len(),
            self.chunks
        ));
        if !self.keys.is_empty() && self.keys.len() <= LIST_LIMIT {
            let ids: Vec<String> = self.keys.iter().map(|k| k.get().to_string()).collect();
            lines.push(format!("API key ids: {}", ids.join(", ")));
        }
        if !self.sessions.is_empty() && self.sessions.len() <= LIST_LIMIT {
            let ids: Vec<String> = self.sessions.iter().map(SessionId::to_text).collect();
            lines.push(format!("Session ids: {}", ids.join(", ")));
        }
        if !unlinked.is_empty() {
            lines.push(format!(
                "Note: only the session-end hooks linked into this command ran. Installed addons \
                 not linked here: {}.",
                unlinked.join(", ")
            ));
        }
        lines.join("\n")
    }
}

/// Most ids the report lists; a larger run prints counts only.
const LIST_LIMIT: usize = 500;

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, reason = "test assertions")]
mod tests {
    use super::*;
    use crate::args::EmergencyReasonArg;

    fn args() -> EmergencyRevokeArgs {
        EmergencyRevokeArgs {
            config: "/nonexistent/invalid-target-must-not-read-config".into(),
            reason: EmergencyReasonArg::Lost,
            key: Vec::new(),
            session: Vec::new(),
            user: None,
            keys: false,
            sessions: false,
        }
    }

    fn refused(args: &EmergencyRevokeArgs, arg: &str, reason: &str) {
        let error = match request(args) {
            Err(error) => error,
            Ok(_) => panic!("invalid target accepted"),
        };
        assert!(
            matches!(&error, CliError::InvalidInput { arg: actual, reason: why }
            if actual == arg && why == reason)
        );
        assert_eq!(
            error.to_string(),
            format!("Invalid argument '{arg}': {reason}")
        );
    }

    #[test]
    fn request_refuses_scope_flags_with_individual_targets() {
        for key_target in [true, false] {
            for keys_flag in [true, false] {
                let mut input = args();
                if key_target {
                    input.key.push(1);
                } else {
                    input
                        .session
                        .push("01234567-89ab-7def-8123-456789abcdef".to_owned());
                }
                input.keys = keys_flag;
                input.sessions = !keys_flag;
                refused(
                    &input,
                    "--keys/--sessions",
                    "scope flags require --user and cannot accompany --key or --session",
                );
            }
        }
    }

    #[test]
    fn request_refuses_missing_or_multiple_targets_and_unscoped_users() {
        let mut input = args();
        let reason = "name exactly one of --key, --session or --user";
        refused(&input, "target", reason);
        input.keys = true;
        refused(
            &input,
            "--keys/--sessions",
            "scope flags require --user and cannot accompany --key or --session",
        );
        input.keys = false;
        input.user = Some(7);
        refused(&input, "--user", "choose --keys, --sessions or both");
        input.key.push(1);
        refused(&input, "target", reason);
        input.user = None;
        input
            .session
            .push("01234567-89ab-7def-8123-456789abcdef".to_owned());
        refused(&input, "target", reason);
        input.key.clear();
        input.user = Some(7);
        refused(&input, "target", reason);
    }

    #[test]
    fn request_accepts_valid_target_groups() {
        let mut input = args();
        input.key.push(5);
        match request(&input).unwrap() {
            Request::Keys(ids) => assert_eq!(ids[0].get(), 5),
            _ => panic!("expected keys"),
        }
        input.key.clear();
        input
            .session
            .push("01234567-89ab-7def-8123-456789abcdef".to_owned());
        match request(&input).unwrap() {
            Request::Sessions(ids) => assert_eq!(ids[0].to_text(), input.session[0]),
            _ => panic!("expected sessions"),
        }
        input.session.clear();
        input.user = Some(7);
        for (keys, sessions) in [(true, false), (false, true), (true, true)] {
            input.keys = keys;
            input.sessions = sessions;
            match request(&input).unwrap() {
                Request::User {
                    user,
                    keys: actual_keys,
                    sessions: actual_sessions,
                } => {
                    assert_eq!(user.get(), 7);
                    assert_eq!((actual_keys, actual_sessions), (keys, sessions));
                }
                _ => panic!("expected user"),
            }
        }
    }
}
