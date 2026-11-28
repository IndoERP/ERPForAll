// SPDX-License-Identifier: MIT OR Apache-2.0
//! Budget admission and durable metering for explicit, single-message text turns.
use erp_sdk::{prelude::*, settings::ParameterDecl};

/// Daily or monthly spending policy for one company, optionally further restricted to a user.
/// No default budget authorizes spending. Rates are micro-units of the company currency per
/// thousand input/output tokens for the exact provider and model named by the operator.
#[model(name="ai.budget",description="Assistant Budget", company_access=direct(field="company_id",branch="exact",empty="deny"), constraints=[
    ("_scope","UNIQUE NULLS NOT DISTINCT (company_id,user_id)","Only one assistant budget is allowed per scope."),
    ("_pricing","CHECK (user_id IS NOT NULL OR input_rate > 0 OR output_rate > 0 OR COALESCE(confirmed_free,false))","Explicitly confirm a free provider or enter the applicable rates."),
    ("_user_pricing","CHECK (user_id IS NULL OR (input_rate=0 AND output_rate=0 AND NOT COALESCE(confirmed_free,false)))","A user limit inherits pricing from the company budget."),
    ("_limits","CHECK (token_limit BETWEEN 1 AND 1000000000 AND cost_limit BETWEEN 1 AND 1000000000000 AND input_rate BETWEEN 0 AND 1000000 AND output_rate BETWEEN 0 AND 1000000)","Enter bounded positive allowances and nonnegative model rates.")
])]
struct AiBudget {
    #[field(char, required = true, size = 128)]
    name: Option<Arc<str>>,
    #[field(
        many2one,
        comodel = "res.company",
        required = true,
        ondelete = "cascade",
        index = true
    )]
    company_id: Option<RecId>,
    #[field(many2one, comodel = "res.users", ondelete = "cascade", index = true)]
    user_id: Option<RecId>,
    #[field(
        many2one,
        comodel = "ai.provider",
        required = true,
        ondelete = "restrict"
    )]
    provider_id: Option<RecId>,
    #[field(char, required = true, size = 1024)]
    model: Option<Arc<str>>,
    #[field(selection,required=true,default="month",selection=[("day","Day"),("month","Month")])]
    period: Option<Arc<str>>,
    #[field(integer, required = true)]
    token_limit: i32,
    #[field(bigint, required = true)]
    cost_limit: erp_sdk::BigInt,
    #[field(integer, required = true, default = 0)]
    input_rate: i32,
    #[field(boolean, default = false)]
    confirmed_free: bool,
    #[field(integer, required = true, default = 0)]
    output_rate: i32,
    #[field(bigint, readonly = true)]
    revision: erp_sdk::BigInt,
}

/// One reservation and settlement for one real dispatch. No customer prompt or reply is stored.
#[model(name="ai.usage",description="Assistant Usage",order="id desc",company_access=direct(field="company_id",branch="exact",empty="deny"), constraints=[
   ("_active_session","UNIQUE(active_session)","An assistant turn is already running in this session."),
   ("_user_slot","UNIQUE(user_id,active_slot)","Another assistant turn took this user slot.")
])]
struct AiUsage {
    #[field(
        many2one,
        comodel = "res.company",
        required = true,
        ondelete = "restrict",
        index = true
    )]
    company_id: Option<RecId>,
    #[field(
        many2one,
        comodel = "res.users",
        required = true,
        ondelete = "restrict",
        index = true
    )]
    user_id: Option<RecId>,
    #[field(char, required = true, size = 63)]
    provider: Option<Arc<str>>,
    #[field(char, required = true, size = 1024)]
    model: Option<Arc<str>>,
    #[field(char, size = 64, groups = "base.group_system")]
    active_session: Option<Arc<str>>,
    #[field(bigint, groups = "base.group_system")]
    active_slot: Option<erp_sdk::BigInt>,
    #[field(selection,required=true,selection=[("reserved","Reserved"),("settled","Settled"),("failed","Failed"),("expired","Expired")])]
    state: Option<Arc<str>>,
    #[field(datetime, required = true, index = true)]
    started_at: Option<DateTime>,
    #[field(datetime, required = true)]
    expires_at: Option<DateTime>,
    #[field(bigint, required = true)]
    tokens: erp_sdk::BigInt,
    #[field(bigint, required = true)]
    cost: erp_sdk::BigInt,
    #[field(bigint)]
    input_tokens: erp_sdk::BigInt,
    #[field(bigint)]
    output_tokens: erp_sdk::BigInt,
    #[field(bigint)]
    elapsed_ms: erp_sdk::BigInt,
}

fn parameters() -> Result<Vec<ParameterDecl>> {
    Ok(vec![
        ParameterDecl::integer(
            "ai_base.turn_message_bytes",
            "Assistant message length in bytes",
            4096,
            (1, 32768),
        )?,
        ParameterDecl::integer(
            "ai_base.turn_output_tokens",
            "Assistant output token limit",
            512,
            (1, 4096),
        )?,
        ParameterDecl::integer(
            "ai_base.tab_history_turns",
            "Recent assistant exchanges per browser tab",
            32,
            (1, 200),
        )?,
        ParameterDecl::integer(
            "ai_base.tab_history_bytes",
            "Assistant transcript storage per company and tab in bytes",
            262144,
            (4096, 2097152),
        )?,
        ParameterDecl::integer(
            "ai_base.turn_user_concurrency",
            "Concurrent assistant turns per user",
            2,
            (1, 8),
        )?,
        ParameterDecl::integer(
            "ai_base.answer_correction_rounds",
            "Assistant retries when a final reply is unusable",
            1,
            (1, 3),
        )?,
    ])
}
erp_sdk::system_parameters!(parameters);

/// Browser transcript policy shared by the dock and the dedicated workspace.
pub async fn tab_history(tx: &mut Transaction) -> Result<serde_json::Value> {
    let (turns, bytes, message) = history_bounds(tx).await?;
    Ok(serde_json::json!({ "turns": turns, "bytes": bytes, "message_bytes": message }))
}

/// The typed transcript bounds a turn re-validates submitted history against:
/// `(exchanges, transcript bytes, message bytes)`. The transcript byte bound is measured on the
/// history's serialized JSON (role and text keys included), the same shape the tab resends;
/// single-entry text is measured in UTF-8 bytes against `turn_message_bytes`.
pub async fn history_bounds(tx: &mut Transaction) -> Result<(usize, usize, usize)> {
    let turns = crate::catalog::limit(tx, "ai_base.tab_history_turns").await?;
    let bytes = crate::catalog::limit(tx, "ai_base.tab_history_bytes").await?;
    let message = crate::catalog::limit(tx, "ai_base.turn_message_bytes").await?;
    Ok((turns, bytes, message))
}

/// Fixed, user-independent bounds read from the current database.
pub async fn limits(tx: &mut Transaction) -> Result<(usize, u32)> {
    let bytes = crate::catalog::limit(tx, "ai_base.turn_message_bytes").await?;
    let tokens = crate::catalog::limit(tx, "ai_base.turn_output_tokens").await?;
    Ok((
        bytes,
        u32::try_from(tokens).map_err(|_| Error::validation("Invalid assistant output limit."))?,
    ))
}

/// The identifier and snapshotted rates of a committed reservation; it contains no session token.
#[derive(Clone, Copy)]
pub struct Reservation {
    id: i64,
    company: i64,
    input_rate: i64,
    output_rate: i64,
}

fn cost(input: i64, output: i64, input_rate: i64, output_rate: i64) -> Result<i64> {
    input
        .checked_mul(input_rate)
        .and_then(|i| {
            output
                .checked_mul(output_rate)
                .and_then(|o| i.checked_add(o))
        })
        .and_then(|total| total.checked_add(999))
        .map(|n| n / 1000)
        .ok_or_else(|| Error::validation("erp.ai.usage_invalid: invalid metered cost."))
}

// All admission and settlement transactions lock a user before touching its company budget.
// The shared budget generation is the conflict witness across *different* users' snapshots.
async fn lock_user(tx: &mut Transaction, uid: i64) -> Result<()> {
    tx.query(
        "SELECT pg_advisory_xact_lock(hashtextextended('ai.turn.user.'||$1::bigint::text,0))",
        &[&uid],
    )
    .await?;
    Ok(())
}

/// Reserve one bounded provider exchange against every applicable policy. A company row is
/// required and its provider/model must match. This transaction must commit before network I/O.
///
/// The caller's `input` estimate is the byte length of the entire serialized provider payload
/// (system instructions, resent transcript, message, tools and framing) and is metered against
/// the budget's `token_limit` without conversion: a budget sized in tokens therefore caps the
/// request at roughly that many payload bytes. Raising `ai_base.tab_history_bytes` without
/// re-sizing the budget shortens the usable conversation before any token is spent.
/// # Errors
/// Refuses missing budgets, exhausted limits, admission contention and malformed usage estimates.
pub async fn reserve(
    tx: &mut Transaction,
    session: &str,
    route: &crate::routing::ConfiguredRoute,
    input: i64,
    output: u32,
    expiry_seconds: i32,
) -> Result<Reservation> {
    if !crate::ordinary_user(&tx.env()) {
        return Err(Error::access("An interactive session is required."));
    }
    let uid = tx.env().user_id().get();
    let company = tx
        .env()
        .company_id()?
        .ok_or_else(|| Error::access("Select a company."))?;
    let concurrency =
        i64::try_from(crate::catalog::limit(tx, "ai_base.turn_user_concurrency").await?)
            .map_err(|_| Error::validation("Invalid assistant concurrency limit."))?;
    lock_user(tx, uid).await?;
    tx.execute("UPDATE ai_usage SET state='expired',active_session=NULL,active_slot=NULL WHERE user_id=$1 AND state='reserved' AND expires_at <= (clock_timestamp() AT TIME ZONE 'UTC')",&[&uid]).await?;
    let busy=tx.query("SELECT count(*),count(*) FILTER(WHERE active_session=$2) FROM ai_usage WHERE user_id=$1 AND state='reserved'",&[&uid,&session]).await?;
    if busy[0].get::<_, i64>(0) >= concurrency || busy[0].get::<_, i64>(1) > 0 {
        return Err(Error::user(
            "erp.ai.busy: another assistant turn is in progress.",
        ));
    }
    // A real write, even under repeatable-read, either serializes this policy revision or fails
    // before transmission. Both company and optional user budgets participate in the same tx.
    let budgets=tx.query("UPDATE ai_budget b SET revision=COALESCE(revision,0)+1 FROM ai_provider p WHERE b.provider_id=p.id AND b.company_id=$1 AND (b.user_id IS NULL OR b.user_id=$2) RETURNING b.user_id,b.period,b.token_limit,b.cost_limit,b.input_rate,b.output_rate,p.name,b.model",&[&company,&uid]).await?;
    let company_policy = budgets
        .iter()
        .find(|r| r.get::<_, Option<i64>>(0).is_none())
        .ok_or_else(|| {
            Error::user("erp.ai.budget_missing: configure an assistant budget for this company.")
        })?;
    let in_rate = i64::from(company_policy.get::<_, i32>(4));
    let out_rate = i64::from(company_policy.get::<_, i32>(5));
    let tokens = input
        .checked_add(i64::from(output))
        .ok_or_else(|| Error::validation("Invalid request estimate."))?;
    let reserved_cost = cost(input, output.into(), in_rate, out_rate)?;
    for row in budgets {
        if row.get::<_, String>(6) != route.provider() || row.get::<_, String>(7) != route.model() {
            return Err(Error::user(
                "erp.ai.budget_missing: update the budget for the selected provider and model.",
            ));
        }
        let user: Option<i64> = row.get(0);
        let period: String = row.get(1);
        let sums=tx.query("SELECT COALESCE(SUM(tokens),0)::bigint,COALESCE(SUM(cost),0)::bigint FROM ai_usage WHERE company_id=$1 AND ($2::bigint IS NULL OR user_id=$2) AND started_at >= date_trunc($3::text,clock_timestamp() AT TIME ZONE 'UTC')",&[&company,&user,&period]).await?;
        let max_tokens = i64::from(row.get::<_, i32>(2));
        let max_cost: i64 = row.get(3);
        if sums[0].get::<_, i64>(0) > max_tokens - tokens
            || sums[0].get::<_, i64>(1) > max_cost - reserved_cost
        {
            return Err(Error::user(if user.is_some() {
                "erp.ai.budget_exhausted: the user allowance is insufficient."
            } else {
                "erp.ai.budget_exhausted: the company allowance is insufficient."
            }));
        }
    }
    let provider = route.provider();
    let model = route.model();
    // The unique constraints close the snapshot race for a user signing in to two companies.
    let inserted=tx.query("INSERT INTO ai_usage (company_id,user_id,provider,model,active_session,active_slot,state,started_at,expires_at,tokens,cost) SELECT $1,$2,$3,$4,$5,s,'reserved',clock_timestamp() AT TIME ZONE 'UTC',(clock_timestamp() AT TIME ZONE 'UTC')+make_interval(secs=>$6::int),$7,$8 FROM generate_series(1::bigint,$9::bigint) s WHERE NOT EXISTS (SELECT 1 FROM ai_usage running WHERE running.user_id=$2 AND running.active_slot=s) ORDER BY s LIMIT 1 ON CONFLICT DO NOTHING RETURNING id",&[&company,&uid,&provider,&model,&session,&expiry_seconds,&tokens,&reserved_cost,&concurrency]).await?;
    let id = inserted
        .first()
        .ok_or_else(|| Error::user("erp.ai.busy: another assistant turn is in progress."))?
        .get(0);
    Ok(Reservation {
        id,
        company,
        input_rate: in_rate,
        output_rate: out_rate,
    })
}
impl Reservation {
    /// Settle to verified usage once. Ambiguous errors retain the reserved allowance and release
    /// the admission lease. This native callback cannot choose another ledger row or policy.
    /// # Errors
    /// Fails when the settlement is unknown or a response's arithmetic cannot be represented.
    pub async fn settle(&self, tx: &mut Transaction, usage: Option<(i64, i64)>) -> Result<()> {
        self.settle_lease(tx, usage, false).await
    }

    /// Release a live lease when replacing it or publishing a completed reply. An expired
    /// lease can still be charged by the accounting finalizer, but cannot publish stale work.
    /// # Errors
    /// Refuses completion if another turn could have taken over or the charge cannot be stored.
    pub async fn settle_active(
        &self,
        tx: &mut Transaction,
        usage: Option<(i64, i64)>,
    ) -> Result<()> {
        self.settle_lease(tx, usage, true).await
    }

    async fn settle_lease(
        &self,
        tx: &mut Transaction,
        usage: Option<(i64, i64)>,
        require_active: bool,
    ) -> Result<()> {
        let uid = tx.env().user_id().get();
        let (input, output) = usage.unzip();
        let tokens = usage
            .map(|(i, o)| {
                i.checked_add(o)
                    .ok_or_else(|| Error::validation("Invalid token count."))
            })
            .transpose()?;
        let spent = usage
            .map(|(i, o)| cost(i, o, self.input_rate, self.output_rate))
            .transpose()?;
        // Move the policy generation in the same transaction as the actual usage charge.
        // An admission with an older repeatable-read snapshot cannot spend against the
        // pre-settlement amount after this commit. Removed policies still leave usage recorded.
        lock_user(tx, uid).await?;
        tx.execute(
            "UPDATE ai_budget SET revision=COALESCE(revision,0)+1
             WHERE company_id=$1 AND (user_id IS NULL OR user_id=$2)",
            &[&self.company, &uid],
        )
        .await?;
        let state = if usage.is_some() { "settled" } else { "failed" };
        let n=tx.execute("UPDATE ai_usage SET state=$3,active_session=NULL,active_slot=NULL,input_tokens=$4,output_tokens=$5,tokens=COALESCE($6,tokens),cost=COALESCE($7,cost),elapsed_ms=(extract(epoch FROM ((clock_timestamp() AT TIME ZONE 'UTC')-started_at))*1000)::bigint WHERE id=$1 AND user_id=$2 AND state IN ('reserved','expired') AND (NOT $8::boolean OR (state='reserved' AND expires_at > clock_timestamp() AT TIME ZONE 'UTC'))",&[&self.id,&uid,&state,&input,&output,&tokens,&spent,&require_active]).await?;
        if n != 1 {
            if require_active {
                return Err(Error::user(
                    "erp.ai.lease_expired: the active turn expired. Reopen the conversation to check its result.",
                ));
            }
            // The final commit can have reached PostgreSQL even if the connection did not
            // deliver its confirmation. A repeat of the identical charge is already settled.
            let same = tx.query("SELECT 1 FROM ai_usage WHERE id=$1 AND user_id=$2 AND state=$3 AND input_tokens IS NOT DISTINCT FROM $4 AND output_tokens IS NOT DISTINCT FROM $5", &[&self.id,&uid,&state,&input,&output]).await?;
            if same.is_empty() {
                return Err(Error::internal("Assistant metering could not be settled."));
            }
        }
        Ok(())
    }
}
