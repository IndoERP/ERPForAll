// SPDX-License-Identifier: MIT OR Apache-2.0
//! Private execution envelopes. Only the authenticated assistant transport can prepare them.
//! The originating user and company are checked on every access including after approval.
use erp_sdk::prelude::*;

/// There are deliberately no generic ACL grants for these original method arguments.
#[model(name = "ai.operation", description = "Assistant Operation",
    company_access = direct(field = "company_id", branch = "exact", empty = "deny"),
    constraints = [("_owner_key", "UNIQUE(owner_user_id,request_key)", "An operation key identifies one request per user.")])]
pub struct AiOperation {
    #[field(
        many2one,
        comodel = "ai.session",
        required = true,
        ondelete = "cascade",
        index = true
    )]
    pub session_id: Option<RecId>,
    #[field(many2one, comodel = "res.users", required = true, ondelete = "cascade")]
    pub owner_user_id: Option<RecId>,
    #[field(
        many2one,
        comodel = "res.company",
        required = true,
        ondelete = "restrict"
    )]
    pub company_id: Option<RecId>,
    #[field(char, required = true, size = 36, index = true)]
    pub turn_request: Option<Arc<str>>,
    #[field(char, size = 128)]
    pub result_model: Option<Arc<str>>,
    #[field(bigint)]
    pub result_id: Option<erp_sdk::BigInt>,
    #[field(char, required = true, size = 36)]
    pub request_key: Option<Arc<str>>,
    #[field(char, required = true, size = 128)]
    pub tool: Option<Arc<str>>,
    #[field(text, required = true)]
    pub arguments: Option<Arc<str>>,
    #[field(text)]
    pub draft_version: Option<Arc<str>>,
    #[field(boolean, default = false)]
    pub completed: bool,
}

fn actor(tx: &Transaction) -> Result<(i64, i64)> {
    let env = tx.env();
    let company = env
        .company_id()?
        .ok_or_else(|| Error::access("Select a company first."))?;
    if !crate::ordinary_user(&env) {
        return Err(Error::access("An interactive company session is required."));
    }
    Ok((env.user_id().get(), company))
}

// Approval may outlive a transient composer. Bind its row versions to the private request,
// and lock them inside the final RPC transaction before it queues an external message.
async fn draft_version(
    tx: &mut Transaction,
    tool: &str,
    args: &str,
    lock: bool,
) -> Result<Option<String>> {
    let model = tool
        .rsplit_once('.')
        .ok_or_else(|| Error::validation("Invalid assistant action."))?
        .0;
    let records = tx.env().records(model)?;
    if records.spec().transient_policy.is_none() {
        return Ok(None);
    }
    let args: serde_json::Value = serde_json::from_str(args).map_err(shape)?;
    let Some(ids) = args.get("ids") else {
        return Ok(None);
    };
    let ids: Vec<i64> = serde_json::from_value(ids.clone()).map_err(shape)?;
    let records = records.browse(ids.iter().copied())?;
    records.check_access(tx, erp_sdk::Operation::Read).await?;
    if lock {
        records.lock_for_update(tx, true).await?;
    }
    let sql = format!(
        "SELECT id,xmin::text FROM {} WHERE id=ANY($1::bigint[]) ORDER BY id",
        records.spec().sql_table()
    );
    let rows = tx.query(&sql, &[&ids]).await?;
    if rows.len() != ids.len() {
        return Err(Error::validation(
            "The draft changed or expired. Open a new draft.",
        ));
    }
    let versions: Vec<(i64, String)> = rows
        .iter()
        .map(|r| Ok((r.try_get(0).map_err(shape)?, r.try_get(1).map_err(shape)?)))
        .collect::<Result<_>>()?;
    serde_json::to_string(&versions).map(Some).map_err(shape)
}

/// Bind the exact canonical payload before RPC. A retry cannot replace an earlier step.
///
/// # Errors
/// Refuses a changed binding, inaccessible session or missing migration.
pub async fn prepare(
    tx: &mut Transaction,
    session: i64,
    request: erp_sdk::Uuid,
    key: &str,
    tool: &str,
    arguments: &str,
) -> Result<()> {
    let (uid, company) = actor(tx)?;
    let version = draft_version(tx, tool, arguments, false).await?;
    let rows = tx.query("INSERT INTO ai_operation(session_id,owner_user_id,company_id,request_key,tool,arguments,completed,turn_request,draft_version)
        SELECT id,$2,$3,$4,$5,$6,false,$7,$8 FROM ai_session WHERE id=$1 AND owner_user_id=$2 AND company_id=$3 AND state='open'
        ON CONFLICT (owner_user_id,request_key) DO UPDATE SET request_key=EXCLUDED.request_key
        WHERE ai_operation.session_id=EXCLUDED.session_id AND ai_operation.company_id=EXCLUDED.company_id
          AND ai_operation.turn_request=EXCLUDED.turn_request AND ai_operation.tool=EXCLUDED.tool AND ai_operation.arguments=EXCLUDED.arguments AND ai_operation.draft_version IS NOT DISTINCT FROM EXCLUDED.draft_version RETURNING id",
        &[&session,&uid,&company,&key,&tool,&arguments,&request.to_string(),&version]).await?;
    if rows.is_empty() {
        return Err(Error::validation(
            "erp.ai.operation_changed: the operation or its session changed; review the original request.",
        ));
    }
    Ok(())
}

/// Marks the operation in its business transaction. An expired idempotency key cannot repeat a committed effect.
///
/// # Errors
/// Refuses a missing or completed operation.
pub async fn claim(tx: &mut Transaction, key: &str) -> Result<()> {
    let (uid, company) = actor(tx)?;
    let rows=tx.query("SELECT op.id,op.tool::text,op.arguments,op.draft_version FROM ai_operation op
        JOIN ai_session s ON s.id=op.session_id AND s.state='open'
        WHERE op.owner_user_id=$1 AND op.company_id=$2 AND op.request_key=$3 AND NOT op.completed FOR UPDATE OF op", &[&uid,&company,&key]).await?;
    let row = rows.first().ok_or_else(|| {
        Error::validation(
            "erp.ai.operation_consumed: this operation is already complete or its session closed.",
        )
    })?;
    if let Some(saved) = row.try_get::<_, Option<&str>>(3).map_err(shape)? {
        let current = draft_version(
            tx,
            row.try_get(1).map_err(shape)?,
            row.try_get(2).map_err(shape)?,
            true,
        )
        .await?;
        if current.as_deref() != Some(saved) {
            return Err(Error::validation(
                "The draft was modified after submission. Review it and submit a new request.",
            ));
        }
    }
    let id: i64 = row.try_get(0).map_err(shape)?;
    tx.execute("UPDATE ai_operation SET completed=true WHERE id=$1", &[&id])
        .await?;
    Ok(())
}

/// The current receipt of an approved workflow or the original arguments awaiting execution.
pub enum Resume {
    /// Completed, denied or expired, with currently readable record references only.
    Receipt(serde_json::Value),
    /// A pending or eligible operation must go through the full native policy gate again.
    Dispatch {
        key: String,
        name: String,
        args: String,
    },
}

/// Recover an enrolled RPC only by its approval receipt and authenticated owner.
///
/// # Errors
/// Conceals unknown or inaccessible operations.
pub async fn pending(tx: &mut Transaction, approval: i64) -> Result<Resume> {
    let (uid, company) = actor(tx)?;
    let rows = tx.query("SELECT op.request_key::text,op.tool::text,op.arguments,op.session_id,op.turn_request::text FROM ai_operation op
        JOIN ai_session s ON s.id=op.session_id AND s.state='open'
        JOIN ir_approval_request r ON r.requester_id=op.owner_user_id AND r.request_key=op.request_key
        WHERE r.id=$1 AND op.owner_user_id=$2 AND op.company_id=$3",
        &[&approval,&uid,&company]).await?;
    let row = rows
        .first()
        .ok_or_else(|| Error::access("The original approval operation is unavailable."))?;
    let session = row.try_get(3).map_err(shape)?;
    let turn = erp_sdk::Uuid::parse(row.try_get(4).map_err(shape)?)?;
    let key: &str = row.try_get(0).map_err(shape)?;
    let receipt = visible_receipts(tx, session, turn, Some(key))
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| Error::access("The original approval operation is unavailable."))?;
    if receipt["status"] != "awaiting_approval" {
        return Ok(Resume::Receipt(receipt));
    }
    Ok(Resume::Dispatch {
        key: key.to_owned(),
        name: row.try_get(1).map_err(shape)?,
        args: row.try_get(2).map_err(shape)?,
    })
}

fn shape(error: impl std::error::Error + Send + Sync + 'static) -> Error {
    Error::db("Invalid assistant operation receipt.").with_source(error)
}

/// Record a native action's destination in the transaction that created it.
/// Only model and record ID are retained, never rendered text or report values.
///
/// # Errors
/// Returns an error if a prepared operation or response is malformed.
pub async fn record_result(tx: &mut Transaction, key: &str, body: &[u8]) -> Result<()> {
    if body.len() > 16384 {
        return Ok(());
    }
    let value: serde_json::Value = serde_json::from_slice(body).map_err(shape)?;
    if let Some((model, id)) = destination(&value) {
        if id <= 0 || model.len() > 128 || tx.registry().model(model).is_none() {
            return Err(Error::invariant("Invalid assistant result record."));
        }
        let (uid, company) = actor(tx)?;
        tx.execute("UPDATE ai_operation SET result_model=$4,result_id=$5 WHERE owner_user_id=$1 AND company_id=$2 AND request_key=$3 AND completed",
            &[&uid,&company,&key,&model,&id]).await?;
    }
    Ok(())
}

/// Return the record intended for navigation by a native action.
#[must_use]
pub fn destination(value: &serde_json::Value) -> Option<(&str, i64)> {
    let target = match value["type"].as_str()? {
        "ir.actions.act_window" => value,
        "ir.actions.client" if value["tag"] == "display_notification" => {
            value.get("source_record")?
        }
        _ => return None,
    };
    Some((target["res_model"].as_str()?, target["res_id"].as_i64()?))
}

async fn readable(tx: &mut Transaction, model: &str, ids: &[i64]) -> Result<bool> {
    if tx.registry().model(model).is_none() {
        return Ok(false);
    }
    let records = tx.env().records(model)?.browse(ids.iter().copied())?;
    if !records.has_access(tx, erp_sdk::Operation::Read).await? {
        return Ok(false);
    }
    Ok(ids.is_empty() || records.exists(tx).await?.ids().len() == ids.len())
}

/// Get the status of each saved mutation using the reader's current rights. Returning a
/// native reference requires read access to the referenced record at the time of retrieval.
///
/// # Errors
/// Refuses foreign sessions, changed companies and malformed private receipts.
pub async fn receipts(
    tx: &mut Transaction,
    session: i64,
    request: erp_sdk::Uuid,
) -> Result<Vec<serde_json::Value>> {
    visible_receipts(tx, session, request, None).await
}

async fn visible_receipts(
    tx: &mut Transaction,
    session: i64,
    request: erp_sdk::Uuid,
    key: Option<&str>,
) -> Result<Vec<serde_json::Value>> {
    let (uid, company) = actor(tx)?;
    let rows=tx.query("SELECT o.tool::text,o.arguments,o.completed,r.id,o.result_model::text,o.result_id::bigint,
        CASE WHEN r.state IN ('pending','approved') AND (r.expires_at<=clock_timestamp() OR r.revision_archived_at IS DISTINCT FROM p.last_archived_at)
            THEN 'expired' ELSE r.state::text END
        FROM ai_operation o JOIN ai_session s ON s.id=o.session_id AND s.owner_user_id=o.owner_user_id AND s.company_id=o.company_id
        LEFT JOIN ir_approval_request r ON r.requester_id=o.owner_user_id AND r.request_key=o.request_key
        LEFT JOIN ir_approval_policy p ON p.id=r.revision_id
        WHERE o.owner_user_id=$1 AND o.company_id=$2 AND o.session_id=$3 AND o.turn_request=$4 AND ($5::text IS NULL OR o.request_key=$5) ORDER BY o.id",
        &[&uid,&company,&session,&request.to_string(),&key]).await?;
    let mut receipts = Vec::new();
    for row in rows {
        let tool: String = row.try_get(0).map_err(shape)?;
        let model = tool
            .rsplit_once('.')
            .ok_or_else(|| Error::invariant("Invalid operation name."))?
            .0;
        let args: serde_json::Value =
            serde_json::from_str(row.try_get::<_, &str>(1).map_err(shape)?).map_err(shape)?;
        let ids: Vec<i64> = args
            .get("ids")
            .map(|ids| serde_json::from_value(ids.clone()))
            .transpose()
            .map_err(shape)?
            .unwrap_or_default();
        let source_visible = readable(tx, model, &ids).await?;
        let done: bool = row.try_get(2).map_err(shape)?;
        let approval: Option<i64> = row.try_get(3).map_err(shape)?;
        let state: Option<&str> = row.try_get(6).map_err(shape)?;
        let status = if done {
            "completed"
        } else {
            match state {
                Some("pending" | "approved") => "awaiting_approval",
                Some("denied") => "denied",
                Some("expired") => "expired",
                Some(_) => "closed",
                None => "incomplete",
            }
        };
        if !source_visible && !readable(tx, model, &[]).await? {
            continue;
        }
        let status = match (source_visible, status) {
            (false, "awaiting_approval" | "incomplete") => "unavailable",
            _ => status,
        };
        let mut item = serde_json::json!({"name":tool,"model":model,"ids":if source_visible { ids } else { Vec::new() },"status":status});
        if source_visible && let Some(id) = approval {
            item["approval_request_id"] = serde_json::json!(id);
        }
        if let (Some(target), Some(id)) = (
            row.try_get::<_, Option<&str>>(4).map_err(shape)?,
            row.try_get::<_, Option<i64>>(5).map_err(shape)?,
        ) && readable(tx, target, &[id]).await?
        {
            item["result"] = serde_json::json!({"res_model":target,"res_id":id});
        }
        receipts.push(item);
    }
    Ok(receipts)
}
