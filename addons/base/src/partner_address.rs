//! Native address values and bounded, caller-authorized address selection.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use erp_sdk::prelude::*;
use erp_sdk::{FieldType, ValueMap};

const MAX_DEPTH: usize = 256;
const MAX_IDENTITIES: usize = 4096;
const MAX_PREFERENCES: usize = 256;
const MAX_PREFERENCE_BYTES: usize = 65536;
const METADATA_FIELDS: [&str; 3] = ["type", "parent_id", "is_company"];

fn identity_limit() -> Error {
    Error::validation("Partner address maximum visited nodes limit of 4096 exceeded.")
}

fn depth_limit() -> Error {
    Error::validation("Partner address maximum depth of 256 exceeded.")
}

fn recursive_hierarchy() -> Error {
    Error::programming("res.partner.address_get: recursive partner hierarchy")
}

/// Resolve a native field-list hook, preserving its order and removing duplicates.
pub(crate) async fn fields(
    tx: &mut Transaction,
    records: &Records,
    hook: &'static str,
) -> Result<Vec<String>> {
    let proposed: Vec<String> = records.call(tx, hook, ()).await?;
    let mut seen = HashSet::new();
    let mut result = Vec::new();
    for name in proposed {
        let field = records.spec().field(&name).ok_or_else(|| {
            Error::programming(format!("res.partner.{hook}: unknown field '{name}'"))
        })?;
        if field.readonly
            || field.is_generated()
            || (field.compute.is_some() && field.inverse.is_none() && !field.store)
            || matches!(
                field.ty,
                FieldType::Id | FieldType::RowVersion | FieldType::One2many { .. }
            )
        {
            return Err(Error::programming(format!(
                "res.partner.{hook}: unsupported synchronization field '{name}'"
            )));
        }
        if seen.insert(name.clone()) {
            result.push(name);
        }
    }
    Ok(result)
}

/// Snapshot fields in recordset order, retaining x2many identity cells for comparison.
pub(crate) async fn cells(
    tx: &mut Transaction,
    records: &Records,
    names: &[String],
) -> Result<Vec<RawVals>> {
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    records.fetch(tx, Some(&refs)).await?;
    let mut result = vec![RawVals::new(); records.len()];
    for name in names {
        let values = records.cells(tx, name).await?;
        if values.len() != result.len() {
            return Err(Error::programming(format!(
                "res.partner address snapshot: expected {} values for '{name}', got {}",
                result.len(),
                values.len()
            )));
        }
        for (row, value) in result.iter_mut().zip(values) {
            row.insert(name.clone().into(), value);
        }
    }
    Ok(result)
}

/// PARTNER §5b keeps numeric zero empty, including extension float coordinates.
pub(crate) fn has_address(values: &RawVals) -> bool {
    values.iter().any(|(_, value)| match value {
        // A presence check must not expand every relation id into a temporary
        // dynamic Value list. The scalar conversion otherwise preserves the
        // shared truthiness contract, including JSON and numeric zero.
        Cell::Ids(ids) => !ids.is_empty(),
        scalar => Value::from_cell(scalar).is_truthy(),
    })
}

/// Convert snapshot x2many cells into ordinary replacement commands before writing.
pub(crate) fn replacement(records: &Records, values: &RawVals) -> Result<RawVals> {
    let mut result = RawVals::new();
    for (name, value) in values.iter() {
        let field = records.spec().field(name).ok_or_else(|| {
            Error::programming(format!(
                "res.partner address snapshot: unknown field '{name}'"
            ))
        })?;
        if matches!(field.ty, FieldType::Many2many { .. }) {
            let Cell::Ids(ids) = value else {
                return Err(Error::programming(format!(
                    "res.partner address snapshot: expected identity list for '{name}'"
                )));
            };
            result.insert_commands(name.to_owned().into(), vec![Command::Set(ids.to_vec())]);
        } else {
            result.insert(name.to_owned().into(), value.clone());
        }
    }
    Ok(result)
}

#[derive(Clone)]
struct Node {
    kind: Option<Arc<str>>,
    parent: Option<i64>,
    company: bool,
    // A shared sibling cohort lets one authorized query discover the next layer.
    // Sharing avoids quadratic identity copies on broad trees.
    cohort: Arc<[i64]>,
    children: Option<Vec<i64>>,
}

fn metadata(values: &RawVals, cohort: Arc<[i64]>) -> Result<Node> {
    let kind = match values.get("type") {
        Some(Cell::Null) => None,
        Some(Cell::Str(value)) => Some(Arc::clone(value)),
        _ => {
            return Err(Error::programming(
                "res.partner.address_get: expected string field 'type'",
            ));
        }
    };
    let parent = match values.get("parent_id") {
        Some(Cell::Null) => None,
        Some(Cell::Id(id)) => Some(*id),
        _ => {
            return Err(Error::programming(
                "res.partner.address_get: expected identity field 'parent_id'",
            ));
        }
    };
    let company = match values.get("is_company") {
        Some(Cell::Bool(value)) => *value,
        _ => {
            return Err(Error::programming(
                "res.partner.address_get: expected boolean field 'is_company'",
            ));
        }
    };
    Ok(Node {
        kind,
        parent,
        company,
        cohort,
        children: None,
    })
}

#[derive(Default)]
struct Graph {
    nodes: HashMap<i64, Node>,
}

impl Graph {
    fn node(&self, id: i64) -> Result<&Node> {
        self.nodes.get(&id).ok_or_else(|| {
            Error::programming("res.partner.address_get: missing loaded partner metadata")
        })
    }

    async fn load(
        &mut self,
        tx: &mut Transaction,
        records: &Records,
        cohort: Arc<[i64]>,
    ) -> Result<()> {
        let mut pending = HashSet::new();
        for id in records.rec_ids() {
            let id = id
                .as_real()
                .ok_or_else(|| {
                    Error::programming(
                        "res.partner.address_get: expected persisted partner records",
                    )
                })?
                .get();
            if !self.nodes.contains_key(&id)
                && pending.insert(id)
                && self
                    .nodes
                    .len()
                    .checked_add(pending.len())
                    .ok_or_else(identity_limit)?
                    > MAX_IDENTITIES
            {
                return Err(identity_limit());
            }
        }
        let names: Vec<String> = METADATA_FIELDS.into_iter().map(str::to_owned).collect();
        let snapshots = cells(tx, records, &names).await?;
        for (id, snapshot) in records.rec_ids().iter().zip(snapshots) {
            let id = id
                .as_real()
                .ok_or_else(|| {
                    Error::programming(
                        "res.partner.address_get: expected persisted partner records",
                    )
                })?
                .get();
            if let std::collections::hash_map::Entry::Vacant(entry) = self.nodes.entry(id) {
                entry.insert(metadata(&snapshot, Arc::clone(&cohort))?);
            }
        }
        Ok(())
    }

    async fn ensure_node(&mut self, tx: &mut Transaction, model: &Records, id: i64) -> Result<()> {
        if !self.nodes.contains_key(&id) {
            let record = model.browse([id])?;
            self.load(tx, &record, Arc::from([id])).await?;
        }
        Ok(())
    }

    async fn ensure_children(
        &mut self,
        tx: &mut Transaction,
        model: &Records,
        id: i64,
    ) -> Result<()> {
        if self.node(id)?.children.is_some() {
            return Ok(());
        }
        let cohort = Arc::clone(&self.node(id)?.cohort);
        let mut parents = Vec::new();
        for candidate in cohort.iter().copied() {
            if self.node(candidate)?.children.is_none() {
                parents.push(candidate);
            }
        }
        let domain = Domain::and([
            Domain::condition("parent_id", Operator::In, parents.clone())?,
            Domain::condition("is_company", Operator::Eq, false)?,
        ]);
        // A bounded sentinel page is essential: do not materialize an unbounded
        // child list before discovering that it exceeds the identity ceiling.
        let children = model
            .search_with(
                tx,
                &domain,
                &SearchOptions::new()
                    .limit(4097)
                    .fetch_fields(METADATA_FIELDS),
            )
            .await?;
        let child_ids = children.ids();
        if child_ids.len() > MAX_IDENTITIES {
            return Err(identity_limit());
        }
        let child_cohort: Arc<[i64]> = Arc::from(child_ids.as_slice());
        self.load(tx, &children, child_cohort).await?;
        for parent in parents {
            let node = self.nodes.get_mut(&parent).ok_or_else(|| {
                Error::programming("res.partner.address_get: missing loaded parent metadata")
            })?;
            node.children = Some(Vec::new());
        }
        for child in child_ids {
            let parent = self.node(child)?.parent.ok_or_else(|| {
                Error::programming("res.partner.address_get: searched child has no parent")
            })?;
            let node = self.nodes.get_mut(&parent).ok_or_else(|| {
                Error::programming(
                    "res.partner.address_get: searched child has an unexpected parent",
                )
            })?;
            let children = node.children.as_mut().ok_or_else(|| {
                Error::programming(
                    "res.partner.address_get: searched parent children were not initialized",
                )
            })?;
            children.push(child);
        }
        Ok(())
    }
}

enum Visit {
    Enter { id: i64, depth: usize },
    Leave(i64),
}

fn preferences(input: Vec<String>) -> Result<Vec<String>> {
    if input.len() > MAX_PREFERENCES {
        return Err(Error::validation(
            "Partner address preferences exceed the 256-entry limit",
        ));
    }
    let mut bytes = 0_usize;
    for preference in &input {
        bytes = bytes.checked_add(preference.len()).ok_or_else(|| {
            Error::validation("Partner address preferences exceed the 65536-byte limit")
        })?;
        if bytes > MAX_PREFERENCE_BYTES {
            return Err(Error::validation(
                "Partner address preferences exceed the 65536-byte limit",
            ));
        }
    }
    let mut seen = HashSet::new();
    let mut result = Vec::new();
    for preference in input {
        if seen.insert(preference.clone()) {
            result.push(preference);
        }
    }
    if seen.insert("contact".to_owned()) {
        result.push("contact".to_owned());
    }
    Ok(result)
}

/// Select requested address types, with first-match and fallback semantics from PARTNER §4.5.
pub(crate) async fn selection(
    tx: &mut Transaction,
    records: &Records,
    input_preferences: Vec<String>,
) -> Result<ValueMap> {
    let requested = preferences(input_preferences)?;
    let mut input_ids = Vec::new();
    let mut initial_seen = HashSet::new();
    for id in records.rec_ids() {
        let id = id
            .as_real()
            .ok_or_else(|| {
                Error::programming("res.partner.address_get: expected persisted partner records")
            })?
            .get();
        if initial_seen.insert(id) {
            if input_ids.len() == MAX_IDENTITIES {
                return Err(identity_limit());
            }
            input_ids.push(id);
        }
    }
    if input_ids.is_empty() {
        return Ok(requested
            .into_iter()
            .map(|key| (key, Value::False))
            .collect());
    }
    let model = records.browse([])?;
    let inputs = model.browse(input_ids.iter().copied())?;
    let mut graph = Graph::default();
    // Each starting node gets a singleton cohort. Processing one input must not
    // expand another input's unrelated subtree before its turn.
    graph
        .load(tx, &inputs, Arc::from(input_ids.as_slice()))
        .await?;
    for id in &input_ids {
        let node = graph.nodes.get_mut(id).ok_or_else(|| {
            Error::programming("res.partner.address_get: missing starting partner metadata")
        })?;
        node.cohort = Arc::from([*id]);
    }
    let wanted: HashSet<&str> = requested.iter().map(String::as_str).collect();
    let mut found: HashMap<String, i64> = HashMap::new();
    let mut visited = HashSet::new();
    'inputs: for input in &input_ids {
        let mut root = *input;
        let mut ancestor_depth = 0_usize;
        let mut ancestors = HashSet::new();
        loop {
            if !ancestors.insert(root) {
                return Err(recursive_hierarchy());
            }
            graph.ensure_node(tx, &model, root).await?;
            let mut path = HashSet::new();
            let mut stack = vec![Visit::Enter {
                id: root,
                depth: ancestor_depth,
            }];
            while let Some(visit) = stack.pop() {
                let (id, depth) = match visit {
                    Visit::Leave(id) => {
                        path.remove(&id);
                        continue;
                    }
                    Visit::Enter { id, depth } => (id, depth),
                };
                if path.contains(&id) {
                    return Err(recursive_hierarchy());
                }
                if visited.contains(&id) {
                    continue;
                }
                if depth > MAX_DEPTH {
                    return Err(depth_limit());
                }
                visited.insert(id);
                path.insert(id);
                let node = graph.node(id)?;
                if let Some(kind) = node.kind.as_deref()
                    && wanted.contains(kind)
                    && !found.contains_key(kind)
                {
                    found.insert(kind.to_owned(), id);
                    if found.len() == requested.len() {
                        break 'inputs;
                    }
                }
                graph.ensure_children(tx, &model, id).await?;
                let node = graph.node(id)?;
                let children = node.children.as_ref().ok_or_else(|| {
                    Error::programming("res.partner.address_get: children were not loaded")
                })?;
                let child_depth = depth.checked_add(1).ok_or_else(depth_limit)?;
                stack.push(Visit::Leave(id));
                for child in children.iter().rev().copied() {
                    // Charge the actual path before adding another stack entry.
                    if !visited.contains(&child) && child_depth > MAX_DEPTH {
                        return Err(depth_limit());
                    }
                    stack.push(Visit::Enter {
                        id: child,
                        depth: child_depth,
                    });
                }
            }
            let node = graph.node(root)?;
            if node.company {
                break;
            }
            let Some(parent) = node.parent else {
                break;
            };
            ancestor_depth = ancestor_depth.checked_add(1).ok_or_else(depth_limit)?;
            if ancestor_depth > MAX_DEPTH {
                return Err(depth_limit());
            }
            root = parent;
        }
    }
    let fallback = match found.get("contact").copied() {
        Some(id) => id,
        None if records.len() == 1 => input_ids[0],
        None => {
            return Err(Error::programming(format!(
                "res.partner.address_get: expected one record for contact fallback, got {}",
                records.len()
            )));
        }
    };
    Ok(requested
        .into_iter()
        .map(|key| {
            // Missing requested types intentionally fall back to contact or the
            // singleton input under PARTNER §4.5 /.
            let id = found.get(&key).copied().unwrap_or(fallback);
            (key, Value::Int(id))
        })
        .collect())
}
