//! Partner hierarchy serialization fence and bounded structural validation (R4).
//!
//! Provides a single-row hierarchy fence table (`res_partner_hierarchy_fence`) registered via
//! core DDL. Under `REPEATABLE READ`, acquiring the row-version fence (`UPDATE ... SET id=id`)
//! ensures that concurrent hierarchy mutations serialize: a contending stale writer receives
//! SQLSTATE 40001, which is automatically retried by `Orm::run` across the whole transaction.
//!
//! Structural validation bounds traversal to at most 256 depth and 4096 visited identities
//! (tightening-only context overrides). Integrity-only queries read `(id, parent_id)` edges
//! directly from `res_partner` after normal write authorization, safely checking archived and
//! inaccessible ancestors without disclosing unpermitted record names or IDs in error messages.

use std::collections::{HashMap, HashSet};

use erp_sdk::__private::{CoreTableRequirement, core_ddl};
use erp_sdk::prelude::*;

/// Table name for partner hierarchy row-version fence.
pub const HIERARCHY_FENCE_TABLE_NAME: &str = "res_partner_hierarchy_fence";

/// DDL creating the single-row hierarchy fence table and revoking public access.
pub const HIERARCHY_FENCE_DDL: &str = "\
CREATE TABLE IF NOT EXISTS res_partner_hierarchy_fence (
    id int4 PRIMARY KEY CHECK (id = 1)
);
INSERT INTO res_partner_hierarchy_fence (id) VALUES (1) ON CONFLICT DO NOTHING;
REVOKE ALL ON res_partner_hierarchy_fence FROM PUBLIC;";

core_ddl!(
    PARTNER_HIERARCHY_FENCE,
    owner = "base",
    name = "res_partner_hierarchy_fence",
    purpose = "partner hierarchy serialization fence",
    tables = ["res_partner_hierarchy_fence"],
    sql = HIERARCHY_FENCE_DDL
);

/// Requirement reference for linker guard.
pub static HIERARCHY_FENCE_REQUIREMENT: CoreTableRequirement =
    CoreTableRequirement::new("res_partner_hierarchy_fence", "base");

/// Base's partner hierarchy and company lifecycle table requirements.
///
/// Runtime build sites keep this list as the linker/admission guard for both registrations.
/// The existing entry point remains compatible for callers that already link base.
pub fn core_tables() -> &'static [CoreTableRequirement] {
    std::hint::black_box(&PARTNER_HIERARCHY_FENCE);
    crate::company_lifecycle::keep_registration();
    crate::attachments::keep_registration();
    static REQUIREMENTS: [CoreTableRequirement; 3] = [
        CoreTableRequirement::new("res_partner_hierarchy_fence", "base"),
        crate::company_lifecycle::REQUIREMENT,
        crate::attachments::REQUIREMENT,
    ];
    &REQUIREMENTS
}

/// SQL statement that acquires a row-version fence on `res_partner_hierarchy_fence`.
pub const HIERARCHY_FENCE_SQL: &str = "UPDATE res_partner_hierarchy_fence SET id = id WHERE id = 1";

/// Acquire the row-version fence on `res_partner_hierarchy_fence`.
///
/// Under `REPEATABLE READ`, this `UPDATE ... SET id = id` generates a new row version
/// on the single row `id = 1`. Any concurrent transaction attempting to acquire the fence
/// will serialize behind this transaction; when this transaction commits, the concurrent
/// transaction aborts with SQLSTATE 40001 (serialization failure), ensuring that no two
/// transactions can race to form cycles across opposing snapshots. The 40001 error is
/// automatically retried as a whole transaction by `Orm::run`.
///
/// # Errors
///
/// Returns `Database` when the singleton row is missing or the affected-row count is not one.
/// Database errors, including retryable SQLSTATE `40001`, propagate without local retry.
pub async fn acquire_hierarchy_fence(tx: &mut Transaction) -> Result<()> {
    let count = tx.execute(HIERARCHY_FENCE_SQL, &[]).await?;
    if count != 1 {
        return Err(Error::db(format!(
            "res_partner_hierarchy_fence row missing: expected 1 row updated, got {count}"
        )));
    }
    Ok(())
}

/// Ceiling for partner hierarchy depth.
pub const DEFAULT_MAX_HIERARCHY_DEPTH: usize = 256;
/// Ceiling for total visited partner identities during structural validation.
pub const DEFAULT_MAX_HIERARCHY_VISITED: usize = 4096;

/// Context key for tightening hierarchy max depth.
pub const CONTEXT_KEY_MAX_DEPTH: &str = "partner_hierarchy_max_depth";
/// Context key for tightening hierarchy max visited nodes.
pub const CONTEXT_KEY_MAX_VISITED: &str = "partner_hierarchy_max_visited";

/// Resolve active limits from transaction context, enforcing tightening-only rule.
///
/// # Errors
///
/// Returns `Programming` for non-integer, nonpositive or above-ceiling values. Mutations resolve
/// limits from their triggering record environment; this helper inspects the transaction's context.
pub fn resolve_hierarchy_limits(tx: &Transaction) -> Result<(usize, usize)> {
    resolve_context_limits(tx.env().context())
}

fn resolve_context_limits(ctx: &erp_sdk::Context) -> Result<(usize, usize)> {
    let mut max_depth = DEFAULT_MAX_HIERARCHY_DEPTH;
    let mut max_visited = DEFAULT_MAX_HIERARCHY_VISITED;

    if let Some(val) = ctx.get(CONTEXT_KEY_MAX_DEPTH) {
        let requested = match val {
            Value::Int(n) if n > 0 => usize::try_from(n).map_err(|_| {
                Error::programming(format!(
                    "partner_hierarchy_max_depth ({n}) cannot exceed ceiling of {DEFAULT_MAX_HIERARCHY_DEPTH}"
                ))
            })?,
            Value::Int(_) => {
                return Err(Error::programming(
                    "partner_hierarchy_max_depth must be greater than zero",
                ));
            }
            _ => {
                return Err(Error::programming(
                    "partner_hierarchy_max_depth must be an integer",
                ));
            }
        };
        if requested > DEFAULT_MAX_HIERARCHY_DEPTH {
            return Err(Error::programming(format!(
                "partner_hierarchy_max_depth ({requested}) cannot exceed ceiling of {DEFAULT_MAX_HIERARCHY_DEPTH}"
            )));
        }
        max_depth = requested;
    }

    if let Some(val) = ctx.get(CONTEXT_KEY_MAX_VISITED) {
        let requested = match val {
            Value::Int(n) if n > 0 => usize::try_from(n).map_err(|_| {
                Error::programming(format!(
                    "partner_hierarchy_max_visited ({n}) cannot exceed ceiling of {DEFAULT_MAX_HIERARCHY_VISITED}"
                ))
            })?,
            Value::Int(_) => {
                return Err(Error::programming(
                    "partner_hierarchy_max_visited must be greater than zero",
                ));
            }
            _ => {
                return Err(Error::programming(
                    "partner_hierarchy_max_visited must be an integer",
                ));
            }
        };
        if requested > DEFAULT_MAX_HIERARCHY_VISITED {
            return Err(Error::programming(format!(
                "partner_hierarchy_max_visited ({requested}) cannot exceed ceiling of {DEFAULT_MAX_HIERARCHY_VISITED}"
            )));
        }
        max_visited = requested;
    }

    Ok((max_depth, max_visited))
}

/// Validate partner hierarchy structure and prevent cycles.
///
/// - `records_to_check`: pairs of `(partner_id, target_parent_id)`.
/// - Reads only `(id, parent_id)` edges directly from `res_partner` without active-record or company filtering.
/// - Errors never disclose inaccessible partner names or IDs.
pub(crate) async fn validate_partner_hierarchy(
    tx: &mut Transaction,
    records_to_check: &[(i64, Option<i64>)],
    context: &erp_sdk::Context,
) -> Result<()> {
    if records_to_check.is_empty() {
        return Ok(());
    }
    // If all target parents are None, no cycle can be created (fast path).
    if records_to_check.iter().all(|(_, p)| p.is_none()) {
        return Ok(());
    }

    let (max_depth, max_visited) = resolve_context_limits(context)?;

    // Only parent edges are needed. Flushing unrelated computes before cycle validation can
    // enter the very recursive dependency that this constraint is about to refuse.
    let partner_model = tx
        .registry()
        .model_id("res.partner")
        .ok_or_else(|| Error::programming("partner hierarchy requires res.partner"))?;
    let parent_field = tx
        .registry()
        .model_by_id(partner_model)
        .field_id("parent_id")
        .ok_or_else(|| Error::programming("partner hierarchy requires res.partner.parent_id"))?;
    tx.flush_fields(partner_model, &[parent_field]).await?;

    // Map of in-flight mutations from the batch/caller
    let in_flight: HashMap<i64, Option<i64>> = records_to_check.iter().copied().collect();

    // Cache of committed database edges: id -> parent_id
    let mut db_edges: HashMap<i64, Option<i64>> = HashMap::new();

    for &(start_id, start_parent) in records_to_check {
        let mut path_visited = HashSet::new();
        path_visited.insert(start_id);

        let mut depth = 0;
        let mut curr = start_parent;

        while let Some(pid) = curr {
            depth += 1;
            if depth > max_depth {
                return Err(Error::validation(format!(
                    "Partner hierarchy maximum depth of {max_depth} exceeded."
                )));
            }
            if !path_visited.insert(pid) {
                return Err(Error::validation(
                    "You cannot create recursive Partner hierarchies.",
                ));
            }
            // Look up pid's parent:
            // 1) First check in-flight mutations
            if let Some(&next_parent) = in_flight.get(&pid) {
                curr = next_parent;
            } else if let Some(&next_parent) = db_edges.get(&pid) {
                curr = next_parent;
            } else {
                // Only an uncached database ancestor consumes this safety allowance.
                // In-flight batch identities and shared cached edges do not add reads.
                if db_edges.len() >= max_visited {
                    return Err(Error::validation(format!(
                        "Partner hierarchy maximum visited nodes limit of {max_visited} exceeded."
                    )));
                }
                // 2) Integrity-only database query: reads only parent_id, including archived records
                let rows = tx
                    .query("SELECT parent_id FROM res_partner WHERE id = $1", &[&pid])
                    .await?;
                let next_parent = if let Some(row) = rows.first() {
                    row.try_get::<_, Option<i64>>(0)
                        .map_err(|e| Error::db(format!("reading parent_id of res_partner: {e}")))?
                } else {
                    None
                };
                db_edges.insert(pid, next_parent);
                curr = next_parent;
            }
        }
    }

    Ok(())
}
