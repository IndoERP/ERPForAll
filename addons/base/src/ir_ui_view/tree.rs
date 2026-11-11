//! Reading view rows for validation (§11): the inheritance trees of
//! a set of views in one recursive statement, and the few lookups around it. Every read flushes
//! the model's pending writes first.

use std::collections::HashMap;

use erp_sdk::prelude::*;
use erp_sdk::views::record::{RecordLimit, ViewMode, ViewRecordError, ViewRow};
use erp_sdk::views::{LimitKind, ViewLimits};
use erp_sdk::{Error, Row};

use super::{MODEL, view_error};

/// One fetched view as the check labels it (its arch is in the [`Trees::rows`] row only: the arch
/// is never copied, review S-3).
pub(crate) struct FetchedView {
    pub(crate) id: i64,
    pub(crate) view_type: Option<String>,
    /// `None` for a view without a model (a QWeb template).
    pub(crate) model: Option<String>,
    pub(crate) name: String,
    /// The `res.groups` ids of the view's `group_ids`, ascending. Filled only by
    /// [`fetch_trees_read_only`] (the statement that serves `get_view`); empty from
    /// [`lock_and_fetch_trees`], which does not read the relation.
    pub(crate) group_ids: Vec<i64>,
}

/// Marker key of the transaction's set of `ir.ui.view` roots already written (
/// [`Transaction::mark_scoped_id`]). Private on purpose: the A3-D1 conflict-write guard skips the
/// write for roots in this set, so no other module may name the key and add to it.
struct ViewRootKey;

/// The fetched trees: each view's labels, and the engine's rows (the archs), in id order, plus
/// reached roots.
pub(crate) struct Trees {
    pub(crate) views: Vec<FetchedView>,
    pub(crate) rows: Vec<ViewRow>,
    pub(crate) roots: Vec<i64>,
}

/// Write the pending values of `ir.ui.view` before reading its table.
pub(crate) async fn flush(tx: &mut Transaction) -> Result<()> {
    let model = tx
        .registry()
        .model(MODEL)
        .map(|m| m.id)
        .ok_or_else(|| Error::invariant("the registry has no ir.ui.view model"))?;
    tx.flush_model(model).await
}

/// `row.try_get(index)` with an error naming the `ir_ui_view` column (the value's type comes from
/// the binding).
macro_rules! column {
    ($row:expr, $index:expr, $name:literal) => {
        $row.try_get($index).map_err(|e| {
            Error::db(format!(
                concat!("unexpected value in ir_ui_view.", $name, ": {}"),
                e
            ))
        })
    };
}

/// The upward walk of the inheritance trees of `$1` (`up(id, inherit_id, depth)`), shared by
/// [`lock_roots`] and [`lock_and_fetch_trees`]: `depth` stops a cycle one past `$2`, the
/// engine's [`LimitKind::InheritanceDepth`] (it reports a path that long itself). A macro, so the
/// statements are literals built at compile time (review A3-R5).
macro_rules! up_cte {
    () => {
        "up(id, inherit_id, depth) AS ( \
           SELECT id, inherit_id, 1 FROM ir_ui_view WHERE id = ANY($1) \
           UNION \
           SELECT v.id, v.inherit_id, u.depth + 1 FROM ir_ui_view v \
             JOIN up u ON v.id = u.inherit_id WHERE u.depth <= $2)"
    };
}

/// The roots reached by `up`, locked `FOR NO KEY UPDATE` in id order (`roots(id, mine)`), then
/// written with their own values (`touched(id)`), except the roots this transaction already
/// wrote (`mine`: their row version is its own, a concurrent writer conflicts on them already;
/// reviews A3-D1, D-7). See [`lock_roots`] for why both the lock and the write.
///
/// `mine` checks whether the root id is present in the transaction's tracked roots
/// (`$touched_param`), tracked per transaction across savepoints. Roots touched in a savepoint
/// that rolls back are forgotten so they are rewritten on a subsequent check.
macro_rules! lock_roots_cte {
    ($touched_param:literal) => {
        concat!(
            "roots AS ( \
               SELECT v.id, v.id = ANY(",
            $touched_param,
            ") AS mine FROM ir_ui_view v \
                WHERE v.id IN (SELECT id FROM up WHERE inherit_id IS NULL) \
                ORDER BY v.id FOR NO KEY UPDATE), \
             touched AS ( \
               UPDATE ir_ui_view v SET inherit_id = v.inherit_id FROM roots r \
                WHERE v.id = r.id AND NOT r.mine RETURNING v.id)"
        )
    };
}

/// [`lock_roots`]: the lock and write, then the current parent of each given view and the reached roots.
const LOCK_ROOTS_SQL: &str = concat!(
    "WITH RECURSIVE ",
    up_cte!(),
    ", ",
    lock_roots_cte!("$3"),
    // `touched` runs to completion although the result does not read it (a data-modifying CTE)
    " SELECT u.id, u.inherit_id, false AS is_root FROM up u WHERE u.depth = 1 \
      UNION ALL \
      SELECT r.id, NULL::bigint, true AS is_root FROM roots r"
);

/// [`lock_and_fetch_trees`]: the lock and write, then the trees from the roots down.
const LOCK_AND_FETCH_SQL: &str = concat!(
    "WITH RECURSIVE ",
    up_cte!(),
    ", ",
    lock_roots_cte!("$5"),
    ", ",
    "down(id, depth) AS ( \
       SELECT id, 1 FROM roots \
       UNION \
       SELECT v.id, d.depth + 1 FROM ir_ui_view v \
         JOIN down d ON v.inherit_id = d.id WHERE d.depth <= $2), \
     payload AS ( \
       SELECT v.id, v.name, v.model, v.type, v.inherit_id, v.mode, v.priority, v.active, \
              v.arch_db, \
              2 * coalesce(octet_length(v.name)::bigint, 0) + \
              2 * coalesce(octet_length(v.model)::bigint, 0) + \
              coalesce(octet_length(v.type)::bigint, 0) + \
              coalesce(octet_length(v.mode)::bigint, 0) + \
              coalesce(octet_length(v.arch_db)::bigint, 0) AS bytes \
         FROM ir_ui_view v \
        WHERE v.id IN (SELECT id FROM up UNION SELECT id FROM down) \
        ORDER BY v.id LIMIT $3), \
     fetched AS (SELECT *, sum(bytes) OVER (ORDER BY id ROWS UNBOUNDED PRECEDING)::bigint \
         AS total FROM payload) \
     SELECT id, CASE WHEN total <= $4 THEN name END, \
            CASE WHEN total <= $4 THEN model END, CASE WHEN total <= $4 THEN type END, \
            inherit_id, CASE WHEN total <= $4 THEN mode END, priority, active, \
            CASE WHEN total <= $4 THEN arch_db END, total FROM fetched \
      WHERE total - bytes <= $4 ORDER BY id"
);

/// [`fetch_trees_read_only`]: trees from roots down without root locks or updates.
///
/// Reads each view's `group_ids` (`ir_ui_view_group_rel`) in the same statement, as the last
/// column; the ids are charged to the byte bound at eight bytes each, like the other
/// variable-width payload, and are NULL on the crossing row.
const FETCH_TREES_READ_ONLY_SQL: &str = concat!(
    "WITH RECURSIVE ",
    up_cte!(),
    ", ",
    "roots AS ( \
       SELECT id FROM up WHERE inherit_id IS NULL), \
     down(id, depth) AS ( \
       SELECT id, 1 FROM roots \
       UNION \
       SELECT v.id, d.depth + 1 FROM ir_ui_view v \
         JOIN down d ON v.inherit_id = d.id WHERE d.depth <= $2), \
     payload AS ( \
       SELECT v.id, v.name, v.model, v.type, v.inherit_id, v.mode, v.priority, v.active, \
              v.arch_db, gr.ids AS group_ids, \
              2 * coalesce(octet_length(v.name)::bigint, 0) + \
              2 * coalesce(octet_length(v.model)::bigint, 0) + \
              coalesce(octet_length(v.type)::bigint, 0) + \
              coalesce(octet_length(v.mode)::bigint, 0) + \
              coalesce(octet_length(v.arch_db)::bigint, 0) + \
              8 * cardinality(gr.ids)::bigint AS bytes \
         FROM ir_ui_view v \
         CROSS JOIN LATERAL (SELECT ARRAY(SELECT g.group_id::bigint FROM ir_ui_view_group_rel g \
                                           WHERE g.view_id = v.id ORDER BY g.group_id) AS ids) gr \
        WHERE v.id IN (SELECT id FROM up UNION SELECT id FROM down) \
        ORDER BY v.id LIMIT $3), \
     fetched AS (SELECT *, sum(bytes) OVER (ORDER BY id ROWS UNBOUNDED PRECEDING)::bigint \
         AS total FROM payload) \
     SELECT id, CASE WHEN total <= $4 THEN name END, \
            CASE WHEN total <= $4 THEN model END, CASE WHEN total <= $4 THEN type END, \
            inherit_id, CASE WHEN total <= $4 THEN mode END, priority, active, \
            CASE WHEN total <= $4 THEN arch_db END, total, \
            CASE WHEN total <= $4 THEN group_ids END FROM fetched \
      WHERE total - bytes <= $4 ORDER BY id"
);

/// Refuse a registry whose `ir.ui.view` has a row version: the check writes the roots
/// of the trees it checks with their own values, and a row-version trigger would bump their
/// versions on every check while the ORM's cache keeps the old ones (review A3-R2). Called by
/// `_register_hook` (the module system's final registry) and before every lock.
///
/// # Errors
///
/// `Programming` when the model has a row version, or is not in the registry.
pub(crate) fn refuse_row_version(registry: &erp_sdk::Registry) -> Result<()> {
    let spec = registry
        .model(MODEL)
        .ok_or_else(|| Error::invariant("the registry has no ir.ui.view model"))?;
    // the model's own flag: set when a declaration turned the row version on (reviews A4-S2,
    // A4-R4)
    if spec.row_version.is_some() {
        return Err(Error::invariant(
            "ir.ui.view cannot have a row version (row_version = true): its view \
             check writes the roots of the trees it checks with their own values, which would \
             bump their versions on every check",
        ));
    }
    Ok(())
}

/// The inheritance depth limit as the statements bind it.
fn depth_limit() -> Result<i32> {
    i32::try_from(ViewLimits::default().get(LimitKind::InheritanceDepth))
        .map_err(|_| Error::invariant("inheritance depth limit beyond i32"))
}

/// Lock and write the roots of the inheritance trees of `ids` (one statement, after a flush),
/// then read every view of those trees: every ancestor of each id, then every descendant of the
/// roots (one statement, `WITH RECURSIVE` up then down; the lock is the one of [`lock_roots`],
/// taken in the same statement, so a check costs one statement).
///
/// Both the rows and the bytes are bounded in the statement, so an oversized tree is refused
/// without transferring or decoding it: at most [`LimitKind::CatalogueViews`] rows plus one, and
/// rows in id order only while their variable-width payload totals at most
/// [`LimitKind::CatalogueBytes`]. Names and models are charged twice for their retained copies.
/// The crossing row carries only its size and fixed-width columns: every text column is NULL,
/// so even one oversized metadata value is refused before transfer (review SS-1). The database still walks the whole tree: the walk is
/// bounded in depth ([`LimitKind::InheritanceDepth`] plus one on each side, where the engine
/// reports a path too long), not in width. A cycle has no root: its views come from the upward
/// walk, and nothing is locked for them beyond the written rows themselves.
///
/// # Errors
///
/// A database error, `Concurrency` (`40001`/`40P01`) included, see [`lock_roots`];
/// `Validation` for a tree over the limit of views or of bytes.
pub(crate) async fn lock_and_fetch_trees(tx: &mut Transaction, ids: &[i64]) -> Result<Trees> {
    refuse_row_version(tx.registry())?;
    flush(tx).await?;
    let depth = depth_limit()?;
    let limits = ViewLimits::default();
    let max_views = limits.get(LimitKind::CatalogueViews);
    let max_bytes = limits.get(LimitKind::CatalogueBytes);
    let fetch = i64::try_from(max_views)
        .ok()
        .and_then(|n| n.checked_add(1))
        .ok_or_else(|| Error::invariant("tree size limit beyond i64"))?;
    let bytes =
        i64::try_from(max_bytes).map_err(|_| Error::invariant("tree bytes limit beyond i64"))?;
    let touched = tx.scoped_ids::<ViewRootKey>();
    let rows = tx
        .query(
            LOCK_AND_FETCH_SQL,
            &[&ids, &depth, &fetch, &bytes, &touched.as_slice()],
        )
        .await?;
    let trees = decode_trees(&rows, max_views, max_bytes, false)?;
    for &root_id in &trees.roots {
        tx.mark_scoped_id::<ViewRootKey>(root_id);
    }
    Ok(trees)
}

/// Fetch the inheritance trees of `ids` in a read-only statement without root locking or writes
/// (§11).
///
/// # Errors
///
/// A database error; `Validation` for a tree over the limit of views or bytes.
pub(crate) async fn fetch_trees_read_only(tx: &mut Transaction, ids: &[i64]) -> Result<Trees> {
    flush(tx).await?;
    let depth = depth_limit()?;
    let limits = ViewLimits::default();
    let max_views = limits.get(LimitKind::CatalogueViews);
    let max_bytes = limits.get(LimitKind::CatalogueBytes);
    let fetch = i64::try_from(max_views)
        .ok()
        .and_then(|n| n.checked_add(1))
        .ok_or_else(|| Error::invariant("tree size limit beyond i64"))?;
    let bytes =
        i64::try_from(max_bytes).map_err(|_| Error::invariant("tree bytes limit beyond i64"))?;
    let rows = tx
        .query(FETCH_TREES_READ_ONLY_SQL, &[&ids, &depth, &fetch, &bytes])
        .await?;
    decode_trees(&rows, max_views, max_bytes, true)
}

/// `with_groups`: column 10 holds the view's `group_ids` ([`FETCH_TREES_READ_ONLY_SQL`] only).
fn decode_trees(
    rows: &[Row],
    max_views: usize,
    max_bytes: usize,
    with_groups: bool,
) -> Result<Trees> {
    if rows.len() > max_views {
        return Err(view_error(ViewRecordError::Limit {
            limit: RecordLimit::CatalogueViews,
            actual: rows.len(),
            max: max_views,
        }));
    }
    // the running total of the last row: past the limit only on the row that crosses it
    if let Some(last) = rows.last() {
        // `sum(int)` is a bigint
        let total: i64 = column!(last, 9, "fetched payload (bytes)")?;
        let total = usize::try_from(total).map_err(|_| {
            Error::db(format!(
                "unexpected size of the fetched view payload: {total}"
            ))
        })?;
        if total > max_bytes {
            return Err(view_error(ViewRecordError::Limit {
                limit: RecordLimit::CatalogueBytes,
                actual: total,
                max: max_bytes,
            }));
        }
    }
    let mut views = Vec::with_capacity(rows.len());
    let mut view_rows = Vec::with_capacity(rows.len());
    let mut roots = Vec::new();
    for row in rows {
        let id: i64 = column!(row, 0, "id")?;
        let name: String = column!(row, 1, "name")?;
        let model: Option<String> = column!(row, 2, "model")?;
        let view_type: Option<String> = column!(row, 3, "type")?;
        let parent: Option<i64> = column!(row, 4, "inherit_id")?;
        if parent.is_none() {
            roots.push(id);
        }
        let mode: String = column!(row, 5, "mode")?;
        let priority: i32 = column!(row, 6, "priority")?;
        let active: bool = column!(row, 7, "active")?;
        let arch: Option<String> = column!(row, 8, "arch_db")?;
        let group_ids: Vec<i64> = if with_groups {
            // NULL only on the crossing row, which the byte check above already refused
            let ids: Option<Vec<i64>> = column!(row, 10, "group_ids (ir_ui_view_group_rel)")?;
            ids.ok_or_else(|| Error::db("ir_ui_view_group_rel ids missing on a served row"))?
        } else {
            Vec::new()
        };
        let mode = ViewMode::parse(&mode).map_err(view_error)?;
        let arch = match arch {
            Some(arch) if !arch.is_empty() => arch,
            _ if parent.is_some() => "<data/>".to_owned(),
            _ => String::new(),
        };
        view_rows.push(
            ViewRow::new(id, mode)
                .name(name.clone())
                .model(model.clone())
                .arch(arch)
                .inherit_id(parent)
                .priority(priority)
                .active(active),
        );
        views.push(FetchedView {
            id,
            view_type,
            model,
            name,
            group_ids,
        });
    }
    Ok(Trees {
        views,
        rows: view_rows,
        roots,
    })
}

/// Lock the roots of the inheritance trees of `ids` and write them with their own values (one
/// statement, after a flush), so concurrent writers of one tree cannot both commit: neither a
/// cycle nor a spec broken by a change the other's check did not see (reviews A-D4, A2-D1,
/// A2-R1; the pattern of the ORM's parent store, which locks the rows a move reads).
///
/// The lock alone is not enough under `REPEATABLE READ`: a second transaction waits for the
/// first, then locks a row the first never changed and goes on with its old snapshot. The write
/// (`inherit_id = inherit_id`, a no-key update: children's foreign keys are not blocked) makes
/// the root a row the first transaction changed, so the waiting lock fails with `40001`
/// ("could not serialize access due to concurrent update"). Two transactions locking each
/// other's roots fail one of them with `40P01`. Both surface as `ErrorKind::Concurrency` with
/// the SQLSTATE; callers retry the whole transaction (`Orm::run` does), never the statement.
///
/// A root this transaction already wrote is not written again: its row version is already this
/// transaction's, so a concurrent writer conflicts on it (one row version per root per
/// transaction across savepoints; reviews A3-D1, D-7).
///
/// A write of `inherit_id` calls it on the written views and the new parent, so the roots of the
/// trees the views leave and of the tree they join are all locked here, in one id order, before
/// the write (review D-3); the check after it ([`lock_and_fetch_trees`]) finds them locked and
/// already written. Returns the current parent of each of `ids` that exists, read in the same
/// statement (review A3-D5).
///
/// # Errors
///
/// A database error; `Concurrency` (`40001`, `40P01`) as above.
pub(crate) async fn lock_roots(
    tx: &mut Transaction,
    ids: &[i64],
) -> Result<HashMap<i64, Option<i64>>> {
    let mut parents = HashMap::with_capacity(ids.len());
    if ids.is_empty() {
        return Ok(parents);
    }
    refuse_row_version(tx.registry())?;
    flush(tx).await?;
    let depth = depth_limit()?;
    let touched = tx.scoped_ids::<ViewRootKey>();
    let rows = tx
        .query(LOCK_ROOTS_SQL, &[&ids, &depth, &touched.as_slice()])
        .await?;
    for row in &rows {
        let id: i64 = column!(row, 0, "id")?;
        let is_root: bool = column!(row, 2, "is_root")?;
        if is_root {
            tx.mark_scoped_id::<ViewRootKey>(id);
        } else {
            let parent: Option<i64> = column!(row, 1, "inherit_id")?;
            parents.insert(id, parent);
        }
    }
    Ok(parents)
}

/// [`subtree_children_first`]: the views `$1` that exist and every view below them, each at its
/// greatest depth from `$1` (a view of `$1` under another one of `$1` included), deepest first.
/// The walk stops at depth `$2` (the engine's [`LimitKind::InheritanceDepth`]), so a cycle ends;
/// `(id, depth)` rows are distinct per depth, so a view appears at most once per level.
const SUBTREE_SQL: &str = "WITH RECURSIVE below(id, depth) AS ( \
       SELECT id, 0 FROM ir_ui_view WHERE id = ANY($1) \
       UNION \
       SELECT v.id, b.depth + 1 FROM ir_ui_view v JOIN below b ON v.inherit_id = b.id \
        WHERE b.depth < $2) \
     SELECT id FROM below GROUP BY id ORDER BY max(depth) DESC, id";

/// `ids` and every view built on them at any depth (archived ones too), children before their
/// parents (deepest first), in one statement after a flush: the order a delete of the set needs,
/// since a delete runs in chunks and `inherit_id` is `ondelete="restrict"` (reviews S-5, D-6).
/// The views of `ids` among one another are ordered too, so a view of `ids` built on another one
/// comes first. A cycle in `inherit_id` ends: the walk is bounded by the inheritance depth limit.
/// A subtree deeper than that limit is returned only down to it; the caller that deletes calls
/// again on the result until nothing is left outside it (the `force_delete` cascade of `unlink`
/// does, by unlinking the result through the whole chain). The ids of `ids` that do not exist
/// are not returned.
pub(crate) async fn subtree_children_first(tx: &mut Transaction, ids: &[i64]) -> Result<Vec<i64>> {
    flush(tx).await?;
    let depth = depth_limit()?;
    let rows = tx.query(SUBTREE_SQL, &[&ids, &depth]).await?;
    rows.iter()
        .map(|row| -> Result<i64> { column!(row, 0, "id") })
        .collect()
}

/// Whether the registry has external ids (`ir.model.data` with a table).
pub(super) fn has_external_ids(tx: &Transaction) -> bool {
    tx.registry()
        .model("ir.model.data")
        .is_some_and(|m| m.has_table())
}

/// Flush external-id changes before a raw join or lookup (review CDB-2).
pub(super) async fn flush_external_ids(tx: &mut Transaction) -> Result<()> {
    if let Some(model) = tx
        .registry()
        .model("ir.model.data")
        .filter(|m| m.has_table())
        .map(|m| m.id)
    {
        tx.flush_model(model).await?;
    }
    Ok(())
}

/// The views of `module` named `names` whose external id is `noupdate`.
pub(crate) async fn noupdate_views(
    tx: &mut Transaction,
    module: &str,
    names: &[String],
) -> Result<Vec<i64>> {
    if !has_external_ids(tx) {
        return Err(Error::invariant(
            "ir.ui.view._validate_module_views needs ir.model.data in the registry",
        ));
    }
    flush(tx).await?;
    flush_external_ids(tx).await?;
    let rows = tx
        .query(
            "SELECT v.id FROM ir_ui_view v \
               JOIN ir_model_data md ON md.model = 'ir.ui.view' AND md.res_id = v.id \
              WHERE md.module = $1 AND md.name = ANY($2) AND md.noupdate \
              ORDER BY v.id",
            &[&module, &names],
        )
        .await?;
    rows.iter()
        .map(|row| -> Result<i64> { column!(row, 0, "id") })
        .collect()
}

/// Bounded alias payload, with a NULL-text crossing row that reports the rejected size.
const EXTERNAL_IDS_SQL: &str = "WITH payload AS (
    SELECT id, res_id, module, name,
           octet_length(module)::bigint + octet_length(name)::bigint AS bytes
      FROM ir_model_data WHERE model = 'ir.ui.view' AND res_id = ANY($1)
      ORDER BY id LIMIT $3),
    fetched AS (SELECT *, sum(bytes) OVER (ORDER BY id ROWS UNBOUNDED PRECEDING)::bigint AS total
      FROM payload)
    SELECT res_id, CASE WHEN total <= $2 THEN module END,
           CASE WHEN total <= $2 THEN name END, total FROM fetched
     WHERE total - bytes <= $2 ORDER BY id";

fn check_payload_bytes(total: i64, max: usize, limit: RecordLimit) -> Result<()> {
    let actual = usize::try_from(total)
        .map_err(|_| Error::db(format!("unexpected fetched byte size: {total}")))?;
    if actual > max {
        return Err(view_error(ViewRecordError::Limit { limit, actual, max }));
    }
    Ok(())
}

/// The external ids of the views `ids`, `(module, name)` per view, oldest first. Empty without
/// `ir.model.data` (no external id can exist).
pub(crate) async fn external_ids(
    tx: &mut Transaction,
    ids: &[i64],
) -> Result<HashMap<i64, Vec<(String, String)>>> {
    let mut found: HashMap<i64, Vec<(String, String)>> = HashMap::new();
    if ids.is_empty() || !has_external_ids(tx) {
        return Ok(found);
    }
    flush_external_ids(tx).await?;
    let limits = ViewLimits::default();
    let max_bytes = limits.get(LimitKind::CatalogueBytes);
    let max_aliases = limits.get(LimitKind::CatalogueViews);
    let bytes =
        i64::try_from(max_bytes).map_err(|_| Error::invariant("alias bytes limit beyond i64"))?;
    let fetch = i64::try_from(max_aliases)
        .map_err(|_| Error::invariant("alias count limit beyond i64"))?
        .checked_add(1)
        .ok_or_else(|| Error::invariant("alias count limit beyond i64"))?;
    let rows = tx.query(EXTERNAL_IDS_SQL, &[&ids, &bytes, &fetch]).await?;
    if rows.len() > max_aliases {
        return Err(view_error(ViewRecordError::Limit {
            limit: RecordLimit::Aliases,
            actual: rows.len(),
            max: max_aliases,
        }));
    }
    if let Some(last) = rows.last() {
        let total: i64 = last
            .try_get(3)
            .map_err(|e| Error::db(format!("unexpected alias bytes: {e}")))?;
        check_payload_bytes(total, max_bytes, RecordLimit::AliasBytes)?;
    }
    for row in &rows {
        let id: i64 = row
            .try_get(0)
            .map_err(|e| Error::db(format!("unexpected value in ir_model_data.res_id: {e}")))?;
        let module: String = row
            .try_get(1)
            .map_err(|e| Error::db(format!("unexpected value in ir_model_data.module: {e}")))?;
        let name: String = row
            .try_get(2)
            .map_err(|e| Error::db(format!("unexpected value in ir_model_data.name: {e}")))?;
        found.entry(id).or_default().push((module, name));
    }
    Ok(found)
}

/// `arch_fs` of the views `ids` (one statement); a view that is not found has no entry.
pub(crate) async fn arch_files(
    tx: &mut Transaction,
    ids: &[i64],
) -> Result<HashMap<i64, Option<String>>> {
    let mut files = HashMap::with_capacity(ids.len());
    if ids.is_empty() {
        return Ok(files);
    }
    flush(tx).await?;
    let max = ViewLimits::default().get(LimitKind::CatalogueBytes);
    let bytes =
        i64::try_from(max).map_err(|_| Error::invariant("view file bytes limit beyond i64"))?;
    let rows = tx.query(
        "WITH payload AS (SELECT id, arch_fs, coalesce(octet_length(arch_fs), 0)::bigint AS bytes
            FROM ir_ui_view WHERE id = ANY($1)),
         fetched AS (SELECT *, sum(bytes) OVER (ORDER BY id ROWS UNBOUNDED PRECEDING)::bigint AS total
            FROM payload)
         SELECT id, CASE WHEN total <= $2 THEN arch_fs END, total FROM fetched
            WHERE total - bytes <= $2 ORDER BY id", &[&ids, &bytes]).await?;
    if let Some(last) = rows.last() {
        let total: i64 = column!(last, 2, "arch_fs (bytes)")?;
        check_payload_bytes(total, max, RecordLimit::CatalogueBytes)?;
    }
    for row in &rows {
        let id: i64 = column!(row, 0, "id")?;
        let file: Option<String> = column!(row, 1, "arch_fs")?;
        files.insert(id, file);
    }
    Ok(files)
}

/// Which of the group references `refs` (`module.name`) name a `res.groups` record. Without
/// `ir.model.data`, none does.
pub(crate) async fn existing_groups(
    tx: &mut Transaction,
    refs: &[&str],
) -> Result<std::collections::HashSet<String>> {
    let mut found = std::collections::HashSet::new();
    if refs.is_empty() || !has_external_ids(tx) {
        return Ok(found);
    }
    let mut modules: Vec<&str> = Vec::with_capacity(refs.len());
    let mut names: Vec<&str> = Vec::with_capacity(refs.len());
    for r in refs {
        if let Some((module, name)) = r.split_once('.') {
            modules.push(module);
            names.push(name);
        }
    }
    if modules.is_empty() {
        return Ok(found);
    }
    flush_external_ids(tx).await?;
    let rows = tx
        .query(
            "SELECT md.module || '.' || md.name FROM ir_model_data md \
               JOIN unnest($1::text[], $2::text[]) AS r(module, name) \
                 ON md.module = r.module AND md.name = r.name \
              WHERE md.model = 'res.groups'",
            &[&modules, &names],
        )
        .await?;
    for row in &rows {
        found.insert(
            row.try_get::<_, String>(0)
                .map_err(|e| Error::db(format!("unexpected value in ir_model_data: {e}")))?,
        );
    }
    Ok(found)
}

#[cfg(test)]
#[path = "tree_bounds_tests.rs"]
mod bounds_tests;
