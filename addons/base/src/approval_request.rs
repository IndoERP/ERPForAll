//! A reviewed RPC request. The requester retries their bound call after review. Execution,
//! consuming the approval, and the idempotency receipt commit in that requester's transaction.

use erp_sdk::prelude::*;
use erp_sdk::rpc::Effect;
use erp_sdk::serde_json::{Value as Json, json};
use erp_sdk::{CompanyId, ErrorKind, Operation, PrincipalKind, TimestampTz, UserId};

use crate::ir_approval_policy::{
    ApprovalCall, PolicyOutcome, PolicyRevisionId, current_effect, eligible_approvers, requesters,
    resolve,
};

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
        ("_finite_expiry", "CHECK (isfinite(expires_at))", "Invalid approval request expiry."),
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
    #[field(json, required = true)]
    pub companies: Option<Json>,
    #[field(json, required = true, groups = ".")]
    pub excluded: Option<Json>,
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
            "SELECT expires_at > clock_timestamp() FROM ir_approval_request WHERE id = $1",
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
            if model == "ir.approval.request" && matches!(method, "erp_decide" | "erp_status") =>
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
             target_model, target_method, revision_id, companies, excluded, state, expires_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, 'pending', \
                     clock_timestamp() + interval '24 hours') \
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
            let decider = UserId::new(row.try_get(6).map_err(|_| shape())?)?;
            if !approvers.contains(decider) {
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

fn receipt(
    row: &erp_sdk::Row,
    state: &str,
    operation: Option<&str>,
    can_decide: bool,
) -> Result<Value> {
    Ok(Value::Json(Arc::new(json!({
        "id": row.try_get::<_, i64>(0).map_err(|_| shape())?,
        "model": row.try_get::<_, &str>(1).map_err(|_| shape())?,
        "method": row.try_get::<_, &str>(2).map_err(|_| shape())?,
        "payload_sha256": row.try_get::<_, &str>(3).map_err(|_| shape())?,
        "requester_id": row.try_get::<_, i64>(4).map_err(|_| shape())?,
        "companies": row.try_get::<_, Json>(6).map_err(|_| shape())?,
        "state": state,
        "operation": operation,
        "can_decide": can_decide,
    }))))
}

async fn review(
    tx: &mut Transaction,
    id: i64,
    decision: Option<bool>,
    payload_sha256: Option<&str>,
) -> Result<Value> {
    let env = tx.env();
    let principal = env.principal();
    if principal.kind() != PrincipalKind::User
        || (decision.is_some() && principal.actor().is_some())
    {
        return Err(concealed());
    }
    let lock = if decision.is_some() {
        "FOR UPDATE OF r"
    } else {
        ""
    };
    let rows = tx
        .query(
            &format!(
                "SELECT r.id, r.target_model, r.target_method, r.payload, r.requester_id, \
                 r.revision_id, r.companies, r.excluded, r.state, r.operation, p.active, r.decider_id \
                 FROM ir_approval_request r JOIN ir_approval_policy p ON p.id = r.revision_id \
                 WHERE r.id = $1 {lock}"
            ),
            &[&id],
        )
        .await?;
    let row = rows.first().ok_or_else(concealed)?;
    let state: &str = row.try_get(8).map_err(|_| shape())?;
    let operation = row.try_get(9).map_err(|_| shape())?;
    let subject = principal.subject();
    let requester: i64 = row.try_get(4).map_err(|_| shape())?;
    if decision.is_none() && requester == subject.get() {
        let state = if matches!(state, "pending" | "approved") && !live(tx, id).await? {
            "expired"
        } else {
            state
        };
        return receipt(row, state, operation, false);
    }
    let active: bool = row.try_get(10).map_err(|_| shape())?;
    if !active {
        return Err(concealed());
    }
    let revision = PolicyRevisionId::new(row.try_get(5).map_err(|_| shape())?)?;
    let ids = |column| -> Result<Vec<i64>> {
        erp_sdk::serde_json::from_value(row.try_get::<_, Json>(column).map_err(|_| shape())?)
            .map_err(|_| shape())
    };
    let companies = ids(6)?
        .into_iter()
        .map(|id| CompanyId::new(id).map_err(|_| shape()))
        .collect::<Result<Vec<_>>>()?;
    let excluded = ids(7)?
        .into_iter()
        .map(|id| UserId::new(id).map_err(|_| shape()))
        .collect::<Result<Vec<_>>>()?;
    if !eligible_approvers(tx, revision, &excluded, &companies)
        .await?
        .contains(subject)
    {
        return Err(concealed());
    }
    if let Some(digest) = payload_sha256
        && digest != row.try_get::<_, &str>(3).map_err(|_| shape())?
    {
        return Err(Error::validation(
            "The supplied approval payload digest does not match.",
        ));
    }
    if matches!(state, "pending" | "approved") && !live(tx, id).await? {
        return receipt(row, "expired", None, false);
    }
    let Some(approve) = decision else {
        return receipt(
            row,
            state,
            operation,
            state == "pending" && principal.actor().is_none(),
        );
    };
    let desired = if approve { "approved" } else { "denied" };
    if state != "pending" {
        let decider: Option<i64> = row.try_get(11).map_err(|_| shape())?;
        if (state == desired || (approve && state == "executed")) && decider == Some(subject.get())
        {
            return receipt(row, state, operation, false);
        }
        return Err(Error::validation(
            "This approval request has already been decided.",
        ));
    }
    let updated = tx
        .execute(
            "UPDATE ir_approval_request SET state = $2, decider_id = $3
         WHERE id = $1 AND expires_at > clock_timestamp()",
            &[&id, &desired, &subject.get()],
        )
        .await?;
    receipt(
        row,
        if updated == 1 { desired } else { "expired" },
        None,
        false,
    )
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
    ) -> Result<Value> {
        review(tx, request_id, Some(approve), payload_sha256.as_deref()).await
    }

    #[method]
    #[rpc(effect = read, model)]
    async fn erp_status(
        _this: &RecordSet<IrApprovalRequest>,
        tx: &mut Transaction,
        request_id: i64,
    ) -> Result<Value> {
        review(tx, request_id, None, None).await
    }
}
