WITH RECURSIVE configured AS (
    SELECT v.value_text FROM ir_config_setting_value v JOIN ir_config_setting_scope s ON s.id=v.scope_id
    WHERE s.identity='global' AND v.setting_key='base.approval_max_approvers'
), limits AS (
    SELECT CASE WHEN count(*)=0 THEN 64 WHEN count(*)=1 AND length(max(value_text))<=32
               AND btrim(max(value_text), E' \n\r\t') ~ '^([1-9]|[1-5][0-9]|6[0-4])$'
           THEN btrim(max(value_text))::bigint ELSE 0 END AS bound FROM configured
), candidates AS (
    SELECT r.id, r.requester_id, r.revision_id, r.current_step, r.state, r.expires_at,
           r.companies, r.excluded, positions.step, d.reviewer_id,
           CASE WHEN positions.step=r.current_step THEN r.decider_id END AS final_decider
    FROM ir_approval_request r JOIN ir_approval_policy p ON p.id=r.revision_id
    CROSS JOIN LATERAL generate_series(1,r.current_step) positions(step)
    LEFT JOIN ir_approval_decision d ON d.request_id=r.id AND d.position=positions.step
    WHERE p.active AND jsonb_array_length(r.companies) BETWEEN 1 AND {companies}
), scopes AS (
    SELECT DISTINCT revision_id, companies, excluded, step, NULL::bigint AS viewer FROM candidates
), {eligibility}, grants AS (
    SELECT id, requester_id AS viewer_id, true AS is_mine, false AS to_review, false AS is_reviewed
    FROM ir_approval_request
    UNION ALL
    SELECT c.id, member.uid, false,
           c.step=c.current_step AND c.state='pending' AND c.expires_at>statement_timestamp(),
           member.uid=coalesce(c.reviewer_id,c.final_decider,0)
    FROM candidates c JOIN eligibility e USING (revision_id,companies,excluded,step)
    CROSS JOIN LATERAL unnest(e.users) member(uid), limits
    WHERE c.requester_id<>member.uid AND e.named<=limits.bound AND cardinality(e.users)<=limits.bound
      AND (c.step=c.current_step OR member.uid=coalesce(c.reviewer_id,c.final_decider,0))
), viewers AS (
    SELECT id AS request_id, viewer_id, bool_or(is_mine) AS is_mine,
           bool_or(to_review) AS to_review, bool_or(is_reviewed) AS is_reviewed
    FROM grants GROUP BY id,viewer_id
)
-- One slot per current reviewer (at most 64), prior signer (at most 15), and owner.
-- Partitioning by request allows a form or a history lookup to restrict the input first.
SELECT (request_id-1)*80 + row_number() OVER (PARTITION BY request_id ORDER BY viewer_id) AS id,
       viewers.* FROM viewers
