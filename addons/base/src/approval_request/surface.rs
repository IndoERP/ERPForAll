//! Read-only ORM models for native list and form actions. The request and history views
//! contain only receipt fields. The private inputs remain on the lifecycle models.
use crate::ir_approval_policy::{ELIGIBILITY, MAX_COMPANIES};
use erp_sdk::__private::{BASE_MODEL_VIEWS, BaseModelView, linkme};
use erp_sdk::access::{AccessCtx, AccessSuper};
use erp_sdk::domain::{Domain, Value as D};
use erp_sdk::prelude::*;
use erp_sdk::{
    FieldSearchContext, FieldSearchRequest, Operation, Operator, PrincipalKind, SUPERUSER_ID,
};

#[linkme::distributed_slice(BASE_MODEL_VIEWS)]
#[linkme(crate = linkme)]
static REQUEST: BaseModelView = BaseModelView {
    model: "approval.request.view",
    select: request_sql,
};
#[linkme::distributed_slice(BASE_MODEL_VIEWS)]
#[linkme(crate = linkme)]
static SCOPE: BaseModelView = BaseModelView {
    model: "approval.request.scope",
    select: scope_sql,
};
#[linkme::distributed_slice(BASE_MODEL_VIEWS)]
#[linkme(crate = linkme)]
static STAGE: BaseModelView = BaseModelView {
    model: "approval.request.stage",
    select: stage_sql,
};

fn request_sql() -> String {
    format!("SELECT r.id, ('Request #' || r.id)::text AS name,
         CASE WHEN r.state IN ('pending','approved') AND NOT {live}
            THEN 'expired' ELSE r.state END AS state,
         r.target_model, r.target_method, coalesce(nullif(u.name,''),'User #' || r.requester_id) AS requester_name,
         p.name AS policy_name, r.current_step, greatest(1,jsonb_array_length(nullif(p.review_steps,'null'))) AS step_count,
         coalesce(p.review_steps -> (r.current_step-1) ->> 'name',p.name) AS current_step_name,
         {expiry} AT TIME ZONE 'UTC' AS expires_at,r.payload AS payload_sha256,r.operation AS execution_receipt
     FROM ir_approval_request r JOIN res_users u ON u.id=r.requester_id JOIN ir_approval_policy p ON p.id=r.revision_id",
        live = super::queue::LIVE, expiry = super::queue::EXPIRY)
}

fn scope_sql() -> String {
    let eligible = ELIGIBILITY
        .replace("$2", "(SELECT bound FROM limits)")
        .replace("$3", &SUPERUSER_ID.get().to_string());
    include_str!("visibility.sql")
        .replace("{companies}", &MAX_COMPANIES.to_string())
        .replace("{eligibility}", &eligible)
        .replace("{live}", super::queue::LIVE)
}

fn stage_sql() -> String {
    format!("WITH stages AS (
        SELECT r.id AS request_id,n.position,coalesce(p.review_steps -> (n.position-1) ->> 'name',p.name) AS name,
          coalesce(d.reviewer_id, CASE WHEN r.current_step=n.position AND greatest(0,jsonb_array_length(nullif(p.review_steps,'null')))=0
                   THEN r.decider_id END) AS signer,
          CASE WHEN d.id IS NOT NULL THEN CASE WHEN d.approved THEN 'approved' ELSE 'denied' END
               WHEN r.decider_id IS NOT NULL AND r.current_step=n.position THEN CASE WHEN r.state='denied' THEN 'denied' ELSE 'approved' END
               WHEN n.position<>r.current_step THEN 'waiting'
               WHEN NOT {live} THEN 'expired' ELSE 'pending' END AS state,d.decided_at
        FROM ir_approval_request r JOIN ir_approval_policy p ON p.id=r.revision_id
        CROSS JOIN LATERAL generate_series(1,greatest(1,jsonb_array_length(nullif(p.review_steps,'null')))) n(position)
        LEFT JOIN ir_approval_decision d ON d.request_id=r.id AND d.position=n.position
     )
     SELECT (s.request_id-1)*16+s.position AS id,s.request_id,s.position,s.name,s.state,
            coalesce(nullif(u.name,''),'User #' || u.id) AS decider_name,s.decided_at AT TIME ZONE 'UTC' AS decided_at
     FROM stages s LEFT JOIN res_users u ON u.id=s.signer", live = super::queue::LIVE)
}

#[model(
    name = "approval.request.view",
    description = "Approval Request",
    order = "id desc",
    log_access = false
)]
pub struct ApprovalRequestView {
    #[field(char, readonly)]
    pub name: Option<Arc<str>>,
    #[field(selection, selection=[("pending","Pending"),("approved","Approved"),("denied","Denied"),("executed","Executed"),("expired","Expired")], readonly)]
    pub state: Option<Arc<str>>,
    #[field(char, readonly)]
    pub target_model: Option<Arc<str>>,
    #[field(char, readonly)]
    pub target_method: Option<Arc<str>>,
    #[field(char, readonly)]
    pub requester_name: Option<Arc<str>>,
    #[field(char, readonly)]
    pub policy_name: Option<Arc<str>>,
    #[field(integer, readonly)]
    pub current_step: i32,
    #[field(integer, readonly)]
    pub step_count: i32,
    #[field(char, readonly)]
    pub current_step_name: Option<Arc<str>>,
    #[field(datetime, readonly)]
    pub expires_at: Option<DateTime>,
    #[field(char, readonly)]
    pub payload_sha256: Option<Arc<str>>,
    #[field(char, readonly)]
    pub execution_receipt: Option<Arc<str>>,
    #[field(char, compute = "_compute_flags", readonly)]
    pub company_names: Option<Arc<str>>,
    #[field(
        boolean,
        compute = "_compute_flags",
        readonly,
        search_method = "_search_flags"
    )]
    pub can_decide: bool,
    #[field(
        boolean,
        compute = "_compute_flags",
        readonly,
        search_method = "_search_flags"
    )]
    pub to_review: bool,
    #[field(
        boolean,
        compute = "_compute_flags",
        readonly,
        search_method = "_search_flags"
    )]
    pub is_mine: bool,
    #[field(
        boolean,
        compute = "_compute_flags",
        readonly,
        search_method = "_search_flags"
    )]
    pub is_reviewed: bool,
    #[field(
        one2many,
        comodel = "approval.request.stage",
        inverse_name = "request_id",
        readonly
    )]
    pub stage_ids: Vec<RecId>,
    #[field(
        one2many,
        comodel = "approval.request.scope",
        inverse_name = "request_id",
        readonly
    )]
    pub scope_ids: Vec<RecId>,
}

#[model(
    name = "approval.request.stage",
    description = "Approval Stage",
    order = "position",
    log_access = false
)]
pub struct ApprovalRequestStage {
    #[field(many2one, comodel = "approval.request.view", readonly)]
    pub request_id: Option<RecId>,
    #[field(integer, readonly)]
    pub position: i32,
    #[field(char, readonly)]
    pub name: Option<Arc<str>>,
    #[field(selection, selection=[("waiting","Waiting"),("pending","Pending"),("approved","Approved"),("denied","Denied"),("expired","Expired")], readonly)]
    pub state: Option<Arc<str>>,
    #[field(char, readonly)]
    pub decider_name: Option<Arc<str>>,
    #[field(datetime, readonly)]
    pub decided_at: Option<DateTime>,
}

#[model(
    name = "approval.request.scope",
    description = "My Approval Visibility",
    log_access = false
)]
pub struct ApprovalRequestScope {
    #[field(many2one, comodel = "approval.request.view", readonly)]
    pub request_id: Option<RecId>,
    #[field(many2one, comodel = "res.users", readonly)]
    pub viewer_id: Option<RecId>,
    #[field(
        boolean,
        compute = "_compute_session_visible",
        search_method = "_search_session_visible",
        readonly
    )]
    pub session_visible: bool,
    #[field(boolean, readonly)]
    pub to_review: bool,
    #[field(boolean, readonly)]
    pub is_mine: bool,
    #[field(boolean, readonly)]
    pub is_reviewed: bool,
}

// A search method in the rule is expanded with the calling environment on both record checks
// and searches. Never cache a viewer's identity into a compiled group-wide access rule.
fn session(env: &Env) -> bool {
    let p = env.principal();
    p.kind() == PrincipalKind::User && p.scope().is_none() && p.actor().is_none()
}
fn rule(ctx: &AccessCtx<'_>, op: Operation, sup: AccessSuper<'_>, binding: &str) -> Result<Domain> {
    Ok(if op == Operation::Read {
        sup.call(ctx, op)? & Domain::condition(binding, Operator::Eq, true)?
    } else {
        Domain::FALSE
    })
}

#[methods(ApprovalRequestScope, addon = "base")]
impl ApprovalRequestScope {
    #[compute(depends=["viewer_id"], depends_context=["uid"])]
    async fn _compute_session_visible(
        this: &RecordSet<ApprovalRequestScope>,
        _tx: &mut Transaction,
        out: &mut Assign<'_, ApprovalRequestScope>,
    ) -> Result<()> {
        for i in 0..this.len() {
            out.set(
                i,
                ApprovalRequestScope::SESSION_VISIBLE,
                session(this.env()),
            )?;
        }
        Ok(())
    }
    #[search_method]
    fn _search_session_visible(
        req: &FieldSearchRequest<'_>,
        cx: &mut FieldSearchContext<'_>,
    ) -> Result<Domain> {
        let own = if session(cx.env()) {
            Domain::condition("viewer_id", Operator::Eq, cx.env().uid())?
        } else {
            Domain::FALSE
        };
        bool_filter(req, own)
    }
    #[access_domain]
    fn _access_domain(ctx: &AccessCtx, op: Operation, sup: AccessSuper) -> Result<Domain> {
        rule(ctx, op, sup, "session_visible")
    }
}

#[methods(ApprovalRequestStage, addon = "base")]
impl ApprovalRequestStage {
    #[access_domain]
    fn _access_domain(ctx: &AccessCtx, op: Operation, sup: AccessSuper) -> Result<Domain> {
        rule(ctx, op, sup, "request_id.scope_ids.session_visible")
    }
}

#[methods(ApprovalRequestView, addon = "base")]
impl ApprovalRequestView {
    #[access_domain]
    fn _access_domain(ctx: &AccessCtx, op: Operation, sup: AccessSuper) -> Result<Domain> {
        rule(ctx, op, sup, "scope_ids.session_visible")
    }

    #[search_method]
    fn _search_flags(
        req: &FieldSearchRequest<'_>,
        cx: &mut FieldSearchContext<'_>,
    ) -> Result<Domain> {
        let name = match cx.field().name {
            "can_decide" => "to_review",
            other => other,
        };
        let matched = if session(cx.env()) {
            Domain::condition(
                "scope_ids",
                Operator::Any,
                D::Domain(
                    Domain::condition("viewer_id", Operator::Eq, cx.env().uid())?
                        & Domain::condition(name, Operator::Eq, true)?,
                ),
            )?
        } else {
            Domain::FALSE
        };
        bool_filter(req, matched)
    }

    #[compute(depends=["state","current_step","scope_ids"],depends_context=["uid"])]
    async fn _compute_flags(
        this: &RecordSet<ApprovalRequestView>,
        tx: &mut Transaction,
        out: &mut Assign<'_, ApprovalRequestView>,
    ) -> Result<()> {
        let uid = this.env().uid();
        let rows = tx
            .query(
                "SELECT s.request_id,s.to_review,s.is_mine,s.is_reviewed,
            (SELECT string_agg(coalesce(c.name,'Company #' || x.id),', ' ORDER BY x.seq)
             FROM jsonb_array_elements_text(r.companies) WITH ORDINALITY x(id,seq)
             LEFT JOIN res_company_users_rel m ON m.cid=x.id::bigint AND m.user_id=$2
             LEFT JOIN res_company c ON c.id=m.cid) AS companies
            FROM approval_request_scope s JOIN ir_approval_request r ON r.id=s.request_id
            WHERE s.request_id=ANY($1::bigint[]) AND s.viewer_id=$2",
                &[&this.ids(), &uid],
            )
            .await?;
        let mut data = std::collections::BTreeMap::new();
        for r in rows {
            data.insert(r.try_get::<_, i64>(0).map_err(|_| super::shape())?, r);
        }
        for (i, id) in this.ids().into_iter().enumerate() {
            let r = data.get(&id).ok_or_else(super::concealed)?;
            let review: bool = r.try_get(1).map_err(|_| super::shape())?;
            out.set(
                i,
                ApprovalRequestView::CAN_DECIDE,
                review && session(this.env()),
            )?;
            out.set(
                i,
                ApprovalRequestView::TO_REVIEW,
                review && session(this.env()),
            )?;
            out.set(
                i,
                ApprovalRequestView::IS_MINE,
                r.try_get::<_, bool>(2).map_err(|_| super::shape())?,
            )?;
            out.set(
                i,
                ApprovalRequestView::IS_REVIEWED,
                r.try_get::<_, bool>(3).map_err(|_| super::shape())? && session(this.env()),
            )?;
            let names: String = r.try_get(4).map_err(|_| super::shape())?;
            out.set(i, ApprovalRequestView::COMPANY_NAMES, names.as_str())?;
        }
        Ok(())
    }

    #[method]
    #[rpc(effect=write)]
    async fn action_approve(
        this: &RecordSet<ApprovalRequestView>,
        tx: &mut Transaction,
    ) -> Result<()> {
        decide(this, tx, true).await
    }
    #[method]
    #[rpc(effect=write)]
    async fn action_deny(
        this: &RecordSet<ApprovalRequestView>,
        tx: &mut Transaction,
    ) -> Result<()> {
        decide(this, tx, false).await
    }
}

fn bool_filter(req: &FieldSearchRequest<'_>, matched: Domain) -> Result<Domain> {
    use Operator::{Eq, In, Ne, NotIn};
    let include = |flag| match (req.operator(), req.operand()) {
        (Eq, D::Bool(v)) => Ok(flag == *v),
        (Ne, D::Bool(v)) => Ok(flag != *v),
        (In, D::List(v)) => Ok(v.contains(&D::Bool(flag))),
        (NotIn, D::List(v)) => Ok(!v.contains(&D::Bool(flag))),
        (In, D::Set(v)) => Ok(v.contains(&D::Bool(flag))),
        (NotIn, D::Set(v)) => Ok(!v.contains(&D::Bool(flag))),
        _ => Err(Error::validation(
            "Approval scope filters require a boolean comparison.",
        )),
    };
    Ok(match (include(true)?, include(false)?) {
        (true, false) => matched,
        (false, true) => !matched,
        (true, true) => Domain::TRUE,
        (false, false) => Domain::FALSE,
    })
}

async fn decide(
    this: &RecordSet<ApprovalRequestView>,
    tx: &mut Transaction,
    approve: bool,
) -> Result<()> {
    this.as_records().check_access(tx, Operation::Read).await?;
    let id = this.as_records().ensure_one()?.int()?;
    let context = this.env().context();
    let (Some(Value::Int(step)), Some(Value::Str(digest))) =
        (context.get("approval_step"), context.get("approval_digest"))
    else {
        return Err(Error::validation(
            "Refresh the approval and decide from its displayed stage and digest.",
        ));
    };
    super::review(tx, id, Some(approve), Some(&digest), Some(step)).await?;
    Ok(())
}
