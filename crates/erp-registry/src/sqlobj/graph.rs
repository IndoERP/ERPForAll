// SPDX-License-Identifier: MIT OR Apache-2.0
//! The resolved SQL object graph of a registry: qualified identities, owners, resolved references
//! and a deterministic dependency order. [`resolve`] validates the declarations of the addon set
//! against the composed registry and builds it.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use erp_fields::{Error, Result};

use super::decl::{
    SqlColumnSet, SqlCompareColumn, SqlConditionDecl, SqlDeferral, SqlDep, SqlFunctionDecl,
    SqlLanguage, SqlObjectDecl, SqlObjectKindDecl, SqlObjectManifest, SqlResult, SqlSecurity,
    SqlType, SqlVolatility,
};
use super::validate::{Who, check, check_key, decl_bytes, describe_dep};
use super::{SqlObjectLimits, within};
use crate::spec::Registry;

/// The qualified identity of a SQL object: owner addon and local key (`addon/key`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SqlObjectId {
    addon: &'static str,
    key: &'static str,
}

impl SqlObjectId {
    /// The owner addon.
    pub fn addon(&self) -> &'static str {
        self.addon
    }

    /// The local key, unique within the owner addon.
    pub fn key(&self) -> &'static str {
        self.key
    }

    fn who(&self) -> Who<'static> {
        (self.addon, self.key)
    }
}

impl fmt::Display for SqlObjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.addon, self.key)
    }
}

/// The SQL objects of a registry, validated and resolved, in dependency order.
#[derive(Debug, Default)]
pub struct SqlObjectGraph {
    objects: Vec<SqlObject>,
    index: BTreeMap<(&'static str, &'static str), usize>,
    limits: SqlObjectLimits,
    /// `(SQLSTATE, identifier)` to the position of the object declaring that condition.
    conditions: BTreeMap<(&'static str, &'static str), usize>,
}

impl SqlObjectGraph {
    /// Every object in dependency order: each after the objects it references, ties broken by
    /// identity.
    pub fn objects(&self) -> &[SqlObject] {
        &self.objects
    }

    /// The object `addon/key`.
    pub fn get(&self, addon: &str, key: &str) -> Option<&SqlObject> {
        let index: &BTreeMap<(&str, &str), usize> = &self.index;
        index.get(&(addon, key)).and_then(|&i| self.objects.get(i))
    }

    /// The admission bounds the declarations were validated with; schema sync applies them to
    /// catalog-derived bodies it renders.
    pub fn limits(&self) -> SqlObjectLimits {
        self.limits
    }

    /// The declared condition raised with `sqlstate` and the CONSTRAINT diagnostic `identifier`,
    /// with its owning object, or `None` when that exact pair is not declared. Both parts must
    /// match: an identifier alone, or under another SQLSTATE, is not a declared condition.
    pub fn condition(
        &self,
        sqlstate: &str,
        identifier: &str,
    ) -> Option<(SqlObjectId, &SqlCondition)> {
        let conditions: &BTreeMap<(&str, &str), usize> = &self.conditions;
        let object = conditions
            .get(&(sqlstate, identifier))
            .and_then(|&i| self.objects.get(i))?;
        match &object.kind {
            SqlObjectKind::Condition(c) => Some((object.id, c)),
            // only conditions are indexed (`order`); a broken index falls back to `Database`
            // mapping in release builds and fails here in debug and test builds
            _ => {
                debug_assert!(false, "condition index points at {}", object.id);
                None
            }
        }
    }
}

/// One resolved SQL object.
#[derive(Debug)]
pub struct SqlObject {
    id: SqlObjectId,
    references: Vec<SqlReference>,
    kind: SqlObjectKind,
}

impl SqlObject {
    /// The qualified identity.
    pub fn id(&self) -> SqlObjectId {
        self.id
    }

    /// The owner addon (the identity's addon).
    pub fn owner(&self) -> &'static str {
        self.id.addon
    }

    /// The resolved direct references, in declaration order: a function's declared dependencies;
    /// a trigger's function, then its model table.
    pub fn references(&self) -> &[SqlReference] {
        &self.references
    }

    /// What the object is.
    pub fn kind(&self) -> &SqlObjectKind {
        &self.kind
    }
}

/// A resolved reference of a SQL object.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum SqlReference {
    /// Another SQL object of the graph.
    Object(SqlObjectId),
    /// The table of a model of the registry.
    Model(SqlModelRef),
}

/// A model referenced by a SQL object, with its table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqlModelRef {
    model: &'static str,
    table: String,
}

impl SqlModelRef {
    /// The model name.
    pub fn model(&self) -> &'static str {
        self.model
    }

    /// The model's table (current schema).
    pub fn table(&self) -> &str {
        &self.table
    }
}

/// What a resolved SQL object is.
#[derive(Debug)]
#[non_exhaustive]
pub enum SqlObjectKind {
    /// A function.
    Function(SqlFunction),
    /// A trigger.
    Trigger(SqlTrigger),
    /// A condition.
    Condition(SqlCondition),
}

/// A validated function.
#[derive(Debug)]
pub struct SqlFunction(SqlFunctionDecl);

impl SqlFunction {
    /// Schema.
    pub fn schema(&self) -> &'static str {
        self.0.schema
    }
    /// Function name.
    pub fn name(&self) -> &'static str {
        self.0.name
    }
    /// Body language.
    pub fn language(&self) -> SqlLanguage {
        self.0.language
    }
    /// Argument types, in order (part of the database identity).
    pub fn args(&self) -> &'static [SqlType] {
        self.0.args
    }
    /// Result type.
    pub fn result(&self) -> SqlResult {
        self.0.result
    }
    /// Trusted native body; empty for a catalog-derived function ([`Self::render`]).
    pub fn body(&self) -> &'static str {
        self.0.body
    }
    /// The table and allow-list of a catalog-derived function.
    pub fn column_set(&self) -> Option<&SqlColumnSet> {
        self.0.derived.as_ref().map(|(columns, _)| columns)
    }
    /// The body for the compared `columns`: the rendered body of a catalog-derived function,
    /// otherwise [`Self::body`] (the columns are not used).
    pub fn render(&self, columns: &[SqlCompareColumn]) -> String {
        match self.0.derived {
            Some((_, render)) => render(columns),
            None => self.0.body.to_owned(),
        }
    }
    /// Volatility.
    pub fn volatility(&self) -> SqlVolatility {
        self.0.volatility
    }
    /// Security mode.
    pub fn security(&self) -> SqlSecurity {
        self.0.security
    }
}

/// A validated trigger on a model table.
#[derive(Debug)]
pub struct SqlTrigger {
    name: &'static str,
    model: SqlModelRef,
    function: SqlObjectId,
    shape: SqlTriggerShape,
}

impl SqlTrigger {
    /// Trigger name (unique per table).
    pub fn name(&self) -> &'static str {
        self.name
    }
    /// The model whose table carries the trigger.
    pub fn model(&self) -> &SqlModelRef {
        &self.model
    }
    /// The function object it executes (returns `trigger`).
    pub fn function(&self) -> SqlObjectId {
        self.function
    }
    /// Its shape: one of the four supported variants.
    pub fn shape(&self) -> SqlTriggerShape {
        self.shape
    }
}

/// The four supported trigger shapes (contract §2). Closed: every consumer matches all of them,
/// and a new shape needs an amendment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SqlTriggerShape {
    /// `AFTER <event> FOR EACH STATEMENT` with the transition tables of its one DML event.
    AfterStatement(SqlTransition),
    /// `BEFORE TRUNCATE FOR EACH STATEMENT`.
    BeforeTruncate,
    /// `BEFORE INSERT FOR EACH ROW`.
    BeforeInsertRow,
    /// `CREATE CONSTRAINT TRIGGER … AFTER INSERT FOR EACH ROW` with an explicit deferral.
    AfterInsertConstraintRow(SqlDeferral),
}

/// The DML event of an AFTER STATEMENT trigger with its matching transition aliases.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SqlTransition {
    /// `INSERT … REFERENCING NEW TABLE AS new_table`.
    Insert {
        /// `NEW TABLE` alias.
        new_table: &'static str,
    },
    /// `UPDATE … REFERENCING OLD TABLE AS old_table NEW TABLE AS new_table` (distinct aliases).
    Update {
        /// `OLD TABLE` alias.
        old_table: &'static str,
        /// `NEW TABLE` alias.
        new_table: &'static str,
    },
    /// `DELETE … REFERENCING OLD TABLE AS old_table`.
    Delete {
        /// `OLD TABLE` alias.
        old_table: &'static str,
    },
}

/// A validated condition: SQLSTATE, identifier and static public message.
#[derive(Debug)]
pub struct SqlCondition(SqlConditionDecl);

impl SqlCondition {
    /// Five-character SQLSTATE in the `ERP` namespace.
    pub fn sqlstate(&self) -> &'static str {
        self.0.sqlstate
    }
    /// Condition identifier (the CONSTRAINT diagnostic the raising function sets).
    pub fn identifier(&self) -> &'static str {
        self.0.identifier
    }
    /// Static public message.
    pub fn message(&self) -> &'static str {
        self.0.message
    }
}

/// A validated declaration with its trigger shape (`None` for other kinds).
type Entry = (&'static SqlObjectDecl, Option<SqlTriggerShape>);

/// Validate the declarations of `registry`'s addon set against the required `manifests` and build
/// their graph. Explicit declarations must belong to the set; manifests and (with `linked`) linked
/// declarations of other addons are ignored.
pub(crate) fn resolve(
    registry: &Registry,
    manifests: &[&'static SqlObjectManifest],
    decls: &[&'static SqlObjectDecl],
    linked: bool,
    limits: SqlObjectLimits,
) -> Result<SqlObjectGraph> {
    limits.check_not_raised()?;
    let graph_limits = limits;
    // `linked(false)` specifies no linked declarations, not a fallback
    let linked_decls = linked.then(crate::linked_sql_objects).unwrap_or_default();
    // each addon with itself and its transitive dependencies, in one pass over the load order
    let mut reach: Reach = BTreeMap::new();
    for a in registry.addons() {
        let mut set = BTreeSet::from([a.name]);
        for dep in a.depends {
            set.extend(reach.get(dep).into_iter().flatten().copied());
        }
        reach.insert(a.name, set);
    }

    // required manifests of the addon set, sorted by addon
    let mut manifests: Vec<_> = manifests
        .iter()
        .filter(|m| reach.contains_key(m.addon))
        .collect();
    manifests.sort_by_key(|m| m.addon);
    let mut expected: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for m in &manifests {
        let addon = m.addon;
        if expected.contains_key(addon) {
            return Err(Error::programming(format!(
                "SQL object manifest of addon '{addon}' is declared more than once"
            )));
        }
        let mut keys = BTreeSet::new();
        for (i, key) in m.expected.iter().enumerate() {
            let who = (addon, *key);
            check_key(who)?;
            within(
                who,
                "addon objects",
                i.saturating_add(1),
                limits.addon_objects,
            )?;
            if !keys.insert(*key) {
                return Err(err!(
                    who,
                    "listed more than once in the manifest of addon '{addon}'"
                ));
            }
        }
        expected.insert(addon, keys);
    }

    // declarations, sorted by identity: duplicates are adjacent and the first error is stable
    if let Some(d) = decls.iter().find(|d| !reach.contains_key(d.addon)) {
        let addon = d.addon;
        return Err(err!(
            (addon, d.key),
            "addon '{addon}' is not in the addon set"
        ));
    }
    let linked_decls = linked_decls.iter().filter(|d| reach.contains_key(d.addon));
    let mut decls: Vec<&'static SqlObjectDecl> =
        decls.iter().copied().chain(linked_decls).collect();
    decls.sort_by_key(|d| (d.addon, d.key));
    let mut by_id: BTreeMap<SqlObjectId, Entry> = BTreeMap::new();
    let mut per_addon: BTreeMap<&str, (usize, usize)> = BTreeMap::new();
    let mut registry_bytes = 0usize;
    for decl in decls {
        let (addon, key) = (decl.addon, decl.key);
        let who = (addon, key);
        check_key(who)?;
        let id = SqlObjectId { addon, key };
        if by_id.contains_key(&id) {
            return Err(err!(who, "declared more than once"));
        }
        within(
            who,
            "registry objects",
            by_id.len().saturating_add(1),
            limits.registry_objects,
        )?;
        let (count, bytes) = per_addon.entry(addon).or_insert((0, 0));
        within(
            who,
            "addon objects",
            count.saturating_add(1),
            limits.addon_objects,
        )?;
        if !expected.get(addon).is_some_and(|keys| keys.contains(key)) {
            return Err(err!(
                who,
                "not listed in the SQL object manifest of addon '{addon}'"
            ));
        }
        let shape = check(decl, &limits)?;
        // an overflowing sum counts as over every limit
        let own = decl_bytes(decl).unwrap_or(usize::MAX);
        *count = count.saturating_add(1);
        *bytes = bytes.saturating_add(own);
        within(who, "addon bytes", *bytes, limits.addon_bytes)?;
        registry_bytes = registry_bytes.saturating_add(own);
        within(who, "registry bytes", registry_bytes, limits.registry_bytes)?;
        by_id.insert(id, (decl, shape));
    }
    for (addon, keys) in &expected {
        if let Some(key) = keys
            .iter()
            .find(|key| !by_id.contains_key(&SqlObjectId { addon, key }))
        {
            return Err(Error::programming(format!(
                "SQL object '{addon}/{key}' required by addon '{addon}' was not registered"
            )));
        }
    }

    let mut objects = Vec::with_capacity(by_id.len());
    for (&id, &(decl, shape)) in &by_id {
        objects.push(resolve_one(registry, &by_id, &reach, id, decl, shape)?);
    }
    check_collisions(&objects)?;
    let mut graph = order(objects)?;
    graph.limits = graph_limits;
    Ok(graph)
}

/// Each addon of the set with itself and its transitive manifest dependencies.
type Reach<'a> = BTreeMap<&'a str, BTreeSet<&'a str>>;

/// True when addon `from` is `to` or depends on it, directly or transitively.
fn reaches(reach: &Reach<'_>, from: &str, to: &str) -> bool {
    reach.get(from).is_some_and(|set| set.contains(to))
}

fn resolve_one(
    registry: &Registry,
    by_id: &BTreeMap<SqlObjectId, Entry>,
    reach: &Reach<'_>,
    id: SqlObjectId,
    decl: &SqlObjectDecl,
    shape: Option<SqlTriggerShape>,
) -> Result<SqlObject> {
    let (references, kind) = match decl.kind {
        SqlObjectKindDecl::Function(f) => {
            let mut references = Vec::with_capacity(f.deps.len());
            for dep in f.deps {
                references.push(match *dep {
                    SqlDep::Object { addon, key } => {
                        let target = SqlObjectId { addon, key };
                        object_ref(by_id, reach, id, target)?;
                        SqlReference::Object(target)
                    }
                    SqlDep::Model(model) => {
                        SqlReference::Model(model_ref(registry, reach, id, model)?)
                    }
                });
            }
            if let Some((columns, _)) = f.derived {
                let table = model_ref(registry, reach, id, columns.model)?;
                allow_list(registry, id, &columns)?;
                if !references.contains(&SqlReference::Model(table.clone())) {
                    references.push(SqlReference::Model(table));
                }
            }
            (references, SqlObjectKind::Function(SqlFunction(f)))
        }
        SqlObjectKindDecl::Trigger(t) => {
            let (addon, key) = t.function;
            let function = SqlObjectId { addon, key };
            match object_ref(by_id, reach, id, function)?.kind {
                SqlObjectKindDecl::Function(f) if f.result == SqlResult::Trigger => {}
                SqlObjectKindDecl::Function(_) => {
                    return Err(err!(
                        id.who(),
                        "trigger function '{function}' does not return \
                                               trigger"
                    ));
                }
                SqlObjectKindDecl::Trigger(_) | SqlObjectKindDecl::Condition(_) => {
                    return Err(err!(
                        id.who(),
                        "trigger function '{function}' is not a function"
                    ));
                }
            }
            let model = model_ref(registry, reach, id, t.model)?;
            let shape = shape.ok_or_else(|| err!(id.who(), "trigger shape was not validated"))?;
            let references = vec![
                SqlReference::Object(function),
                SqlReference::Model(model.clone()),
            ];
            let trigger = SqlTrigger {
                name: t.name,
                model,
                function,
                shape,
            };
            (references, SqlObjectKind::Trigger(trigger))
        }
        SqlObjectKindDecl::Condition(c) => (Vec::new(), SqlObjectKind::Condition(SqlCondition(c))),
    };
    Ok(SqlObject {
        id,
        references,
        kind,
    })
}

/// An object reference: the target is registered and its addon is `id`'s addon or one of its
/// dependencies.
fn object_ref(
    by_id: &BTreeMap<SqlObjectId, Entry>,
    reach: &Reach<'_>,
    id: SqlObjectId,
    target: SqlObjectId,
) -> Result<&'static SqlObjectDecl> {
    let (found, _) = by_id
        .get(&target)
        .copied()
        .ok_or_else(|| err!(id.who(), "dependency '{target}' is not registered"))?;
    if !reaches(reach, id.addon, target.addon) {
        let (addon, other) = (id.addon, target.addon);
        return Err(err!(
            id.who(),
            "depends on '{target}', but addon '{addon}' does not depend on addon '{other}'"
        ));
    }
    Ok(found)
}

/// A model reference: the model exists, is an addon model with an ordinary table in the current
/// schema, and its creating addon is the object's addon or one of its dependencies.
fn model_ref(
    registry: &Registry,
    reach: &Reach<'_>,
    id: SqlObjectId,
    model: &'static str,
) -> Result<SqlModelRef> {
    let who = id.who();
    let spec = registry
        .model(model)
        .ok_or_else(|| err!(who, "model '{model}' is not in the registry"))?;
    if !spec.has_generic_table() {
        return Err(err!(
            who,
            "model '{model}' has no table in the current schema"
        ));
    }
    let owner = spec
        .mro
        .iter()
        .rev()
        .find(|d| d.name == model && !d.is_extension())
        .map(|d| d.addon)
        .ok_or_else(|| err!(who, "model '{model}' has no creating addon"))?;
    if !reaches(reach, id.addon, owner) {
        let (addon, dep) = (id.addon, describe_dep(&SqlDep::Model(model)));
        return Err(err!(
            who,
            "depends on {dep}, but addon '{addon}' does not depend on addon '{owner}'"
        ));
    }
    Ok(SqlModelRef {
        model,
        table: spec.table.clone(),
    })
}

/// Each allow-list entry is a stored, non-generated column field of the model.
fn allow_list(registry: &Registry, id: SqlObjectId, columns: &SqlColumnSet) -> Result<()> {
    let model = columns.model;
    let spec = registry
        .model(model)
        .ok_or_else(|| err!(id.who(), "model '{model}' is not in the registry"))?;
    for name in columns.allow {
        let stored = spec
            .field(name)
            .is_some_and(|f| f.column().is_some() && !f.is_generated());
        if !stored {
            return Err(err!(
                id.who(),
                "allow-list field '{model}.{name}' is not a stored column"
            ));
        }
    }
    Ok(())
}

/// Physical identities: function schema + name + argument types, trigger table + name,
/// condition SQLSTATE + identifier. The later object in identity order is refused.
fn check_collisions(objects: &[SqlObject]) -> Result<()> {
    let mut functions: BTreeMap<(&str, &str, &[SqlType]), SqlObjectId> = BTreeMap::new();
    let mut triggers: BTreeMap<(&str, &str), SqlObjectId> = BTreeMap::new();
    let mut conditions: BTreeMap<(&str, &str), SqlObjectId> = BTreeMap::new();
    for object in objects {
        let (what, previous) = match &object.kind {
            SqlObjectKind::Function(SqlFunction(f)) => (
                format!(
                    "function {}.{} with the same argument types",
                    f.schema, f.name
                ),
                functions.insert((f.schema, f.name, f.args), object.id),
            ),
            SqlObjectKind::Trigger(t) => (
                format!("trigger '{}' on table '{}'", t.name, t.model.table),
                triggers.insert((t.model.table.as_str(), t.name), object.id),
            ),
            SqlObjectKind::Condition(SqlCondition(c)) => (
                format!("condition {} '{}'", c.sqlstate, c.identifier),
                conditions.insert((c.sqlstate, c.identifier), object.id),
            ),
        };
        if let Some(other) = previous {
            return Err(err!(
                object.id.who(),
                "{what} is also declared by '{other}'"
            ));
        }
    }
    Ok(())
}

/// Kahn's algorithm over object references with a `BTreeSet` of ready identities (iterative,
/// deterministic). A cycle is refused naming its members.
fn order(objects: Vec<SqlObject>) -> Result<SqlObjectGraph> {
    let ids: BTreeMap<SqlObjectId, usize> =
        objects.iter().enumerate().map(|(i, o)| (o.id, i)).collect();
    let deps: Vec<Vec<usize>> = objects
        .iter()
        .map(|o| {
            // every object reference was resolved by `object_ref`, so the lookup always finds it
            let targets = o.references.iter().filter_map(|r| match r {
                SqlReference::Object(target) => ids.get(target).copied(),
                SqlReference::Model(_) => None,
            });
            targets.collect()
        })
        .collect();
    let mut pending: Vec<usize> = deps.iter().map(Vec::len).collect();
    let mut dependents: Vec<Vec<usize>> = vec![Vec::new(); objects.len()];
    for (i, targets) in deps.iter().enumerate() {
        for &target in targets {
            dependents[target].push(i);
        }
    }
    let mut ready: BTreeSet<(SqlObjectId, usize)> = (0..objects.len())
        .filter(|&i| pending[i] == 0)
        .map(|i| (objects[i].id, i))
        .collect();
    let mut sequence = Vec::with_capacity(objects.len());
    while let Some((_, i)) = ready.pop_first() {
        sequence.push(i);
        for &d in &dependents[i] {
            // `d` waits on `i`, so its count is at least one here
            pending[d] = pending[d].saturating_sub(1);
            if pending[d] == 0 {
                ready.insert((objects[d].id, d));
            }
        }
    }
    if sequence.len() < objects.len() {
        return Err(cycle(&objects, &pending, &deps));
    }
    let mut slots: Vec<Option<SqlObject>> = objects.into_iter().map(Some).collect();
    let mut graph = SqlObjectGraph::default();
    for object in sequence.into_iter().filter_map(|i| slots[i].take()) {
        graph
            .index
            .insert((object.id.addon, object.id.key), graph.objects.len());
        if let SqlObjectKind::Condition(c) = &object.kind {
            let previous = graph
                .conditions
                .insert((c.sqlstate(), c.identifier()), graph.objects.len());
            // `check_collisions` refused a second declaration of one pair
            debug_assert!(previous.is_none(), "condition {} declared twice", object.id);
        }
        graph.objects.push(object);
    }
    Ok(graph)
}

/// The members of one cycle among the objects Kahn could not order: from the smallest left-over
/// identity, follow the smallest left-over dependency until an object repeats; the repeated
/// suffix is the cycle. Objects that only depend on a cycle are not named.
fn cycle(objects: &[SqlObject], pending: &[usize], deps: &[Vec<usize>]) -> Error {
    let left = |i: &usize| pending[*i] > 0;
    let mut at = (0..objects.len())
        .filter(left)
        .min_by_key(|&i| objects[i].id);
    let mut path: Vec<usize> = Vec::new();
    let mut position: Vec<Option<usize>> = vec![None; objects.len()];
    while let Some(i) = at {
        if let Some(start) = position[i] {
            let mut members: Vec<SqlObjectId> =
                path[start..].iter().map(|&p| objects[p].id).collect();
            members.sort_unstable();
            let names: Vec<String> = members.iter().map(|m| format!("'{m}'")).collect();
            let Some(first) = members.first() else { break };
            return err!(first.who(), "dependency cycle among {}", names.join(", "));
        }
        position[i] = Some(path.len());
        path.push(i);
        // every left-over object has a left-over dependency (otherwise Kahn would have ordered it)
        at = deps[i]
            .iter()
            .copied()
            .filter(left)
            .min_by_key(|&d| objects[d].id);
    }
    Error::programming("SQL object dependency order failed without a cycle")
}
