//! `<menuitem>` as `ir.ui.menu` records..
//!
//! Here one level of the tree is one [`model_data::load_records_cached`] call: the siblings
//! of every parent of a level are written together, the parent level first, so a tree of any
//! size costs a statement set per depth, not per menu. The external ids of the whole tree and
//! its actions are resolved with one lookup first, and the names of the actions the menus take
//! their name from are read with one query per action model.
//!
//! The name is the attribute, else the action's name for the action kinds that
//! have one (`act_window`, `wizard`, `url`, `client`, `server`), else the menu's id.
//!
//! The menus run in the container's environment without the `install_*` context keys that a
//! `<record>` adds (`_tag_menuitem` loads through the plain environment).

use std::collections::HashMap;

use erp_fields::{Cell, Value};
use erp_orm::Transaction;

use super::error::OpLabel;
use super::expr::{Known, qualify};
use super::{FileContext, LoadError, LoadErrorKind, LoadSession, env};
use crate::ast::{GroupCommand, MenuItemOp, XmlId};
use crate::model_data::{self, LoadRecord};
use crate::reader::ConvertMode;

/// The menu model.
const MENU_MODEL: &str = "ir.ui.menu";

/// Where the menu model comes from: a registry without addon `base` has none.
const MENU_SLICE: &str = "addon base: add 'base' to the module's depends";

/// Action model suffixes whose records have a `name` a menu takes.
const NAMED_ACTION_SUFFIXES: [&str; 5] = ["act_window", "wizard", "url", "client", "server"];

/// One menu of the tree with its depth.
struct Node<'a> {
    item: &'a MenuItemOp,
    /// Index of the parent in the previous level (`None` for the root of the tree).
    parent: Option<usize>,
}

fn label(item: &MenuItemOp) -> OpLabel {
    OpLabel::new(format!("<menuitem id=\"{}\">", item.id()))
}

fn fail(item: &MenuItemOp, kind: LoadErrorKind) -> LoadError {
    LoadError::at(item.source_file(), item.line(), label(item), kind)
}

fn invalid(e: crate::ast::InvalidXmlId) -> LoadErrorKind {
    LoadErrorKind::Orm(erp_fields::Error::validation(e.to_string()))
}

/// Create or update the `ir.ui.menu` records of a `<menuitem>` tree.
pub(crate) async fn load(
    session: &mut LoadSession,
    tx: &mut Transaction,
    file: &FileContext,
    op: &MenuItemOp,
) -> Result<(), LoadError> {
    let module = file.module();
    let mut known: HashMap<String, Known> = HashMap::new();
    let env = env::operation_env(session, tx, file, op.env(), &mut known)
        .await
        .map_err(|k| fail(op, k))?;
    if env.registry().model(MENU_MODEL).is_none() {
        return Err(fail(
            op,
            LoadErrorKind::ModelArrivesLater {
                what: "<menuitem>".to_owned(),
                model: MENU_MODEL.to_owned(),
                slice: MENU_SLICE.to_owned(),
            },
        ));
    }
    let menus = env
        .records(MENU_MODEL)
        .map_err(|e| fail(op, LoadErrorKind::Orm(e)))?;

    // the tree by level
    let mut levels: Vec<Vec<Node<'_>>> = vec![vec![Node {
        item: op,
        parent: None,
    }]];
    while let Some(last) = levels.last() {
        let mut next = Vec::new();
        for (index, node) in last.iter().enumerate() {
            for child in node.item.children() {
                next.push(Node {
                    item: child,
                    parent: Some(index),
                });
            }
        }
        if next.is_empty() {
            break;
        }
        levels.push(next);
    }

    // every external id the tree names, in one lookup
    let mut names: Vec<String> = Vec::new();
    for node in levels.iter().flatten() {
        let item = node.item;
        if node.parent.is_none()
            && let Some(parent) = item.parent()
        {
            names.push(qualify(&parent.to_string(), module));
        }
        if let Some(action) = item.action() {
            names.push(qualify(&action.to_string(), module));
        }
        for group in item.groups() {
            names.push(qualify(&group.xml_id().to_string(), module));
        }
    }
    session
        .resolve(tx, &names, &mut known)
        .await
        .map_err(|k| fail(op, k))?;
    let actions = action_names(session, tx, &env, module, op, &levels, &known).await?;

    let update = file.mode() == ConvertMode::Update;
    let mut previous: Vec<i64> = Vec::new();
    for level in &levels {
        let mut batch = Vec::with_capacity(level.len());
        for node in level {
            let item = node.item;
            let parent_id = match node.parent {
                Some(index) => Some(*previous.get(index).ok_or_else(|| {
                    fail(
                        item,
                        LoadErrorKind::Internal(
                            "a menu's parent was not written before it".to_owned(),
                        ),
                    )
                })?),
                None => match item.parent() {
                    Some(parent) => {
                        Some(known_id(&known, module, parent).map_err(|k| fail(item, k))?)
                    }
                    None => None,
                },
            };
            let pairs = menu_values(item, module, parent_id, &known, &actions)
                .map_err(|k| fail(item, k))?;
            let vals = menus
                .vals_from_values(pairs.iter().map(|(k, v)| (k, v)))
                .map_err(|e| fail(item, LoadErrorKind::Orm(e)))?;
            let xid = XmlId::parse(&item.id().complete_name(module))
                .map_err(|e| fail(item, invalid(e)))?;
            batch.push(LoadRecord::new(Some(xid), vals, item.env().noupdate()));
        }
        let xids: Vec<XmlId> = batch.iter().filter_map(|r| r.xml_id().cloned()).collect();
        let written =
            model_data::load_records_cached(tx, &menus, batch, update, &mut session.cache)
                .await
                .map_err(|e| fail(level[0].item, LoadErrorKind::Orm(e)))?;
        previous = written.ids();
        for xid in &xids {
            session.mark_loaded(xid);
        }
    }
    Ok(())
}

/// The id of the record `xid` names, from `known` (resolved before).
fn known_id(
    known: &HashMap<String, Known>,
    module: &str,
    xid: &XmlId,
) -> Result<i64, LoadErrorKind> {
    let name = qualify(&xid.to_string(), module);
    match known.get(&name).and_then(|k| k.id()) {
        Some(id) => Ok(id),
        None => Err(LoadErrorKind::ExternalIdNotFound { xid: name }),
    }
}

/// The record behind each action external id of the tree: its model and id, and its name for the
/// menus that need it. The action must exist (`self.env.ref`).
struct Action {
    model: String,
    id: i64,
    name: Option<String>,
}

async fn action_names(
    session: &mut LoadSession,
    tx: &mut Transaction,
    env: &erp_orm::Env,
    module: &str,
    root: &MenuItemOp,
    levels: &[Vec<Node<'_>>],
    known: &HashMap<String, Known>,
) -> Result<HashMap<String, Action>, LoadError> {
    // model -> [(qualified xid, id, needs the name)]
    let mut by_model: HashMap<String, Vec<(String, i64, bool)>> = HashMap::new();
    for node in levels.iter().flatten() {
        let item = node.item;
        let Some(action) = item.action() else {
            continue;
        };
        let name = qualify(&action.to_string(), module);
        let id = known_id(known, module, action).map_err(|k| fail(item, k))?;
        let xid = XmlId::parse(&name).map_err(|e| fail(item, invalid(e)))?;
        let row = session.cache.get(&xid).ok_or_else(|| {
            fail(
                item,
                LoadErrorKind::ExternalIdNotFound { xid: name.clone() },
            )
        })?;
        let needs_name = item.name().is_none() && has_name(&row.model);
        let entry = by_model.entry(row.model.clone()).or_default();
        match entry.iter_mut().find(|(n, _, _)| *n == name) {
            Some(existing) => existing.2 |= needs_name,
            None => entry.push((name, id, needs_name)),
        }
    }
    let mut out = HashMap::new();
    let sudo = env.sudo(true);
    for (model, wanted) in by_model {
        let first = root;
        let place = |e: erp_fields::Error| fail(first, LoadErrorKind::Orm(e));
        let ids: Vec<i64> = wanted.iter().map(|(_, id, _)| *id).collect();
        let records = sudo
            .records(&model)
            .map_err(place)?
            .browse(ids.clone())
            .map_err(place)?;
        let alive = records.exists(tx).await.map_err(place)?;
        let alive_ids = alive.ids();
        let mut names: HashMap<i64, Option<String>> = HashMap::new();
        if wanted.iter().any(|(_, _, needs)| *needs) {
            let cells = alive.cells(tx, "name").await.map_err(place)?;
            for (id, cell) in alive_ids.iter().zip(cells) {
                // a falsy name is no name (`and act.name`)
                let text = match cell {
                    Cell::Str(s) if !s.is_empty() => Some(s.to_string()),
                    _ => None,
                };
                names.insert(*id, text);
            }
        }
        for (name, id, _) in wanted {
            if !alive_ids.contains(&id) {
                return Err(fail(
                    first,
                    LoadErrorKind::Orm(erp_fields::Error::missing(format!(
                        "No record found for unique ID {name}. It may have been deleted."
                    ))),
                ));
            }
            out.insert(
                name,
                Action {
                    model: model.clone(),
                    id,
                    name: names.get(&id).cloned().flatten(),
                },
            );
        }
    }
    Ok(out)
}

fn has_name(action_model: &str) -> bool {
    NAMED_ACTION_SUFFIXES
        .iter()
        .any(|suffix| action_model.ends_with(suffix))
}

/// The field values of one menu.
fn menu_values(
    item: &MenuItemOp,
    module: &str,
    parent_id: Option<i64>,
    known: &HashMap<String, Known>,
    actions: &HashMap<String, Action>,
) -> Result<Vec<(String, Value)>, LoadErrorKind> {
    let mut pairs: Vec<(String, Value)> = Vec::new();
    pairs.push((
        "parent_id".to_owned(),
        parent_id.map_or(Value::False, Value::Int),
    ));
    pairs.push((
        "active".to_owned(),
        Value::Bool(item.active().unwrap_or(true)),
    ));
    if let Some(sequence) = item.sequence() {
        pairs.push(("sequence".to_owned(), Value::Int(i64::from(sequence))));
    }
    // only a menu without parent has an icon (the reader refuses one elsewhere)
    if parent_id.is_none()
        && let Some(icon) = item.web_icon()
    {
        pairs.push(("web_icon".to_owned(), Value::str(icon)));
    }
    let mut name = item.name().map(str::to_owned);
    if let Some(action) = item.action() {
        let qualified = qualify(&action.to_string(), module);
        let found = actions
            .get(&qualified)
            .ok_or(LoadErrorKind::ExternalIdNotFound { xid: qualified })?;
        pairs.push((
            "action".to_owned(),
            Value::str(format!("{},{}", found.model, found.id)),
        ));
        if name.is_none() && has_name(&found.model) {
            name.clone_from(&found.name);
        }
    }
    // `values['name'] = rec_id or '?'`
    let name = name.unwrap_or_else(|| item.id().to_string());
    pairs.push(("name".to_owned(), Value::str(name)));
    let mut groups = Vec::new();
    for group in item.groups() {
        let (code, xid) = match group {
            GroupCommand::Link(xid) => (4, xid),
            GroupCommand::Unlink(xid) => (3, xid),
        };
        let id = known_id(known, module, xid)?;
        groups.push(Value::List(
            vec![Value::Int(code), Value::Int(id), Value::Int(0)].into(),
        ));
    }
    if !groups.is_empty() {
        pairs.push(("group_ids".to_owned(), Value::List(groups.into())));
    }
    Ok(pairs)
}
