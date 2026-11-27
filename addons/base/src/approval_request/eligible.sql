-- scopes supplies the frozen revision, the stage being checked, company/exclusion arrays,
-- and an optional viewer. $2 is the configured bound and $3 is the superuser.
stage_refs AS (
    SELECT p.id, s.step, p.outcome,
           CASE WHEN jsonb_array_length(nullif(p.review_steps,'null')) > 0 THEN
               ARRAY(SELECT jsonb_array_elements_text(p.review_steps -> (s.step - 1) -> 'user_ids')::bigint)
           ELSE ARRAY(SELECT r.user_id FROM ir_approval_policy_users_rel r WHERE r.policy_id = p.id) END AS user_ids,
           CASE WHEN jsonb_array_length(nullif(p.review_steps,'null')) > 0 THEN
               ARRAY(SELECT jsonb_array_elements_text(p.review_steps -> (s.step - 1) -> 'group_ids')::bigint)
           ELSE ARRAY(SELECT r.group_id FROM ir_approval_policy_groups_rel r WHERE r.policy_id = p.id) END AS group_ids
    FROM (SELECT DISTINCT revision_id, step FROM scopes) s JOIN ir_approval_policy p ON p.id = s.revision_id
    WHERE s.step BETWEEN 1 AND greatest(1, jsonb_array_length(nullif(p.review_steps,'null')))
),
revisions AS (
    SELECT *, (cardinality(user_ids) + cardinality(group_ids))::bigint AS named FROM stage_refs
),
holding(policy_id, step, gid) AS (
    SELECT p.id, p.step, unnest(p.group_ids) FROM revisions p
    WHERE p.outcome = 'approve' AND p.named <= $2
    UNION
    SELECT h.policy_id, h.step, i.gid FROM holding h JOIN res_groups_implied_rel i ON i.hid = h.gid
),
members(policy_id, step, uid) AS NOT MATERIALIZED (
    SELECT p.id, p.step, unnest(p.user_ids) FROM revisions p
    WHERE p.outcome = 'approve' AND p.named <= $2
    UNION ALL
    SELECT h.policy_id, h.step, m.uid FROM holding h JOIN res_groups_users_rel m ON m.gid = h.gid
),
eligibility AS MATERIALIZED (
    SELECT s.*, p.outcome, p.named, ARRAY(
        SELECT u.id FROM res_users u
        WHERE u.active AND NOT u.share AND u.id <> $3::bigint
          AND EXISTS (SELECT FROM members m WHERE m.policy_id = s.revision_id AND m.step = s.step AND m.uid = u.id)
          AND NOT s.excluded @> to_jsonb(u.id)
          AND NOT EXISTS (
              SELECT FROM jsonb_array_elements_text(s.companies) c(cid)
              WHERE NOT EXISTS (
                  SELECT FROM res_company_users_rel a WHERE a.user_id = u.id AND a.cid = c.cid::bigint
              )
          )
        ORDER BY u.id LIMIT ($2::bigint + 1)
    ) AS users
    FROM scopes s JOIN revisions p ON p.id = s.revision_id AND p.step = s.step
    WHERE s.viewer IS NULL OR EXISTS (
        SELECT FROM members m WHERE m.policy_id = s.revision_id AND m.step = s.step AND m.uid = s.viewer
    )
)
