//! Row encoding, the single INSERT, and the `Transaction` and `Orm` entry points of the audit
//! trail (design `m3-audit` §5.3, §6, §7).

use std::sync::Arc;

use erp_fields::{Error, Result};
use erp_registry::Registry;
use postgres_types::ToSql;
use serde_json::Value;

use super::buffer::{AttemptEvidence, Attribution, AuditBuffer, ElevationEntry};
use super::elevation::{
    ElevationGrant, ElevationObservation, ElevationReason, FullCause, SuperuserAction, grant_json,
};
use super::{
    AuditCall, AuditError, AuditOrigin, AuditOutcome, AuthEvent, ErrorClass, InsertionCause,
    MAX_COMPANIES, MAX_ROW_BYTES, SCHEMA_VERSION, limit,
};
use crate::{Env, OperationId, OperationIdentity, Orm, Transaction, TxIntent};

/// [`Transaction::audit_attachment_claim`] from a session whose role does not own the claim table.
const CLAIM_UNREADABLE: &str =
    "Attachment claim audit requires the connection that owns ir_blob_owner_claim.";
/// [`Transaction::audit_attachment_claim`] for an id that has no claim row.
const CLAIM_UNKNOWN: &str = "Attachment claim audit names no recorded claim.";

/// The single boolean of a precondition query of the claim audit.
fn claim_flag(rows: &[tokio_postgres::Row]) -> Result<bool> {
    rows.first()
        .ok_or_else(|| Error::db("Attachment claim audit precondition returned no row"))?
        .try_get(0)
        .map_err(|error| {
            Error::db("Attachment claim audit precondition returned an invalid value")
                .with_source(error)
        })
}

/// The supplied columns, in the order of the parameters. The generated four (`id`, `entry_id`,
/// `create_date`, `xid`) are database defaults the runtime role cannot set.
pub(crate) const INSERT: &str = "INSERT INTO \"erp_event\".\"ir_audit_entry\" \
(schema_version, entry_kind, origin, outcome, error_class, attempt, operation_id, \
 correlation_id, causation_id, original_operation_id, principal_kind, subject_id, actor_kind, \
 actor_id, delegating_user_id, company_ids, registry_generation, policy_generation, model, \
 method, effect, target_ids, args_digest, redaction_version, events, encoded_bytes) \
VALUES ($1, $2, $3, $4, $5, $6, $7::text::uuid, $8::text::uuid, $9::text::uuid, $10::text::uuid, \
 $11, $12, $13, $14, $15, $16, $17, $18, $19, $20, $21, $22::text::jsonb, $23, $24, \
 $25::text::jsonb, $26)";

/// One validated row, every parameter as it is sent.
struct Row {
    entry_kind: &'static str,
    origin: &'static str,
    outcome: &'static str,
    error_class: Option<&'static str>,
    attempt: i16,
    operation_id: String,
    correlation_id: String,
    original_operation_id: Option<String>,
    principal_kind: Option<&'static str>,
    subject_id: Option<i64>,
    actor_kind: Option<&'static str>,
    actor_id: Option<i64>,
    delegating_user_id: Option<i64>,
    company_ids: Vec<i64>,
    registry_generation: Option<i64>,
    policy_generation: Option<i64>,
    model: Option<String>,
    method: Option<String>,
    effect: Option<&'static str>,
    target_ids: String,
    args_digest: Option<Vec<u8>>,
    redaction_version: Option<i16>,
    events: String,
    encoded_bytes: i32,
}

/// What a row is built from.
struct RowInput<'a> {
    kind: &'static str,
    origin: AuditOrigin,
    outcome: AuditOutcome,
    error_class: Option<ErrorClass>,
    attempt: u16,
    identity: &'a OperationIdentity,
    original_operation: Option<&'a OperationId>,
    attribution: Option<&'a Attribution>,
    companies: Vec<i64>,
    generations: Option<(u64, u64)>,
    call: Option<&'a AuditCall>,
    auth: &'a [(usize, AuthEvent)],
    elevations: &'a [ElevationEntry],
    extra_events: Vec<Value>,
    registry: &'a Registry,
}

fn generation(value: u64, what: &str) -> Result<i64> {
    i64::try_from(value).map_err(|_| {
        AuditError::Invalid(format!("the {what} generation {value} does not fit bigint")).into()
    })
}

impl Row {
    fn build(input: RowInput<'_>) -> Result<Self> {
        if input.companies.len() > MAX_COMPANIES {
            return Err(limit(format!("more than {MAX_COMPANIES} companies in one row")).into());
        }
        for (i, id) in input.companies.iter().enumerate() {
            if *id <= 0 || input.companies[..i].contains(id) {
                return Err(AuditError::Invalid(
                    "audit companies must be positive and unique".to_owned(),
                )
                .into());
            }
        }
        let attempt = i16::try_from(input.attempt).map_err(|_| {
            AuditError::Invalid(format!(
                "attempt number {} does not fit smallint",
                input.attempt
            ))
        })?;
        let mut events: Vec<Value> = input.auth.iter().map(|(_, e)| e.to_json()).collect();
        for entry in input.elevations {
            events.push(elevation_json(entry, input.registry));
        }
        events.extend(input.extra_events);
        let events = serde_json::to_string(&Value::Array(events)).map_err(|e| {
            Error::new(
                erp_fields::ErrorKind::Internal,
                "audit: event encoding failed",
            )
            .with_source(e)
        })?;
        let (registry_generation, policy_generation) = match input.generations {
            Some((r, p)) => (
                Some(generation(r, "registry")?),
                Some(generation(p, "policy")?),
            ),
            None => (None, None),
        };
        let a = input.attribution;
        let mut row = Self {
            entry_kind: input.kind,
            origin: input.origin.as_str(),
            outcome: input.outcome.as_str(),
            error_class: input.error_class.map(ErrorClass::as_str),
            attempt,
            operation_id: input.identity.operation().as_str().to_owned(),
            correlation_id: input.identity.correlation().as_str().to_owned(),
            original_operation_id: input.original_operation.map(|o| o.as_str().to_owned()),
            principal_kind: a.map(|a| a.principal_kind),
            subject_id: a.map(|a| a.subject),
            actor_kind: a.map(|a| a.actor_kind),
            actor_id: a.map(|a| a.actor_id),
            delegating_user_id: a.and_then(|a| a.delegating_user_id),
            company_ids: input.companies,
            registry_generation,
            policy_generation,
            model: input
                .call
                .and_then(|c| c.model.as_deref().map(str::to_owned)),
            method: input.call.map(|c| c.method.to_string()),
            effect: input.call.map(|c| c.effect.as_str()),
            target_ids: input
                .call
                .map_or_else(|| "[]".to_owned(), |c| c.targets.to_json()),
            args_digest: input
                .call
                .and_then(|c| c.args.as_ref())
                .map(|d| d.sha256.to_vec()),
            redaction_version: match input.call.and_then(|c| c.args.as_ref()) {
                Some(d) => Some(i16::try_from(d.redaction_version).map_err(|_| {
                    AuditError::Invalid("redaction version does not fit smallint".to_owned())
                })?),
                None => None,
            },
            events,
            encoded_bytes: 0,
        };
        let bytes = row.measure()?;
        row.encoded_bytes = i32::try_from(bytes)
            .map_err(|_| limit("the encoded row does not fit integer bytes"))?;
        Ok(row)
    }

    /// The checked byte length of the parameters: a conservative estimate (see `measure_with`).
    fn measure(&self) -> Result<usize> {
        self.measure_with(MAX_ROW_BYTES)
    }

    fn measure_with(&self, max: usize) -> Result<usize> {
        let mut total: usize = 0;
        let mut add = |n: usize| -> Result<()> {
            total = total
                .checked_add(n)
                .filter(|t| *t <= max)
                .ok_or_else(|| limit(format!("the encoded row exceeds {max} bytes")))?;
            Ok(())
        };
        for text in [self.entry_kind, self.origin, self.outcome] {
            add(text.len())?;
        }
        for text in [
            self.error_class,
            self.principal_kind,
            self.actor_kind,
            self.effect,
        ]
        .into_iter()
        .flatten()
        {
            add(text.len())?;
        }
        add(self.operation_id.len())?;
        add(self.correlation_id.len())?;
        add(self.original_operation_id.as_ref().map_or(0, String::len))?;
        add(self.model.as_ref().map_or(0, String::len))?;
        add(self.method.as_ref().map_or(0, String::len))?;
        add(self.target_ids.len())?;
        add(self.args_digest.as_ref().map_or(0, Vec::len))?;
        add(self.events.len())?;
        // fixed-width columns, rounded up: 4 + 8 * 7 counts two smallints and seven bigints; the
        // row really sends three smallints, one integer and five bigints (50 bytes), so the
        // estimate is never below the real width. The company array follows.
        add(4 + 8 * 7)?;
        add(self
            .company_ids
            .len()
            .checked_mul(8)
            .ok_or_else(|| limit("company bytes"))?)?;
        Ok(total)
    }
}

fn elevation_json(entry: &ElevationEntry, registry: &Registry) -> Value {
    let mut object = serde_json::json!({
        "type": "elevation",
        "reason": entry.reason.as_str(),
        "grant": grant_json(&entry.key, entry.block_seq, registry),
        "original": {
            "principal_kind": entry.original.principal_kind,
            "subject": entry.original.subject,
            "actor_kind": entry.original.actor_kind,
            "actor_id": entry.original.actor_id,
            "delegating_user_id": entry.original.delegating_user_id,
        },
        "effective_subject": entry.effective_subject,
        "count": entry.count,
        "state": entry.state(),
    });
    if entry.rolled_back > 0
        && let Some(map) = object.as_object_mut()
    {
        map.insert("rolled_back".to_owned(), Value::from(entry.rolled_back));
    }
    object
}

/// The INSERT, exactly one row.
async fn insert(tx: &mut Transaction, row: &Row) -> Result<()> {
    let params: [&(dyn ToSql + Sync); 26] = [
        &SCHEMA_VERSION,
        &row.entry_kind,
        &row.origin,
        &row.outcome,
        &row.error_class,
        &row.attempt,
        &row.operation_id,
        &row.correlation_id,
        &None::<&str>,
        &row.original_operation_id,
        &row.principal_kind,
        &row.subject_id,
        &row.actor_kind,
        &row.actor_id,
        &row.delegating_user_id,
        &row.company_ids,
        &row.registry_generation,
        &row.policy_generation,
        &row.model,
        &row.method,
        &row.effect,
        &row.target_ids,
        &row.args_digest,
        &row.redaction_version,
        &row.events,
        &row.encoded_bytes,
    ];
    match tx.execute(INSERT, &params).await {
        Ok(1) => Ok(()),
        Ok(n) => Err(AuditError::Insertion {
            what: format!("the audit INSERT wrote {n} rows, expected 1"),
            cause: None,
        }
        .into()),
        Err(error) => Err(insertion_error(error)),
    }
}

/// An INSERT failure. A serialization failure or deadlock (`40001`, `40P01`) is returned as it is,
/// so the caller's retry rule still sees its kind and SQLSTATE. Anything else becomes
/// [`AuditError::Insertion`] named by its SQLSTATE and never by row content, with the database
/// error kept as the source. The "no partition" hint is given only for the partition-routing
/// error itself: a breach of the table's own shape or subject CHECK is `23514` too and points at
/// an encoder defect instead.
pub(crate) fn insertion_error(error: Error) -> Error {
    let state = error.sqlstate().unwrap_or("none").to_owned();
    if matches!(state.as_str(), "40001" | "40P01") {
        return error;
    }
    let hint = match state.as_str() {
        "23514" if crate::event_schema::is_partition_miss(&error, super::AUDIT_TABLE.table) => {
            "; no partition of erp_event.ir_audit_entry covers the transaction's day; the event \
             partition upkeep has not run (it runs at server start and hourly; `erp audit \
             maintain` refreshes both event tables)"
        }
        "23514" => "; a check constraint of the audit table refused the row",
        "42501" => "; the runtime role lacks the INSERT right, republish the runtime grants",
        _ => "",
    };
    AuditError::Insertion {
        what: format!("the audit INSERT failed (SQLSTATE {state}){hint}"),
        cause: Some(InsertionCause(Arc::new(error))),
    }
    .into()
}

/// A `Full` elevation from an environment of a scoped elevation block is a widening (
/// D2): refused before any evidence is recorded, so the trail never shows an elevation that the
/// returned environment would refuse to use.
fn refuse_in_block(env: &Env) -> Result<()> {
    match env.block() {
        Some(life) => Err(life.widened_error()),
        None => Ok(()),
    }
}

impl Transaction {
    fn audit_ready(&self) -> Result<()> {
        let state = self.shared.audit_store().state();
        if state.allows_writes() {
            Ok(())
        } else {
            Err(AuditError::Unavailable(state.describe()).into())
        }
    }

    /// The state of the audit store as this transaction's ORM last checked it
    /// ([`Orm::check_audit_store`]); [`Unchecked`](super::AuditStoreState::Unchecked) until a
    /// check ran. A caller that must refuse before any statement of its own reads this, because
    /// [`audit_auth`](Self::audit_auth) only refuses at the first event it records.
    pub fn audit_store_state(&self) -> super::AuditStoreState {
        self.shared.audit_store().state()
    }

    /// Admission of an elevation: a write transaction passes (its audit row is written at commit),
    /// a read-only transaction passes only with a declared read-audit owner
    /// ([`ReadAuditOwner`](crate::ReadAuditOwner)), which writes the attempt evidence as a
    /// `read_ok` row before releasing a result (A2).
    ///
    /// # Errors
    ///
    /// `Programming` for a read-only transaction without an owner.
    pub(crate) fn audited_read_admission(&self) -> Result<()> {
        match (self.intent(), self.read_audit_owner()) {
            (TxIntent::Write, _) | (TxIntent::Read { .. }, Some(_)) => Ok(()),
            (TxIntent::Read { .. }, None) => Err(Error::programming(
                "scoped elevation requires an audited transaction: this read-only transaction has \
                 no read-audit owner",
            )),
        }
    }

    fn audit_writable(&self) -> Result<()> {
        match self.intent() {
            TxIntent::Write => Ok(()),
            _ => Err(AuditError::ReadOnlyTransaction.into()),
        }
    }

    /// Record a typed auth lifecycle change; it is written in this transaction's audit row at
    /// commit, with the attribution of this transaction's environment, and dropped if a savepoint
    /// rolls it back. Nothing but ids, enums and counts can be recorded.
    ///
    /// # Errors
    ///
    /// Before any statement: `Internal` ([`AuditError::Unavailable`]) when the audit store is not
    /// ready, `Programming` ([`AuditError::ReadOnlyTransaction`]) in a read-only transaction,
    /// ([`AuditError::Frozen`]) after the commit preamble froze the buffer, and
    /// `ResourceExhausted` ([`AuditError::CaptureLimit`]) beyond 512 events.
    pub fn audit_auth(&mut self, event: AuthEvent) -> Result<()> {
        self.audit_ready()?;
        self.audit_writable()?;
        let env = self.env();
        let depth = self.savepoint_depth();
        self.audit.push_auth(depth, event, &env)
    }

    /// Set the call summary of this transaction attempt (the RPC layer calls it once per attempt).
    /// Allowed in a read-only transaction too: the summary then only travels with the evidence of
    /// an attempt row, never into an INSERT of this transaction.
    ///
    /// # Errors
    ///
    /// Before any statement: [`AuditError::Unavailable`] when the store is not ready,
    /// [`AuditError::Frozen`] after the commit preamble froze the buffer;
    /// `Programming` ([`AuditError::HeaderTwice`]) for a second summary.
    pub fn audit_call(&mut self, call: AuditCall) -> Result<()> {
        self.audit_ready()?;
        let env = self.env();
        self.audit.set_call(call, &env)
    }

    /// Record this transaction as one phase of an attachment ownership recovery claim:
    /// origin CLI, method `attachments.claim.<phase>`, the claim row as the only target. Addons
    /// get this fixed summary instead of the general call and origin writers.
    ///
    /// Only the owner connection that drives the recovery may call it. The phase is validated
    /// first (no statement), then, before the origin is set: the session's role must own
    /// `ir_blob_owner_claim` (checked through the catalog, so a request-role transaction is refused
    /// without a raw permission error aborting it, even when some role was granted SELECT on it),
    /// and the claim row must exist. The call summary is set before the origin, so a refused call
    /// (including a second summary) leaves the transaction's audit origin unchanged.
    ///
    /// # Errors
    ///
    /// `Programming` for a phase other than `decision`, `owner`, `resolution` or `completed`;
    /// `Access` when the claim table is missing or not owned by this session's role; `Programming`
    /// when no claim row has the id; database errors; as [`audit_call`](Self::audit_call) and
    /// [`audit_origin`](Self::audit_origin).
    #[doc(hidden)]
    pub async fn audit_attachment_claim(&mut self, phase: &str, claim: i64) -> Result<()> {
        let method = match phase {
            "decision" => "attachments.claim.decision",
            "owner" => "attachments.claim.owner",
            "resolution" => "attachments.claim.resolution",
            "completed" => "attachments.claim.completed",
            _ => {
                return Err(Error::programming(format!(
                    "Unknown attachment claim phase {phase:?}"
                )));
            }
        };
        let readable = self
            .query(
                "SELECT COALESCE(bool_or(pg_get_userbyid(p.relowner) = current_user), false) \
                 FROM pg_class p WHERE p.oid = to_regclass('public.ir_blob_owner_claim')",
                &[],
            )
            .await?;
        if !claim_flag(&readable)? {
            return Err(Error::access(CLAIM_UNREADABLE));
        }
        let known = self
            .query(
                "SELECT EXISTS(SELECT 1 FROM public.ir_blob_owner_claim WHERE id = $1)",
                &[&claim],
            )
            .await?;
        if !claim_flag(&known)? {
            return Err(Error::programming(CLAIM_UNKNOWN));
        }
        // The call summary first: a second summary fails before the origin is relabelled.
        self.audit_call(AuditCall::new(
            None,
            method.into(),
            crate::rpc::Effect::Write,
            super::TargetIds::from_ids(&[claim])?,
            None,
        )?)?;
        self.audit_origin(AuditOrigin::Cli)
    }

    /// Set the origin of the row of a non-RPC transaction (default [`AuditOrigin::Internal`]).
    ///
    /// # Errors
    ///
    /// [`AuditError::Frozen`] after the commit preamble froze the buffer.
    pub fn audit_origin(&mut self, origin: AuditOrigin) -> Result<()> {
        self.audit.set_origin(origin)
    }

    /// Move this attempt's audit content out before the rollback, for an independent attempt row
    /// ([`AuditAttempt`]). Auth events are dropped: they describe a change that did not happen.
    pub fn take_audit_attempt(&mut self) -> AttemptEvidence {
        self.audit.take_attempt()
    }

    /// The environment a flagged server action runs: records the
    /// elevation, then returns `env` with superuser mode (an ordinary `Full` environment,
    /// D4). The observation is buffered here and written with the transaction's audit row.
    ///
    /// # Errors
    ///
    /// As [`audit_auth`](Self::audit_auth); `Programming` ([`AuditError::Elevation`]) when the
    /// action names no group; `Programming` for an environment of a scoped elevation block,
    /// which never reaches superuser mode, before anything is recorded; the
    /// admission refusals of `env` (a `with_user` switch its switcher may not make, A4),
    /// before anything is recorded.
    pub fn audited_superuser_env(&mut self, env: &Env, action: &SuperuserAction) -> Result<Env> {
        refuse_in_block(env)?;
        // the admission check of every engine entry, before anything is recorded (A4)
        self.check_env_block(env)?;
        self.audit_ready()?;
        self.audit_writable()?;
        if action.group_ids.is_empty() {
            return Err(AuditError::Elevation(
                "a superuser run needs at least one group".to_owned(),
            )
            .into());
        }
        let reason = ElevationReason::new("base.server_action.run_as_superuser")?;
        self.record_elevation_in(
            env,
            reason,
            ElevationGrant::Full {
                cause: FullCause::ServerAction(action.clone()),
            },
        )?;
        Ok(env.sudo(true))
    }

    /// Record a use of `sudo` outside a server action (the unscoped escape hatch of the SDK),
    /// once per call (decision of the lead on, design question 4). In a read-only
    /// transaction it is admitted only when the opener declared a read-audit owner
    /// ([`ReadAuditOwner`](crate::ReadAuditOwner)): the evidence then reaches the attempt row
    /// (`read_ok`), not a change row.
    ///
    /// # Errors
    ///
    /// As [`audit_auth`](Self::audit_auth), except that a read-only transaction with an owner is
    /// admitted and one without gives `Programming` ("no read-audit owner"); `Programming` for an environment of a scoped
    /// elevation block, which never reaches superuser mode, before anything is
    /// recorded; the admission refusals of `env` (a `with_user` switch its switcher may not
    /// make, A4), before anything is recorded.
    pub fn audited_unscoped_sudo(&mut self, env: &Env, addon: &'static str) -> Result<Env> {
        refuse_in_block(env)?;
        // the admission check of every engine entry, before anything is recorded (A4)
        self.check_env_block(env)?;
        // the programming refusal (no owner) does not depend on the store's state
        self.audited_read_admission()?;
        self.audit_ready()?;
        let reason = ElevationReason::new("core.sudo.unscoped")?;
        self.record_elevation_in(
            env,
            reason,
            ElevationGrant::Full {
                cause: FullCause::UnscopedSudo { addon },
            },
        )?;
        Ok(env.sudo(true))
    }

    /// Record the opening of scoped elevation block `block` of `scope` from `env` (the base
    /// environment of the block: its principal without elevation), before the block's body runs
    /// (D4). `Transaction::elevated` calls it after admission.
    ///
    /// # Errors
    ///
    /// As [`record_elevation`](Self::record_elevation): `Internal` when the store is not ready.
    pub(crate) fn record_scoped_elevation(
        &mut self,
        env: &Env,
        reason: ElevationReason,
        scope: Arc<crate::ElevationScope>,
        block: crate::BlockId,
    ) -> Result<()> {
        self.record_elevation_in(env, reason, ElevationGrant::Scoped { scope, block })
    }

    fn record_elevation_in(
        &mut self,
        env: &Env,
        reason: ElevationReason,
        grant: ElevationGrant,
    ) -> Result<()> {
        let original = Attribution::from_env(env)?;
        let effective_subject = env.principal().subject();
        self.record_elevation(
            ElevationObservation {
                reason,
                grant,
                original,
                effective_subject,
            },
            env,
        )
    }

    /// The hook calls when a block opens, before its body runs (contract in the
    /// module documentation of [`crate::audit`]). Buffers only: no statement, no second
    /// connection. Allowed in a read-only transaction, where the evidence reaches only an attempt
    /// row ([`take_audit_attempt`](Self::take_audit_attempt)).
    pub(crate) fn record_elevation(
        &mut self,
        observation: ElevationObservation,
        env: &Env,
    ) -> Result<()> {
        self.audit_ready()?;
        let depth = self.savepoint_depth();
        self.audit.observe(depth, observation, env)
    }

    /// Write this transaction's audit row in the commit preamble: one INSERT when the transaction
    /// is a write transaction and has a call summary, an auth event or an elevation; none
    /// otherwise. Freezes the buffer first, so a cancelled future forces a rollback.
    pub(crate) async fn persist_audit(&mut self) -> Result<()> {
        if !self.audit.wants_row() || !matches!(self.intent(), TxIntent::Write) {
            return Ok(());
        }
        self.audit_ready()?;
        self.audit.freeze()?;
        let row = self.build_change_row().await?;
        insert(self, &row).await
    }

    async fn build_change_row(&mut self) -> Result<Row> {
        let env = self
            .audit
            .env()
            .cloned()
            .ok_or_else(|| Error::programming("audit: an audit row has no environment"))?;
        let identity = env
            .operation_identity()
            .ok_or_else(|| {
                Error::programming("audit: an audited transaction requires an OperationIdentity")
            })?
            .clone();
        let attribution = Attribution::from_env(&env)?;
        let companies: Vec<i64> = self
            .companies(&env)
            .await?
            .into_iter()
            .map(|c| c.get())
            .collect();
        let generation = self.policy.generation();
        let registry = std::sync::Arc::clone(env.registry());
        let buffer: &AuditBuffer = &self.audit;
        Row::build(RowInput {
            kind: "change",
            origin: buffer.origin(),
            outcome: AuditOutcome::Committed,
            error_class: None,
            attempt: buffer.call().map_or(1, |c| c.attempt),
            identity: &identity,
            original_operation: None,
            attribution: Some(&attribution),
            companies,
            generations: Some((generation.registry(), generation.policy())),
            call: buffer.call(),
            auth: buffer.auth(),
            elevations: buffer.elevations(),
            extra_events: Vec::new(),
            registry: &registry,
        })
    }
}

/// An independent audit row for an attempt that did not commit (denial, failure, replay, unknown
/// commit) or for a read that policy selected for audit (`attempt` rows).
/// Written by [`Orm::record_audit_attempt`] in its own transaction.
#[derive(Debug)]
pub struct AuditAttempt {
    evidence: AttemptEvidence,
    outcome: AuditOutcome,
    error_class: Option<ErrorClass>,
    attempt: u16,
    original_operation: Option<OperationId>,
    identity: Option<OperationIdentity>,
    extra_events: Vec<Value>,
}

impl AuditAttempt {
    /// The attempt row of a call whose evidence was taken with
    /// [`Transaction::take_audit_attempt`]; `attempt` is the 1-based transaction attempt number.
    pub fn new(evidence: AttemptEvidence, outcome: AuditOutcome, attempt: u16) -> Self {
        Self {
            evidence,
            outcome,
            error_class: None,
            attempt,
            original_operation: None,
            identity: None,
            extra_events: Vec::new(),
        }
    }

    /// An attempt without a principal (a failed login): origin `auth`, no subject, no login text.
    pub fn unauthenticated(identity: OperationIdentity, error_class: ErrorClass) -> Self {
        let evidence = AttemptEvidence {
            origin: Some(AuditOrigin::Auth),
            ..AttemptEvidence::default()
        };
        Self {
            evidence,
            outcome: AuditOutcome::Failed,
            error_class: Some(error_class),
            attempt: 1,
            original_operation: None,
            identity: Some(identity),
            extra_events: Vec::new(),
        }
    }

    /// The bounded class of the error that ended the attempt.
    #[must_use]
    pub fn error_class(mut self, class: ErrorClass) -> Self {
        self.error_class = Some(class);
        self
    }

    /// The operation this attempt refers to (an unknown commit's own operation, or a replayed
    /// call's original).
    #[must_use]
    pub fn original_operation(mut self, operation: OperationId) -> Self {
        self.original_operation = Some(operation);
        self
    }

    /// The idempotency row a replay was answered from (event `replay`).
    #[must_use]
    pub fn replay_of(mut self, idempotency_row: i64) -> Self {
        self.extra_events
            .push(serde_json::json!({"type": "replay", "idempotency_row": idempotency_row}));
        self
    }
}

impl Orm {
    /// Write one independent `attempt` row in its own transaction (one INSERT, autocommit): a
    /// denied, failed, replayed or unknown-commit call, or a policy-selected read. It never runs
    /// inside the failed business transaction.
    ///
    /// # Errors
    ///
    /// `Internal` ([`AuditError::Unavailable`]) before any statement when the store is not ready;
    /// `Internal` ([`AuditError::Insertion`]) when the INSERT fails; `Programming` when the
    /// evidence has neither an environment nor an identity.
    pub async fn record_audit_attempt(&self, attempt: AuditAttempt) -> Result<()> {
        let state = self.shared.audit_store().state();
        if !state.allows_writes() {
            return Err(AuditError::Unavailable(state.describe()).into());
        }
        let AuditAttempt {
            evidence,
            outcome,
            error_class,
            attempt,
            original_operation,
            identity,
            extra_events,
        } = attempt;
        let (attribution, companies, identity, registry) = match &evidence.env {
            Some(env) => {
                let identity = env.operation_identity().cloned().ok_or_else(|| {
                    Error::programming("audit: an audit attempt requires an OperationIdentity")
                })?;
                let companies: Vec<i64> = env.companies()?.iter().map(|c| c.get()).collect();
                (
                    Some(Attribution::from_env(env)?),
                    companies,
                    identity,
                    std::sync::Arc::clone(env.registry()),
                )
            }
            None => (
                None,
                Vec::new(),
                identity
                    .ok_or_else(|| Error::programming("audit: an audit attempt has no identity"))?,
                std::sync::Arc::clone(&self.shared.registry),
            ),
        };
        let row = Row::build(RowInput {
            kind: "attempt",
            origin: evidence.origin.unwrap_or(if evidence.call.is_some() {
                AuditOrigin::Rpc
            } else {
                AuditOrigin::Internal
            }),
            outcome,
            error_class,
            attempt,
            identity: &identity,
            original_operation: original_operation.as_ref(),
            attribution: attribution.as_ref(),
            companies,
            generations: None,
            call: evidence.call.as_ref(),
            auth: &[],
            elevations: &evidence.elevations,
            extra_events,
            registry: &registry,
        })?;
        let mut tx = self.begin(crate::PrincipalHint::system()).await?;
        let written = insert(&mut tx, &row).await;
        match written {
            Ok(()) => tx.commit().await,
            Err(error) => {
                let _ = tx.rollback().await;
                Err(error)
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]
mod tests {
    use std::sync::Arc;

    use erp_registry::RegistryBuilder;

    use super::*;
    use crate::audit::{ApiKeysCollected, TargetIds};
    use crate::rpc::Effect;
    use crate::{ApiKeyId, Principal};

    fn registry() -> Arc<Registry> {
        use erp_registry::AddonDescriptor;
        static ADDON: AddonDescriptor = AddonDescriptor {
            name: "audit_unit",
            depends: &["base"],
            exact_only: false,
        };
        Arc::new(RegistryBuilder::new([&ADDON]).build().unwrap())
    }

    fn base_input<'a>(
        identity: &'a OperationIdentity,
        registry: &'a Registry,
        call: Option<&'a AuditCall>,
        auth: &'a [(usize, AuthEvent)],
        companies: Vec<i64>,
    ) -> RowInput<'a> {
        RowInput {
            kind: "change",
            origin: AuditOrigin::Rpc,
            outcome: AuditOutcome::Committed,
            error_class: None,
            attempt: 1,
            identity,
            original_operation: None,
            attribution: None,
            companies,
            generations: Some((1, 2)),
            call,
            auth,
            elevations: &[],
            extra_events: Vec::new(),
            registry,
        }
    }

    #[test]
    fn companies_are_bounded_positive_and_unique() {
        let identity = OperationIdentity::generate().unwrap();
        let registry = registry();
        let ok = Row::build(base_input(&identity, &registry, None, &[], vec![3, 1])).unwrap();
        assert_eq!(ok.company_ids, [3, 1]);
        for bad in [vec![0], vec![-1], vec![2, 2]] {
            let error = Row::build(base_input(&identity, &registry, None, &[], bad))
                .err()
                .unwrap();
            assert_eq!(error.kind(), erp_fields::ErrorKind::Programming);
            assert_eq!(
                error.message(),
                "a later release (invalid): audit companies must be positive and unique"
            );
        }
        let many: Vec<i64> = (1..=257).collect();
        let error = Row::build(base_input(&identity, &registry, None, &[], many))
            .err()
            .unwrap();
        assert_eq!(error.kind(), erp_fields::ErrorKind::ResourceExhausted);
        assert_eq!(
            error.message(),
            "a later release (capture-limit): CaptureLimit: more than 256 companies in one row"
        );
    }

    /// bytes and not characters.
    #[test]
    fn row_cap_is_exact_in_bytes_with_non_ascii_text() {
        let identity = OperationIdentity::generate().unwrap();
        let registry = registry();
        let call = AuditCall::new(
            Some("mödel.é".into()),
            "méthode".into(),
            Effect::Destructive,
            TargetIds::from_ids(&[1, 2, 3, 9]).unwrap(),
            None,
        )
        .unwrap();
        let row = Row::build(base_input(&identity, &registry, Some(&call), &[], vec![1])).unwrap();
        let exact = row.measure().unwrap();
        assert_eq!(usize::try_from(row.encoded_bytes).unwrap(), exact);
        // `é` is two bytes: the model and the method are counted by bytes, not characters
        assert_eq!(row.model.as_deref().map(str::len), Some("mödel.é".len()));
        assert_eq!(
            row.effect,
            Some("destructive"),
            "the effect is recorded by its name"
        );
        assert!("mödel.é".len() > "mödel.é".chars().count());
        assert!(row.measure_with(exact).is_ok());
        let error = row.measure_with(exact - 1).unwrap_err();
        assert_eq!(error.kind(), erp_fields::ErrorKind::ResourceExhausted);
        assert_eq!(
            error.message(),
            format!(
                "a later release (capture-limit): CaptureLimit: the encoded row exceeds {} bytes",
                exact - 1
            )
        );
    }

    /// recording order, and a credential-shaped canary cannot be expressed by any constructor.
    #[test]
    fn several_events_encode_into_one_row() {
        let identity = OperationIdentity::generate().unwrap();
        let registry = registry();
        let user = crate::UserId::new(7).unwrap();
        let key = ApiKeyId::new(91).unwrap();
        let events = [
            (
                0,
                AuthEvent::PasswordChanged(crate::audit::PasswordChanged::new(user, user)),
            ),
            (
                0,
                AuthEvent::ApiKeyRevoked(crate::audit::ApiKeyRevoked::new(
                    key,
                    user,
                    crate::KeyKind::Agent,
                    crate::audit::KeyRouteScope::Rpc,
                    crate::audit::RevokeCause::Renewed { replacement: key },
                )),
            ),
            (
                0,
                AuthEvent::ApiKeysCollected(ApiKeysCollected::new(vec![key]).unwrap()),
            ),
        ];
        let row = Row::build(base_input(&identity, &registry, None, &events, vec![1])).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&row.events).unwrap();
        let names: Vec<&str> = parsed
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["event"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            ["password.changed", "api_key.revoked", "api_keys.collected"]
        );
        assert_eq!(parsed[1]["cause"]["kind"], "renewed");
        assert_eq!(parsed[1]["cause"]["replacement"], 91);
        assert_eq!(row.target_ids, "[]");
        assert_eq!(row.registry_generation, Some(1));
        assert_eq!(row.policy_generation, Some(2));
    }

    /// wrapped.
    #[test]
    fn a_generation_beyond_bigint_is_programming_not_wrapped() {
        let identity = OperationIdentity::generate().unwrap();
        let registry = registry();
        let mut input = base_input(&identity, &registry, None, &[], vec![1]);
        input.generations = Some((u64::MAX, 0));
        let error = Row::build(input).err().unwrap();
        assert_eq!(error.kind(), erp_fields::ErrorKind::Programming);
        assert_eq!(
            error.message(),
            format!(
                "a later release (invalid): the registry generation {} does not fit bigint",
                u64::MAX
            )
        );
        let _ = Principal::for_uid(crate::UserId::new(1).unwrap());
    }

    fn db_error(kind: erp_fields::ErrorKind, state: &str, message: &str) -> Error {
        Error::new(kind, message).with_sqlstate(Some(state.to_owned()))
    }

    /// INSERT is returned as it is, so the call is retried and not answered 503.
    #[test]
    fn a_serialization_failure_of_the_insert_stays_retryable() {
        for state in ["40001", "40P01"] {
            let original = db_error(
                erp_fields::ErrorKind::Concurrency,
                state,
                "could not serialize",
            );
            let error = insertion_error(original);
            assert_eq!(error.kind(), erp_fields::ErrorKind::Concurrency, "{state}");
            assert_eq!(error.sqlstate(), Some(state));
            assert!(
                std::error::Error::source(&error)
                    .is_none_or(|s| s.downcast_ref::<AuditError>().is_none())
            );
        }
    }

    /// the source, behind the audit error that names the SQLSTATE.
    #[test]
    fn another_insert_failure_keeps_the_database_error_as_source() {
        let original = db_error(
            erp_fields::ErrorKind::Database,
            "08006",
            "connection lost: LOST-LINK",
        );
        let error = insertion_error(original);
        assert_eq!(error.kind(), erp_fields::ErrorKind::Internal);
        assert!(
            error.message().contains("SQLSTATE 08006"),
            "{}",
            error.message()
        );
        assert!(
            !error.message().contains("LOST-LINK"),
            "the message is not the database's"
        );
        let audit = std::error::Error::source(&error)
            .and_then(|s| s.downcast_ref::<AuditError>())
            .expect("an audit error");
        assert!(matches!(audit, AuditError::Insertion { .. }));
        let cause = std::error::Error::source(audit).expect("the database error");
        assert!(cause.to_string().contains("LOST-LINK"), "{cause}");
    }

    /// error only; a check constraint of the table gets its own wording.
    #[test]
    fn the_partition_hint_is_only_for_a_partition_miss() {
        let miss = insertion_error(db_error(
            erp_fields::ErrorKind::Database,
            "23514",
            "no partition of relation \"ir_audit_entry\" found for row",
        ));
        assert!(
            miss.message().contains("erp audit maintain"),
            "{}",
            miss.message()
        );
        let check = insertion_error(
            db_error(
                erp_fields::ErrorKind::Database,
                "23514",
                "new row violates check constraint",
            )
            .with_constraint("ir_audit_entry_shape_check"),
        );
        assert!(
            !check.message().contains("erp audit maintain"),
            "{}",
            check.message()
        );
        assert!(
            check.message().contains("check constraint"),
            "{}",
            check.message()
        );
        let unnamed_check = insertion_error(db_error(
            erp_fields::ErrorKind::Database,
            "23514",
            "new row for relation \"ir_audit_entry_2026\" violates check constraint",
        ));
        assert!(!unnamed_check.message().contains("erp audit maintain"));
    }
}
