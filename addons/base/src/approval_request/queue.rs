//! A count and one page from the same set of visible requests.
use super::{concealed, shape};
use crate::ir_approval_policy::{ELIGIBILITY, MAX_COMPANIES};
use erp_sdk::prelude::*;
use erp_sdk::serde_json::Value as Json;
use erp_sdk::{PrincipalKind, SUPERUSER_ID};

/// Fields shared by the queue and individual receipts. No credential or submitted arguments.
pub(super) const SUMMARY: &str = r#"jsonb_build_object(
    'id', r.id, 'model', r.target_model, 'method', r.target_method,
    'requester_id', r.requester_id,
    'requester_name', coalesce(nullif(u.name, ''), 'User #' || r.requester_id),
    'policy_name', p.name, 'companies', r.companies,
    'current_step', r.current_step, 'step_count', greatest(1, jsonb_array_length(nullif(p.review_steps,'null'))),
    'current_step_name', coalesce(p.review_steps -> (r.current_step - 1) ->> 'name', p.name),
    'expires_at', to_char(r.expires_at AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS.US"Z"')
)"#;

pub(super) const JOINS: &str = "JOIN ir_approval_policy p ON p.id = r.revision_id
                                JOIN res_users u ON u.id = r.requester_id";

/// Scope and bind placeholders come only from the branches below. An empty or past-end page
/// still returns the total. LIMIT 0 skips the summary joins entirely.
fn page(visible: &str, pagination: &str, can_decide: bool) -> String {
    format!(
        "WITH RECURSIVE {visible},
         page AS (
             SELECT id FROM visible ORDER BY id DESC {pagination}
         )
         SELECT jsonb_build_object(
             'total', (SELECT count(*) FROM visible),
             'items', coalesce((SELECT jsonb_agg({SUMMARY} || jsonb_build_object(
                 'state', CASE WHEN r.state IN ('pending', 'approved')
                                 AND r.expires_at <= statement_timestamp()
                               THEN 'expired' ELSE r.state END,
                 'can_decide', {can_decide}) ORDER BY r.id DESC)
                 FROM page JOIN ir_approval_request r ON r.id = page.id {JOINS}), '[]'::jsonb)
         )"
    )
}

pub(super) async fn queue(
    tx: &mut Transaction,
    scope: &str,
    offset: Option<i64>,
    limit: Option<i64>,
) -> Result<Value> {
    let env = tx.env();
    let principal = env.principal();
    if principal.kind() != PrincipalKind::User {
        return Err(concealed());
    }
    let offset = offset.unwrap_or(0);
    let limit = limit.unwrap_or(25);
    if offset < 0 || !(0..=100).contains(&limit) {
        return Err(Error::validation(
            "Approval queue offset must be nonnegative and limit must be between 0 and 100.",
        ));
    }
    let uid = principal.subject().get();
    let rows = match scope {
        "mine" => {
            let sql = page(
                "visible AS (SELECT id FROM ir_approval_request WHERE requester_id = $1)",
                "LIMIT $2 OFFSET $3",
                false,
            );
            tx.query(&sql, &[&uid, &limit, &offset]).await?
        }
        "review" | "reviewed" => {
            let actionable = scope == "review";
            if principal.actor().is_some() {
                return Err(concealed());
            }
            // Previous-stage reviewers see their history only while their authority for the
            // stage they signed still holds. Old single-stage receipts have no decision row.
            let stages = if actionable {
                "SELECT r.current_step AS step WHERE r.state = 'pending' AND r.expires_at > statement_timestamp()"
            } else {
                "SELECT d.position AS step FROM ir_approval_decision d WHERE d.request_id=r.id AND d.reviewer_id=$1
                 UNION SELECT r.current_step WHERE r.decider_id=$1"
            };
            // Evaluate each distinct policy/company/exclusion set once. Invalid or over-limit
            // revisions cannot be decided and contribute neither items nor counts.
            let visible = format!(
                "requests AS (
                     SELECT r.id, r.revision_id, r.companies, r.excluded, levels.step
                     FROM ir_approval_request r JOIN ir_approval_policy p ON p.id = r.revision_id
                     CROSS JOIN LATERAL ({stages}) levels
                     WHERE p.active AND r.requester_id <> $1
                       AND NOT r.excluded @> to_jsonb($1::bigint)
                       AND jsonb_array_length(r.companies) BETWEEN 1 AND {MAX_COMPANIES}
                 ),
                 scopes AS (SELECT DISTINCT revision_id, companies, excluded, step, $1::bigint AS viewer FROM requests),
                 {ELIGIBILITY},
                 visible AS (
                     SELECT DISTINCT r.id FROM requests r JOIN eligibility e USING (revision_id, companies, excluded, step)
                     WHERE e.named <= $2 AND cardinality(e.users) <= $2 AND $1 = ANY(e.users)
                 )"
            );
            let sql = page(&visible, "LIMIT $4 OFFSET $5", actionable);
            let max = i64::try_from(crate::approval_settings::max_approvers(tx).await?)
                .map_err(|_| shape())?;
            tx.query(&sql, &[&uid, &max, &SUPERUSER_ID.get(), &limit, &offset])
                .await?
        }
        _ => return Err(Error::validation("Unknown approval queue scope.")),
    };
    let result: Json = rows
        .first()
        .ok_or_else(shape)?
        .try_get(0)
        .map_err(|_| shape())?;
    Ok(Value::Json(Arc::new(result)))
}
