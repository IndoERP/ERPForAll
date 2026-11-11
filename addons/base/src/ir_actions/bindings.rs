//! Contextual actions of a model (`get_bindings`,,
//! ).
//!
//! The shared part is the same for every user: the actions bound to the
//! model with `name`, `binding_view_types` and, where the typed model has them, `sequence` and
//! `domain` (an empty domain dropped), plus the groups and `res_model` the per-user part needs.
//! `action`
//! bindings are sorted by `sequence` (absent: 0), stably on id.
//!
//! Machine view: a warm call is one cache lookup (an `Arc` clone of the typed shared value, whose
//! group ids were validated when it was read), one membership taken once and checked per binding
//! with groups without a statement or an allocation, one model-level access check per binding
//! with a `res_model` (no statement once the transaction has resolved the user), and one
//! `Binding` per visible action holding an `Arc` of the shared values: no per-binding map is
//! built per call.

use std::fmt;

use erp_sdk::access::GroupId;
use erp_sdk::prelude::*;
use erp_sdk::{FieldType, OrmCacheKey, ValueMap};

use super::{ACTIONS_CACHE, ident};

/// The model the cache entries are counted under (`ormcache_stats`).
const CACHE_MODEL: &str = "ir.actions.actions";
/// The method the cache entries are counted under.
const CACHE_METHOD: &str = "_get_bindings";

/// The id of an action of any type (positive; ids are unique across types).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ActionId(i64);

impl ActionId {
    /// The action id `id`.
    ///
    /// # Errors
    ///
    /// `Validation` unless `id` is positive.
    pub fn new(id: i64) -> Result<Self> {
        if id > 0 {
            Ok(Self(id))
        } else {
            Err(Error::validation(format!("{id} is not an action id")))
        }
    }

    /// The raw id.
    pub fn get(self) -> i64 {
        self.0
    }
}

impl fmt::Display for ActionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// One contextual action the user may run.
#[derive(Debug, Clone, PartialEq)]
pub struct Binding {
    id: ActionId,
    values: Arc<ValueMap>,
}

impl Binding {
    /// The action.
    pub fn id(&self) -> ActionId {
        self.id
    }

    pub fn values(&self) -> &Arc<ValueMap> {
        &self.values
    }
}

/// The contextual actions of a model, by binding type.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Bindings {
    action: Vec<Binding>,
    report: Vec<Binding>,
}

impl Bindings {
    /// `action` bindings, by `sequence` then id.
    pub fn action(&self) -> &[Binding] {
        &self.action
    }

    /// `report` bindings, by id.
    pub fn report(&self) -> &[Binding] {
        &self.report
    }
}

/// An existing action: its id and its type, an action model of the registry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ActionRef {
    id: ActionId,
    model: &'static str,
}

impl ActionRef {
    /// The action's id.
    pub fn id(self) -> ActionId {
        self.id
    }

    /// The action's typed model (`ir.actions.act_window`, …), as the registry names it.
    pub fn model(self) -> &'static str {
        self.model
    }
}

/// The name of the action model of the registry named `kind`: one that inherits
/// `ir.actions.base` and has a table.
fn action_model(registry: &erp_sdk::Registry, kind: &str) -> Result<&'static str> {
    registry
        .model(kind)
        .filter(|spec| spec.inherits("ir.actions.base") && spec.has_table())
        .map(|spec| spec.name)
        .ok_or_else(|| not_an_action(kind))
}

/// The error of a key row whose type is not an action model of the registry.
fn not_an_action(kind: &str) -> Error {
    Error::programming(format!(
        "ir_actions names the action type '{kind}', which is not an action model of the registry"
    ))
}

/// The action `id` with its type, when it exists: one statement on the key table.
///
/// # Errors
///
/// `Database` for a row whose type cannot be read; `Programming` when the key table names a type
/// that is not an action model of the registry (a row written outside the ORM, or an action
/// type whose addon is not loaded).
pub async fn action_by_id(tx: &mut Transaction, id: ActionId) -> Result<Option<ActionRef>> {
    let rows = tx
        .query("SELECT type FROM ir_actions WHERE id = $1", &[&id.get()])
        .await?;
    let Some(row) = rows.first() else {
        return Ok(None);
    };
    let kind: String = row
        .try_get(0)
        .map_err(|e| Error::db(format!("unexpected value in ir_actions.type: {e}")))?;
    let registry = Arc::clone(tx.env().registry());
    let model = action_model(&registry, &kind)?;
    Ok(Some(ActionRef { id, model }))
}

/// The contextual actions of `model` that the user of `env` may see.
///
/// ```no_run
/// # #![recursion_limit = "256"]
/// # async fn demo(tx: &mut erp_sdk::Transaction) -> erp_sdk::Result<()> {
/// let env = tx.env();
/// let bindings = erp_addon_base::ir_actions::get_bindings(tx, &env, "res.groups").await?;
/// for binding in bindings.action() {
///     println!("{} {:?}", binding.id(), binding.values().get("name"));
/// }
/// # Ok(())
/// # }
/// ```
///
/// # Errors
///
/// `Validation` when `model` is not a model of the registry (the cache holds one entry per
/// model, so its size is bounded by the registry); `Programming` when the key table names an
/// action type that is not an action model or a bound action whose typed row is missing (rows
/// written outside the ORM); the access engine's errors.
pub async fn get_bindings(tx: &mut Transaction, env: &Env, model: &str) -> Result<Bindings> {
    if env.registry().model(model).is_none() {
        return Err(Error::validation(format!(
            "get_bindings: there is no model '{model}' in the registry"
        )));
    }
    let key = OrmCacheKey::new(CACHE_MODEL, CACHE_METHOD, vec![Value::str(model)]);
    let shared: Arc<SharedBindings> = tx
        .ormcache_arc(ACTIONS_CACHE, key, async move |tx: &mut Transaction| {
            shared_bindings(tx, model).await
        })
        .await?;
    // one membership for every binding: the principal is resolved once, nothing is allocated per
    // binding
    let membership = tx.group_membership(env).await?;
    let mut out = Bindings::default();
    for (entries, target) in [
        (&shared.action, &mut out.action),
        (&shared.report, &mut out.report),
    ] {
        for entry in entries.iter() {
            if !entry.groups.is_empty() && !membership.has_any(&entry.groups)? {
                continue;
            }
            if let Some(res_model) = &entry.res_model {
                // a model the registry does not have hides the binding
                if env.registry().model(res_model).is_none() {
                    continue;
                }
                let records = env.records(res_model)?;
                if !records.has_access(tx, erp_sdk::Operation::Read).await? {
                    continue;
                }
            }
            target.push(Binding {
                id: entry.id,
                values: Arc::clone(&entry.values),
            });
        }
    }
    Ok(out)
}

/// The shared part of [`get_bindings`] for one model, as cached: validated once when it is read,
/// so a call only filters.
struct SharedBindings {
    /// `action` bindings, by `sequence` then id.
    action: Box<[CachedBinding]>,
    /// `report` bindings, by id.
    report: Box<[CachedBinding]>,
}

/// One bound action in the cache.
struct CachedBinding {
    id: ActionId,
    /// What the caller gets.
    values: Arc<ValueMap>,
    /// The action's groups (none: everybody), validated `res.groups` ids.
    groups: Box<[GroupId]>,
    /// The model the user must be able to read, when the action type has one.
    res_model: Option<Arc<str>>,
}

/// One bound action as read from its typed table.
struct Read {
    sequence: i64,
    binding_type: String,
    entry: CachedBinding,
}

/// The shared part of [`get_bindings`] for `model` (the cached value).
async fn shared_bindings(tx: &mut Transaction, model: &str) -> Result<SharedBindings> {
    tx.flush().await?;
    let rows = tx
        .query(
            "SELECT a.id, a.type, a.binding_type FROM ir_actions a \
             JOIN ir_model m ON a.binding_model_id = m.id WHERE m.model = $1 ORDER BY a.id",
            &[&model],
        )
        .await?;
    let mut bound: Vec<(i64, String, String)> = Vec::with_capacity(rows.len());
    for row in &rows {
        let read = |i: usize, what: &str| {
            Error::db(format!(
                "unexpected value in ir_actions.{what} (column {i})"
            ))
        };
        let id: i64 = row.try_get(0).map_err(|_| read(0, "id"))?;
        let kind: String = row.try_get(1).map_err(|_| read(1, "type"))?;
        let binding_type: String = row.try_get(2).map_err(|_| read(2, "binding_type"))?;
        bound.push((id, kind, binding_type));
    }
    // one read per action type present, in the order the types first appear
    let mut types: Vec<&str> = Vec::new();
    for (_, kind, _) in &bound {
        if !types.contains(&kind.as_str()) {
            types.push(kind);
        }
    }
    let mut read: Vec<(i64, Read)> = Vec::with_capacity(bound.len());
    for kind in types {
        let ids: Vec<i64> = bound
            .iter()
            .filter(|(_, k, _)| k == kind)
            .map(|(id, _, _)| *id)
            .collect();
        read_typed(tx, kind, &ids, &bound, &mut read).await?;
    }
    read.sort_by_key(|(id, _)| *id);
    let mut action: Vec<(i64, CachedBinding)> = Vec::new();
    let mut report: Vec<CachedBinding> = Vec::new();
    for (_, r) in read {
        match r.binding_type.as_str() {
            "action" => action.push((r.sequence, r.entry)),
            "report" => report.push(r.entry),
            other => {
                return Err(Error::programming(format!(
                    "ir_actions.binding_type is '{other}'; get_bindings knows 'action' and \
                     'report'"
                )));
            }
        }
    }
    // stable: actions of one sequence stay in id order
    action.sort_by_key(|(sequence, _)| *sequence);
    Ok(SharedBindings {
        action: action.into_iter().map(|(_, entry)| entry).collect(),
        report: report.into_boxed_slice(),
    })
}

/// Read the bound actions `ids` of the action model `kind`: one statement.
async fn read_typed(
    tx: &mut Transaction,
    kind: &str,
    ids: &[i64],
    bound: &[(i64, String, String)],
    out: &mut Vec<(i64, Read)>,
) -> Result<()> {
    let registry = Arc::clone(tx.env().registry());
    let spec = registry
        .model(kind)
        .filter(|spec| spec.inherits("ir.actions.base") && spec.has_table())
        .ok_or_else(|| not_an_action(kind))?;
    let column = |name: &str| -> Result<bool> {
        match spec.field(name) {
            None => Ok(false),
            Some(field) if field.column().is_some() => Ok(true),
            Some(_) => Err(Error::programming(format!(
                "get_bindings reads {kind}.{name}, which has no column; only stored fields are \
                 read"
            ))),
        }
    };
    let has_res_model = column("res_model")?;
    let has_sequence = column("sequence")?;
    let has_domain = column("domain")?;
    let mut select = vec![
        "t.id".to_owned(),
        "t.name".to_owned(),
        "t.binding_view_types".to_owned(),
    ];
    select.push(
        if has_res_model {
            "t.res_model"
        } else {
            "NULL::varchar"
        }
        .to_owned(),
    );
    select.push(
        if has_sequence {
            "t.sequence::int8"
        } else {
            "NULL::int8"
        }
        .to_owned(),
    );
    select.push(
        if has_domain {
            "t.domain::text"
        } else {
            "NULL::text"
        }
        .to_owned(),
    );
    match spec.field("group_ids").map(|f| &f.ty) {
        None => select.push("ARRAY[]::int8[]".to_owned()),
        Some(FieldType::Many2many {
            relation: Some(relation),
            ..
        }) => select.push(format!(
            "ARRAY(SELECT r.{c2}::int8 FROM {rel} r WHERE r.{c1} = t.id ORDER BY 1)",
            rel = ident(&relation.table),
            c1 = ident(&relation.column1),
            c2 = ident(&relation.column2),
        )),
        Some(_) => {
            return Err(Error::programming(format!(
                "get_bindings reads {kind}.group_ids, which is not a stored many2many"
            )));
        }
    }
    let sql = format!(
        "SELECT {} FROM {} t WHERE t.id = ANY($1) ORDER BY t.id",
        select.join(", "),
        ident(&spec.table)
    );
    let rows = tx.query(&sql, &[&ids]).await?;
    if rows.len() != ids.len() {
        return Err(Error::programming(format!(
            "ir_actions binds {} actions of type '{kind}', but {} has {} of them (rows written \
             outside the ORM?)",
            ids.len(),
            spec.table,
            rows.len()
        )));
    }
    let bad = |what: &str| Error::db(format!("unexpected value in {}.{what}", spec.table));
    for row in &rows {
        let id: i64 = row.try_get(0).map_err(|_| bad("id"))?;
        let name: Option<String> = row.try_get(1).map_err(|_| bad("name"))?;
        let view_types: Option<String> = row.try_get(2).map_err(|_| bad("binding_view_types"))?;
        let res_model: Option<String> = row.try_get(3).map_err(|_| bad("res_model"))?;
        let sequence: Option<i64> = row.try_get(4).map_err(|_| bad("sequence"))?;
        let domain: Option<String> = row.try_get(5).map_err(|_| bad("domain"))?;
        let groups: Vec<i64> = row.try_get(6).map_err(|_| bad("group_ids"))?;
        let groups = groups
            .into_iter()
            .map(|g| {
                GroupId::new(g).map_err(|e| {
                    Error::db(format!(
                        "{}.group_ids holds {g}, which is not a res.groups id: {e}",
                        spec.table
                    ))
                })
            })
            .collect::<Result<Box<[GroupId]>>>()?;
        let text = |v: Option<String>| v.map_or(Value::False, Value::str);
        let mut values = ValueMap::with_capacity(5);
        values.insert("id".to_owned(), Value::Int(id));
        values.insert("name".to_owned(), text(name));
        values.insert("binding_view_types".to_owned(), text(view_types));
        if has_sequence {
            values.insert("sequence".to_owned(), Value::Int(sequence.unwrap_or(0)));
        }
        // an empty domain is dropped
        if let Some(domain) = domain.filter(|d| !d.is_empty()) {
            values.insert("domain".to_owned(), Value::str(domain));
        }
        let binding_type = bound
            .iter()
            .find(|(b, _, _)| *b == id)
            .map(|(_, _, t)| t.clone())
            .ok_or_else(|| Error::programming(format!("action {id} was not asked for")))?;
        let entry = CachedBinding {
            id: ActionId::new(id).map_err(|_| bad("id"))?,
            values: Arc::new(values),
            groups,
            res_model: res_model.map(Arc::from),
        };
        out.push((
            id,
            Read {
                sequence: sequence.unwrap_or(0),
                binding_type,
                entry,
            },
        ));
    }
    Ok(())
}
