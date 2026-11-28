// SPDX-License-Identifier: MIT OR Apache-2.0
//! The server-owned transcript for interactive turns. Retrieved business values are read anew.
use crate::session::{self, AiSession, NewEvent, NewSession};
use erp_sdk::prelude::*;
use serde_json::{Value as Json, json};

/// One accepted browser request. A unique row arbitrates simultaneous retries across nodes,
/// including submissions made before the browser learned its new conversation ID.
#[model(name = "ai.submission", description = "Assistant Turn Submission",
    company_access = unrestricted(reason = "private idempotency keys have no model access grants and are claimed only inside an owner-checked conversation"),
    constraints = [("_owner_request", "UNIQUE(owner_user_id, request_id)", "This assistant request has already been submitted.")])]
pub struct AiSubmission {
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
    #[field(uuid, required = true)]
    pub request_id: Option<Uuid>,
}

const VOLATILE_ANSWER: &str =
    "ERP data was used for this answer. Ask for the current figures to refresh it.";
const OVERSIZED_ANSWER: &str = "This reply exceeded the saved history limit.";

fn identity(tx: &Transaction) -> Result<(i64, i64)> {
    let env = tx.env();
    if !crate::ordinary_user(&env) {
        return Err(Error::access("An interactive user is required."));
    }
    let company = env
        .company_id()?
        .ok_or_else(|| Error::access("Select a company."))?;
    Ok((env.user_id().get(), company))
}

async fn check(tx: &mut Transaction, id: i64, open: bool) -> Result<()> {
    let (uid, company) = identity(tx)?;
    let rows = tx
        .query(
            "SELECT state::text FROM ai_session WHERE id=$1 AND owner_user_id=$2 AND company_id=$3",
            &[&id, &uid, &company],
        )
        .await?;
    let Some(row) = rows.first() else {
        return Err(Error::access(
            "This conversation is unavailable in the active company.",
        ));
    };
    if open && row.get::<_, String>(0) != "open" {
        return Err(Error::user(
            "erp.ai.session_archived: restore or fork this conversation to continue.",
        ));
    }
    Ok(())
}

/// Navigation metadata for the signed-in browser. The framework authorizes the action when opened.
/// # Errors
/// Returns a configuration error for a missing workspace or underlying database failures.
pub async fn workspace_action(tx: &mut Transaction) -> Result<i64> {
    let rows = tx.query("SELECT res_id::bigint FROM ir_model_data WHERE module='ai_base' AND name='workspace_action' AND model='ir.actions.client'", &[]).await?;
    rows.first()
        .and_then(|row| row.get::<_, Option<i64>>(0))
        .ok_or_else(|| Error::invariant("Missing AI Assistant workspace action."))
}

/// Admit and record a user message in the transaction that reserves the turn budget.
/// # Errors
/// Returns access, duplicate-submission, capacity, or persistence errors before dispatch.
pub async fn begin(
    tx: &mut Transaction,
    requested: Option<i64>,
    turn: Uuid,
    text: &str,
) -> Result<i64> {
    let (uid, company) = identity(tx)?;
    // Serializes simultaneous submissions even when both first requests have no session ID yet.
    tx.query(
        "SELECT pg_advisory_xact_lock(hashtextextended('ai.conversation.' || $1::bigint::text,0))",
        &[&uid],
    )
    .await?;
    let uuid = turn.to_string();
    let id = match requested {
        Some(id) => {
            check(tx, id, true).await?;
            id
        }
        None => {
            let mut new = NewSession::new(company);
            new.title = Some(text.chars().take(64).collect::<String>().into());
            session::open_session(tx, &new).await?
        }
    };
    session::admit_turn_with_capacity(tx, id, 3).await?;
    if crate::catalog::limit(tx, session::EVENT_BYTES).await? < 128 {
        return Err(Error::validation(
            "Assistant event payloads must allow at least 128 bytes for interactive receipts.",
        ));
    }
    if tx.query("INSERT INTO ai_submission (session_id,owner_user_id,request_id) VALUES ($1,$2,$3::text::uuid)
        ON CONFLICT (owner_user_id,request_id) DO NOTHING RETURNING id", &[&id,&uid,&uuid]).await?.is_empty() {
        return Err(Error::user("erp.ai.duplicate_turn: this request was already submitted. Open its conversation to check the result."));
    }
    session::append(
        tx,
        id,
        &NewEvent {
            kind: "user_message",
            version: 1,
            turn_id: Some(turn),
            payload: json!({"text":text}),
        },
    )
    .await?;
    Ok(id)
}

/// Record completion. Tool-derived text is volatile and is never replayed under older permissions.
/// # Errors
/// Returns access, capacity or persistence errors.
pub async fn finish(
    tx: &mut Transaction,
    id: i64,
    turn: Uuid,
    text: &str,
    rounds: i64,
    calls: i64,
) -> Result<()> {
    check(tx, id, false).await?;
    let cap = crate::catalog::limit(tx, session::EVENT_BYTES).await?;
    let snapshot = if calls > 0 {
        VOLATILE_ANSWER
    } else if json!({"text":text}).to_string().len() > cap {
        OVERSIZED_ANSWER
    } else {
        text
    };
    session::append(
        tx,
        id,
        &NewEvent {
            kind: "assistant_message",
            version: 1,
            turn_id: Some(turn),
            payload: json!({"text":snapshot}),
        },
    )
    .await?;
    session::append(tx,id,&NewEvent{kind:"turn_end",version:1,turn_id:Some(turn),payload:json!({"outcome":"interactive_completed","round_trips":rounds,"tool_calls":calls})}).await?;
    Ok(())
}

/// Recent conversations in chronological order. Only answers written by this transport can
/// be replayed. Historic unverified answers and incomplete turns are never model instructions.
/// # Errors
/// Returns an access, policy or persistence error.
pub async fn transcript(tx: &mut Transaction, id: i64, receipts: bool) -> Result<Vec<Json>> {
    check(tx, id, false).await?;
    let (turns, bytes, per_message) = crate::ledger::history_bounds(tx).await?;
    let limit =
        i64::try_from(turns).map_err(|_| Error::invariant("Invalid conversation bound."))?;
    let rows = tx.query(
        "WITH questions AS (SELECT seq,turn_id,user_text FROM ai_session_event WHERE session_id=$1 AND kind='user_message' AND turn_id IS NOT NULL ORDER BY seq DESC LIMIT $2)
         SELECT q.turn_id::text,q.user_text, a.payload->>'text',
          EXISTS(SELECT 1 FROM ai_operation o WHERE o.session_id=$1 AND o.turn_request=q.turn_id::text)
         FROM questions q LEFT JOIN LATERAL (
           SELECT e.payload FROM ai_session_event e WHERE e.session_id=$1 AND e.turn_id=q.turn_id AND e.kind='assistant_message'
           AND EXISTS (SELECT 1 FROM ai_session_event t WHERE t.session_id=$1 AND t.turn_id=q.turn_id AND t.kind='turn_end' AND t.payload->>'outcome'='interactive_completed')
           ORDER BY e.seq DESC LIMIT 1) a ON true ORDER BY q.seq DESC", &[&id,&limit]).await?;
    let mut pairs = Vec::new();
    let mut size = 2;
    for row in rows {
        let question: String = row.get(1);
        let answer: Option<String> = row.get(2);
        if answer.is_none() && !receipts && !row.get::<_, bool>(3) {
            continue;
        }
        let answer = answer.as_deref().unwrap_or(
            "This turn has no recorded completion. Check its operations before submitting again.",
        );
        if question.len() > per_message || answer.len() > per_message {
            continue;
        }
        let mut pair = [
            json!({"role":"user","text":question}),
            json!({"role":"assistant","text":answer}),
        ];
        if row.get::<_, bool>(3) {
            let turn = Uuid::parse(row.get(0))?;
            let ops = crate::operation::receipts(tx, id, turn).await?;
            if !ops.is_empty() {
                if receipts {
                    pair[1]["operations"] = json!(ops);
                } else {
                    let text = format!("ERP operation receipts: {}", json!(ops));
                    if text.len() <= per_message {
                        pair[1]["text"] = json!(text);
                    }
                }
            }
        }
        let length = serde_json::to_vec(&pair)
            .map_err(|_| Error::invariant("Invalid transcript"))?
            .len();
        if size + length > bytes {
            break;
        }
        size += length;
        pairs.push(pair);
    }
    Ok(pairs.into_iter().rev().flatten().collect())
}

#[methods(AiSession, addon = "ai_base")]
impl AiSession {
    #[method(override)]
    async fn write(
        this: &RecordSet<AiSession>,
        tx: &mut Transaction,
        vals: RawVals,
        sup: Super,
    ) -> Result<()> {
        if vals
            .iter()
            .any(|(key, _)| !matches!(key, "state" | "title"))
            || vals.iter_commands().next().is_some()
        {
            return Err(Error::access(
                "Only the title and archive state of a conversation can be edited.",
            ));
        }
        sup.call(tx, this.as_records(), (vals,)).await
    }

    #[method(sealed)]
    #[rpc(effect = read, model)]
    async fn locate_submission(
        _this: &RecordSet<AiSession>,
        tx: &mut Transaction,
        request: String,
    ) -> Result<Value> {
        let (uid, company) = identity(tx)?;
        let request = Uuid::parse(&request)?.to_string();
        let rows=tx.query("SELECT s.id::bigint FROM ai_submission r JOIN ai_session s ON s.id=r.session_id
            WHERE r.owner_user_id=$1 AND s.owner_user_id=$1 AND s.company_id=$2 AND r.request_id=$3::text::uuid", &[&uid,&company,&request]).await?;
        let id = rows.first().map(|row| row.get::<_, i64>(0));
        Ok(Value::Json(Arc::new(json!({"session_id":id}))))
    }

    #[method(sealed)]
    #[rpc(effect = read, model)]
    async fn conversation_history(
        _this: &RecordSet<AiSession>,
        tx: &mut Transaction,
        id: i64,
    ) -> Result<Value> {
        Ok(Value::Json(Arc::new(
            json!({"id":id,"messages":transcript(tx,id,true).await?}),
        )))
    }
}
