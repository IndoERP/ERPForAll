//! Specs: in.
//!
//! What the model adds on top of its columns:
//!
//! - `mode` follows `inherit_id` when it is not given, per record (BUG-074);
//!   `type` comes from the parent or the arch's root tag; `name` defaults to `"<model> <type>"`;
//!   a QWeb view without `key` gets `gen_key.<6 hex>`; `arch` is written to `arch_db`, with
//!   `arch_fs` from the data file being loaded.
//! - Every create, and every write of `active`, `arch`/`arch_db`, `inherit_id`, `mode`,
//!   `priority`, `model` or `type` (the last four: BUG-075), checks the
//!   written views and the views their change can break ([`check_views`]):
//!   recursion, the combination, the root tag, the fields, `attrs`/`states`. A failure is a
//!   `Validation` error naming the view, its external id, file and line, with the typed
//!   [`ViewRecordError`] as its source.
//! - An extension view may not have `group_ids`.
//! - Every write invalidates the `templates` named cache, across nodes through
//!   `cache:templates`.
//! - [`select::default_view`] and [`select::select_view`] pick the view a client asks for.
//!
//! Not here: the cached, per-user `get_view`; full per-tag validation, x2many
//! sub-views, generated default views, the view-type registry; translated
//! archs; the editor fields `arch_prev`, `warning_info`, `invalid_locators`, `notes`.

pub mod get_view;
pub mod select;
mod tree;
mod type_admission;
mod validation;

pub use get_view::{ViewResponse, get_view, with_checked_panel_view, with_checked_view};
pub(crate) use type_admission::require_loaded_view_type;

use erp_sdk::prelude::*;
use erp_sdk::views::record::{
    RecordLimit, ViewMode, ViewRecordError, mode_on_create, mode_on_write,
};
use erp_sdk::views::{LimitKind, ViewLimits};

pub use validation::{check_views, view_error};

pub const TEMPLATES: &str = "templates";

/// The model name.
pub const MODEL: &str = "ir.ui.view";

/// `ir.ui.view`: one view architecture.
#[model(
    name = "ir.ui.view",
    description = "View",
    order = "priority, name, id",
    constraints = [
        (
            "_inheritance_mode",
            "CHECK (mode <> 'extension' OR inherit_id IS NOT NULL)",
            "Invalid inheritance mode: if the mode is 'extension', the view must extend an other view"
        ),
        (
            "_qweb_required_key",
            "CHECK (type <> 'qweb' OR key IS NOT NULL)",
            "Invalid key: QWeb view should have a key"
        ),
    ],
    indexes = [index(
        name = "_default_view",
        fields = ["model", "type", "priority", "name", "id"],
        filter = "[('mode', '=', 'primary'), ('active', '=', True)]"
    )],
)]
pub struct IrUiView {
    /// View name.
    #[field(char, required = true)]
    pub name: Option<Arc<str>>,
    /// The model the view shows (empty for QWeb templates).
    #[field(char, index = true)]
    pub model: Option<Arc<str>>,
    /// Template key (`module.name`), required for QWeb views.
    #[field(char, index = "btree_not_null")]
    pub key: Option<Arc<str>>,
    /// Lower applies first; the default view of a type is the primary one with the lowest.
    #[field(integer, default = 16, required = true)]
    pub priority: i32,
    /// View type; addons add types with `selection_add`.
    #[field(
        selection,
        selection = [
            ("list", "List"),
            ("form", "Form"),
            ("graph", "Graph"),
            ("pivot", "Pivot"),
            ("calendar", "Calendar"),
            ("kanban", "Kanban"),
            ("search", "Search"),
            ("card", "Card"),
            ("qweb", "QWeb"),
        ],
        string = "View Type"
    )]
    pub r#type: Option<Arc<str>>,
    /// The stored architecture.
    #[field(text, string = "Arch Blob")]
    pub arch_db: Option<Arc<str>>,
    /// The architecture as written and read (stored in `arch_db`).
    #[field(
        text,
        compute = "_compute_arch",
        inverse = "_inverse_arch",
        string = "View Architecture"
    )]
    pub arch: Option<Arc<str>>,
    /// The data file the arch was loaded from (`<module>/<path>`).
    #[field(char, string = "Arch Filename")]
    pub arch_fs: Option<Arc<str>>,
    /// The arch was changed outside a data load.
    #[field(boolean, string = "Modified Architecture")]
    pub arch_updated: bool,
    /// The inherited view.
    #[field(
        many2one,
        comodel = "ir.ui.view",
        string = "Inherited View",
        ondelete = "restrict",
        index = true
    )]
    pub inherit_id: Option<RecId>,
    /// The views that inherit this one.
    #[field(
        one2many,
        comodel = "ir.ui.view",
        inverse_name = "inherit_id",
        string = "Views which inherit from this one"
    )]
    pub inherit_children_ids: Vec<RecId>,
    /// `primary` or `extension`; follows `inherit_id` when not given.
    #[field(
        selection,
        selection = [("primary", "Base view"), ("extension", "Extension View")],
        required = true,
        string = "View inheritance mode"
    )]
    pub mode: Option<Arc<str>>,
    /// Inactive views are not applied (extensions) or not offered (primary views).
    #[field(boolean, default = true)]
    pub active: bool,
    /// Groups a primary view is limited to (QWeb access); never on an extension.
    #[field(
        many2many,
        comodel = "res.groups",
        relation = "ir_ui_view_group_rel",
        column1 = "view_id",
        column2 = "group_id",
        string = "Groups"
    )]
    pub group_ids: Vec<RecId>,
    /// Written by `<template>`.
    #[field(char)]
    pub technical_usage: Option<Arc<str>>,
    /// Written by `<template>`.
    #[field(boolean)]
    pub track: bool,
    /// Written by `<template>`.
    #[field(boolean)]
    pub customize_show: bool,
}

const CHECKED_ON_WRITE: [&str; 8] = erp_sdk::load_state::CHECKED_VIEW_FIELDS;

/// A text value of `vals` (`None` when absent or empty).
///
/// # Errors
///
/// `Programming` for a value that is not text.
fn text<'v>(vals: &'v RawVals, field: &str) -> Result<Option<&'v str>> {
    match vals.get(field) {
        None | Some(Cell::Null) => Ok(None),
        Some(Cell::Str(s)) if s.is_empty() => Ok(None),
        Some(Cell::Str(s)) => Ok(Some(s)),
        Some(other) => Err(Error::programming(format!(
            "ir.ui.view {field} was given as {other:?}; it must be text"
        ))),
    }
}

/// The `inherit_id` of `vals`: absent (`None`), cleared (`Some(None)`) or set.
///
/// # Errors
///
/// `Programming` for a value that is not a record id.
fn parent_of(vals: &RawVals) -> Result<Option<Option<i64>>> {
    match vals.get("inherit_id") {
        None => Ok(None),
        Some(Cell::Null) => Ok(Some(None)),
        Some(Cell::Id(id)) => Ok(Some(Some(*id))),
        Some(other) => Err(Error::programming(format!(
            "ir.ui.view inherit_id was given as {other:?}; it must be a record id"
        ))),
    }
}

/// The `mode` of `vals`, when given.
fn explicit_mode(vals: &RawVals) -> Result<Option<ViewMode>> {
    text(vals, "mode")?
        .map(|m| ViewMode::parse(m).map_err(view_error))
        .transpose()
}

fn move_arch(vals: &mut RawVals) -> Result<()> {
    if let Some(arch) = vals.remove("arch") {
        vals.insert("arch_db".into(), arch);
    }
    if let Some(arch) = text(vals, "arch_db")? {
        let max = ViewLimits::default().get(LimitKind::ArchBytes);
        if arch.len() > max {
            return Err(view_error(ViewRecordError::Limit {
                limit: RecordLimit::ArchBytes,
                actual: arch.len(),
                max,
            }));
        }
    }
    Ok(())
}

/// The loader's `install_filename` is
/// already `<module>/<path>` (`erp_data::load::FileContext::install_filename`, the one place it
/// is built): it is stored as given, after checking it is a file of `install_module`.
///
/// # Errors
///
/// `Programming` when the loader's context keys are not text or only one of them is set; when
/// the file is not `<module>/<path>` with a non-empty module and no empty, `.` or `..`
/// component (review A3-R3).
fn install_file(context: &erp_sdk::Context) -> Result<Option<Arc<str>>> {
    match (
        context.get(erp_sdk::load_state::INSTALL_MODULE),
        context.get(erp_sdk::load_state::INSTALL_FILENAME),
    ) {
        (None, None) => Ok(None),
        (Some(Value::Str(module)), Some(Value::Str(path))) => {
            if is_module_file(&module, &path) {
                Ok(Some(path))
            } else {
                Err(Error::invariant(format!(
                    "ir.ui.view: install_filename '{path}' is not a file of module '{module}': \
                     it must be '<module>/<path>', without empty, '.' or '..' components"
                )))
            }
        }
        (module, path) => Err(Error::invariant(format!(
            "ir.ui.view: the data loader's context has install_module {module:?} and \
             install_filename {path:?}; both must be text"
        ))),
    }
}

/// An `arch_fs` given by the caller (not the loader's file) must name a module file too, as the
/// loader's does: it is shown in error labels now, and a later reset-arch or file reader would
/// open it (review S-7). Clearing it is allowed.
///
/// # Errors
///
/// `Validation` when the value is not `<module>/<path>` without empty, `.` or `..` components;
/// `Programming` when it is not text.
fn check_arch_fs(vals: &RawVals) -> Result<()> {
    let Some(path) = text(vals, "arch_fs")? else {
        return Ok(());
    };
    let module = path.split('/').next().unwrap_or_default();
    if is_module_file(module, path) {
        Ok(())
    } else {
        Err(Error::validation(format!(
            "ir.ui.view: arch_fs '{path}' is not a module file: it must be '<module>/<path>', \
             without empty, '.' or '..' components"
        )))
    }
}

/// Whether `path` is `<module>/<p1>/.../<pn>` with a non-empty module and every component
/// non-empty and neither `.` nor `..`.
fn is_module_file(module: &str, path: &str) -> bool {
    let Some(rest) = path
        .strip_prefix(module)
        .and_then(|rest| rest.strip_prefix('/'))
    else {
        return false;
    };
    !module.is_empty()
        && !module.contains('/')
        && rest
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
}

/// The `type` selection values of the registry (with every addon's `selection_add`).
fn view_types(tx: &Transaction) -> Result<Vec<String>> {
    let spec = tx
        .registry()
        .model(MODEL)
        .ok_or_else(|| Error::invariant("the registry has no ir.ui.view model"))?;
    match spec.field("type").map(|f| &f.ty) {
        Some(erp_sdk::FieldType::Selection {
            values: Some(values),
            ..
        }) => Ok(values.iter().map(|(v, _)| v.clone()).collect()),
        _ => Err(Error::invariant(
            "ir.ui.view.type is not a static selection field",
        )),
    }
}

/// The root tag of an arch, read from its first start tag without parsing the document (review
/// R-8): the check after the insert parses it once.
fn root_tag(arch: &str) -> Option<String> {
    erp_sdk::views::root_tag(arch).map(str::to_owned)
}

/// Whether `arch` parses on its own (its errors are reported by the check after the insert).
fn parses(arch: &str) -> bool {
    use erp_sdk::views::{ViewCatalogue, ViewDescriptor};
    ViewCatalogue::new(vec![ViewDescriptor::new(0, "", "", arch)])
        .and_then(|c| c.combine(0))
        .is_ok()
}

/// `gen_key.<6 hex>` for a QWeb view without key, from the
/// transaction's own random source in the database.
async fn generated_keys(tx: &mut Transaction, count: usize) -> Result<Vec<String>> {
    let count = i64::try_from(count)
        .map_err(|_| Error::programming("too many QWeb views without key in one create"))?;
    let rows = tx
        .query(
            "SELECT 'gen_key.' || substr(md5(random()::text || clock_timestamp()::text), 1, 6) \
             FROM generate_series(1::int8, $1::int8)",
            &[&count],
        )
        .await?;
    rows.iter()
        .map(|row| {
            row.try_get::<_, String>(0)
                .map_err(|e| Error::db(format!("unexpected generated key: {e}")))
        })
        .collect()
}

/// The `type` of the parents in `ids` (flushed first: a parent can be pending in this
/// transaction).
async fn parent_types(
    tx: &mut Transaction,
    ids: &[i64],
) -> Result<std::collections::HashMap<i64, Option<String>>> {
    if ids.is_empty() {
        return Ok(std::collections::HashMap::new());
    }
    tree::flush(tx).await?;
    let rows = tx
        .query(
            "SELECT id, type FROM ir_ui_view WHERE id = ANY($1)",
            &[&ids],
        )
        .await?;
    let mut types = std::collections::HashMap::with_capacity(rows.len());
    for row in &rows {
        let id: i64 = row
            .try_get(0)
            .map_err(|e| Error::db(format!("unexpected value in ir_ui_view.id: {e}")))?;
        let ty: Option<String> = row
            .try_get(1)
            .map_err(|e| Error::db(format!("unexpected value in ir_ui_view.type: {e}")))?;
        types.insert(id, ty);
    }
    Ok(types)
}

/// The current `inherit_id` of each of `ids` (the records of `records`), read through the ORM.
///
/// # Errors
///
/// `Programming` when the read does not return one parent per view; a read error.
async fn current_parents(
    tx: &mut Transaction,
    records: &Records,
    ids: &[i64],
) -> Result<std::collections::HashMap<i64, Option<i64>>> {
    // Keep warm cache hits, but never prefetch arch_db or other columns on a cold read.
    let parents_only = records.with_context([("prefetch_fields", Value::Bool(false))]);
    let cells = parents_only.cells(tx, "inherit_id").await?;
    if cells.len() != ids.len() {
        return Err(Error::invariant(format!(
            "ir.ui.view: read {} parents for {} views",
            cells.len(),
            ids.len()
        )));
    }
    let mut parents = std::collections::HashMap::with_capacity(ids.len());
    for (id, cell) in ids.iter().zip(cells) {
        let parent = match cell {
            Cell::Id(p) => Some(p),
            Cell::Null => None,
            other => {
                return Err(Error::invariant(format!(
                    "ir.ui.view {id}: inherit_id read as {other:?}"
                )));
            }
        };
        parents.insert(*id, parent);
    }
    Ok(parents)
}

#[methods(IrUiView)]
impl IrUiView {
    /// `arch` is `arch_db` (translations come with).
    #[compute(depends = ["arch_db"])]
    fn _compute_arch(batch: &mut Batch<'_, IrUiView>) -> Result<()> {
        for i in batch.indices() {
            let arch = batch.get(i, IrUiView::ARCH_DB)?;
            batch.set(i, IrUiView::ARCH, arch)?;
        }
        Ok(())
    }

    /// Writing `arch` writes `arch_db`.
    #[method]
    async fn _inverse_arch(this: &RecordSet<IrUiView>, tx: &mut Transaction) -> Result<()> {
        for rec in this.iter() {
            let arch = rec.get(tx, IrUiView::ARCH).await?;
            rec.set(tx, IrUiView::ARCH_DB, arch).await?;
        }
        Ok(())
    }

    /// Module data may only reference view types admitted for its declaring module.
    #[constrains("type")]
    async fn _check_h18_type(this: &RecordSet<IrUiView>, tx: &mut Transaction) -> Result<()> {
        for cell in this.as_records().cells(tx, "type").await? {
            match cell {
                Cell::Str(key) => {
                    type_admission::require_loaded_view_type(tx, this.as_records().env(), &key)?
                }
                Cell::Null => {}
                other => {
                    return Err(Error::programming(format!(
                        "ir.ui.view.type read as {other:?}, not text"
                    )));
                }
            }
        }
        Ok(())
    }

    /// An extension view may not have groups.
    #[constrains("group_ids", "inherit_id", "mode")]
    async fn _check_groups(this: &RecordSet<IrUiView>, tx: &mut Transaction) -> Result<()> {
        for rec in this.iter() {
            let groups = rec.get(tx, IrUiView::GROUP_IDS).await?;
            // Without a restriction there is nothing to reject; avoid fetching unrelated
            // columns merely to evaluate an already-false conjunction (DD-1).
            if groups.is_empty() {
                continue;
            }
            let parent = rec.get(tx, IrUiView::INHERIT_ID).await?;
            if parent.is_none() {
                continue;
            }
            let mode = rec.get(tx, IrUiView::MODE).await?;
            if mode.as_deref() != Some("primary") {
                return Err(view_error(ViewRecordError::GroupsOnExtension));
            }
        }
        Ok(())
    }

    #[method(override)]
    async fn create(
        this: &RecordSet<IrUiView>,
        tx: &mut Transaction,
        mut vals_list: Vec<RawVals>,
        sup: Super,
    ) -> Result<Records> {
        let records = this.as_records();
        let context = records.env().context();
        let install_file = install_file(context)?;
        let allowed = view_types(tx)?;
        let parents: Vec<i64> = vals_list
            .iter()
            .map(parent_of)
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .flatten()
            .flatten()
            .collect();
        let parent_type = parent_types(tx, &parents).await?;
        let mut keyless = 0usize;
        for vals in &mut vals_list {
            // only `arch` is a data file's arch: `arch_db` given directly records no file
            // (review A2-R5)
            let from_file = vals.contains("arch");
            check_arch_fs(vals)?;
            move_arch(vals)?;
            let parent = parent_of(vals)?.flatten();
            if text(vals, "type")?.is_none() {
                let ty = match parent {
                    Some(p) => parent_type.get(&p).cloned().flatten(),
                    None => {
                        let Some(arch) = text(vals, "arch_db")? else {
                            return Err(view_error(ViewRecordError::MissingArch));
                        };
                        match root_tag(arch) {
                            Some(tag) if allowed.contains(&tag) => Some(tag),
                            Some(tag) if parses(arch) => {
                                return Err(view_error(ViewRecordError::InvalidType {
                                    tag,
                                    allowed: allowed.clone(),
                                }));
                            }
                            _ => None,
                        }
                    }
                };
                if let Some(ty) = ty {
                    vals.insert("type".into(), Cell::Str(Arc::from(ty)));
                }
            }
            let ty = text(vals, "type")?.map(str::to_owned);
            if text(vals, "key")?.is_none() && ty.as_deref() == Some("qweb") {
                keyless += 1;
            }
            if text(vals, "name")?.is_none() {
                let model = text(vals, "model")?.unwrap_or("None").to_owned();
                let name = format!("{model} {}", ty.as_deref().unwrap_or("None"));
                vals.insert("name".into(), Cell::Str(Arc::from(name)));
            }
            if let Some(file) = &install_file
                && from_file
            {
                vals.insert("arch_fs".into(), Cell::Str(Arc::clone(file)));
                vals.insert("arch_updated".into(), Cell::Bool(false));
            }
            let mode = mode_on_create(parent, explicit_mode(vals)?);
            vals.insert("mode".into(), Cell::Str(Arc::from(mode.as_str())));
        }
        if keyless > 0 {
            let mut keys = generated_keys(tx, keyless).await?.into_iter();
            for vals in &mut vals_list {
                if text(vals, "key")?.is_none() && text(vals, "type")? == Some("qweb") {
                    let key = keys
                        .next()
                        .ok_or_else(|| Error::invariant("fewer generated keys than views"))?;
                    vals.insert("key".into(), Cell::Str(Arc::from(key)));
                }
            }
        }
        let mut has_arch = Vec::with_capacity(vals_list.len());
        for vals in &vals_list {
            has_arch.push(text(vals, "arch_db")?.is_some());
        }
        let created: Records = sup.call(tx, records, (vals_list,)).await?;
        tx.invalidate_ormcache(TEMPLATES);
        // the arch warnings are posted to the transaction notice channel and logged with their
        // views' labels: `create` cannot return them directly.
        let created_ids = created.ids();
        if created_ids.len() != has_arch.len() {
            return Err(Error::invariant(format!(
                "ir.ui.view create returned {} records for {} values",
                created_ids.len(),
                has_arch.len()
            )));
        }
        let checked: Vec<i64> = created_ids
            .iter()
            .zip(&has_arch)
            .filter_map(|(&id, &arch)| arch.then_some(id))
            .collect();
        let to_check = created.browse(checked.iter().copied())?;
        validation::check_and_log(
            tx,
            &to_check,
            &checked,
            validation::Written::Created,
            validation::Loading::Context,
        )
        .await?;
        Ok(created)
    }

    #[method(override)]
    async fn write(
        this: &RecordSet<IrUiView>,
        tx: &mut Transaction,
        mut vals: RawVals,
        sup: Super,
    ) -> Result<()> {
        let records = this.as_records();
        check_arch_fs(&vals)?;
        if vals.contains("arch") {
            match &install_file(records.env().context())? {
                Some(file) => {
                    vals.insert("arch_fs".into(), Cell::Str(Arc::clone(file)));
                    vals.insert("arch_updated".into(), Cell::Bool(false));
                }
                None if !vals.contains("arch_updated") => {
                    vals.insert("arch_updated".into(), Cell::Bool(true));
                }
                None => {}
            }
        }
        move_arch(&mut vals)?;
        let ids = records.ids();
        // `mode` per record (BUG-074): uniform when the parent is cleared or the mode is given;
        // otherwise the views without a parent become extensions and the others keep theirs
        let mut to_extension: Vec<i64> = Vec::new();
        // A moved extension or an extension turned primary stops contributing to its old
        // scope. Keep that parent as a validation seed after the write (BUG-081).
        let mut former_scopes = Vec::new();
        if vals.contains("mode") && !vals.contains("inherit_id") {
            former_scopes.extend(
                current_parents(tx, records, &ids)
                    .await?
                    .into_values()
                    .flatten(),
            );
        }
        if let Some(new_parent) = parent_of(&vals)? {
            // the roots of the trees the views leave (review A-D4) and of the tree they join are
            // locked in one statement, in id order, before the write: two crossed moves wait for
            // each other instead of locking roots in opposite orders (review D-3)
            //
            // A write that keeps every view's parent (a data file's view on upgrade always gives
            // `inherit_id`) moves nothing between trees: the parents are read through the ORM
            // (its cache, or a read of inherit_id only) and nothing is locked before the write; the check
            // after it locks the tree, as for a write of `arch` (review D-1). A concurrent move of
            // one of these views makes the write itself fail under `REPEATABLE READ`.
            let current = current_parents(tx, records, &ids).await?;
            let old = if current.values().all(|p| *p == new_parent) {
                current
            } else {
                let mut lock_ids = ids.clone();
                lock_ids.extend(new_parent);
                tree::lock_roots(tx, &lock_ids).await?
            };
            former_scopes.extend(old.values().filter_map(|parent| {
                if *parent != new_parent || vals.contains("mode") {
                    *parent
                } else {
                    None
                }
            }));
            let explicit = explicit_mode(&vals)?;
            let modes: Vec<(i64, Option<ViewMode>)> = ids
                .iter()
                .map(|id| {
                    let had_parent = old.get(id).copied().flatten().is_some();
                    (*id, mode_on_write(had_parent, new_parent, explicit))
                })
                .collect();
            let first = modes.first().map(|(_, m)| *m);
            if modes.iter().all(|(_, m)| Some(*m) == first) {
                if let Some(Some(mode)) = first {
                    vals.insert("mode".into(), Cell::Str(Arc::from(mode.as_str())));
                }
            } else {
                // only a newly set parent mixes `extension` with `keep`
                for (id, mode) in &modes {
                    match mode {
                        Some(ViewMode::Extension) => to_extension.push(*id),
                        None => {}
                        Some(ViewMode::Primary) => {
                            return Err(Error::invariant(
                                "ir.ui.view: a write that sets a parent made a view primary",
                            ));
                        }
                    }
                }
            }
        }
        let checked = CHECKED_ON_WRITE.iter().any(|f| vals.contains(f));
        sup.call(tx, records, (vals,)).await?;
        if !to_extension.is_empty() {
            // a separate write of `mode` (through the whole chain) on the views that had no parent
            let vals = RawVals::new().set_cell("mode", Cell::Str(Arc::from("extension")));
            records.browse(to_extension)?.write(tx, vals).await?;
        }
        tx.invalidate_ormcache(TEMPLATES);
        if checked {
            let in_tracked_batch = records
                .env()
                .context()
                .get(erp_sdk::load_state::TRACK_BATCH)
                .is_some();
            if in_tracked_batch && former_scopes.is_empty() {
                // Batch-level upgrade check validates these views in one check per window
                // (review D-1).
            } else {
                // When former_scopes is non-empty (e.g. view moved to a different parent),
                // validate former scopes immediately here so the orphaned/former tree is
                // verified before further operations; the window check will re-verify the
                // view in its final target position (R-6).
                let mut affected = ids;
                affected.extend(former_scopes);
                affected.sort_unstable();
                affected.dedup();
                // posted to the transaction notice channel and logged, as in `create`
                validation::check_and_log(
                    tx,
                    records,
                    &affected,
                    validation::Written::Changed,
                    validation::Loading::Context,
                )
                .await?;
            }
        }
        Ok(())
    }

    #[method(override)]
    async fn unlink(this: &RecordSet<IrUiView>, tx: &mut Transaction, sup: Super) -> Result<()> {
        let records = this.as_records();
        // an uninstall deletes the views built on them too, as `ondelete="cascade"` would
        // : collect the subtree children first, then re-enter unlink
        // through the whole chain so other addon overrides see every cascaded record too.
        // That re-entry repeats the walk; bypassing it with `sup` would skip overrides above
        // this one for the newly found records (DD-2). The walk ends on a cycle in
        // `inherit_id` (review S-5). The ORM deletes in chunks and `inherit_id` is
        // `ondelete="restrict"`, so the set itself is deleted children first too, its own views
        // included: no chunk removes a parent while a later chunk still holds its child (review
        // D-6). The call on the subtree finds nothing more outside it, unless the subtree was
        // deeper than the walk's bound, and then goes on from there.
        if records
            .env()
            .context()
            .get("force_delete")
            .is_some_and(|v| v.is_truthy())
        {
            let ids = records.ids();
            let given: std::collections::HashSet<i64> = ids.iter().copied().collect();
            let mut ordered = tree::subtree_children_first(tx, &ids).await?;
            let outside = ordered.iter().any(|id| !given.contains(id));
            // the ids that do not exist stay in the call, as without `force_delete`
            let found: std::collections::HashSet<i64> = ordered.iter().copied().collect();
            ordered.extend(ids.iter().copied().filter(|id| !found.contains(id)));
            if outside {
                return records.browse(ordered)?.unlink(tx).await;
            }
            let ordered = records.browse(ordered)?;
            sup.call(tx, &ordered, ()).await?;
            tx.invalidate_ormcache(TEMPLATES);
            return Ok(());
        }
        sup.call(tx, records, ()).await?;
        tx.invalidate_ormcache(TEMPLATES);
        Ok(())
    }

    /// Refuse a row-versioned `ir.ui.view` on the final registry of every load (review A3-R2;
    /// `tree::refuse_row_version`).
    #[method(override)]
    async fn _register_hook(
        this: &RecordSet<IrUiView>,
        tx: &mut Transaction,
        sup: Super,
    ) -> Result<()> {
        tree::refuse_row_version(tx.registry())?;
        sup.call(tx, this.as_records(), ()).await
    }

    /// Check the views of `module` that this operation loaded under a `noupdate` external id:
    /// their writes were skipped, so nothing checked them against the upgraded views
    /// (`_validate_module_views`). `names` are the names (without
    /// the module) the operation loaded; `pending` the modules of the operation whose data has
    /// not loaded yet: their sibling views still hold the archs of before the upgrade and are not
    /// combined, whatever the context (review F-2). Called by the module
    /// system at the end of each upgraded module's data.
    #[method]
    async fn _validate_module_views(
        this: &RecordSet<IrUiView>,
        tx: &mut Transaction,
        module: String,
        names: Vec<String>,
        pending: Vec<String>,
    ) -> Result<()> {
        if names.is_empty() {
            return Ok(());
        }
        let ids = tree::noupdate_views(tx, &module, &names).await?;
        if ids.is_empty() {
            return Ok(());
        }
        let records = this.as_records().browse(ids.iter().copied())?;
        validation::check_and_log(
            tx,
            &records,
            &ids,
            validation::Written::Changed,
            validation::Loading::AfterModule(&pending),
        )
        .await?;
        Ok(())
    }

    /// Check the xml of the views in this recordset (`_check_xml`).
    /// In a data load window with tracked view batch, called once per window across all updated views
    /// (review D-1).
    #[method]
    async fn _check_xml(this: &RecordSet<IrUiView>, tx: &mut Transaction) -> Result<()> {
        let records = this.as_records();
        let ids = records.ids();
        if ids.is_empty() {
            return Ok(());
        }
        validation::check_and_log(
            tx,
            records,
            &ids,
            validation::Written::Changed,
            validation::Loading::Context,
        )
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod h18_admission_tests {
    use super::*;

    #[test]
    fn caller_context_does_not_establish_module_loading_authority() {
        let context = erp_sdk::Context::from_pairs([
            ("install_module", Value::str("web")),
            ("__erp_data_module", Value::str("web")),
        ]);
        assert_eq!(
            super::type_admission::loading_module(false, &context).unwrap(),
            None
        );
        assert_eq!(
            super::type_admission::loading_module(true, &context)
                .unwrap()
                .as_deref(),
            Some("web")
        );
    }

    #[test]
    fn managed_module_loading_context_refuses_a_wrong_type() {
        let context = erp_sdk::Context::from_pairs([("__erp_data_module", Value::Int(4))]);
        let error = super::type_admission::loading_module(true, &context).unwrap_err();
        assert_eq!(error.kind(), erp_sdk::ErrorKind::Programming);
        assert_eq!(
            error.message(),
            "H18 declaring module context __erp_data_module must be text"
        );
    }
}
