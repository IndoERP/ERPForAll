//! Filter records provide named search filters and favourites, either global to a model or scoped
//! to a specific action and optional embedded action context. Favourites are stored unpartitioned
//! across companies and respect subject privacy: filters restricted to specific users (`user_ids`)
//! are never returned to callers outside that subject set.

// `create_or_replace` takes its RPC parameters by name, and the methods macro repeats them in the
// code it generates for the impl block.
#![allow(
    clippy::too_many_arguments,
    reason = "RPC parameters are named, not bundled"
)]

use std::sync::Arc;

use erp_sdk::domain::{Operator, Value as DomainValue};
use erp_sdk::prelude::*;
use erp_sdk::serde_json::{self, Value as Json, json};
use erp_sdk::{Operation, RecId, SearchOptions, ValueMap};

/// Longest name of a favorite, in characters.
const MAX_FAVORITE_NAME: usize = 256;
/// Most fields a favorite groups by (the list's own bound).
const MAX_FAVORITE_GROUP_BY: usize = 32;
/// Longest stored domain, context or sort of a favorite, in bytes.
const MAX_FAVORITE_TEXT: usize = 64 * 1024;
/// Deepest nesting a stored favorite expression may have.
const MAX_FAVORITE_DEPTH: usize = 32;

/// `ir.filters`: saved filters and favourites for search views.
#[model(
    name = "ir.filters",
    description = "Filter",
    table = "ir_filters",
    order = "model_id, name, id desc",
    indexes = [(
        "_get_filters_index",
        "(model_id, action_id, embedded_action_id, embedded_parent_res_id)"
    )],
    constraints = [
        (
            "_check_res_id_only_when_embedded_action",
            "CHECK (NOT (embedded_parent_res_id IS NOT NULL AND embedded_action_id IS NULL))",
            "Constraint to ensure that the embedded_parent_res_id is only defined when an embedded action is defined."
        ),
        (
            "_check_sort_json",
            "CHECK (sort IS NULL OR jsonb_typeof(sort::jsonb) = 'array')",
            "Invalid sort definition"
        ),
    ]
)]
pub struct IrFilters {
    /// Filter name.
    #[field(char, required = true, string = "Filter Name")]
    pub name: Option<Arc<str>>,
    /// Users the filter is shared with. If empty, the filter is public to all users.
    #[field(
        many2many,
        comodel = "res.users",
        relation = "ir_filters_res_users_rel",
        column1 = "filter_id",
        column2 = "user_id",
        string = "Users"
    )]
    pub user_ids: Vec<RecId>,
    /// Domain specification JSON string.
    #[field(text, required = true, default = "[]")]
    pub domain: Option<Arc<str>>,
    /// Context specification JSON string.
    #[field(text, required = true, default = "{}")]
    pub context: Option<Arc<str>>,
    /// Sort specification JSON string.
    #[field(char, required = true, default = "[]")]
    pub sort: Option<Arc<str>>,
    /// Model name string.
    #[field(char, required = true, string = "Model")]
    pub model_id: Option<Arc<str>>,
    /// Whether this filter is the default filter for the view.
    #[field(boolean, string = "Default Filter")]
    pub is_default: bool,
    /// The action this filter applies to (empty: applies to all menus/actions for this model).
    #[field(
        many2one,
        comodel = "ir.actions.actions",
        ondelete = "cascade",
        string = "Action"
    )]
    pub action_id: Option<RecId>,
    /// The embedded action this filter is applied to.
    #[field(integer, string = "Embedded Action")]
    pub embedded_action_id: Option<i32>,
    /// The record id the filter should be applied to for embedded actions.
    #[field(integer, string = "Embedded Parent Res Id")]
    pub embedded_parent_res_id: Option<i32>,
    /// Whether this filter is active.
    #[field(boolean, default = true)]
    pub active: bool,
}

#[methods(IrFilters)]
impl IrFilters {
    /// Return the list of filters available for the user on the given model.
    ///
    /// Validates parameter constraints and target model access, then filters matching public
    /// and user-private filters according to action context.
    #[method]
    #[rpc(effect = read, model)]
    pub async fn get_filters(
        this: &RecordSet<IrFilters>,
        tx: &mut Transaction,
        model: String,
        action_id: Option<i64>,
        embedded_action_id: Option<i64>,
        embedded_parent_res_id: Option<i64>,
    ) -> Result<Value> {
        Self::read_filters(
            this,
            tx,
            model,
            action_id,
            embedded_action_id,
            embedded_parent_res_id,
        )
        .await
    }

    /// The caller's favorites of `model` for the web client's search panel: the filters
    /// `get_filters` returns for the action (no embedded action), each with its domain, its
    /// `group_by` fields and its sort as JSON, whether it is shared and whether it is the
    /// caller's own. A favorite the client cannot apply (a stored text that is not a literal,
    /// a context with keys other than `group_by`) carries `problem` with the reason instead.
    #[method]
    #[rpc(effect = read, model)]
    pub async fn erp_get_favorites(
        this: &RecordSet<IrFilters>,
        tx: &mut Transaction,
        model: String,
        action_id: Option<i64>,
    ) -> Result<Value> {
        let env = this.as_records().env();
        let uid = env.uid();
        let domain = visible_filters(this, tx, &model, action_id, None, None).await?;
        let fields = [
            "id",
            "name",
            "is_default",
            "domain",
            "context",
            "user_ids",
            "sort",
        ];
        let rows = env
            .records("ir.filters")?
            .search_read(tx, &domain, &fields, &SearchOptions::default())
            .await?;
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            out.push(favorite(&row, uid)?);
        }
        Ok(Value::Json(Arc::new(Json::Array(out))))
    }

    /// Save the current search as a favorite of the caller for `model` and the action:
    /// `domain`, the `group_by` fields and a name. A private favorite of the caller with the same
    /// name (ignoring case) for the same model and action is replaced. `is_default` makes it the
    /// caller's default for the action and clears the caller's previous default; a shared
    /// favorite (visible to every user) cannot be a default. The domain is checked against the
    /// model under the caller's rights before anything is stored.
    #[method]
    #[rpc(effect = write, model)]
    pub async fn create_or_replace(
        this: &RecordSet<IrFilters>,
        tx: &mut Transaction,
        name: String,
        model: String,
        action_id: Option<i64>,
        domain: Domain,
        group_by: Vec<String>,
        is_default: bool,
        shared: bool,
    ) -> Result<Value> {
        let env = this.as_records().env();
        let uid = env.uid();
        let name = name.trim();
        if name.is_empty() || name.chars().count() > MAX_FAVORITE_NAME {
            return Err(Error::validation(format!(
                "A favorite needs a name of 1 to {MAX_FAVORITE_NAME} characters."
            )));
        }
        if shared && is_default {
            return Err(Error::validation(
                "A shared favorite cannot be a default: save it as your own favorite to make it your default.",
            ));
        }
        let spec = env.registry().model(&model).ok_or_else(|| {
            Error::validation(format!("create_or_replace: unknown model '{model}'"))
        })?;
        if group_by.len() > MAX_FAVORITE_GROUP_BY {
            return Err(Error::validation(format!(
                "A favorite groups by at most {MAX_FAVORITE_GROUP_BY} fields."
            )));
        }
        for spec_text in &group_by {
            let field = spec_text.split(':').next().unwrap_or_default();
            if spec.field(field).is_none() {
                return Err(Error::validation(format!(
                    "A favorite cannot group by '{}': '{model}' has no such field.",
                    spec_text.chars().take(64).collect::<String>()
                )));
            }
        }
        // The domain must name fields the caller may search: a bounded count under the caller's
        // rights checks it the way the list will use it.
        let target = env.records(&model)?;
        target.check_access(tx, Operation::Read).await?;
        target.search_count(tx, &domain, Some(1)).await?;
        let domain_text = stored_text(&domain.to_json().map_err(Error::from)?)?;
        let context_text = stored_text(&if group_by.is_empty() {
            json!({})
        } else {
            json!({ "group_by": group_by })
        })?;

        // Concurrent saves of one user are serialized on the user's own row: under the
        // repeatable-read transaction a second save waits, fails to serialize and is retried
        // on a fresh snapshot that sees the first one's default.
        tx.execute("UPDATE res_users SET id = id WHERE id = $1::int8", &[&uid])
            .await?;
        let filters = env.records("ir.filters")?;
        let own = Domain::and([
            Domain::condition("model_id", Operator::Eq, model.as_str()).map_err(Error::from)?,
            match action_id {
                Some(id) => Domain::condition("action_id", Operator::Eq, id),
                None => Domain::condition("action_id", Operator::Eq, false),
            }
            .map_err(Error::from)?,
            Domain::condition("embedded_action_id", Operator::Eq, false).map_err(Error::from)?,
            Domain::condition(
                "user_ids",
                Operator::In,
                DomainValue::List(vec![DomainValue::Int(uid)]),
            )
            .map_err(Error::from)?,
        ]);
        let mine = filters
            .search_read(
                tx,
                &own,
                &["id", "name", "is_default", "user_ids"],
                &SearchOptions::default(),
            )
            .await?;
        let lower = name.to_lowercase();
        let same_name = |row: &std::collections::BTreeMap<String, Value>| matches!(row.get("name"), Some(Value::Str(text)) if text.to_lowercase() == lower);
        if shared {
            let shared_domain = Domain::and([
                Domain::condition("model_id", Operator::Eq, model.as_str()).map_err(Error::from)?,
                match action_id {
                    Some(id) => Domain::condition("action_id", Operator::Eq, id),
                    None => Domain::condition("action_id", Operator::Eq, false),
                }
                .map_err(Error::from)?,
                Domain::condition("user_ids", Operator::Eq, false).map_err(Error::from)?,
            ]);
            let existing = filters
                .search_read(
                    tx,
                    &shared_domain,
                    &["id", "name"],
                    &SearchOptions::default(),
                )
                .await?;
            if existing.iter().any(same_name) {
                return Err(Error::validation(format!(
                    "A shared favorite named '{name}' already exists for this list."
                )));
            }
        }
        if is_default {
            let previous: Vec<i64> = mine
                .iter()
                .filter(|row| {
                    matches!(row.get("is_default"), Some(Value::Bool(true))) && !same_name(row)
                })
                .filter_map(row_id)
                .collect();
            if !previous.is_empty() {
                filters
                    .browse(previous)?
                    .write(tx, RawVals::new().set_cell("is_default", Cell::Bool(false)))
                    .await?;
            }
        }
        let vals = RawVals::new()
            .set_cell("name", Cell::Str(name.into()))
            .set_cell("domain", Cell::Str(domain_text.into()))
            .set_cell("context", Cell::Str(context_text.into()))
            .set_cell("sort", Cell::Str("[]".into()))
            .set_cell("is_default", Cell::Bool(is_default));
        let replaced = if shared {
            None
        } else {
            mine.iter().find(|row| same_name(row)).and_then(row_id)
        };
        let id = match replaced {
            Some(id) => {
                filters.browse([id])?.write(tx, vals).await?;
                id
            }
            None => {
                let users = if shared {
                    Vec::new()
                } else {
                    vec![Command::set([RecId::from_real(uid).ok_or_else(|| {
                        Error::invariant("the caller has no real user id")
                    })?])]
                };
                let vals = vals
                    .set_cell("model_id", Cell::Str(model.as_str().into()))
                    .set_cell("action_id", action_id.map_or(Cell::Null, Cell::Id))
                    .set_commands("user_ids", users);
                filters.create(tx, vec![vals]).await?.ids()[0]
            }
        };
        let rows = filters
            .browse([id])?
            .read(
                tx,
                &[
                    "id",
                    "name",
                    "is_default",
                    "domain",
                    "context",
                    "user_ids",
                    "sort",
                ],
            )
            .await?;
        let row = rows
            .first()
            .ok_or_else(|| Error::invariant("a saved favorite is not readable by its author"))?;
        Ok(Value::Json(Arc::new(favorite(row, uid)?)))
    }

    /// Alias for `get_filters`.
    #[method]
    #[rpc(effect = read, model)]
    pub async fn erp_get_filters(
        this: &RecordSet<IrFilters>,
        tx: &mut Transaction,
        model: String,
        action_id: Option<i64>,
        embedded_action_id: Option<i64>,
        embedded_parent_res_id: Option<i64>,
    ) -> Result<Value> {
        Self::read_filters(
            this,
            tx,
            model,
            action_id,
            embedded_action_id,
            embedded_parent_res_id,
        )
        .await
    }
}

impl IrFilters {
    async fn read_filters(
        this: &RecordSet<IrFilters>,
        tx: &mut Transaction,
        model: String,
        action_id: Option<i64>,
        embedded_action_id: Option<i64>,
        embedded_parent_res_id: Option<i64>,
    ) -> Result<Value> {
        let full_domain = visible_filters(
            this,
            tx,
            &model,
            action_id,
            embedded_action_id,
            embedded_parent_res_id,
        )
        .await?;
        let filter_records = this.as_records().env().records("ir.filters")?;
        let fields = [
            "name",
            "is_default",
            "domain",
            "context",
            "user_ids",
            "sort",
            "embedded_action_id",
            "embedded_parent_res_id",
        ];

        let results = filter_records
            .search_read(tx, &full_domain, &fields, &SearchOptions::default())
            .await?;

        let mut out = Vec::with_capacity(results.len());
        for row in results {
            let mut map = ValueMap::new();
            for (k, v) in row {
                map.insert(k, v);
            }
            out.push(Value::Map(Arc::new(map)));
        }

        Ok(Value::List(out.into()))
    }
}

/// The filters of `model` the caller sees for the action and embedded action: shared ones and
/// the caller's own, active, global to the model or scoped to the action. Checks the parameters
/// and that the caller may read `model`.
async fn visible_filters(
    this: &RecordSet<IrFilters>,
    tx: &mut Transaction,
    model: &str,
    action_id: Option<i64>,
    embedded_action_id: Option<i64>,
    embedded_parent_res_id: Option<i64>,
) -> Result<Domain> {
    // Parameter constraint: embedded_parent_res_id requires embedded_action_id
    if embedded_parent_res_id.is_some() && embedded_action_id.is_none() {
        return Err(Error::validation(
            "Constraint to ensure that the embedded_parent_res_id is only defined when an embedded action is defined.",
        ));
    }

    // Validate target model exists in registry and caller has Read access to it
    let env = this.as_records().env();
    if env.registry().model(model).is_none() {
        return Err(Error::validation(format!(
            "get_filters: unknown model '{model}'"
        )));
    }
    let target_records = env.records(model)?;
    target_records.check_access(tx, Operation::Read).await?;

    let action_cond = match action_id {
        Some(aid) => Domain::condition(
            "action_id",
            Operator::In,
            DomainValue::List(vec![DomainValue::Int(aid), DomainValue::Bool(false)]),
        )
        .map_err(Error::from)?,
        None => Domain::condition("action_id", Operator::Eq, false).map_err(Error::from)?,
    };

    let embedded_cond = match embedded_action_id {
        Some(eaid) => {
            Domain::condition("embedded_action_id", Operator::Eq, eaid).map_err(Error::from)?
        }
        None => {
            Domain::condition("embedded_action_id", Operator::Eq, false).map_err(Error::from)?
        }
    };

    let parent_cond =
        match (embedded_action_id, embedded_parent_res_id) {
            (Some(_), Some(pid)) => Domain::condition("embedded_parent_res_id", Operator::Eq, pid)
                .map_err(Error::from)?,
            _ => Domain::condition("embedded_parent_res_id", Operator::Eq, false)
                .map_err(Error::from)?,
        };

    let model_cond = Domain::condition("model_id", Operator::Eq, model).map_err(Error::from)?;

    let uid = env.uid();
    let user_cond = Domain::condition(
        "user_ids",
        Operator::In,
        DomainValue::List(vec![DomainValue::Int(uid), DomainValue::Bool(false)]),
    )
    .map_err(Error::from)?;

    let active_cond = Domain::condition("active", Operator::Eq, true).map_err(Error::from)?;

    Ok(Domain::and([
        action_cond,
        embedded_cond,
        parent_cond,
        model_cond,
        user_cond,
        active_cond,
    ]))
}

fn row_id(row: &std::collections::BTreeMap<String, Value>) -> Option<i64> {
    match row.get("id") {
        Some(Value::Int(id)) => Some(*id),
        _ => None,
    }
}

fn stored_text(value: &Json) -> Result<String> {
    let text = serde_json::to_string(value)
        .map_err(|error| Error::validation(format!("A favorite cannot be stored: {error}")))?;
    if text.len() > MAX_FAVORITE_TEXT {
        return Err(Error::validation(format!(
            "A favorite's domain is longer than {MAX_FAVORITE_TEXT} bytes."
        )));
    }
    Ok(text)
}

/// One favorite as the web client applies it (see `erp_get_favorites`).
fn favorite(row: &std::collections::BTreeMap<String, Value>, uid: i64) -> Result<Json> {
    let id = row_id(row).ok_or_else(|| Error::invariant("a filter row has no id"))?;
    let text = |key: &str| match row.get(key) {
        Some(Value::Str(text)) => text.to_string(),
        _ => String::new(),
    };
    let users: Vec<i64> = match row.get("user_ids") {
        Some(Value::List(items)) => items
            .iter()
            .filter_map(|item| match item {
                Value::Int(id) => Some(*id),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    };
    let mut out = json!({
        "id": id,
        "name": text("name"),
        "is_default": matches!(row.get("is_default"), Some(Value::Bool(true))),
        "shared": users.is_empty(),
        "own": users == [uid],
    });
    let read = || -> std::result::Result<(Json, Vec<Json>, Json), String> {
        let domain = stored_literal(&text("domain"), uid)?;
        Domain::from_json(&domain).map_err(|error| format!("its domain is invalid: {error}"))?;
        let context = stored_literal(&text("context"), uid)?;
        let Json::Object(context) = context else {
            return Err("its context is not a dictionary".into());
        };
        let mut group_by = Vec::new();
        for (key, value) in context {
            match (key.as_str(), value) {
                ("group_by", Json::String(field)) => group_by.push(Json::String(field)),
                ("group_by", Json::Array(fields)) if fields.iter().all(Json::is_string) => {
                    group_by.extend(fields);
                }
                ("group_by", _) => {
                    return Err("its group_by is not a field name or a list of them".into());
                }
                (other, _) => {
                    return Err(format!(
                        "its context sets '{}', which the search panel does not apply",
                        other.chars().take(64).collect::<String>()
                    ));
                }
            }
        }
        let sort = stored_literal(&text("sort"), uid)?;
        if !sort
            .as_array()
            .is_some_and(|terms| terms.iter().all(Json::is_string))
        {
            return Err("its sort is not a list of field orders".into());
        }
        Ok((domain, group_by, sort))
    };
    match read() {
        Ok((domain, group_by, sort)) => {
            out["domain"] = domain;
            out["group_by"] = Json::Array(group_by);
            out["sort"] = sort;
        }
        Err(problem) => out["problem"] = Json::String(problem),
    }
    Ok(out)
}

/// A stored favorite text as JSON: JSON as this client writes it, or a literal expression
/// (`[('active', '=', True)]`, with `uid`) as data files write it.
fn stored_literal(text: &str, uid: i64) -> std::result::Result<Json, String> {
    if text.len() > MAX_FAVORITE_TEXT {
        return Err(format!("it is longer than {MAX_FAVORITE_TEXT} bytes"));
    }
    if let Ok(value) = serde_json::from_str::<Json>(text) {
        return Ok(value);
    }
    let parsed = erp_sdk::expr::parse(text.trim())
        .map_err(|error| format!("it is not a literal: {error}"))?;
    literal(&parsed, uid, 0)
}

fn literal(
    expr: &erp_sdk::expr::Expr,
    uid: i64,
    depth: usize,
) -> std::result::Result<Json, String> {
    use erp_sdk::expr::{Expr, Literal, UnaryOp};
    if depth > MAX_FAVORITE_DEPTH {
        return Err(format!("it nests deeper than {MAX_FAVORITE_DEPTH} levels"));
    }
    Ok(match expr {
        Expr::Literal(Literal::None) => Json::Null,
        Expr::Literal(Literal::Bool(value)) => json!(value),
        Expr::Literal(Literal::Int(value)) => json!(value),
        Expr::Literal(Literal::Float(value)) if value.is_finite() => json!(value),
        Expr::Literal(Literal::Str(value)) => json!(value),
        Expr::Name(name) if name == "uid" => json!(uid),
        Expr::Unary {
            op: UnaryOp::USub,
            operand,
        } => match operand.as_ref() {
            Expr::Literal(Literal::Int(value)) => {
                json!(value.checked_neg().ok_or("an integer overflows")?)
            }
            Expr::Literal(Literal::Float(value)) if value.is_finite() => json!(-value),
            _ => return Err("it holds an expression, not a literal".into()),
        },
        Expr::List(items) | Expr::Tuple(items) => Json::Array(
            items
                .iter()
                .map(|item| literal(item, uid, depth + 1))
                .collect::<std::result::Result<_, _>>()?,
        ),
        Expr::Dict(entries) => {
            let mut map = serde_json::Map::new();
            for (key, value) in entries {
                let Expr::Literal(Literal::Str(key)) = key else {
                    return Err("a dictionary key is not a text".into());
                };
                map.insert(key.clone(), literal(value, uid, depth + 1)?);
            }
            Json::Object(map)
        }
        _ => return Err("it holds an expression, not a literal".into()),
    })
}
