//! Actions (`ir.actions.*`; design, §6).
//!
//! Every action type has its own table (`ir_act_window`, `ir_act_url`, `ir_act_client`,
//! `ir_act_server`), and they share one id space: each typed model draws its ids from the
//! named sequence `ir_actions_id_seq`. `ir.actions.actions` is the key table
//! `ir_actions`: one row per action of any type, with the type, the name, the `path` and the
//! binding columns, kept in step by the typed models' `create` and `write` in the same statement
//! batch, and by a statement-level `AFTER DELETE` trigger on every typed table (`follow_deletes`),
//! which also covers the rows a foreign key deletes by cascade (a `multi` action's children, the
//! actions of a deleted `ir.model` or `ir.model.fields`) and bumps the `actions` cache generation
//! for every process.
//!
//! What each model adds:
//!
//! - [`IrActionsBase`] (abstract): the fields every type shares, the `type` default (the typed
//!   model's name), the path checks ([`check_path`]) and the key-table upkeep;
//! - [`IrActionsActWindow`]: the computed `views` and the `view_mode` and model checks
//!   ([`window`]); [`IrActionsActWindowView`], [`IrActionsActUrl`], [`IrActionsClient`];
//! - [`IrActionsServer`]: every column of a server action; running them is;
//! - [`get_bindings`] ([`bindings`]): the contextual actions of a model for a user.
//!
//! Not here yet: `ir.actions.report` (the report runtime slices), translated names,
//! `/web/action/load` and `_get_action_dict`, the clean-up of
//! `ir.actions.todo` and `res.users.action_id` at unlink ([`CLEANUPS`]: those models and
//! fields do not exist yet, and an action unlink is refused once one of them is present without
//! its clean-up; `ir.filters.action_id` is `ondelete = "cascade"`, so the database removes the
//! saved filters of a deleted action).

pub mod bindings;
pub mod path;
mod server_rules;
pub mod window;

use erp_sdk::prelude::*;
use erp_sdk::{ErrorKind, Operation, RecId};

pub use bindings::{ActionId, Binding, Bindings, action_by_id, get_bindings};
pub use path::{ActionPathError, MAX_PATH_BYTES, check_path};

pub const ACTIONS_CACHE: &str = "actions";

/// The id sequence every action type shares.
pub const ACTIONS_ID_SEQUENCE: &str = "ir_actions_id_seq";

/// The key table.
const KEY_TABLE: &str = "ir_actions";

/// The database name of the key table's unique path index (`_path_unique` on `ir_actions`).
const PATH_UNIQUE: &str = "ir_actions_path_unique";

/// No board task brings these models or
/// fields yet; the task named is the one whose scope reaches them first. [`check_cleanups`]
/// refuses an action unlink (and the final registry, `_register_hook`) once one of them exists,
/// so the clean-up cannot be forgotten silently.
pub const CLEANUPS: [(&str, &str, &str); 2] = [
    // the configuration wizards' to-do list: Settings is the first scope that reaches it
    ("ir.actions.todo", "", "a later release"),
    // a user's home action, of the user form
    ("res.users", "action_id", "a later release"),
];

/// Refuse an action model (one that inherits `ir.actions.base` and has a table) that does not
/// draw from [`ACTIONS_ID_SEQUENCE`] or has no follow-delete trigger into the key table: its ids
/// would collide with the other types', or its deleted rows would leave key rows behind.
///
/// # Errors
///
/// `Programming` naming the model and the declaration it lacks.
pub fn check_action_model(registry: &erp_sdk::Registry, model: &str) -> Result<()> {
    let Some(spec) = registry.model(model) else {
        return Err(Error::programming(format!(
            "no model '{model}' in the registry"
        )));
    };
    if !(spec.inherits("ir.actions.base") && spec.has_table()) {
        return Ok(());
    }
    if spec.id_sequence != Some(ACTIONS_ID_SEQUENCE) || !spec.follows_deletes_into(KEY_TABLE) {
        return Err(Error::programming(format!(
            "action model '{model}' must declare id_sequence = ACTIONS_ID_SEQUENCE and \
             follow_deletes = [(\"_keys_follow\", cache = ACTIONS_CACHE, key_table = \
             \"{KEY_TABLE}\")]: action ids are shared across the action tables and their key rows \
             follow every deletion"
        )));
    }
    Ok(())
}

/// # Errors
///
/// `Programming` naming the model or field and the task that adds the clean-up.
pub fn check_cleanups(registry: &erp_sdk::Registry) -> Result<()> {
    for (model, field, task) in CLEANUPS {
        let present = match registry.model(model) {
            Some(_) if field.is_empty() => true,
            Some(spec) => spec.field(field).is_some(),
            None => false,
        };
        if present {
            let what = if field.is_empty() {
                format!("model '{model}'")
            } else {
                format!("field {model}.{field}")
            };
            return Err(Error::programming(format!(
                "the registry has {what}, but deleting actions does not clean it up yet: \
                 {task} must add that clean-up to the unlink of ir.actions.base"
            )));
        }
    }
    Ok(())
}

/// The columns the key table copies from the typed tables (same names on both).
const KEY_COLUMNS: [&str; 6] = [
    "name",
    "type",
    "path",
    "binding_model_id",
    "binding_type",
    "binding_view_types",
];

/// `ir.actions.actions`: the key table, one row per action of any type (design m2-s3 §3).
#[model(
    name = "ir.actions.actions",
    description = "Action",
    table = "ir_actions",
    order = "name, id",
    log_access = false,
    allow_sudo_commands = false,
    id_sequence = ACTIONS_ID_SEQUENCE,
    unique_indexes = [(
        "_path_unique",
        "(path) WHERE path IS NOT NULL",
        "Path to show in the URL must be unique! Please choose another one."
    )],
    indexes = [("_binding_model_id_id_idx", "(binding_model_id, id)")],
    // key rows deleted by a cascade of `binding_model_id` change the bindings of every process
    follow_deletes = [("_cache_follow", cache = ACTIONS_CACHE)]
)]
pub struct IrActionsActions {
    /// Action name.
    #[field(char, required = true, string = "Action Name")]
    pub name: Option<Arc<str>>,
    /// The typed model of the action (`ir.actions.act_window`, …).
    #[field(char, required = true, string = "Action Type")]
    pub r#type: Option<Arc<str>>,
    /// The URL segment of the action.
    #[field(char, string = "Path to show in the URL")]
    pub path: Option<Arc<str>>,
    /// The model whose contextual menu shows the action.
    #[field(many2one, comodel = "ir.model", ondelete = "cascade")]
    pub binding_model_id: Option<RecId>,
    /// `action` or `report`.
    #[field(
        selection,
        selection = [("action", "Action"), ("report", "Report")],
        required = true,
        default = "action"
    )]
    pub binding_type: Option<Arc<str>>,
    /// The view types the binding shows in.
    #[field(char, default = "list,form")]
    pub binding_view_types: Option<Arc<str>>,
}

/// The refusal of a direct write of the key table.
fn key_table_refusal() -> Error {
    Error::programming(
        "ir.actions.actions rows follow their typed action; write the typed action instead",
    )
}

#[methods(IrActionsActions)]
impl IrActionsActions {
    #[method(override)]
    async fn create(
        _this: &RecordSet<IrActionsActions>,
        _tx: &mut Transaction,
        _vals_list: Vec<RawVals>,
        _sup: Super,
    ) -> Result<Records> {
        Err(key_table_refusal())
    }

    #[method(override)]
    async fn write(
        _this: &RecordSet<IrActionsActions>,
        _tx: &mut Transaction,
        _vals: RawVals,
        _sup: Super,
    ) -> Result<()> {
        Err(key_table_refusal())
    }

    #[method(override)]
    async fn unlink(
        _this: &RecordSet<IrActionsActions>,
        _tx: &mut Transaction,
        _sup: Super,
    ) -> Result<()> {
        Err(key_table_refusal())
    }

    #[method(override)]
    async fn upsert(
        _this: &RecordSet<IrActionsActions>,
        _tx: &mut Transaction,
        _call: erp_sdk::UpsertCall,
        _sup: Super,
    ) -> Result<erp_sdk::UpsertResult> {
        Err(key_table_refusal())
    }

    /// The registry is final: refuse it when actions could not be deleted cleanly
    /// ([`check_cleanups`]) or an action model lacks the shared sequence or its trigger
    /// ([`check_action_model`]).
    #[method(override)]
    async fn _register_hook(
        this: &RecordSet<IrActionsActions>,
        tx: &mut Transaction,
        sup: Super,
    ) -> Result<()> {
        let registry = std::sync::Arc::clone(this.as_records().env().registry());
        check_cleanups(&registry)?;
        for spec in registry.models() {
            check_action_model(&registry, spec.name)?;
        }
        sup.call(tx, this.as_records(), ()).await
    }

    /// Read contextual bindings for a model and optional view type.
    /// Returns a map with "action" and "print" arrays of binding definitions.
    #[method]
    #[rpc(effect = read, model)]
    pub async fn erp_get_bindings(
        this: &RecordSet<IrActionsActions>,
        tx: &mut Transaction,
        model: String,
        view_type: Option<String>,
    ) -> Result<Value> {
        Self::read_bindings(this, tx, model, view_type).await
    }

    /// Alias for `erp_get_bindings`.
    #[method]
    #[rpc(effect = read, model)]
    pub async fn get_bindings(
        this: &RecordSet<IrActionsActions>,
        tx: &mut Transaction,
        model: String,
        view_type: Option<String>,
    ) -> Result<Value> {
        Self::read_bindings(this, tx, model, view_type).await
    }
}

impl IrActionsActions {
    async fn read_bindings(
        this: &RecordSet<IrActionsActions>,
        tx: &mut Transaction,
        model: String,
        view_type: Option<String>,
    ) -> Result<Value> {
        let env = this.as_records().env();
        if env.registry().model(&model).is_none() {
            return Err(Error::validation(format!(
                "erp_get_bindings: unknown model '{model}'"
            )));
        }

        // Caller must have Read access on the target model
        let records = env.records(&model)?;
        records.check_access(tx, Operation::Read).await?;

        let bindings = get_bindings(tx, env, &model).await?;

        let matches_view_type = |values: &erp_sdk::ValueMap| -> bool {
            let Some(req) = view_type.as_deref() else {
                return true;
            };
            match values.get("binding_view_types") {
                None | Some(Value::False) => true,
                Some(Value::Str(types)) => types.split(',').map(str::trim).any(|t| t == req),
                _ => true,
            }
        };

        let mut action_list = Vec::new();
        for b in bindings.action() {
            if matches_view_type(b.values()) {
                action_list.push(Value::Map(b.values().clone()));
            }
        }

        let mut print_list = Vec::new();
        for b in bindings.report() {
            if matches_view_type(b.values()) {
                print_list.push(Value::Map(b.values().clone()));
            }
        }

        let mut result = erp_sdk::ValueMap::new();
        result.insert("action".to_owned(), Value::List(action_list.into()));
        result.insert("print".to_owned(), Value::List(print_list.into()));

        Ok(Value::Map(Arc::new(result)))
    }
}

/// `ir.actions.base`: the fields every action type has, and the upkeep of the key table.
#[model(
    name = "ir.actions.base",
    description = "Action (shared fields)",
    kind = "abstract"
)]
pub struct IrActionsBase {
    /// Action name.
    #[field(char, required = true, string = "Action Name")]
    pub name: Option<Arc<str>>,
    /// The typed model of the action; set from the model on create.
    #[field(char, required = true, string = "Action Type")]
    pub r#type: Option<Arc<str>>,
    /// The URL segment of the action ([`check_path`]).
    #[field(char, string = "Path to show in the URL")]
    pub path: Option<Arc<str>>,
    /// Help text for the users.
    #[field(html, string = "Action Description")]
    pub help: Option<Arc<str>>,
    /// The model whose contextual menu shows the action.
    #[field(many2one, comodel = "ir.model", ondelete = "cascade", index = true)]
    pub binding_model_id: Option<RecId>,
    /// `action` or `report`.
    #[field(
        selection,
        selection = [("action", "Action"), ("report", "Report")],
        required = true,
        default = "action"
    )]
    pub binding_type: Option<Arc<str>>,
    /// The view types the binding shows in.
    #[field(char, default = "list,form")]
    pub binding_view_types: Option<Arc<str>>,
}

/// Quote an SQL identifier (registry table names are checked identifiers; the quoting keeps it
/// exact).
pub(crate) fn ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// The `type` of the action model `model` in `vals`: the model's name when absent, refused when
/// it names another model (a row of the key table must say which table holds it).
fn check_type(model: &str, vals: &mut RawVals) -> Result<()> {
    match vals.get("type") {
        None | Some(Cell::Null) => {
            vals.insert("type".into(), Cell::Str(Arc::from(model)));
            Ok(())
        }
        Some(Cell::Str(given)) if &**given == model => Ok(()),
        Some(Cell::Str(given)) => Err(Error::validation(format!(
            "The type of an action of model '{model}' is '{model}', not '{given}'"
        ))),
        Some(_) => Err(Error::validation(format!(
            "The type of an action of model '{model}' is '{model}', not a value that is not text"
        ))),
    }
}

/// The path checks of `vals`, before anything is written.
fn check_vals_path(vals: &RawVals) -> Result<()> {
    match vals.get("path") {
        Some(Cell::Str(path)) => check_path(path).map_err(Error::from),
        // unset, cleared, or a value of another type, which the field conversion refuses
        _ => Ok(()),
    }
}

/// A unique violation of `_path_unique` (by its constraint name) as the typed
/// [`ActionPathError::Duplicate`]; any other error unchanged.
fn path_conflict(error: Error) -> Error {
    if error.kind() == ErrorKind::Validation
        && error.sqlstate() == Some("23505")
        && error.constraint() == Some(PATH_UNIQUE)
    {
        Error::from(ActionPathError::Duplicate)
    } else {
        error
    }
}

/// Write the key rows of `ids` of the typed records' model from its table: one statement.
async fn insert_keys(tx: &mut Transaction, records: &Records, ids: &[i64]) -> Result<()> {
    let spec = records.spec();
    tx.flush_model(spec.id).await?;
    let columns = KEY_COLUMNS.join(", ");
    let sql = format!(
        "INSERT INTO {KEY_TABLE} (id, {columns}) SELECT id, {columns} FROM {} WHERE id = ANY($1) \
         ORDER BY id",
        ident(&spec.table)
    );
    let written = tx.execute(&sql, &[&ids]).await.map_err(path_conflict)?;
    expect_rows(records, "inserted", written, ids.len())
}

/// Copy the key columns of `ids` from the typed table to the key table: one statement.
async fn update_keys(tx: &mut Transaction, records: &Records, ids: &[i64]) -> Result<()> {
    let spec = records.spec();
    tx.flush_model(spec.id).await?;
    let sets = KEY_COLUMNS
        .iter()
        .map(|c| format!("{c} = t.{c}"))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "UPDATE {KEY_TABLE} AS a SET {sets} FROM {} AS t WHERE a.id = t.id AND t.id = ANY($1)",
        ident(&spec.table)
    );
    let written = tx.execute(&sql, &[&ids]).await.map_err(path_conflict)?;
    expect_rows(records, "updated", written, ids.len())
}

fn expect_rows(records: &Records, what: &str, written: u64, expected: usize) -> Result<()> {
    if usize::try_from(written).ok() == Some(expected) {
        return Ok(());
    }
    let spec = records.spec();
    Err(Error::programming(format!(
        "{what} {written} rows of {KEY_TABLE} for {expected} {} records: the key table is out \
         of step with {} (rows written outside the ORM?)",
        spec.name, spec.table
    )))
}

/// Whether `vals` writes a column the key table copies.
fn touches_keys(vals: &RawVals) -> bool {
    KEY_COLUMNS.iter().any(|c| vals.contains(c))
}

/// The database ids of a recordset, sorted and unique (a recordset may repeat a record); a new
/// record (no row) is an error, never dropped.
fn row_ids(records: &Records) -> Result<Vec<i64>> {
    let mut ids = records
        .rec_ids()
        .iter()
        .map(|id| {
            crate::group_graph::row_id(*id).ok_or_else(|| {
                Error::programming(format!(
                    "{} record {id} has no database row; save it first",
                    records.spec().name
                ))
            })
        })
        .collect::<Result<Vec<_>>>()?;
    ids.sort_unstable();
    ids.dedup();
    Ok(ids)
}

/// After any change of action records: the bindings cache and the key table's records.
async fn invalidate(tx: &mut Transaction) -> Result<()> {
    tx.invalidate_ormcache(ACTIONS_CACHE);
    tx.invalidate_model("ir.actions.actions", None).await
}

#[methods(IrActionsBase)]
impl IrActionsBase {
    #[method(override)]
    async fn create(
        this: &RecordSet<IrActionsBase>,
        tx: &mut Transaction,
        mut vals_list: Vec<RawVals>,
        sup: Super,
    ) -> Result<Records> {
        let model = this.as_records().spec().name;
        check_action_model(this.as_records().env().registry(), model)?;
        for vals in &mut vals_list {
            check_type(model, vals)?;
            check_vals_path(vals)?;
        }
        let created: Records = sup.call(tx, this.as_records(), (vals_list,)).await?;
        let ids = row_ids(&created)?;
        if !ids.is_empty() {
            insert_keys(tx, &created, &ids).await?;
        }
        invalidate(tx).await?;
        Ok(created)
    }

    #[method(override)]
    async fn write(
        this: &RecordSet<IrActionsBase>,
        tx: &mut Transaction,
        mut vals: RawVals,
        sup: Super,
    ) -> Result<()> {
        let model = this.as_records().spec().name;
        if vals.contains("type") {
            check_type(model, &mut vals)?;
        }
        check_vals_path(&vals)?;
        let keys = touches_keys(&vals);
        sup.call(tx, this.as_records(), (vals,)).await?;
        let ids = row_ids(this.as_records())?;
        if keys && !ids.is_empty() {
            update_keys(tx, this.as_records(), &ids).await?;
        }
        invalidate(tx).await
    }

    /// The key rows go with the typed rows: the table's `follow_deletes` trigger removes them in
    /// the same statement (and those of the rows a cascade removes). Refused while a clean-up of
    /// [`CLEANUPS`] is missing.
    #[method(override)]
    async fn unlink(
        this: &RecordSet<IrActionsBase>,
        tx: &mut Transaction,
        sup: Super,
    ) -> Result<()> {
        check_cleanups(this.as_records().env().registry())?;
        sup.call(tx, this.as_records(), ()).await?;
        invalidate(tx).await
    }

    /// Refused: `upsert` does not run `create`, so an inserted action would have no
    /// row in the key table, and a key row could not follow an update.
    #[method(override)]
    async fn upsert(
        this: &RecordSet<IrActionsBase>,
        _tx: &mut Transaction,
        _call: erp_sdk::UpsertCall,
        _sup: Super,
    ) -> Result<erp_sdk::UpsertResult> {
        Err(Error::programming(format!(
            "upsert on {} is refused: it does not run create and write, so the key table \
             {KEY_TABLE} would not follow; use create and write",
            this.as_records().spec().name
        )))
    }
}

/// `ir.actions.act_window`: opens views of a model (`views`, `view_mode`: [`window`]).
#[model(
    name = "ir.actions.act_window",
    description = "Action Window",
    table = "ir_act_window",
    inherit = [IrActionsBase],
    order = "name, id",
    allow_sudo_commands = false,
    id_sequence = ACTIONS_ID_SEQUENCE,
    follow_deletes = [
        ("_keys_follow", cache = ACTIONS_CACHE, key_table = KEY_TABLE),
        ("_groups_follow", cache = ACTIONS_CACHE, relation = "group_ids")
    ]
)]
pub struct IrActionsActWindow {
    /// The view to open first for its type.
    #[field(
        many2one,
        comodel = "ir.ui.view",
        ondelete = "set null",
        string = "View Ref."
    )]
    pub view_id: Option<RecId>,
    /// Domain of the records shown, as an expression.
    #[field(char, string = "Domain Value")]
    pub domain: Option<Arc<str>>,
    /// Context of the action, as an expression.
    #[field(char, required = true, default = "{}", string = "Context Value")]
    pub context: Option<Arc<str>>,
    /// Record to open in the form view.
    #[field(integer, string = "Record ID")]
    pub res_id: i32,
    /// The model the action shows.
    #[field(char, required = true, string = "Destination Model")]
    pub res_model: Option<Arc<str>>,
    /// Where the action opens.
    #[field(
        selection,
        selection = [
            ("current", "Current Window"), ("new", "New Window"), ("fullscreen", "Full Screen"),
            ("main", "Main action of Current Window")
        ],
        default = "current",
        string = "Target Window"
    )]
    pub target: Option<Arc<str>>,
    /// Comma-separated view types, in order ([`window`]).
    #[field(char, required = true, default = "list,form")]
    pub view_mode: Option<Arc<str>>,
    /// First view type on small screens.
    #[field(char, default = "kanban")]
    pub mobile_view_mode: Option<Arc<str>>,
    /// Filters menu and home actions.
    #[field(char, string = "Action Usage")]
    pub usage: Option<Arc<str>>,
    /// The views given per type.
    #[field(
        one2many,
        comodel = "ir.actions.act_window.view",
        inverse_name = "act_window_id",
        string = "No of Views"
    )]
    pub view_ids: Vec<RecId>,
    /// `[(view id or false, view type)]` in display order ([`window`]).
    #[field(json, compute = "_compute_views")]
    pub views: Option<erp_sdk::serde_json::Value>,
    /// Default limit of the list view.
    #[field(integer, default = 80)]
    pub limit: i32,
    /// Groups that see the action; none: everybody.
    #[field(
        many2many,
        comodel = "res.groups",
        relation = "ir_act_window_group_rel",
        column1 = "act_id",
        column2 = "gid",
        string = "Groups"
    )]
    pub group_ids: Vec<RecId>,
    /// The search view.
    #[field(many2one, comodel = "ir.ui.view", string = "Search View Ref.")]
    pub search_view_id: Option<RecId>,
    /// A filter action.
    #[field(boolean)]
    pub filter: bool,
    /// Let the client cache the data the views read.
    #[field(boolean, default = true, string = "Data Caching")]
    pub cache: bool,
}

/// `ir.actions.act_window.view`: a view given for one view type of a window action.
#[model(
    name = "ir.actions.act_window.view",
    description = "Action Window View",
    table = "ir_act_window_view",
    order = "sequence, id",
    rec_name = "view_id",
    allow_sudo_commands = false,
    unique_indexes = [(
        "_unique_mode_per_action",
        "(act_window_id, view_mode)",
        "A window action has at most one view per view type."
    )]
)]
pub struct IrActionsActWindowView {
    /// Order among the action's views.
    #[field(integer)]
    pub sequence: i32,
    /// The view.
    #[field(many2one, comodel = "ir.ui.view", string = "View")]
    pub view_id: Option<RecId>,
    /// Its view type.
    #[field(
        selection,
        selection = [
            ("list", "List"), ("form", "Form"), ("graph", "Graph"), ("pivot", "Pivot"),
            ("calendar", "Calendar"), ("kanban", "Kanban")
        ],
        required = true,
        string = "View Type"
    )]
    pub view_mode: Option<Arc<str>>,
    /// The action.
    #[field(
        many2one,
        comodel = "ir.actions.act_window",
        ondelete = "cascade",
        index = true,
        string = "Action"
    )]
    pub act_window_id: Option<RecId>,
    /// Not shown in the toolbar of a form view.
    #[field(boolean, string = "On Multiple Doc.")]
    pub multi: bool,
}

/// `ir.actions.act_url`: opens a URL.
#[model(
    name = "ir.actions.act_url",
    description = "Action URL",
    table = "ir_act_url",
    inherit = [IrActionsBase],
    order = "name, id",
    allow_sudo_commands = false,
    id_sequence = ACTIONS_ID_SEQUENCE,
    follow_deletes = [("_keys_follow", cache = ACTIONS_CACHE, key_table = KEY_TABLE)]
)]
pub struct IrActionsActUrl {
    /// The URL.
    #[field(text, required = true, string = "Action URL")]
    pub url: Option<Arc<str>>,
    /// Where it opens.
    #[field(
        selection,
        selection = [("new", "New Window"), ("self", "This Window"), ("download", "Download")],
        required = true,
        default = "new",
        string = "Action Target"
    )]
    pub target: Option<Arc<str>>,
}

/// `ir.actions.client`: runs a client-side action by its tag.
#[model(
    name = "ir.actions.client",
    description = "Client Action",
    table = "ir_act_client",
    inherit = [IrActionsBase],
    order = "name, id",
    allow_sudo_commands = false,
    id_sequence = ACTIONS_ID_SEQUENCE,
    follow_deletes = [("_keys_follow", cache = ACTIONS_CACHE, key_table = KEY_TABLE)]
)]
pub struct IrActionsClient {
    /// The client action's tag.
    #[field(char, required = true, string = "Client action tag")]
    pub tag: Option<Arc<str>>,
    /// Where it opens.
    #[field(
        selection,
        selection = [
            ("current", "Current Window"), ("new", "New Window"), ("fullscreen", "Full Screen"),
            ("main", "Main action of Current Window")
        ],
        default = "current",
        string = "Target Window"
    )]
    pub target: Option<Arc<str>>,
    /// Optional model.
    #[field(char, string = "Destination Model")]
    pub res_model: Option<Arc<str>>,
    /// Context, as an expression.
    #[field(char, required = true, default = "{}", string = "Context Value")]
    pub context: Option<Arc<str>>,
    /// Parameters passed to the client action.
    #[field(json, string = "Supplementary arguments")]
    pub params: Option<erp_sdk::serde_json::Value>,
}

/// `ir.actions.server`: every column of a server action (design m2-s3 §3). Running them is
/// [`crate::server_action`]; the rules on `run_as_superuser` and the loop check of
/// `parent_id`/`child_ids` are here ([`IrActionsServer`]'s `create` and `write`).
#[model(
    name = "ir.actions.server",
    description = "Server Actions",
    table = "ir_act_server",
    inherit = [IrActionsBase],
    order = "sequence, name, id",
    allow_sudo_commands = false,
    id_sequence = ACTIONS_ID_SEQUENCE,
    follow_deletes = [
        ("_keys_follow", cache = ACTIONS_CACHE, key_table = KEY_TABLE),
        ("_groups_follow", cache = ACTIONS_CACHE, relation = "group_ids")
    ]
)]
pub struct IrActionsServer {
    /// Server action or scheduled action.
    #[field(
        selection,
        selection = [("ir_actions_server", "Server Action"), ("ir_cron", "Scheduled Action")],
        required = true,
        default = "ir_actions_server",
        string = "Usage"
    )]
    pub usage: Option<Arc<str>>,
    /// What the action does; addons add states with `selection_add`.
    #[field(
        selection,
        selection = [
            ("object_write", "Update Record"), ("object_create", "Create Record"),
            ("object_copy", "Duplicate Record"), ("code", "Execute Code"),
            ("webhook", "Send Webhook Notification"), ("multi", "Multi Actions")
        ],
        required = true,
        string = "Type"
    )]
    pub state: Option<Arc<str>>,
    /// Order among the children of a `multi` action and among bindings (lower first).
    #[field(integer, default = 5)]
    pub sequence: i32,
    /// The model the action runs on.
    #[field(
        many2one,
        comodel = "ir.model",
        required = true,
        ondelete = "cascade",
        index = true,
        string = "Model"
    )]
    pub model_id: Option<RecId>,
    /// The Luau body of a `code` action.
    #[field(text, groups = "base.group_system", string = "Code")]
    pub code: Option<Arc<str>>,
    /// The `multi` action this one is a child of.
    #[field(
        many2one,
        comodel = "ir.actions.server",
        ondelete = "cascade",
        index = true,
        string = "Parent Action"
    )]
    pub parent_id: Option<RecId>,
    /// The children of a `multi` action.
    #[field(
        one2many,
        comodel = "ir.actions.server",
        inverse_name = "parent_id",
        string = "Child Actions"
    )]
    pub child_ids: Vec<RecId>,
    /// The model an `object_create` action creates a record of.
    #[field(many2one, comodel = "ir.model", string = "Record to Create")]
    pub crud_model_id: Option<RecId>,
    /// The field path an `object_write` action writes.
    #[field(char, string = "Field to Update Path")]
    pub update_path: Option<Arc<str>>,
    /// The field an `object_write` action writes.
    #[field(
        many2one,
        comodel = "ir.model.fields",
        ondelete = "cascade",
        string = "Field to Update"
    )]
    pub update_field_id: Option<RecId>,
    /// The value written or created.
    #[field(text)]
    pub value: Option<Arc<str>>,
    /// How `value` is read.
    #[field(
        selection,
        selection = [("value", "Update"), ("sequence", "Sequence"), ("equation", "Compute")],
        default = "value",
        string = "Value Type"
    )]
    pub evaluation_type: Option<Arc<str>>,
    /// The many2many operation of an `object_write`.
    #[field(
        selection,
        selection = [
            ("add", "Adding"), ("remove", "Removing"), ("set", "Setting it to"),
            ("clear", "Clearing it")
        ],
        default = "add",
        string = "Many2many Operations"
    )]
    pub update_m2m_operation: Option<Arc<str>>,
    /// The boolean an `object_write` writes.
    #[field(
        selection,
        selection = [("true", "Yes (True)"), ("false", "No (False)")],
        default = "true",
        string = "Boolean Value"
    )]
    pub update_boolean_value: Option<Arc<str>>,
    /// The field of the action's model that links an `object_create`'s new record to the active
    /// record (a many2one, one2many or many2many to the created model).
    #[field(
        many2one,
        comodel = "ir.model.fields",
        ondelete = "cascade",
        index = true,
        string = "Link Field"
    )]
    pub link_field_id: Option<RecId>,
    /// Groups that may run the action; none: everybody with write access on the model and the
    /// records.
    #[field(
        many2many,
        comodel = "res.groups",
        relation = "ir_act_server_group_rel",
        column1 = "act_id",
        column2 = "gid",
        string = "Allowed Groups"
    )]
    pub group_ids: Vec<RecId>,
    /// Run with elevated rights: set only by administrators, needs groups; running
    /// such an action waits for its audit trail.
    #[field(boolean, string = "Run as Superuser")]
    pub run_as_superuser: bool,
}

#[methods(IrActionsServer)]
impl IrActionsServer {
    /// The `run_as_superuser` rules on the stored rows (a value, a default or a copy alike), the
    /// flagged trees (a non-admin may not take an action out of one through `child_ids`, nor put
    /// a new action in one), and the loop check on stored relationships, including defaults.
    #[method(override)]
    async fn create(
        this: &RecordSet<IrActionsServer>,
        tx: &mut Transaction,
        vals_list: Vec<RawVals>,
        sup: Super,
    ) -> Result<Records> {
        let env = this.as_records().env().clone();
        // existing actions the new ones take as children: their trees change
        let mut moved = Vec::new();
        for vals in &vals_list {
            let mut linked = Vec::new();
            server_rules::linked_ids(vals, &mut linked);
            // the parent is checked with the created rows below (defaults included)
            if let Some(Cell::Id(parent)) = vals.get("parent_id") {
                linked.retain(|id| id != parent);
            }
            moved.extend(linked);
        }
        server_rules::check_tree(tx, &env, &moved, server_rules::Reach::Ancestors).await?;
        let created: Records = sup.call(tx, this.as_records(), (vals_list,)).await?;
        let ids = row_ids(&created)?;
        if ids.is_empty() {
            return Ok(created);
        }
        server_rules::check_flagged(tx, &created, &ids).await?;
        server_rules::check_tree(tx, &env, &ids, server_rules::Reach::Ancestors).await?;
        // Raw values omit links introduced by context/model defaults. Check stored parents
        // even in sudo/loader mode, which intentionally skips the flagged-tree policy.
        server_rules::check_loops(tx, &created, &ids).await?;
        Ok(created)
    }

    /// Setting `run_as_superuser`, or changing an action in the tree of one that has it (the
    /// action itself, a descendant, an action moved into or out of the tree), needs an
    /// administrator; a flagged action keeps its groups; `parent_id`/`child_ids` may not make a
    /// loop.
    #[method(override)]
    async fn write(
        this: &RecordSet<IrActionsServer>,
        tx: &mut Transaction,
        vals: RawVals,
        sup: Super,
    ) -> Result<()> {
        let records = this.as_records();
        let env = records.env().clone();
        let ids = row_ids(records)?;
        if ids.is_empty() {
            return sup.call(tx, records, (vals,)).await;
        }
        // a non-admin setting the flag is refused before the write (the stored rows are checked
        // after it as well, `check_flagged`)
        if !env.su()
            && server_rules::sets_flag(&vals)
            && !tx.has_group(&env, "base.group_system").await?
        {
            return Err(crate::server_action::ServerActionError::SuperuserAdminOnly.into());
        }
        let mut touched = ids.clone();
        server_rules::linked_ids(&vals, &mut touched);
        server_rules::check_tree(tx, &env, &touched, server_rules::Reach::Ancestors).await?;
        let flag_or_groups = vals.contains("run_as_superuser") || touches(&vals, "group_ids");
        let links = touches_tree(&vals);
        sup.call(tx, records, (vals,)).await?;
        if flag_or_groups {
            server_rules::check_flagged(tx, records, &ids).await?;
        }
        if links {
            server_rules::check_loops(tx, records, &ids).await?;
        }
        Ok(())
    }

    /// A non-admin may not delete an action of a flagged tree: the action itself, a descendant
    /// of a flagged action, or an action whose cascade (`parent_id` is `ondelete=cascade`)
    /// deletes a flagged one.
    #[method(override)]
    async fn unlink(
        this: &RecordSet<IrActionsServer>,
        tx: &mut Transaction,
        sup: Super,
    ) -> Result<()> {
        let records = this.as_records();
        let ids = row_ids(records)?;
        // Match base.unlink's empty-recordset no-op, but authorize real records before any
        // descendant traversal or row locks performed by the elevated-action guard.
        if !ids.is_empty() {
            records.check_access(tx, Operation::Unlink).await?;
        }
        server_rules::check_tree(
            tx,
            records.env(),
            &ids,
            server_rules::Reach::AncestorsAndDescendants,
        )
        .await?;
        sup.call(tx, records, ()).await
    }

    /// Remove the `_run_as_superuser_not_yet` check an database still has: schema
    /// synchronization adds and replaces declared constraints but drops none, and that check
    /// would refuse every flagged action. Nothing is locked when it is gone.
    #[method(override)]
    async fn init(
        this: &RecordSet<IrActionsServer>,
        tx: &mut Transaction,
        sup: Super,
    ) -> Result<()> {
        let table = this.as_records().spec().table.clone();
        server_rules::drop_obsolete_check(tx, &table).await?;
        sup.call(tx, this.as_records(), ()).await
    }

    /// The registry is final: every state has exactly one runner
    /// ([`crate::server_action::check_runners`]).
    #[method(override)]
    async fn _register_hook(
        this: &RecordSet<IrActionsServer>,
        tx: &mut Transaction,
        sup: Super,
    ) -> Result<()> {
        crate::server_action::check_runners(this.as_records().env().registry())?;
        sup.call(tx, this.as_records(), ()).await
    }
}

/// Whether `vals` writes the x2many or scalar field `field`.
fn touches(vals: &RawVals, field: &str) -> bool {
    vals.contains(field) || vals.commands(field).is_some()
}

/// Whether `vals` changes the parent links of server actions.
fn touches_tree(vals: &RawVals) -> bool {
    touches(vals, "parent_id") || touches(vals, "child_ids")
}
