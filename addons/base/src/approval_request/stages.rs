//! Current authority and the immutable signatures for the stages of one request.
use super::shape;
use crate::ir_approval_policy::ELIGIBILITY;
use erp_sdk::prelude::*;
use erp_sdk::serde_json::{Value as Json, json};
use erp_sdk::{SUPERUSER_ID, UserId};

pub(super) struct Stage {
    pub position: i32,
    pub name: String,
    pub signer: Option<i64>,
    pub approved: Option<bool>,
    pub viewer_eligible: bool,
    pub signer_eligible: bool,
    signer_name: Option<String>,
    decided_at: Option<String>,
}

impl Stage {
    pub fn display(&self, current: i32, request_state: &str) -> Json {
        let state = match self.approved {
            Some(true) => "approved",
            Some(false) => "denied",
            None if self.position != current => "waiting",
            None if request_state == "expired" => "expired",
            None => "pending",
        };
        let mut stage = json!({"position":self.position,"name":self.name,"state":state});
        if let Some(name) = &self.signer_name {
            stage["decider_name"] = json!(name);
        }
        if let Some(at) = &self.decided_at {
            stage["decided_at"] = json!(at);
        }
        stage
    }
}

/// Resolve every stage in one statement. A single-stage request decided before the audit
/// table existed retains its recorded decider and carries no invented decision timestamp.
pub(super) async fn read(tx: &mut Transaction, request: i64, viewer: UserId) -> Result<Vec<Stage>> {
    let max =
        i64::try_from(crate::approval_settings::max_approvers(tx).await?).map_err(|_| shape())?;
    let sql = format!(
        r#"WITH RECURSIVE scopes AS (
         SELECT r.revision_id, r.companies, r.excluded, NULL::bigint AS viewer, n.step
         FROM ir_approval_request r JOIN ir_approval_policy p ON p.id=r.revision_id
         CROSS JOIN LATERAL generate_series(1,greatest(1,jsonb_array_length(nullif(p.review_steps,'null')))) n(step)
         WHERE r.id=$1
     ), {ELIGIBILITY}, signatures AS (
         SELECT e.*, COALESCE(d.reviewer_id, CASE WHEN e.step = 1
             AND greatest(0,jsonb_array_length(nullif(p.review_steps,'null'))) = 0 THEN r.decider_id END) AS signer,
             COALESCE(d.approved, CASE WHEN r.decider_id IS NOT NULL AND e.step = 1
                 AND greatest(0,jsonb_array_length(nullif(p.review_steps,'null'))) = 0 THEN r.state <> 'denied' END) AS approved,
             to_char(d.decided_at AT TIME ZONE 'UTC','YYYY-MM-DD"T"HH24:MI:SS.US"Z"') AS decided_at,
             coalesce(p.review_steps -> (e.step-1) ->> 'name',p.name) AS name
         FROM eligibility e JOIN ir_approval_policy p ON p.id=e.revision_id
         JOIN ir_approval_request r ON r.id=$1
         LEFT JOIN ir_approval_decision d ON d.request_id=r.id AND d.position=e.step
     )
     SELECT s.step, s.name, s.signer, s.approved, s.decided_at,
            coalesce(nullif(u.name,''),'User #' || u.id) AS signer_name,
            s.named <= $2 AND cardinality(s.users) <= $2 AND $4::bigint=ANY(s.users) AS viewer_eligible,
            s.named <= $2 AND cardinality(s.users) <= $2 AND coalesce(s.signer=ANY(s.users),false) AS signer_eligible
     FROM signatures s LEFT JOIN res_users u ON u.id=s.signer ORDER BY s.step"#
    );
    let rows = tx
        .query(&sql, &[&request, &max, &SUPERUSER_ID.get(), &viewer.get()])
        .await?;
    if rows.is_empty() {
        return Err(shape());
    }
    rows.iter()
        .map(|r| {
            Ok(Stage {
                position: r.try_get("step").map_err(|_| shape())?,
                name: r.try_get("name").map_err(|_| shape())?,
                signer: r.try_get("signer").map_err(|_| shape())?,
                approved: r.try_get("approved").map_err(|_| shape())?,
                decided_at: r.try_get("decided_at").map_err(|_| shape())?,
                signer_name: r.try_get("signer_name").map_err(|_| shape())?,
                viewer_eligible: r.try_get("viewer_eligible").map_err(|_| shape())?,
                signer_eligible: r.try_get("signer_eligible").map_err(|_| shape())?,
            })
        })
        .collect()
}
