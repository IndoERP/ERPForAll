//! A reviewed RPC request. The requester retries their bound call after review. Execution,
//! consuming the approval, and the idempotency receipt commit in that requester's transaction.

mod decision;
mod queue;
mod stages;
mod surface;

use erp_sdk::prelude::*;
use erp_sdk::rpc::Effect;
use erp_sdk::serde_json::{Value as Json, json};
use erp_sdk::{ErrorKind, Operation, PrincipalKind, TimestampTz, UserId};

use crate::ir_approval_policy::{ApprovalCall, PolicyOutcome, current_effect, requesters, resolve};

#[model(
    name = "ir.approval.request",
    description = "Approval Request",
    company_access = unrestricted(reason = "The lifecycle gates the request by its authenticated requester and the approver's current rights across every company of the call"),
    constraints = [
        (
            "_state",
            "CHECK ((state = 'pending' AND decider_id IS NULL AND operation IS NULL) OR \
             (state IN ('approved','denied') AND decider_id IS NOT NULL AND operation IS NULL) OR \
             (state = 'executed' AND decider_id IS NOT NULL AND operation IS NOT NULL))",
            "Invalid approval request state."
        ),
        ("_step", "CHECK (current_step BETWEEN 1 AND 16)", "Invalid current approval stage."),
        ("_finite_expiry", "CHECK (isfinite(expires_at))", "Invalid approval request expiry."),
    ],
    indexes = [
        ("_pending_expiry_idx", "(expires_at) WHERE state = 'pending'"),
        ("_decider_history_idx", "(decider_id, id DESC) WHERE decider_id IS NOT NULL"),
    ],
    unique_indexes = [(
        "_submission_uniq",
        "(requester_id, request_key)",
        "An approval request already uses this key."
    )]
)]
pub struct IrApprovalRequest {
    #[field(
        many2one,
        comodel = "res.users",
        required = true,
        ondelete = "restrict"
    )]
    pub requester_id: Option<RecId>,
    #[field(many2one, comodel = "res.users", ondelete = "restrict")]
    pub decider_id: Option<RecId>,
    #[field(char, required = true, groups = ".")]
    pub request_key: Option<Arc<str>>,
    #[field(char, required = true, groups = ".")]
    pub binding: Option<Arc<str>>,
    #[field(char, required = true, groups = ".")]
    pub payload: Option<Arc<str>>,
    #[field(char, required = true)]
    pub target_model: Option<Arc<str>>,
    #[field(char, required = true)]
    pub target_method: Option<Arc<str>>,
    #[field(
        many2one,
        comodel = "ir.approval.policy",
        required = true,
        ondelete = "restrict"
    )]
    pub revision_id: Option<RecId>,
    #[field(timestamptz, readonly, groups = ".")]
    pub revision_archived_at: Option<TimestampTz>,
    #[field(json, required = true)]
    pub companies: Option<Json>,
    #[field(json, required = true, groups = ".")]
    pub excluded: Option<Json>,
    #[field(integer, required, default = 1)]
    pub current_step: i32,
    #[field(char, required = true)]
    pub state: Option<Arc<str>>,
    #[field(timestamptz, required = true, groups = ".")]
    pub expires_at: Option<TimestampTz>,
    #[field(char)]
    pub operation: Option<Arc<str>>,
}

/// Whether a revision can restrict user calls. This check also avoids widening user company
/// scope on databases that use only the shipped Allow or inert agent revisions.
pub async fn has_restrictions(tx: &mut Transaction) -> Result<bool> {
    let rows = tx
        .query(
            "SELECT EXISTS (SELECT FROM ir_approval_policy \
             WHERE active AND principal_kind = 'user' AND outcome <> 'allow')",
            &[],
        )
        .await?;
    rows.first()
        .ok_or_else(shape)?
        .try_get(0)
        .map_err(|_| shape())
}

/// Stable request facts taken by trusted RPC dispatch after argument and credential admission.
#[derive(Debug)]
pub struct Call<'a> {
    pub model: &'a str,
    pub method: &'a str,
    pub effect: Effect,
    pub key: Option<&'a str>,
    pub payload: Option<&'a [u8; 32]>,
    pub binding: Option<&'a [u8; 32]>,
}

fn shape() -> Error {
    Error::invariant("An approval request has invalid stored data.")
}

fn concealed() -> Error {
    Error::access("You cannot access this approval request.")
}

fn digest(value: &[u8; 32]) -> String {
    value.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn pending(id: i64) -> Error {
    Error::new(ErrorKind::Conflict, "This call requires approval.").with_context(Value::Map(
        Arc::new(erp_sdk::ValueMap::from([(
            "approval_request_id".to_owned(),
            Value::Int(id),
        )])),
    ))
}

fn expired() -> Error {
    Error::new(
        ErrorKind::Conflict,
        "This approval has expired. Submit a new request with a new key.",
    )
}

/// Read after taking any request-row lock. Transaction start time may be older than a
/// concurrent decision or execution that made this attempt wait.
async fn live(tx: &mut Transaction, id: i64) -> Result<bool> {
    let rows = tx
        .query(
            "SELECT r.expires_at > clock_timestamp()
                 AND r.revision_archived_at IS NOT DISTINCT FROM p.last_archived_at
             FROM ir_approval_request r JOIN ir_approval_policy p ON p.id=r.revision_id
             WHERE r.id = $1",
            &[&id],
        )
        .await?;
    rows.first()
        .ok_or_else(shape)?
        .try_get(0)
        .map_err(|_| shape())
}

#[derive(Debug)]
pub enum Admission {
    Dispatch,
    KeyRequired,
    Defer(Error),
    ReplayOnly(Error),
}

async fn call_policy(
    tx: &mut Transaction,
    model: &str,
    method: &str,
    effect: Effect,
) -> Result<PolicyOutcome> {
    let env = tx.env();
    match env.principal().kind() {
        PrincipalKind::User
            if (model == "ir.approval.request"
                && matches!(method, "erp_decide" | "erp_status" | "erp_queue"))
                || (model == "approval.request.view"
                    && matches!(method, "action_approve" | "action_deny")) =>
        {
            return Ok(PolicyOutcome::Unmanaged);
        }
        PrincipalKind::User if !has_restrictions(tx).await? => {
            return Ok(PolicyOutcome::Unmanaged);
        }
        PrincipalKind::User | PrincipalKind::Agent => {}
        _ => return Ok(PolicyOutcome::Unmanaged),
    }
    // Arguments can name new companies or move existing records. Resolve the full company scope
    // rather than trusting the currently selected company or a field supplied in the request.
    let call = ApprovalCall::model_level(model, method)?;
    if current_effect(&env, tx, &call)? != effect {
        return Err(Error::invariant(
            "RPC approval received a different effect from the registry.",
        ));
    }
    resolve(&env, tx, &call).await
}

fn unmanaged(kind: PrincipalKind, effect: Effect) -> bool {
    kind != PrincipalKind::Agent || effect == Effect::Read
}

fn can_request(effect: Effect) -> bool {
    matches!(effect, Effect::Write | Effect::Destructive)
}

/// A revision can be archived while a browser is holding an approval receipt. Keep that key
/// reserved even when the new policy would admit a new ordinary call. Older base generations
/// may serve without the request table until the upgrade publishes it.
async fn already_submitted(tx: &mut Transaction, requester: UserId, key: &str) -> Result<bool> {
    let installed = tx
        .query(
            "SELECT to_regclass('public.ir_approval_request') IS NOT NULL",
            &[],
        )
        .await?;
    if !installed
        .first()
        .ok_or_else(shape)?
        .try_get::<_, bool>(0)
        .map_err(|_| shape())?
    {
        return Ok(false);
    }
    let rows = tx
        .query(
            "SELECT EXISTS (SELECT FROM ir_approval_request WHERE requester_id = $1 AND request_key = $2)",
            &[&requester.get(), &key],
        )
        .await?;
    rows.first()
        .ok_or_else(shape)?
        .try_get(0)
        .map_err(|_| shape())
}

fn changed() -> Error {
    Error::new(
        ErrorKind::Conflict,
        "The call or its approval authority changed. Submit a new request with a new key.",
    )
}

/// Check the same policy for discovery without creating or consuming an approval.
pub async fn assistant_visible(
    tx: &mut Transaction,
    model: &str,
    method: &str,
    effect: Effect,
) -> Result<bool> {
    match call_policy(tx, model, method, effect).await {
        Ok(PolicyOutcome::Unmanaged) => Ok(unmanaged(tx.env().principal().kind(), effect)),
        Ok(PolicyOutcome::Allow { .. }) => Ok(true),
        Ok(PolicyOutcome::Approve { .. }) => Ok(can_request(effect)),
        Ok(PolicyOutcome::Deny { .. }) => Ok(false),
        Err(error) if matches!(error.kind(), ErrorKind::Access | ErrorKind::Validation) => {
            Ok(false)
        }
        Err(error) => Err(error),
    }
}

/// Admit a user or agent RPC in its authenticated transaction.
pub async fn admit(tx: &mut Transaction, call: Call<'_>) -> Result<Admission> {
    let env = tx.env();
    let (revision, approvers) = match call_policy(tx, call.model, call.method, call.effect).await? {
        PolicyOutcome::Deny { .. } => {
            return Err(Error::access("An approval policy denies this call."));
        }
        PolicyOutcome::Unmanaged if !unmanaged(env.principal().kind(), call.effect) => {
            return Err(Error::access(
                "This agent call requires an explicit approval policy.",
            ));
        }
        PolicyOutcome::Unmanaged | PolicyOutcome::Allow { .. } => {
            if can_request(call.effect)
                && let Some(key) = call.key
                && already_submitted(tx, env.principal().subject(), key).await?
            {
                return Err(changed());
            }
            return Ok(Admission::Dispatch);
        }
        PolicyOutcome::Approve {
            revision,
            approvers,
        } => (revision, approvers),
    };
    if !can_request(call.effect) {
        return Err(Error::validation(
            "This call cannot be submitted for approval.",
        ));
    }
    let (Some(key), Some(payload), Some(binding)) = (call.key, call.payload, call.binding) else {
        return Ok(Admission::KeyRequired);
    };
    let requester = env.principal().subject().get();
    let payload = digest(payload);
    let binding = digest(binding);
    let companies = json!(
        approvers
            .companies
            .iter()
            .map(|id| id.get())
            .collect::<Vec<_>>()
    );
    let excluded = json!(
        requesters(&env)?
            .iter()
            .map(|id| id.get())
            .collect::<Vec<_>>()
    );
    let inserted = tx
        .query(
            "INSERT INTO ir_approval_request (requester_id, request_key, binding, payload, \
             target_model, target_method, revision_id, companies, excluded, current_step, state, expires_at, revision_archived_at) \
             SELECT $1, $2, $3, $4, $5, $6, p.id, $8, $9, 1, 'pending', \
                    clock_timestamp() + interval '24 hours', p.last_archived_at \
             FROM ir_approval_policy p WHERE p.id=$7 \
             ON CONFLICT (requester_id, request_key) DO NOTHING RETURNING id",
            &[
                &requester,
                &key,
                &binding,
                &payload,
                &call.model,
                &call.method,
                &revision.get(),
                &companies,
                &excluded,
            ],
        )
        .await?;
    if let Some(row) = inserted.first() {
        return Ok(Admission::Defer(pending(
            row.try_get(0).map_err(|_| shape())?,
        )));
    }
    let rows = tx
        .query(
            "SELECT id, binding, payload, revision_id, companies, state, decider_id \
             FROM ir_approval_request WHERE requester_id = $1 AND request_key = $2 FOR UPDATE",
            &[&requester, &key],
        )
        .await?;
    let row = rows.first().ok_or_else(shape)?;
    if row.try_get::<_, &str>(1).map_err(|_| shape())? != binding
        || row.try_get::<_, &str>(2).map_err(|_| shape())? != payload
        || row.try_get::<_, i64>(3).map_err(|_| shape())? != revision.get()
        || row.try_get::<_, Json>(4).map_err(|_| shape())? != companies
    {
        return Err(changed());
    }
    let id = row.try_get(0).map_err(|_| shape())?;
    let state = row.try_get::<_, &str>(5).map_err(|_| shape())?;
    if matches!(state, "pending" | "approved") && !live(tx, id).await? {
        return Err(expired());
    }
    match state {
        "pending" => Ok(Admission::Defer(pending(id))),
        "denied" => Err(Error::access("This approval request was denied.")),
        "approved" | "executed" => {
            let signatures = stages::read(tx, id, env.principal().subject()).await?;
            let excluded_now = requesters(&env)?;
            if signatures.iter().any(|s| {
                s.approved != Some(true)
                    || !s.signer_eligible
                    || excluded_now.iter().any(|user| Some(user.get()) == s.signer)
            }) {
                return Err(Error::access(
                    "The approver is no longer eligible for this call.",
                ));
            }
            if state == "executed" {
                return Ok(Admission::ReplayOnly(Error::new(
                    ErrorKind::Conflict,
                    "This approval has been executed and its RPC replay is no longer available.",
                )));
            }
            {
                let operation = env
                    .operation_identity()
                    .ok_or_else(shape)?
                    .operation()
                    .as_str();
                let updated = tx
                    .execute(
                        "UPDATE ir_approval_request SET state = 'executed', operation = $2
                     WHERE id = $1 AND expires_at > clock_timestamp()",
                        &[&id, &operation],
                    )
                    .await?;
                if updated != 1 {
                    return Err(expired());
                }
            }
            Ok(Admission::Dispatch)
        }
        _ => Err(shape()),
    }
}

struct Review {
    summary: Json,
    payload: String,
    requester: i64,
    active: bool,
    state: String,
    operation: Option<String>,
    current_step: i32,
    steps: Vec<stages::Stage>,
}

impl Review {
    async fn load(tx: &mut Transaction, id: i64, lock: bool) -> Result<Self> {
        let lock = if lock { "FOR UPDATE OF r" } else { "" };
        let sql = format!(
            "SELECT r.requester_id, r.state, r.payload, r.operation, p.active, r.current_step,
                    {summary} AS summary FROM ir_approval_request r {joins} WHERE r.id=$1 {lock}",
            summary = queue::summary(),
            joins = queue::JOINS,
        );
        let rows = tx.query(&sql, &[&id]).await?;
        let r = rows.first().ok_or_else(concealed)?;
        let mut state: String = r.try_get("state").map_err(|_| shape())?;
        if matches!(state.as_str(), "pending" | "approved") && !live(tx, id).await? {
            state = "expired".into();
        }
        let subject = tx.env().principal().subject();
        Ok(Self {
            requester: r.try_get("requester_id").map_err(|_| shape())?,
            active: r.try_get("active").map_err(|_| shape())?,
            operation: r.try_get("operation").map_err(|_| shape())?,
            current_step: r.try_get("current_step").map_err(|_| shape())?,
            payload: r.try_get("payload").map_err(|_| shape())?,
            summary: r.try_get("summary").map_err(|_| shape())?,
            steps: stages::read(tx, id, subject).await?,
            state,
        })
    }

    fn stage(&self, step: i32) -> Option<&stages::Stage> {
        self.steps.iter().find(|s| s.position == step)
    }

    fn can_read(&self, subject: UserId) -> bool {
        self.requester == subject.get()
            || self.active
                && self.steps.iter().any(|s| {
                    s.viewer_eligible
                        && (s.position == self.current_step || s.signer == Some(subject.get()))
                })
    }

    fn receipt(mut self, subject: UserId, direct: bool) -> Result<Value> {
        let can_decide = self.active
            && direct
            && self.requester != subject.get()
            && self.state == "pending"
            && self
                .stage(self.current_step)
                .ok_or_else(shape)?
                .viewer_eligible;
        let fields = self.summary.as_object_mut().ok_or_else(shape)?;
        fields.insert("can_decide".into(), json!(can_decide));
        fields.insert("state".into(), json!(self.state));
        fields.insert("operation".into(), json!(self.operation));
        fields.insert("payload_sha256".into(), json!(self.payload));
        fields.insert(
            "steps".into(),
            Json::Array(
                self.steps
                    .iter()
                    .map(|s| s.display(self.current_step, &self.state))
                    .collect(),
            ),
        );
        Ok(Value::Json(Arc::new(self.summary)))
    }
}

async fn review(
    tx: &mut Transaction,
    id: i64,
    decision: Option<bool>,
    payload_sha256: Option<&str>,
    step: Option<i64>,
) -> Result<Value> {
    let env = tx.env();
    let principal = env.principal();
    let subject = principal.subject();
    let direct = principal.actor().is_none();
    if principal.kind() != PrincipalKind::User || (decision.is_some() && !direct) {
        return Err(concealed());
    }
    let review = Review::load(tx, id, decision.is_some()).await?;
    if !review.can_read(subject) {
        return Err(concealed());
    }
    let Some(approve) = decision else {
        return review.receipt(subject, direct);
    };
    if !review.active || review.requester == subject.get() {
        return Err(concealed());
    }
    if let Some(digest) = payload_sha256
        && digest != review.payload
    {
        return Err(Error::validation(
            "The supplied approval payload digest does not match.",
        ));
    }
    if review.steps.len() > 1 && step.is_none() {
        return Err(Error::validation(
            "Specify the displayed approval stage before deciding.",
        ));
    }
    let expected = step.unwrap_or(i64::from(review.current_step));
    let stage = i32::try_from(expected)
        .ok()
        .and_then(|i| review.stage(i))
        .ok_or_else(|| Error::validation("Unknown approval stage."))?;
    if !stage.viewer_eligible {
        return Err(concealed());
    }
    if stage.signer == Some(subject.get()) && stage.approved == Some(approve) {
        return review.receipt(subject, direct);
    }
    if stage.position != review.current_step {
        return Err(Error::new(
            ErrorKind::Conflict,
            "The approval stage changed. Refresh the request before deciding.",
        ));
    }
    if review.state == "expired" {
        return review.receipt(subject, direct);
    }
    if review.state != "pending" {
        return Err(Error::validation(
            "This approval request has already been decided.",
        ));
    }
    let last = usize::try_from(stage.position).map_err(|_| shape())? == review.steps.len();
    let (state, next, decider) = match (approve, last) {
        (false, _) => ("denied", stage.position, Some(subject.get())),
        (true, true) => ("approved", stage.position, Some(subject.get())),
        (true, false) => ("pending", stage.position + 1, None),
    };
    // The audit insert draws from the guarded update. A conflict or failed insert rolls back
    // the advance, and a deadline passed while waiting produces no signature.
    tx.execute(
        "WITH advanced AS (
             UPDATE ir_approval_request SET state=$2, current_step=$3, decider_id=$4
             WHERE id=$1 AND state='pending' AND current_step=$5 AND expires_at>clock_timestamp() RETURNING id
         ) INSERT INTO ir_approval_decision (request_id,position,reviewer_id,approved,decided_at)
           SELECT id,$5,$6,$7,clock_timestamp() FROM advanced",
        &[&id,&state,&next,&decider,&stage.position,&subject.get(),&approve],
    ).await?;
    Review::load(tx, id, false).await?.receipt(subject, direct)
}

#[methods(IrApprovalRequest, addon = "base")]
impl IrApprovalRequest {
    #[method(override)]
    async fn create(
        this: &RecordSet<IrApprovalRequest>,
        tx: &mut Transaction,
        _values: Vec<RawVals>,
        _sup: Super,
    ) -> Result<Records> {
        this.as_records()
            .check_access(tx, Operation::Create)
            .await?;
        Err(Error::access(
            "Approval requests can only be created by RPC admission.",
        ))
    }

    #[method(override)]
    async fn write(
        this: &RecordSet<IrApprovalRequest>,
        tx: &mut Transaction,
        _values: RawVals,
        _sup: Super,
    ) -> Result<()> {
        this.as_records().check_access(tx, Operation::Write).await?;
        Err(Error::access(
            "Approval requests can only be changed through their lifecycle.",
        ))
    }

    #[method(override)]
    async fn unlink(
        this: &RecordSet<IrApprovalRequest>,
        tx: &mut Transaction,
        _sup: Super,
    ) -> Result<()> {
        this.as_records()
            .check_access(tx, Operation::Unlink)
            .await?;
        Err(Error::access(
            "Approval receipts cannot be deleted through the ORM.",
        ))
    }

    #[method]
    #[rpc(effect = write, model)]
    async fn erp_decide(
        _this: &RecordSet<IrApprovalRequest>,
        tx: &mut Transaction,
        request_id: i64,
        approve: bool,
        payload_sha256: Option<String>,
        step: Option<i64>,
    ) -> Result<Value> {
        review(
            tx,
            request_id,
            Some(approve),
            payload_sha256.as_deref(),
            step,
        )
        .await
    }

    #[method]
    #[rpc(effect = read, model)]
    async fn erp_queue(
        _this: &RecordSet<IrApprovalRequest>,
        tx: &mut Transaction,
        scope: String,
        offset: Option<i64>,
        limit: Option<i64>,
    ) -> Result<Value> {
        queue::queue(tx, &scope, offset, limit).await
    }

    #[method]
    #[rpc(effect = read, model)]
    async fn erp_status(
        _this: &RecordSet<IrApprovalRequest>,
        tx: &mut Transaction,
        request_id: i64,
    ) -> Result<Value> {
        review(tx, request_id, None, None, None).await
    }
}
