//! Composition of declarations into a [`Registry`] (MODEL §4.4–4.5, FIELD §4.2).
//!
//! Declarations are processed in load order.
//! Fields merge over the MRO oldest first.

use std::sync::atomic::{AtomicU64, Ordering};

use erp_fields::{ColumnType, Error, FieldType, IndexKind, OnDelete, Result, default_label};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::decl::{
    AddonDescriptor, DefaultDecl, FieldDecl, KindDecl, ModelDecl, ModelKind, SelectionOnDeleteDecl,
    StaticValue, TableObjectKind,
};
use crate::drop::{DropReason, DroppedField, ManualFieldPolicy, Sink};
use crate::ident::{MAX_IDENTIFIER_LEN, is_valid_pg_name, pg_make_object_name, shorten_identifier};
use crate::order::load_order;
use crate::overlay::{MANUAL_ADDON, Overlay};
use crate::spec::{
    CheckCompany, ComputeRead, FieldDefault, FieldId, FieldSpec, ModelId, ModelSpec, Registry,
    TableObject,
};
use crate::sqlobj::SqlObjectLimits;
use crate::sqlobj::decl::{SqlObjectDecl, SqlObjectManifest};

static GENERATION: AtomicU64 = AtomicU64::new(1);

/// The core addon every registry contains.
pub static BASE_ADDON: AddonDescriptor = AddonDescriptor {
    name: "base",
    depends: &[],
    exact_only: false,
};

pub static WEB_ADDON: AddonDescriptor = AddonDescriptor {
    name: "web",
    depends: &["base"],
    exact_only: false,
};

/// The `web` addon's extension of `base` (no fields; the web methods hang on it).
pub static WEB_DECL: ModelDecl = ModelDecl::extension("web", "base", &["base"])
    .kind(ModelKind::Abstract)
    .at(file!(), line!());

const BASE_FIELDS: &[FieldDecl] = &[
    FieldDecl::new("id", KindDecl::Id)
        .string("ID")
        .readonly(true),
    FieldDecl::char("display_name")
        .compute("_compute_display_name")
        .store(false),
];

/// The `base` model: abstract, inherited by every model, with `id` and `display_name`
/// (MODEL §4.2).
pub static BASE_DECL: ModelDecl = ModelDecl::new("base", "base")
    .kind(ModelKind::Abstract)
    .description("Base")
    .fields(BASE_FIELDS)
    .at(file!(), line!());

const USERS_FIELDS: &[FieldDecl] = &[
    FieldDecl::char("name").required(true),
    FieldDecl::char("login"),
    FieldDecl::boolean("active").default(StaticValue::Bool(true)),
];

/// `res.users`, which the audit fields reference. It belongs to the core `base` addon until the real
/// `addons/base` takes it over (it then moves there unchanged).
pub static USERS_DECL: ModelDecl = ModelDecl::new("base", "res.users")
    .description("User")
    .fields(USERS_FIELDS)
    .at(file!(), line!());

/// Audit fields added to definitions that create a model with log access (MODEL-011).
const MAGIC_FIELDS: &[FieldDecl] = &[
    FieldDecl::many2one("create_uid", "res.users")
        .string("Created by")
        .readonly(true)
        .ondelete(OnDelete::SetNull)
        .index(None),
    FieldDecl::datetime("create_date")
        .string("Created on")
        .readonly(true),
    FieldDecl::many2one("write_uid", "res.users")
        .string("Last Updated by")
        .readonly(true)
        .ondelete(OnDelete::SetNull)
        .index(None),
    FieldDecl::datetime("write_date")
        .string("Last Updated on")
        .readonly(true),
];

/// Names of the four audit fields.
pub const MAGIC_FIELD_NAMES: [&str; 4] = ["create_uid", "create_date", "write_uid", "write_date"];

/// Builds a [`Registry`] from an addon set.
#[derive(Debug)]
pub struct RegistryBuilder {
    addons: Vec<&'static AddonDescriptor>,
    extra: Vec<&'static ModelDecl>,
    /// Declarations of the closed event namespace (core path only, see `with_core_event_decls`).
    events: Vec<&'static ModelDecl>,
    overlays: Vec<Overlay>,
    linked: bool,
    policy: ManualFieldPolicy,
    sql_manifests: Vec<&'static SqlObjectManifest>,
    sql_decls: Vec<&'static SqlObjectDecl>,
    sql_limits: SqlObjectLimits,
}

impl RegistryBuilder {
    /// Start from the installed addons (any order; `base` and `web` are always added).
    pub fn new(addons: impl IntoIterator<Item = &'static AddonDescriptor>) -> Self {
        Self {
            addons: addons.into_iter().collect(),
            extra: Vec::new(),
            events: Vec::new(),
            overlays: Vec::new(),
            linked: true,
            policy: ManualFieldPolicy::Strict,
            sql_manifests: Vec::new(),
            sql_decls: Vec::new(),
            sql_limits: SqlObjectLimits::DEFAULT,
        }
    }

    /// Add declarations built at runtime (tests, per-database overlays). Their addon must be in the
    /// set.
    #[must_use]
    pub fn with_decls(mut self, decls: impl IntoIterator<Item = &'static ModelDecl>) -> Self {
        self.extra.extend(decls);
        self
    }

    /// Add the core declarations of the closed event namespace (A2-D5): each creates a
    /// model whose table lives in [`crate::EVENT_SCHEMA`]. This is the only path that selects the
    /// namespace: addon declarations, linked inventory, overlays and the `ModelDef` mirror have no
    /// way to say it. An event declaration must create its model (no `inherit`, no `inherits`), and
    /// nothing may extend, inherit or reference the model afterwards. Their addon must be in the set.
    #[must_use]
    pub fn with_core_event_decls(
        mut self,
        decls: impl IntoIterator<Item = &'static ModelDecl>,
    ) -> Self {
        self.events.extend(decls);
        self
    }

    /// Apply a per-database overlay: its addon declarations compose with their
    /// addon, its custom (`x_`) models and fields after every addon. Several overlays apply in
    /// call order. Interning and the size limits of the overlay's definitions are checked by
    /// [`RegistryBuilder::build`].
    #[must_use]
    pub fn overlay(mut self, overlay: &Overlay) -> Self {
        self.overlays.push(overlay.clone());
        self
    }

    /// Require the SQL objects `manifests` list: [`RegistryBuilder::build`]
    /// refuses with a `Programming` error naming the object and its addon when an installed
    /// addon's listed key has no declaration, and refuses a declaration its addon's manifest does
    /// not list. A linked declaration is a static nothing calls, so the linker drops it from a
    /// binary that references nothing else of its object file; the expectation lives at the
    /// build site, which is always linked. Manifests of addons outside the set are ignored (one
    /// binary, several addon sets; a misspelled addon name shows only once its declarations link).
    /// Calls add up; the same manifest given twice counts once.
    #[must_use]
    pub fn require_sql_objects(
        mut self,
        manifests: impl IntoIterator<Item = &'static SqlObjectManifest>,
    ) -> Self {
        for manifest in manifests {
            self.sql_manifests.retain(|m| !std::ptr::eq(*m, manifest));
            self.sql_manifests.push(manifest);
        }
        self
    }

    /// Add SQL object declarations built at runtime (tests). Their addon must be in the set; they
    /// are resolved with the linked ones into [`Registry::sql_objects`].
    #[must_use]
    pub fn with_sql_objects(
        mut self,
        decls: impl IntoIterator<Item = &'static SqlObjectDecl>,
    ) -> Self {
        self.sql_decls.extend(decls);
        self
    }

    /// Lower the SQL object admission bounds (default [`SqlObjectLimits::DEFAULT`]); a bound
    /// above its default makes [`RegistryBuilder::build`] fail.
    #[must_use]
    pub fn sql_object_limits(mut self, limits: SqlObjectLimits) -> Self {
        self.sql_limits = limits;
        self
    }

    /// Whether to use the declarations linked into the binary (`MODEL_DECLS` and
    /// `SQL_OBJECT_DECLS`); default true.
    #[must_use]
    pub fn linked(mut self, on: bool) -> Self {
        self.linked = on;
        self
    }

    /// What to do with broken custom fields of the overlays: fail the build
    /// ([`ManualFieldPolicy::Strict`], the default) or leave them out and report them in
    /// [`Registry::dropped_fields`] ([`ManualFieldPolicy::DropBroken`]).
    #[must_use]
    pub fn manual_fields(mut self, policy: ManualFieldPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Compose the registry.
    ///
    /// With [`ManualFieldPolicy::DropBroken`] the registry is composed in rounds: a round that
    /// refuses custom fields drops them from (copies of) the overlays and the next round composes
    /// again, until a round refuses none. The result is the strict composition of the remaining
    /// custom fields, and fields broken only by a dropped one (a related path through it, a
    /// one2many using it as inverse) are dropped by a later round.
    ///
    /// # Errors
    ///
    /// `Programming` or `Validation` for declarations the composer refuses (with
    /// `DropBroken`, everything that is not a refusal of a single custom field: addon
    /// declarations, custom models, a custom field declared by two overlays, …);
    /// `ResourceExhausted` when interning runs out of the overlay account's quota or the
    /// process intern budget.
    pub fn build(self) -> Result<Registry> {
        let mut addons = self.addons.clone();
        if !addons.iter().any(|a| a.name == BASE_ADDON.name) {
            addons.push(&BASE_ADDON);
        }
        if !addons.iter().any(|a| a.name == WEB_ADDON.name) {
            addons.push(&WEB_ADDON);
        }
        if addons.iter().any(|a| a.name == MANUAL_ADDON) {
            return Err(Error::programming(format!(
                "'{MANUAL_ADDON}' names custom declarations of an overlay, not an addon"
            )));
        }
        let order = load_order(&addons)?;
        check_declared_once(&self.overlays)?;
        let mut overlays = self.overlays.clone();
        // each round with drops removes at least one custom field, so this bound is never the
        // reason a build stops; it only makes the loop visibly finite
        let max_rounds = overlays
            .iter()
            .fold(1usize, |n, o| n.saturating_add(o.manual_field_count()));
        let mut dropped: Vec<DroppedField> = Vec::new();
        for _ in 0..max_rounds {
            let mut sink = Sink::new(self.policy);
            let result = self.round(&order, &overlays, &mut sink);
            let drops = sink.into_drops()?;
            if drops.is_empty() {
                let mut registry = result?;
                registry.dropped = dropped;
                registry.sql_objects = crate::sqlobj::graph::resolve(
                    &registry,
                    &self.sql_manifests,
                    &self.sql_decls,
                    self.linked,
                    self.sql_limits,
                )?;
                return Ok(registry);
            }
            overlays = without_dropped(&overlays, drops, &mut dropped)?;
        }
        Err(Error::programming(format!(
            "dropping broken custom fields did not settle after {max_rounds} composition rounds"
        )))
    }

    /// One composition of the addon declarations and `overlays`.
    fn round(
        &self,
        order: &[&'static AddonDescriptor],
        overlays: &[Overlay],
        sink: &mut Sink,
    ) -> Result<Registry> {
        let position: FxHashMap<&str, usize> =
            order.iter().enumerate().map(|(i, a)| (a.name, i)).collect();

        // (addon position, core rank, seq, decl): the core declarations (`base`, `res.users`,
        // `web`) come first inside their addon, whatever the source file of the other
        // declarations of that addon.
        let mut decls: Vec<(usize, u8, usize, &'static ModelDecl)> = Vec::new();
        let mut seq = 0usize;
        let mut push = |decl: &'static ModelDecl, strict: bool| -> Result<()> {
            if decl.custom {
                return Err(Error::programming(format!(
                    "{} is marked custom; custom models come only from an Overlay",
                    decl.describe()
                )));
            }
            let rank = u8::from(
                ![&BASE_DECL, &USERS_DECL, &WEB_DECL]
                    .iter()
                    .any(|core| std::ptr::eq(*core, decl)),
            );
            match position.get(decl.addon) {
                Some(&pos) => {
                    decls.push((pos, rank, seq, decl));
                    seq += 1;
                    Ok(())
                }
                None if strict => Err(Error::programming(format!(
                    "{} belongs to addon '{}', which is not in the addon set",
                    decl.describe(),
                    decl.addon
                ))),
                None => Ok(()),
            }
        };
        push(&BASE_DECL, true)?;
        push(&USERS_DECL, true)?;
        push(&WEB_DECL, true)?;
        if self.linked {
            for decl in crate::linked_decls() {
                push(decl, false)?;
            }
        }
        // the event declarations first: inside an addon they create their model before anything
        // can extend it, so an extension is refused as one, never as a missing model
        for decl in &self.events {
            push(decl, true)?;
        }
        for decl in &self.extra {
            push(decl, true)?;
        }
        let mut manual = Vec::new();
        for overlay in overlays {
            for decl in overlay.addon_decls()? {
                push(decl, true)?;
            }
            manual.extend(overlay.manual_decls()?);
        }
        // Custom models and fields come after every addon, in the overlay's order.
        let after_addons = order.len();
        for decl in manual {
            if decl.addon != MANUAL_ADDON {
                return Err(Error::programming(format!(
                    "{} is not a custom declaration",
                    decl.describe()
                )));
            }
            decls.push((after_addons, 1, seq, decl));
            seq += 1;
        }
        // Load order; inside an addon the root `base` model and its extensions first (every
        // model inherits them), then the core declarations (`res.users`, `web`), then source
        // order (the slice order means nothing).
        decls.sort_by(|a, b| {
            let key = |x: &(usize, u8, usize, &'static ModelDecl)| {
                (x.0, x.3.name != "base", x.1, x.3.file, x.3.line, x.2)
            };
            key(a).cmp(&key(b))
        });
        let decls: Vec<&'static ModelDecl> = decls.into_iter().map(|(_, _, _, d)| d).collect();
        // addons a co-owner may name: the installed set and every addon with linked native code
        let mut known: FxHashSet<&'static str> = order.iter().map(|a| a.name).collect();
        if self.linked {
            known.extend(
                crate::linked_decls()
                    .filter(|d| !d.runtime)
                    .map(|d| d.addon),
            );
        }
        compose(order.to_vec(), &known, &decls, &self.events, sink)
    }
}

/// A delegated alias stores its value on a parent, possibly through several delegates.
/// Inspect that owning column too; checking only the child's local columns would admit Float8.
fn exact_owning_field<'a>(
    models: &'a [ModelSpec],
    mut model: &'a ModelSpec,
    mut field: &'a FieldSpec,
) -> Result<(&'a ModelSpec, &'a FieldSpec)> {
    for _ in 0..=models.len() {
        match field.inherited {
            Some(inherited) => {
                model = &models[inherited.parent.index()];
                field = model.field_by_id(inherited.parent_field);
            }
            None => return Ok((model, field)),
        }
    }
    // relations::finish already rejects delegation cycles. Keep the admission path bounded
    // even if a future composer change violates that invariant.
    Err(Error::programming(
        "Delegated field storage ownership contains a cycle",
    ))
}

/// Refuse a custom field that two overlays declare on the same model, before any round. The
/// composer finds the duplicate too, but only when the first declaration is merged: a broken one
/// (its model gone, its name taken by an addon, its parameters refused) would be dropped once per
/// overlay and the duplicate never reported (review item 3). An error of the overlays, not of one
/// field, so it fails the build in both policies.
///
/// # Errors
///
/// `Programming` naming the first duplicate field.
fn check_declared_once(overlays: &[Overlay]) -> Result<()> {
    if overlays.len() < 2 {
        // one overlay never holds a (model, field) twice (`Overlay::add_manual_field`)
        return Ok(());
    }
    let mut seen: FxHashSet<(&str, &str)> = FxHashSet::default();
    for overlay in overlays {
        for (model, field) in overlay.manual_field_names() {
            if !seen.insert((model, field)) {
                return Err(Error::programming(format!(
                    "custom field '{field}' on model '{model}' is declared twice"
                )));
            }
        }
    }
    Ok(())
}

/// `overlays` without the custom fields of `drops`, which are appended to `dropped` in the order
/// the fields have in the overlays.
///
/// # Errors
///
/// `Programming` when a dropped field is in none of the overlays (the round could not make
/// progress; a composer bug, never a silent loop).
fn without_dropped(
    overlays: &[Overlay],
    drops: Vec<DroppedField>,
    dropped: &mut Vec<DroppedField>,
) -> Result<Vec<Overlay>> {
    let mut position: FxHashMap<(&str, &str), (usize, usize)> = FxHashMap::default();
    for (oi, overlay) in overlays.iter().enumerate() {
        for (fi, (model, field)) in overlay.manual_field_names().enumerate() {
            // unique across overlays (`check_declared_once`), so this is the only place
            position.entry((model, field)).or_insert((oi, fi));
        }
    }
    let mut placed = Vec::with_capacity(drops.len());
    let mut seen: FxHashSet<(&'static str, &'static str)> = FxHashSet::default();
    for drop in drops {
        let Some(&at) = position.get(&(drop.model(), drop.field())) else {
            return Err(Error::programming(format!(
                "custom field '{}' on model '{}' was refused but is in no overlay; {}",
                drop.field(),
                drop.model(),
                drop.error()
            )));
        };
        // a field two checks of one round refused is reported once, with the first reason
        if seen.insert((drop.model(), drop.field())) {
            placed.push((at, drop));
        }
    }
    placed.sort_by_key(|(at, _)| *at);
    let mut remove: Vec<FxHashSet<(&str, &str)>> = vec![FxHashSet::default(); overlays.len()];
    for ((oi, _), drop) in &placed {
        remove[*oi].insert((drop.model(), drop.field()));
    }
    let next = overlays
        .iter()
        .zip(&remove)
        .map(|(overlay, remove)| {
            if remove.is_empty() {
                overlay.clone()
            } else {
                overlay.without_manual_fields(|model, field| remove.contains(&(model, field)))
            }
        })
        .collect();
    dropped.extend(placed.into_iter().map(|(_, drop)| drop));
    Ok(next)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Node {
    Def(usize),
    Class(usize),
}

#[derive(Debug)]
struct Class {
    name: &'static str,
    kind: ModelKind,
    custom: bool,
    /// Created by a core event declaration: the model lives in the event namespace.
    event: bool,
    bases: Vec<Node>,
}

fn add_last(set: &mut Vec<Node>, node: Node) {
    set.retain(|n| *n != node);
    set.push(node);
}

/// Index and constraint names are schema-wide in PostgreSQL (indexes share the relation
/// namespace), and a table object's database name is `<table>_<name>` shortened: two models can
/// produce one name (table `stock` with `_quant_open` and table `stock_quant` with `_open`). Refused
/// here, naming both, instead of one object silently shadowing the other.
fn check_object_names(models: &[ModelSpec]) -> Result<()> {
    #[derive(Clone, PartialEq, Eq)]
    enum GeneratedTarget {
        TablePkey(String),
        TableIdSeq(String),
        FieldIndex(String, &'static str),
        M2mPkey(String, &'static str),
        M2mIdx(String, &'static str),
        Constraint(String, &'static str, &'static str),
    }

    impl GeneratedTarget {
        fn description(&self, model: &str) -> String {
            match self {
                Self::TablePkey(table) => {
                    format!("primary key of model '{model}' on table '{table}'")
                }
                Self::TableIdSeq(table) => {
                    format!("identity sequence of model '{model}' on table '{table}'")
                }
                Self::FieldIndex(table, field) => {
                    format!("field index of '{model}.{field}' on table '{table}'")
                }
                Self::M2mPkey(rel_table, field) => {
                    format!(
                        "primary key of relation table '{rel_table}' for m2m field '{model}.{field}'"
                    )
                }
                Self::M2mIdx(rel_table, field) => {
                    format!("relation index of table '{rel_table}' for m2m field '{model}.{field}'")
                }
                Self::Constraint(table, field, kind) => {
                    format!("{kind} of '{model}.{field}' on table '{table}'")
                }
            }
        }
    }

    let mut owners: FxHashMap<&str, (&str, &str, &str)> = FxHashMap::default();
    let mut generated: FxHashMap<String, Vec<(&str, GeneratedTarget, bool)>> = FxHashMap::default();
    let mut seen_relation_tables: FxHashSet<&str> = FxHashSet::default();

    let mut add_generated = |name: String,
                             model: &'static str,
                             target: GeneratedTarget,
                             is_relation: bool|
     -> Result<()> {
        if is_relation
            && let Some(existing) = generated.get(&name)
            && let Some((other_model, other_target, _)) =
                existing.iter().find(|(.., is_rel)| *is_rel)
            && *other_target != target
        {
            let desc = target.description(model);
            let other_desc = other_target.description(other_model);
            return Err(Error::programming(format!(
                "generated database name '{name}' of model '{model}' ({desc}) clashes with \
                 the {other_desc}; database names of indexes and sequences are schema-wide, \
                 rename one"
            )));
        }
        generated
            .entry(name)
            .or_default()
            .push((model, target, is_relation));
        Ok(())
    };

    for model in models {
        if model.kind == ModelKind::Abstract || model.table.is_empty() {
            continue;
        }
        let table = &model.table;
        let pkey = pg_make_object_name(table, None, "pkey");
        add_generated(
            pkey,
            model.name,
            GeneratedTarget::TablePkey(table.clone()),
            true,
        )?;
        let id_seq = pg_make_object_name(table, Some("id"), "seq");
        add_generated(
            id_seq,
            model.name,
            GeneratedTarget::TableIdSeq(table.clone()),
            true,
        )?;
        for field in &model.fields {
            if field.index.is_some() {
                let idx = if model.parent_path == Some(field.id) {
                    crate::ident::index_name(table, "parent_path_c")
                } else {
                    crate::ident::index_name(table, field.name)
                };
                add_generated(
                    idx,
                    model.name,
                    GeneratedTarget::FieldIndex(table.clone(), field.name),
                    true,
                )?;
            }
            if matches!(field.ty, FieldType::Many2one { .. }) {
                let fkey = shorten_identifier(&format!("{table}_{}_fkey", field.name));
                add_generated(
                    fkey,
                    model.name,
                    GeneratedTarget::Constraint(table.clone(), field.name, "foreign key"),
                    false,
                )?;
            }
            if field.required {
                let nn = shorten_identifier(&format!("{table}_{}_not_null", field.name));
                add_generated(
                    nn,
                    model.name,
                    GeneratedTarget::Constraint(table.clone(), field.name, "not-null constraint"),
                    false,
                )?;
            }
            if let FieldType::Many2many {
                relation: Some(rel),
                ..
            } = &field.ty
                && seen_relation_tables.insert(&rel.table)
            {
                let rel_pkey = pg_make_object_name(&rel.table, None, "pkey");
                add_generated(
                    rel_pkey,
                    model.name,
                    GeneratedTarget::M2mPkey(rel.table.clone(), field.name),
                    true,
                )?;
                let rel_idx =
                    crate::ident::relation_index_name(&rel.table, &rel.column2, &rel.column1);
                add_generated(
                    rel_idx,
                    model.name,
                    GeneratedTarget::M2mIdx(rel.table.clone(), field.name),
                    true,
                )?;
            }
        }
    }

    for model in models {
        for object in &model.table_objects {
            if matches!(
                object.kind,
                TableObjectKind::SignalTrigger(_) | TableObjectKind::FollowDelete(_)
            ) {
                continue;
            }
            let here = (model.name, object.name, model.table.as_str());
            match owners.get(object.full_name.as_str()) {
                Some(&(other, other_name, other_table)) if other != model.name => {
                    return Err(Error::programming(format!(
                        "table object name '{}' is produced by both model '{other}' ('{}' on table \
                         '{}') and model '{}' ('{}' on table '{}'); database names of indexes and \
                         constraints are schema-wide, rename one",
                        object.full_name.escape_debug(),
                        other_name.escape_debug(),
                        other_table,
                        here.0,
                        here.1.escape_debug(),
                        here.2
                    )));
                }
                _ => {
                    owners.insert(object.full_name.as_str(), here);
                }
            }
            if let Some(clashes) = generated.get(object.full_name.as_str())
                && let Some((other_model, other_target, _)) =
                    clashes.iter().find(|(m, _, rel)| *m != model.name || *rel)
            {
                let desc = other_target.description(other_model);
                return Err(Error::programming(format!(
                    "table object name '{}' of model '{}' ('{}' on table '{}') clashes with \
                     the {desc}; database names of indexes and constraints are schema-wide, \
                     rename one",
                    object.full_name.escape_debug(),
                    model.name,
                    object.name.escape_debug(),
                    model.table
                )));
            }
        }
    }
    Ok(())
}

fn compose(
    addons: Vec<&'static AddonDescriptor>,
    known: &FxHashSet<&'static str>,
    decls: &[&'static ModelDecl],
    events: &[&'static ModelDecl],
    sink: &mut Sink,
) -> Result<Registry> {
    let is_event = |decl: &ModelDecl| events.iter().any(|e| std::ptr::eq(*e, decl));
    let mut classes: Vec<Class> = Vec::new();
    let mut class_of: FxHashMap<&'static str, usize> = FxHashMap::default();

    for (di, decl) in decls.iter().enumerate() {
        let mut parents: Vec<&'static str> = decl.inherit.to_vec();
        if decl.name != "base" {
            parents.push("base");
        }
        let ci = if parents.contains(&decl.name) {
            let Some(&ci) = class_of.get(decl.name) else {
                let missing = || {
                    Error::programming(format!("Model '{}' does not exist in registry.", decl.name))
                };
                if decl.addon != MANUAL_ADDON
                    || decl.fields.is_empty()
                    || decl.company_access.is_some()
                {
                    return Err(missing());
                }
                for fd in decl.fields {
                    sink.refuse(
                        true,
                        decl.name,
                        fd.name,
                        DropReason::UnknownModel,
                        missing(),
                    )?;
                }
                continue;
            };
            if is_event(decl) {
                return Err(Error::programming(format!(
                    "{} is in the closed event namespace and must create its model, not extend it",
                    decl.describe()
                )));
            }
            if classes[ci].event {
                let error = Error::programming(format!(
                    "{} extends '{}', which is in the closed event namespace; its owner's \
                     declaration is the only one it takes",
                    decl.describe(),
                    decl.name
                ));
                // a custom field of an overlay is refused like any broken custom field
                if decl.addon == MANUAL_ADDON
                    && !decl.fields.is_empty()
                    && decl.company_access.is_none()
                {
                    for fd in decl.fields {
                        sink.refuse(
                            true,
                            decl.name,
                            fd.name,
                            DropReason::Refused,
                            Error::programming(error.message().to_owned()),
                        )?;
                    }
                    continue;
                }
                return Err(error);
            }
            check_extension(&classes[ci], decl)?;
            ci
        } else {
            if decl.kind_follows_model {
                return Err(Error::programming(format!(
                    "{} defines model '{}' with no kind of its own; only an extension takes the \
                     kind of its model",
                    decl.describe(),
                    decl.name
                )));
            }
            if class_of.contains_key(decl.name) {
                return Err(Error::programming(format!(
                    "{} defines model '{}', which already exists; extend it with inherit = ['{}']",
                    decl.describe(),
                    decl.name,
                    decl.name
                )));
            }
            let event = is_event(decl);
            if event && (!decl.inherit.is_empty() || !decl.inherits.is_empty()) {
                return Err(Error::programming(format!(
                    "{} is in the closed event namespace and must create its model: it takes no \
                     inherit or inherits",
                    decl.describe()
                )));
            }
            classes.push(Class {
                name: decl.name,
                kind: decl.kind,
                custom: decl.custom,
                event,
                bases: Vec::new(),
            });
            class_of.insert(decl.name, classes.len() - 1);
            classes.len() - 1
        };
        let mut bases = vec![Node::Def(di)];
        for parent in parents {
            let Some(&pi) = class_of.get(parent) else {
                return Err(Error::programming(format!(
                    "Model '{}' inherits from non-existing model '{parent}'.",
                    decl.name
                )));
            };
            if parent != decl.name && classes[pi].event {
                return Err(Error::programming(format!(
                    "{} inherits from '{parent}', which is in the closed event namespace",
                    decl.describe()
                )));
            }
            if parent == decl.name {
                for base in classes[ci].bases.clone() {
                    add_last(&mut bases, base);
                }
            } else {
                if classes[ci].kind == ModelKind::Abstract
                    && classes[pi].kind != ModelKind::Abstract
                {
                    return Err(Error::programming(format!(
                        "In {}, abstract model '{}' cannot inherit from non-abstract model '{parent}'.",
                        decl.describe(),
                        decl.name
                    )));
                }
                add_last(&mut bases, Node::Class(pi));
            }
        }
        classes[ci].bases = bases;
    }

    let mut memo: FxHashMap<usize, Vec<Node>> = FxHashMap::default();
    let mut models = Vec::with_capacity(classes.len());
    let mut by_name = FxHashMap::default();
    for ci in 0..classes.len() {
        let mro: Vec<&'static ModelDecl> = linearize(Node::Class(ci), &classes, &mut memo)?
            .into_iter()
            .filter_map(|n| match n {
                Node::Def(d) => Some(decls[d]),
                Node::Class(_) => None,
            })
            .collect();
        let id = ModelId(ci as u32);
        let spec = build_model(id, &classes[ci], &mro, decls, sink)?;
        by_name.insert(spec.name, id);
        models.push(spec);
    }

    for model in &models {
        for field in &model.fields {
            if let Some(comodel) = field.ty.comodel()
                && !by_name.contains_key(comodel)
            {
                sink.field(
                    &model.mro,
                    field,
                    DropReason::UnknownComodel {
                        comodel: comodel.to_owned(),
                    },
                    Error::programming(format!(
                        "Field {}.{} refers to unknown model '{comodel}'",
                        model.name, field.name
                    )),
                )?;
            }
        }
    }
    // A custom model must not take the table of another model (`x_a.b` and an addon's `x_a_b`).
    let mut tables: FxHashMap<&str, &str> = FxHashMap::default();
    for model in models.iter().filter(|m| m.has_table()) {
        if let Some(other) = tables.insert(model.table.as_str(), model.name)
            && (model.custom || models.iter().any(|m| m.name == other && m.custom))
        {
            return Err(Error::validation(format!(
                "models '{other}' and '{}' share the table '{}'; a custom model needs a table of \
                 its own",
                model.name, model.table
            )));
        }
    }
    // A model declared at runtime (model file, overlay) needs a table of its own: models sharing
    // a table would share its constraints, and a runtime object could replace a native one
    // (review S1).
    for decl in decls
        .iter()
        .filter(|d| d.runtime && !d.inherit.contains(&d.name))
    {
        let Some(model) = models.iter().find(|m| m.name == decl.name) else {
            continue;
        };
        if let Some(other) = models
            .iter()
            .find(|m| m.name != model.name && m.table == model.table)
        {
            let other_at = decls
                .iter()
                .find(|d| d.name == other.name && !d.inherit.contains(&d.name))
                .map_or_else(
                    || format!("addon declarations of '{}'", other.name),
                    |d| format!("{}:{}", d.file, d.line),
                );
            return Err(Error::programming(format!(
                "model '{}' (addon '{}', {}:{}) declared at runtime takes the table '{}' of model \
                 '{}' ({other_at}); a model declared at runtime needs a table of its own",
                decl.name, decl.addon, decl.file, decl.line, model.table, other.name
            )));
        }
    }
    crate::relations::finish(&mut models, &by_name, sink)?;
    let valid_currency_fields: Vec<FxHashSet<&'static str>> = models
        .iter()
        .map(|model| {
            model
                .fields
                .iter()
                .filter(|field| monetary_currency_relation(&models, &by_name, model.id, field.id))
                .map(|field| field.name)
                .collect()
        })
        .collect();
    // Delegated currency fields exist only after relational setup.
    for model in &mut models {
        let names: FxHashSet<&'static str> = model.fields.iter().map(|f| f.name).collect();
        let valid_currency = &valid_currency_fields[model.id.index()];
        for f in &mut model.fields {
            let FieldType::Monetary { currency_field } = &mut f.ty else {
                continue;
            };
            if currency_field.is_none() {
                *currency_field = ["currency_id", "x_currency_id"]
                    .into_iter()
                    .find(|c| names.contains(c))
                    .map(str::to_owned);
            }
            if let Some(currency) = currency_field.as_deref().filter(|c| names.contains(c)) {
                if !valid_currency.contains(currency) {
                    let error = Error::programming(format!(
                        "Field {}.{} currency_field {} must be a many2one to res.currency",
                        model.name, f.name, currency,
                    ));
                    sink.field(&model.mro, f, DropReason::Refused, error)?;
                }
                continue;
            }
            let error = Error::programming(format!(
                "Field {}.{} with unknown currency_field {}",
                model.name,
                f.name,
                currency_field.as_deref().unwrap_or("None")
            ));
            sink.field(&model.mro, f, DropReason::Refused, error)?;
        }
    }
    crate::generated::finish(&mut models)?;
    check_id_sequences(&models)?;
    crate::keys::finish(&mut models)?;
    crate::company::finish(&mut models, &by_name)?;
    // after the scopes: declarations name the company field of the scope
    crate::company_access::finish(&mut models, &by_name)?;
    // after every pass: delegated fields are resolved, so `inherited` is final
    check_event_field_names(&models)?;
    // The model's defining addon protects the whole model. An exact-only extension
    // protects only fields it declares; unrelated base columns keep their original policy.
    // Protection is monotone across later redeclarations (lead scope decision 2026-11-06).
    for model in &models {
        let is_exact = |decl: &ModelDecl| {
            addons
                .iter()
                .any(|addon| addon.name == decl.addon && addon.is_exact_only())
        };
        let model_owner = model
            .mro
            .iter()
            .copied()
            .find(|decl| decl.name == model.name && !decl.is_extension() && is_exact(decl));
        for field in &model.fields {
            let owner = model_owner.or_else(|| {
                model.mro.iter().copied().find(|decl| {
                    is_exact(decl)
                        && decl
                            .fields
                            .iter()
                            .any(|declared| declared.name == field.name)
                })
            });
            if let Some(owner) = owner {
                let (physical_model, physical_field) = exact_owning_field(&models, model, field)?;
                if physical_field.column() != Some(ColumnType::Float8) {
                    continue;
                }
                let declaring = physical_model
                    .mro
                    .iter()
                    .copied()
                    .find(|decl| {
                        decl.fields
                            .iter()
                            .any(|declared| declared.name == physical_field.name)
                    })
                    .ok_or_else(|| {
                        Error::programming(format!(
                            "Binary-float column '{}.{}' has no declaring model definition",
                            physical_model.name, physical_field.name
                        ))
                    })?;
                let error = Error::programming(format!(
                    "Exact-only addon '{}' refuses binary-float column '{}.{}' (protecting {}; declaring {}; physical column '{}.{}'); declare numeric digits instead.",
                    owner.addon,
                    model.name,
                    field.name,
                    owner.describe(),
                    declaring.describe(),
                    physical_model.name,
                    physical_field.name
                ));
                // Report on the physical owning field: the inherited copy of a delegated
                // parent field has no overlay declaration of its own and goes with its parent.
                sink.field(
                    &physical_model.mro,
                    physical_field,
                    DropReason::Refused,
                    error,
                )?;
            }
        }
    }
    // after relational setup: guarded relation tables have their final names
    crate::owner_fields::check(&addons, known, decls, &models, sink)?;
    check_object_names(&models)?;
    crate::drop::check_custom_related(&models, &by_name, sink)?;
    // no field may reference an event model, and it has no relational field (A2-D5)
    crate::event::check_references(&models, sink)?;
    // no field may reference a partitioned model
    crate::partition::check_references(&models, sink)?;
    // keys and exclusions of a partitioned model (design §7.1)
    crate::partition::check_objects(&models)?;

    let statement_cache = crate::statement_cache::StatementCache::new(models.len());
    Ok(Registry {
        generation: GENERATION.fetch_add(1, Ordering::Relaxed),
        addons,
        models,
        by_name,
        // set by `RegistryBuilder::build` once a round drops nothing
        dropped: Vec::new(),
        statement_cache,
        // set by `RegistryBuilder::build` once composition succeeded
        sql_objects: crate::sqlobj::graph::SqlObjectGraph::default(),
    })
}

fn check_extension(class: &Class, decl: &ModelDecl) -> Result<()> {
    let def = decl.describe();
    let name = class.name;
    if decl.table.is_some() {
        return Err(Error::programming(format!(
            "{def} cannot redefine _table while extending model '{name}'. \
             That class should either remove '_table', or set a different '_name'."
        )));
    }
    // Custom fields, and runtime extensions stating no kind, take the kind of the model.
    if decl.kind_follows_model {
        return Ok(());
    }
    if class.kind == ModelKind::Abstract && decl.kind != ModelKind::Abstract {
        return Err(Error::programming(format!(
            "{def} transforms the abstract model '{name}' into a non-abstract model. \
             That class should either inherit from AbstractModel, or set a different '_name'."
        )));
    }
    let class_transient = class.kind == ModelKind::Transient;
    let decl_transient = decl.kind == ModelKind::Transient;
    if class_transient && !decl_transient {
        return Err(Error::programming(format!(
            "{def} transforms the transient model '{name}' into a non-transient model. \
             That class should either inherit from TransientModel, or set a different '_name'."
        )));
    }
    if !class_transient && decl_transient {
        return Err(Error::programming(format!(
            "{def} transforms the model '{name}' into a transient model. \
             That class should either inherit from Model, or set a different '_name'."
        )));
    }
    Ok(())
}

/// C3 linearization (Python's MRO) over definitions and model classes.
fn linearize(
    node: Node,
    classes: &[Class],
    memo: &mut FxHashMap<usize, Vec<Node>>,
) -> Result<Vec<Node>> {
    let ci = match node {
        Node::Def(_) => return Ok(vec![node]),
        Node::Class(ci) => ci,
    };
    if let Some(done) = memo.get(&ci) {
        return Ok(done.clone());
    }
    let bases = classes[ci].bases.clone();
    let mut seqs: Vec<Vec<Node>> = Vec::with_capacity(bases.len() + 1);
    for base in &bases {
        seqs.push(linearize(*base, classes, memo)?);
    }
    seqs.push(bases);
    let mut result = vec![node];
    loop {
        seqs.retain(|s| !s.is_empty());
        if seqs.is_empty() {
            break;
        }
        let head = seqs
            .iter()
            .map(|s| s[0])
            .find(|h| !seqs.iter().any(|s| s[1..].contains(h)))
            .ok_or_else(|| {
                Error::programming(format!(
                    "Cannot create a consistent method resolution order for model '{}'",
                    classes[ci].name
                ))
            })?;
        result.push(head);
        for s in &mut seqs {
            if s.first() == Some(&head) {
                s.remove(0);
            }
        }
    }
    memo.insert(ci, result.clone());
    Ok(result)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct IndexSource {
    addon: &'static str,
    file: &'static str,
    line: u32,
    kind: Option<IndexKind>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NativeIndex {
    Declared(IndexSource),
    M2oDefault(IndexSource),
}

impl NativeIndex {
    fn source(&self) -> &IndexSource {
        match self {
            Self::Declared(s) | Self::M2oDefault(s) => s,
        }
    }

    fn kind(&self) -> Option<IndexKind> {
        self.source().kind
    }
}

pub(crate) fn index_kind_str(kind: Option<IndexKind>) -> &'static str {
    match kind {
        Some(k) => k.as_str(),
        None => "none",
    }
}

/// A field being merged across declarations (oldest first).
#[derive(Debug, Clone)]
struct Merged {
    params: FieldDecl,
    selection: Option<Selection>,
    modules: Vec<&'static str>,
    magic: bool,
    manual: bool,
    native: Option<NativeIndex>,
}

#[derive(Debug, Clone, Default)]
struct Selection {
    values: Option<Vec<(String, String)>>,
    method: Option<String>,
    ondelete: FxHashMap<String, SelectionOnDeleteDecl>,
    new_values: Vec<String>,
}

fn same_kind(a: &KindDecl, b: &KindDecl) -> bool {
    std::mem::discriminant(a) == std::mem::discriminant(b)
}

fn native_index_for(fd: &FieldDecl, decl: &'static ModelDecl, magic: bool) -> Option<NativeIndex> {
    if decl.runtime {
        None
    } else if fd.index.is_some() {
        Some(NativeIndex::Declared(IndexSource {
            addon: decl.addon,
            file: decl.file,
            line: decl.line,
            kind: fd.index.flatten(),
        }))
    } else if matches!(fd.kind, KindDecl::Many2one { .. }) && !magic {
        Some(NativeIndex::M2oDefault(IndexSource {
            addon: decl.addon,
            file: decl.file,
            line: decl.line,
            kind: Some(IndexKind::Btree),
        }))
    } else {
        None
    }
}

impl Merged {
    fn start(model: &str, fd: &FieldDecl, decl: &'static ModelDecl, magic: bool) -> Result<Self> {
        let native = native_index_for(fd, decl, magic);
        let mut merged = Self {
            params: FieldDecl::new(fd.name, fd.kind),
            selection: None,
            modules: Vec::new(),
            magic,
            manual: false,
            native,
        };
        merged.apply(model, fd, decl)?;
        Ok(merged)
    }

    fn is_stored(&self) -> bool {
        self.params
            .store
            .unwrap_or_else(|| self.params.compute.is_none() && self.params.related.is_none())
    }

    fn apply(&mut self, model: &str, fd: &FieldDecl, decl: &'static ModelDecl) -> Result<()> {
        let addon = decl.addon;
        if !same_kind(&self.params.kind, &fd.kind) {
            // M2: Refuse runtime type change on a natively indexed field.
            if decl.runtime {
                let native = self.native.filter(|n| match n {
                    NativeIndex::Declared(_) => true,
                    NativeIndex::M2oDefault(_) => self.is_stored(),
                });
                if let Some(source) = native {
                    let s = source.source();
                    let kind_str = index_kind_str(source.kind());
                    return Err(Error::programming(format!(
                        "field '{model}.{}' has index kind '{kind_str}' declared by native addon '{}' \
                         ({}:{}); a runtime extension cannot change the type of a natively indexed \
                         field",
                        fd.name, s.addon, s.file, s.line
                    )));
                }
            }
            // A type change discards everything collected so far.
            // M1(b): A native type change resets and recomputes the default native origin.
            let modules = std::mem::take(&mut self.modules);
            // a type change keeps `secret` (D2)
            let secret = self.params.secret;
            let native = if decl.runtime {
                self.native
            } else {
                native_index_for(fd, decl, self.magic)
            };
            *self = Self {
                params: FieldDecl::new(fd.name, fd.kind),
                selection: None,
                modules,
                magic: self.magic,
                manual: self.manual,
                native,
            };
            self.params.secret = secret;
        }
        if addon == MANUAL_ADDON {
            self.manual = true;
        } else {
            self.modules.push(addon);
        }
        // M1(a): A runtime extension cannot remove a native index by making the field non-stored.
        if decl.runtime && self.is_stored() {
            let would_be_stored = match fd.store {
                Some(s) => s,
                None => {
                    let comp = fd.compute.is_some() || self.params.compute.is_some();
                    let rel = fd.related.is_some() || self.params.related.is_some();
                    self.params.store.unwrap_or(!comp && !rel)
                }
            };
            if !would_be_stored {
                let native = self.native.filter(|n| match n {
                    NativeIndex::Declared(s) => s.kind.is_some(),
                    NativeIndex::M2oDefault(_) => true,
                });
                if let Some(source) = native {
                    let s = source.source();
                    let kind_str = index_kind_str(source.kind());
                    return Err(Error::programming(format!(
                        "field '{model}.{}' has index kind '{kind_str}' declared by native addon '{}' \
                         ({}:{}); a runtime extension declared by addon '{}' ({}:{}) \
                         cannot make a natively indexed field non-stored via store, compute, or related",
                        fd.name, s.addon, s.file, s.line, decl.addon, decl.file, decl.line
                    )));
                }
            }
        }
        if let Some(new_index) = fd.index {
            let new_kind = new_index;
            if decl.runtime {
                let violated = self.native.filter(|native| match native {
                    NativeIndex::Declared(s) => new_kind != s.kind,
                    NativeIndex::M2oDefault(s) => self.is_stored() && new_kind != s.kind,
                });
                if let Some(prev) = violated {
                    let s = prev.source();
                    return Err(Error::programming(format!(
                        "field '{model}.{}' has index kind '{}' declared by native addon '{}' \
                         ({}:{}) and index kind '{}' declared at runtime by \
                         addon '{}' ({}:{}); a model file or overlay cannot change the index \
                         kind of a native field",
                        fd.name,
                        index_kind_str(s.kind),
                        s.addon,
                        s.file,
                        s.line,
                        index_kind_str(new_kind),
                        decl.addon,
                        decl.file,
                        decl.line
                    )));
                }
            } else {
                self.native = Some(NativeIndex::Declared(IndexSource {
                    addon: decl.addon,
                    file: decl.file,
                    line: decl.line,
                    kind: new_kind,
                }));
            }
        }
        let prev_gen =
            self.params.generated.is_some() && !matches!(self.params.generated, Some(None));
        let new_gen = match fd.generated {
            Some(Some(_)) => true,
            Some(None) => {
                return Err(Error::programming(format!(
                    "Invalid generated field '{model}.{}': generation cannot be removed",
                    fd.name
                )));
            }
            None => prev_gen,
        };

        if !new_gen {
            if fd.generated_storage.is_some() {
                return Err(Error::programming(format!(
                    "Invalid generated field '{model}.{}': attribute 'generated_storage' conflicts with generation",
                    fd.name
                )));
            }
        } else {
            if fd.readonly == Some(false) {
                return Err(Error::programming(format!(
                    "Invalid generated field '{model}.{}': attribute 'readonly' conflicts with generation",
                    fd.name
                )));
            }
            if fd.copy == Some(true) {
                return Err(Error::programming(format!(
                    "Invalid generated field '{model}.{}': attribute 'copy' conflicts with generation",
                    fd.name
                )));
            }
            if fd.store == Some(false) {
                return Err(Error::programming(format!(
                    "Invalid generated field '{model}.{}': attribute 'store' conflicts with generation",
                    fd.name
                )));
            }
            if fd.required == Some(true) {
                return Err(Error::programming(format!(
                    "Invalid generated field '{model}.{}': required is not supported",
                    fd.name
                )));
            }
            if fd.default.is_some() {
                return Err(Error::programming(format!(
                    "Invalid generated field '{model}.{}': attribute 'default' conflicts with generation",
                    fd.name
                )));
            }
            if fd.compute.is_some() {
                return Err(Error::programming(format!(
                    "Invalid generated field '{model}.{}': attribute 'compute' conflicts with generation",
                    fd.name
                )));
            }
            if fd.related.is_some() {
                return Err(Error::programming(format!(
                    "Invalid generated field '{model}.{}': attribute 'related' conflicts with generation",
                    fd.name
                )));
            }
            if fd.inverse.is_some() {
                return Err(Error::programming(format!(
                    "Invalid generated field '{model}.{}': attribute 'inverse' conflicts with generation",
                    fd.name
                )));
            }
            if fd.precompute.is_some() {
                return Err(Error::programming(format!(
                    "Invalid generated field '{model}.{}': attribute 'precompute' conflicts with generation",
                    fd.name
                )));
            }
            if fd.compute_sudo.is_some() {
                return Err(Error::programming(format!(
                    "Invalid generated field '{model}.{}': attribute 'compute_sudo' conflicts with generation",
                    fd.name
                )));
            }
            if fd.depends.is_some() {
                return Err(Error::programming(format!(
                    "Invalid generated field '{model}.{}': attribute 'depends' conflicts with generation",
                    fd.name
                )));
            }
            if fd.depends_context.is_some() {
                return Err(Error::programming(format!(
                    "Invalid generated field '{model}.{}': attribute 'depends_context' conflicts with generation",
                    fd.name
                )));
            }

            if let Some(Some(expr)) = fd.generated {
                self.params.generated = Some(Some(expr));
                if fd.generated_storage.is_some() {
                    self.params.generated_storage = fd.generated_storage;
                } else if self.params.generated_storage.is_none() {
                    self.params.generated_storage =
                        Some(crate::generated::GeneratedStorage::Virtual);
                }
            } else if prev_gen && fd.generated_storage.is_some() {
                self.params.generated_storage = fd.generated_storage;
            }
        }

        let p = &mut self.params;
        // `secret` only ever turns on: an extension that asks for `secret = false` after an earlier
        // declaration set it is refused, never ignored (delta D2)
        if fd.secret == Some(false) && p.secret == Some(true) {
            let earlier = self
                .modules
                .iter()
                .filter(|m| **m != addon)
                .copied()
                .collect::<Vec<_>>()
                .join("', '");
            return Err(Error::programming(format!(
                "field '{model}.{}' is declared secret by addon(s) '{earlier}'; addon '{addon}' \
                 cannot switch `secret` off (audit redaction)",
                fd.name
            )));
        }
        if fd.secret.is_some() {
            p.secret = fd.secret;
        }
        macro_rules! take {
            ($($attr:ident),*) => {$( if fd.$attr.is_some() { p.$attr = fd.$attr; } )*};
        }
        take!(
            string,
            help,
            required,
            readonly,
            store,
            copy,
            index,
            default,
            prefetch,
            compute,
            search_method,
            inverse,
            related,
            precompute,
            compute_sudo,
            groups,
            depends,
            reads,
            depends_context,
            domain,
            context,
            check_company,
            bypass_search_access,
            delegate,
            server_set,
            company_dependent,
            change_default,
            user_writeable,
            tracking,
            translate,
            db_default,
            html_policy
        );
        p.kind = match (p.kind, fd.kind) {
            (KindDecl::Char { size }, KindDecl::Char { size: new }) => {
                KindDecl::Char { size: new.or(size) }
            }
            (KindDecl::Float { digits }, KindDecl::Float { digits: new }) => KindDecl::Float {
                digits: new.or(digits),
            },
            (
                KindDecl::Monetary { currency_field },
                KindDecl::Monetary {
                    currency_field: new,
                },
            ) => KindDecl::Monetary {
                currency_field: new.or(currency_field),
            },
            (
                KindDecl::Many2one { ondelete, .. },
                KindDecl::Many2one {
                    comodel,
                    ondelete: new,
                },
            ) => KindDecl::Many2one {
                comodel,
                ondelete: new.or(ondelete),
            },
            (
                KindDecl::Many2many {
                    relation,
                    column1,
                    column2,
                    ondelete,
                    ..
                },
                KindDecl::Many2many {
                    comodel,
                    relation: new_relation,
                    column1: new_column1,
                    column2: new_column2,
                    ondelete: new_ondelete,
                },
            ) => KindDecl::Many2many {
                comodel,
                relation: new_relation.or(relation),
                column1: new_column1.or(column1),
                column2: new_column2.or(column2),
                ondelete: new_ondelete.or(ondelete),
            },
            (
                KindDecl::Reference { models, method },
                KindDecl::Reference {
                    models: new_models,
                    method: new_method,
                },
            ) => {
                if new_models.is_some() || new_method.is_some() {
                    KindDecl::Reference {
                        models: new_models,
                        method: new_method,
                    }
                } else {
                    KindDecl::Reference { models, method }
                }
            }
            (_, other) => other,
        };
        if let KindDecl::Selection {
            values,
            method,
            add,
            ondelete,
        } = fd.kind
        {
            let sel = self.selection.get_or_insert_with(Selection::default);
            if let Some(list) = values {
                sel.values = Some(
                    list.iter()
                        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                        .collect(),
                );
                sel.method = None;
            } else if let Some(m) = method {
                sel.values = None;
                sel.method = Some(m.to_owned());
            }
            if !add.is_empty() {
                let Some(current) = sel.values.as_mut() else {
                    return Err(Error::programming(format!(
                        "{model}.{}: selection_add={add:?} on non-list selection {:?}",
                        fd.name, sel.method
                    )));
                };
                let existing_keys: FxHashSet<&str> =
                    current.iter().map(|(k, _)| k.as_str()).collect();

                for &(key, pol) in ondelete {
                    if let SelectionOnDeleteDecl::Callback(name) = pol
                        && name.is_empty()
                    {
                        return Err(Error::programming(format!(
                            "{model}.{}: selection callback name cannot be empty",
                            fd.name
                        )));
                    }
                    sel.ondelete.insert(key.to_owned(), pol);
                }

                // Check every added value: if new, register it and default to set null if not given
                for &(val, _) in add {
                    if !existing_keys.contains(val) && !sel.new_values.iter().any(|v| v == val) {
                        sel.new_values.push(val.to_owned());
                        sel.ondelete
                            .entry(val.to_owned())
                            .or_insert(SelectionOnDeleteDecl::SetNull);
                    }
                }
                merge_selection(current, add);
            }
        }
        Ok(())
    }
}

fn merge_selection(
    values: &mut Vec<(String, String)>,
    add: &[(&'static str, Option<&'static str>)],
) {
    let mut keys: Vec<String> = values.iter().map(|(k, _)| k.clone()).collect();
    let mut deps: Vec<(String, Vec<String>)> = Vec::new();
    let mut dep_index: FxHashMap<String, usize> = FxHashMap::default();
    let mut record = |seq: &[String]| {
        let mut prev: Option<&String> = None;
        for item in seq {
            let idx = *dep_index.entry(item.clone()).or_insert_with(|| {
                deps.push((item.clone(), Vec::new()));
                deps.len() - 1
            });
            if let Some(p) = prev {
                deps[idx].1.push(p.clone());
            }
            prev = Some(item);
        }
    };
    record(&keys);
    let added: Vec<String> = add.iter().map(|(k, _)| (*k).to_owned()).collect();
    record(&added);

    let mut result: Vec<String> = Vec::new();
    let mut visited: FxHashMap<String, ()> = FxHashMap::default();
    fn visit(
        n: &str,
        deps: &[(String, Vec<String>)],
        index: &FxHashMap<String, usize>,
        visited: &mut FxHashMap<String, ()>,
        result: &mut Vec<String>,
    ) {
        if visited.insert(n.to_owned(), ()).is_some() {
            return;
        }
        if let Some(&i) = index.get(n) {
            for d in &deps[i].1 {
                visit(d, deps, index, visited, result);
            }
            result.push(n.to_owned());
        }
    }
    for (n, _) in &deps {
        visit(n, &deps, &dep_index, &mut visited, &mut result);
    }
    keys.clone_from(&result);

    let mut labels: FxHashMap<String, String> = values.drain(..).collect();
    for (k, label) in add {
        if let Some(label) = label {
            labels.insert((*k).to_owned(), (*label).to_owned());
        }
    }
    *values = keys
        .into_iter()
        .map(|k| {
            let label = labels.remove(&k).unwrap_or_default();
            (k, label)
        })
        .collect();
}

fn magic_applies(decl: &ModelDecl) -> bool {
    !decl.is_extension() && decl.kind != ModelKind::Abstract && decl.log_access.unwrap_or(true)
}

/// The field declarations `decl` contributes in merge order: its own fields, then the audit
/// fields it adds (`true`), then the `row_version` field when it is the declaration that turned
/// the row version on.
pub(crate) fn decl_fields(
    decl: &'static ModelDecl,
    row_version_origin: Option<&'static ModelDecl>,
) -> impl Iterator<Item = (&'static FieldDecl, bool)> {
    let magic: &'static [FieldDecl] = if magic_applies(decl) {
        MAGIC_FIELDS
    } else {
        &[]
    };
    let version: &'static [FieldDecl] = match row_version_origin {
        Some(origin) if std::ptr::eq(origin, decl) => crate::keys::ROW_VERSION_FIELDS,
        _ => &[],
    };
    decl.fields
        .iter()
        .map(|fd| (fd, false))
        .chain(
            magic
                .iter()
                .filter(move |fd| !decl.fields.iter().any(|own| own.name == fd.name))
                .map(|fd| (fd, true)),
        )
        .chain(version.iter().map(|fd| (fd, false)))
}

/// Merge and resolve one field from `entries` (in merge order, as `build_model` meets them)
/// without the registry-wide passes: what the field would be if only these declarations existed.
/// `None` when `entries` is empty.
///
/// # Errors
///
/// What merging or resolving the declarations refuses.
pub(crate) fn merge_declarations(
    model: &str,
    entries: impl IntoIterator<Item = (&'static FieldDecl, &'static ModelDecl, bool)>,
) -> Result<Option<FieldSpec>> {
    let mut merged: Option<Merged> = None;
    for (fd, decl, magic) in entries {
        match &mut merged {
            Some(m) => m.apply(model, fd, decl)?,
            None => merged = Some(Merged::start(model, fd, decl, magic)?),
        }
    }
    merged
        .map(|m| resolve_field(model, FieldId(0), m))
        .transpose()
}

fn build_model(
    id: ModelId,
    class: &Class,
    mro: &[&'static ModelDecl],
    decls: &[&'static ModelDecl],
    sink: &mut Sink,
) -> Result<ModelSpec> {
    let name = class.name;
    // Event capture is monotonic across the MRO, unlike newest-wins model attributes. Keep the
    // opting-in source for diagnostics: an extension can be the declaration that enables it.
    let eventful_origin = mro.iter().find(|decl| decl.eventful);
    let eventful = eventful_origin.is_some();
    // Read auditing also composes by OR, including abstract and transient declarations.
    let audit_reads = mro.iter().any(|decl| decl.audit_reads);
    if let Some(origin) = eventful_origin {
        let reason = match class.kind {
            ModelKind::Abstract => Some("abstract models do not support event capture"),
            ModelKind::Transient => Some("transient models do not support event capture"),
            ModelKind::Regular if class.event => {
                Some("the closed event namespace does not support event capture")
            }
            ModelKind::Regular => None,
        };
        if let Some(reason) = reason {
            return Err(Error::validation(format!(
                "model '{name}' of addon '{}' cannot be eventful: {reason} (a later release)",
                origin.addon,
            )));
        }
    }

    // Attributes taken only from direct definition bases, oldest first (MODEL §4.4 step 6).
    let mut description = name.to_owned();
    let mut table = name.replace('.', "_");
    let mut log_access = class.kind != ModelKind::Abstract;
    for base in class.bases.iter().rev() {
        if let Node::Def(d) = base {
            let decl = decls[*d];
            if let Some(text) = decl.description {
                description = text.to_owned();
            }
            if let Some(t) = decl.table {
                table = t.to_owned();
            }
            if let Some(on) = decl.log_access {
                log_access = on;
            }
        }
    }
    // Class attributes resolved through the MRO (newest first wins).
    let order = mro.iter().find_map(|d| d.order).unwrap_or("id");
    let rec_name = mro.iter().find_map(|d| d.rec_name);
    let rec_names_search = mro
        .iter()
        .find_map(|d| d.rec_names_search)
        .map(<[&'static str]>::to_vec);
    let active_name = mro.iter().find_map(|d| d.active_name);
    let check_company_auto = mro
        .iter()
        .find_map(|d| d.check_company_auto)
        .unwrap_or(false);
    let allow_sudo_commands = mro
        .iter()
        .find_map(|d| d.allow_sudo_commands)
        .unwrap_or(true);
    let inherits_access = mro.iter().find_map(|d| d.inherits_access).unwrap_or(true);
    // transient cleanup thresholds: newest declaration first per attribute
    let transient_policy = crate::transient_policy::resolve(name, class.kind, mro)?;
    // newest declaration first, like `_order`
    let fillfactor = match mro.iter().find(|d| d.fillfactor.is_some()) {
        Some(decl) => match decl.fillfactor {
            Some(ff @ 10..=100) => Some(ff),
            other => {
                return Err(Error::programming(format!(
                    "{} sets fillfactor {} on model '{name}': PostgreSQL accepts 10 to 100 \
                     (percent)",
                    decl.describe(),
                    other.unwrap_or_default()
                )));
            }
        },
        None => None,
    };
    // the model's own declarations only: `_inherit` from another model does not carry it, each
    // table opts
    let mut id_sequence: Option<(&'static str, &ModelDecl)> = None;
    for decl in mro.iter().filter(|d| d.name == name) {
        let Some(seq) = decl.id_sequence else {
            continue;
        };
        match id_sequence {
            Some((other, by)) if other != seq => {
                return Err(Error::programming(format!(
                    "{} sets id_sequence '{seq}' on model '{name}', but {} sets '{other}': the \
                     declarations of a model must name the same id sequence",
                    decl.describe(),
                    by.describe()
                )));
            }
            Some(_) => {}
            None => id_sequence = Some((seq, decl)),
        }
    }
    if let Some((seq, decl)) = id_sequence {
        if !is_valid_pg_name(seq) || seq.len() > MAX_IDENTIFIER_LEN {
            return Err(Error::programming(format!(
                "{} sets id_sequence '{seq}' on model '{name}': a sequence name is a PostgreSQL \
                 identifier of letters, digits, '_' and '$' (not starting with a digit), at most \
                 63 bytes",
                decl.describe()
            )));
        }
        if class.kind == ModelKind::Abstract {
            return Err(Error::programming(format!(
                "{} sets id_sequence '{seq}' on the abstract model '{name}': an abstract model \
                 has no table; set it on each model with a table",
                decl.describe()
            )));
        }
    }
    let id_sequence = id_sequence.map(|(seq, _)| seq);

    let mut inherit: Vec<&'static str> = Vec::new();
    for decl in mro.iter().filter(|d| d.name == name) {
        for parent in decl.inherit {
            if *parent != name && *parent != "base" && !inherit.contains(parent) {
                inherit.push(parent);
            }
        }
    }

    // Fields, merged oldest first.
    let mut order_names: Vec<&'static str> = Vec::new();
    let mut merged: FxHashMap<&'static str, Merged> = FxHashMap::default();
    // custom fields refused while merging: a later custom declaration of the same name (a model
    // inheriting another under a new name) is still a second declaration (review item 3). The
    // error is fatal for the build: dropping the first declaration in the next round would hide it.
    let mut refused_custom: FxHashSet<&'static str> = FxHashSet::default();
    let declared_twice = |sink: &mut Sink, field: &str| {
        sink.fatal(Error::programming(format!(
            "custom field '{field}' on model '{name}' is declared twice"
        )))
    };
    // the declaration that turns the row version on adds its field
    let row_version_origin = crate::keys::row_version_origin(name, mro)?;
    for decl in mro.iter().rev() {
        for (fd, is_magic) in decl_fields(decl, row_version_origin) {
            if decl.addon == MANUAL_ADDON && refused_custom.contains(fd.name) {
                return Err(declared_twice(sink, fd.name));
            }
            match merged.get_mut(fd.name) {
                Some(m) if decl.addon == MANUAL_ADDON => {
                    if m.manual {
                        return Err(declared_twice(sink, fd.name));
                    }
                    // the addon field wins (a newer addon version took the name)
                    sink.refuse(
                        true,
                        decl.name,
                        fd.name,
                        DropReason::DeclaredByAddon {
                            addons: m.modules.clone(),
                        },
                        Error::programming(format!(
                            "custom field '{}' on model '{name}' clashes with a field declared \
                             by an addon; custom fields only add new 'x_' fields",
                            fd.name
                        )),
                    )?;
                    refused_custom.insert(fd.name);
                }
                Some(m) => m.apply(name, fd, decl)?,
                None => match Merged::start(name, fd, decl, is_magic) {
                    Ok(started) => {
                        merged.insert(fd.name, started);
                        order_names.push(fd.name);
                    }
                    Err(error) => {
                        let custom = decl.addon == MANUAL_ADDON;
                        sink.refuse(custom, decl.name, fd.name, DropReason::Refused, error)?;
                        refused_custom.insert(fd.name);
                    }
                },
            }
        }
    }

    let mut fields = Vec::with_capacity(order_names.len());
    let mut by_name = FxHashMap::default();
    for fname in &order_names {
        let Some(m) = merged.remove(fname) else {
            continue;
        };
        let custom = m.manual && m.modules.is_empty();
        let fid = FieldId(fields.len() as u32);
        match resolve_field(name, fid, m) {
            Ok(field) => {
                fields.push(field);
                by_name.insert(*fname, fid);
            }
            Err(error) => {
                // reported on the model whose custom declaration has the field
                let declaring = mro
                    .iter()
                    .find(|d| d.addon == MANUAL_ADDON && d.fields.iter().any(|f| f.name == *fname))
                    .map(|d| d.name);
                match declaring {
                    Some(model) if custom => {
                        sink.refuse(true, model, fname, DropReason::Refused, error)?;
                    }
                    Some(_) | None => return Err(error),
                }
            }
        }
    }

    // Monetary currency field.
    for f in &mut fields {
        let FieldType::Monetary { currency_field } = &mut f.ty else {
            continue;
        };
        if currency_field.is_none() {
            *currency_field = ["currency_id", "x_currency_id"]
                .into_iter()
                .find(|c| by_name.contains_key(c))
                .map(str::to_owned);
        }
        if currency_field
            .as_deref()
            .is_some_and(|c| by_name.contains_key(c))
        {
            continue;
        }
        // Delegated fields are materialized by relations::finish. Preserve a parent's
        // resolved choice before it is copied, but defer a child's inherited lookup.
        if mro.iter().any(|decl| !decl.inherits.is_empty()) {
            continue;
        }
        let error = Error::programming(format!(
            "Field {name}.{} with unknown currency_field {}",
            f.name,
            currency_field.as_deref().unwrap_or("None")
        ));
        sink.field(mro, f, DropReason::Refused, error)?;
    }

    let rec_name = match rec_name {
        Some(n) => Some(by_name.get(n).copied().ok_or_else(|| {
            Error::programming(format!("Invalid _rec_name='{n}' for model '{name}'"))
        })?),
        // custom models fall back to `x_name` (e)
        None => by_name
            .get("name")
            .or_else(|| by_name.get("x_name").filter(|_| class.custom))
            .copied(),
    };
    let active_name = match active_name {
        Some(n) => {
            if !matches!(n, "active" | "x_active") || !by_name.contains_key(n) {
                return Err(Error::programming(format!(
                    "Invalid _active_name='{n}' for model '{name}'; only 'active' and 'x_active' \
                     are supported and the field must be present on the model"
                )));
            }
            by_name.get(n).copied()
        }
        None => by_name
            .get("active")
            .or_else(|| by_name.get("x_active"))
            .copied(),
    };

    // Table objects, oldest first, keyed by full name (later declarations win). A runtime
    // declaration (model file, overlay) never replaces an object of a native (Rust) declaration,
    // and a redeclaration may not change the kind of the object (review S1).
    let mut table_objects: Vec<TableObject> = Vec::new();
    // who declared each object last: (runtime, addon, file, line), parallel to `table_objects`
    let mut object_sources: Vec<(bool, &'static str, &'static str, u32)> = Vec::new();
    for decl in mro.iter().rev() {
        // full names this declaration has put already: a name repeated inside one declaration is a
        // mistake (the second would silently replace the first); across declarations it replaces
        let mut declared: Vec<String> = Vec::new();
        for obj in decl.table_objects {
            let Some(suffix) = obj.name.strip_prefix('_') else {
                return Err(Error::programming(format!(
                    "Names of SQL objects in a model must start with '_' (got '{}' on model '{name}')",
                    obj.name
                )));
            };
            if decl.runtime && (!is_valid_pg_name(obj.name) || obj.name.len() > MAX_IDENTIFIER_LEN)
            {
                return Err(Error::programming(format!(
                    "Names of SQL objects declared at runtime can only contain characters, digits \
                     and underscores (up to 63): '{}' on model '{name}' (addon '{}', {}:{})",
                    obj.name.escape_debug(),
                    decl.addon,
                    decl.file,
                    decl.line
                )));
            }
            let full_name = shorten_identifier(&format!("{table}_{suffix}"));
            if declared.contains(&full_name) {
                return Err(Error::programming(format!(
                    "SQL object '{}' of model '{name}' is declared twice by addon '{}' ({}:{}) \
                     (database name '{full_name}')",
                    obj.name.escape_debug(),
                    decl.addon,
                    decl.file,
                    decl.line
                )));
            }
            declared.push(full_name.clone());
            let source = (decl.runtime, decl.addon, decl.file, decl.line);
            match table_objects.iter().position(|o| o.full_name == full_name) {
                Some(i) => {
                    let (was_runtime, addon, file, line) = object_sources[i];
                    if std::mem::discriminant(&table_objects[i].kind)
                        != std::mem::discriminant(&obj.kind)
                    {
                        return Err(Error::programming(format!(
                            "SQL object '{full_name}' of model '{name}' declared by addon '{}' \
                             ({}:{}) has a different kind than the one declared by addon \
                             '{addon}' ({file}:{line}); a redeclaration keeps the kind",
                            decl.addon, decl.file, decl.line
                        )));
                    }
                    if was_runtime != decl.runtime {
                        let (native, native_at, runtime, runtime_at) = if was_runtime {
                            (decl.addon, (decl.file, decl.line), addon, (file, line))
                        } else {
                            (addon, (file, line), decl.addon, (decl.file, decl.line))
                        };
                        return Err(Error::programming(format!(
                            "SQL object '{full_name}' of model '{name}' is declared by Rust code \
                             in addon '{native}' ({}:{}) and again at runtime in addon '{runtime}' \
                             ({}:{}); a model file or overlay cannot replace a native object, \
                             choose another name",
                            native_at.0, native_at.1, runtime_at.0, runtime_at.1
                        )));
                    }
                    table_objects[i].kind = obj.kind;
                    table_objects[i].name = obj.name;
                    table_objects[i].runtime = decl.runtime;
                    object_sources[i] = source;
                }
                None => {
                    table_objects.push(TableObject {
                        name: obj.name,
                        full_name,
                        kind: obj.kind,
                        runtime: decl.runtime,
                    });
                    object_sources.push(source);
                }
            }
        }
    }
    crate::typed_index::check_typed_indexes(name, &table, &fields, &by_name, &table_objects)?;

    if !is_valid_pg_name(&table) {
        return Err(Error::validation(format!(
            "Invalid characters in table name '{table}'"
        )));
    }
    // leaf partitioning, from the declaration that creates the model
    let partition = crate::partition::resolve(&crate::partition::Subject {
        name,
        kind: class.kind,
        mro,
        table: &table,
        fields: &fields,
        fillfactor,
        namespace: if class.event {
            crate::TableNamespace::Event
        } else {
            crate::TableNamespace::Current
        },
    })?;
    // a native timestamptz is generated by the server only in the closed event namespace; the
    // general mechanism for public models has no slice that adds it
    if !class.event
        && let Some(f) = fields
            .iter()
            .find(|f| f.server_set && matches!(f.ty, FieldType::TimestampTz))
    {
        return Err(Error::programming(format!(
            "field '{name}.{}': a native timestamptz server_set is admitted only in the closed \
             event namespace (a later release); server_set on public models has no slice that adds it",
            f.name
        )));
    }
    if let (Some(seq), Some(_)) = (id_sequence, &partition) {
        return Err(Error::programming(format!(
            "model '{name}' is partitioned and sets id_sequence '{seq}': a partitioned table keeps \
             its own key shape"
        )));
    }

    // A runtime object never takes a name the ORM or PostgreSQL generates (keys, not-null
    // constraints, field indexes): schema sync would treat the generated object as its own.
    for (o, source) in table_objects.iter().zip(&object_sources) {
        if !source.0 {
            continue;
        }
        let generated = ["_pkey", "_fkey", "_not_null"]
            .iter()
            .any(|suffix| o.full_name.ends_with(suffix))
            || o.full_name == shorten_identifier(&format!("{table}_pkey"))
            || fields.iter().any(|f| {
                o.full_name == crate::ident::index_name(&table, f.name)
                    || o.full_name == shorten_identifier(&format!("{table}_{}_fkey", f.name))
                    || o.full_name == shorten_identifier(&format!("{table}_{}_not_null", f.name))
            });
        if generated {
            return Err(Error::programming(format!(
                "SQL object '{}' of model '{name}' (addon '{}', {}:{}) has a name the ORM or \
                 PostgreSQL generates for a primary key, foreign key, not-null constraint or \
                 field index; choose another name",
                o.full_name, source.1, source.2, source.3
            )));
        }
    }
    // Column names follow the same rule as table names (they become identifiers).
    for f in &fields {
        if !is_valid_pg_name(f.name) || f.name.len() > MAX_IDENTIFIER_LEN {
            return Err(Error::validation(format!(
                "Field names can only contain characters, digits and underscores (up to 63): \
                 '{}' on model '{name}'",
                f.name
            )));
        }
    }
    // Texts that end up in SQL comments or messages may not contain NUL bytes.
    let mut texts: Vec<&str> = vec![name, &description];
    for f in &fields {
        texts.push(&f.string);
        texts.extend(f.help);
        if let FieldType::Selection {
            values: Some(v), ..
        } = &f.ty
        {
            texts.extend(v.iter().flat_map(|(k, l)| [k.as_str(), l.as_str()]));
        }
    }
    for o in &table_objects {
        texts.push(&o.full_name);
        match o.kind {
            TableObjectKind::Constraint {
                definition,
                message,
            }
            | TableObjectKind::UniqueIndex {
                definition,
                message,
            } => texts.extend([definition, message]),
            TableObjectKind::Index { definition } => texts.push(definition),
            // field names are checked against the model's fields; the filter is compiled by the ORM
            TableObjectKind::TypedIndex(index) => texts.extend(index.filter),
            TableObjectKind::Key(key) => {
                texts.push(key.message);
                texts.extend(key.fields.iter().copied());
            }
            // the field name is checked against the model's many2many fields once relations
            // are settled (`relations::signal_trigger_rules`)
            TableObjectKind::SignalTrigger(_) => {}
            // the names are checked once relations are settled (`relations::follow_delete_rules`)
            TableObjectKind::FollowDelete(decl) => {
                texts.push(decl.cache);
                texts.extend(decl.key_table);
            }
            TableObjectKind::Exclusion(decl) => {
                texts.extend(decl.equal);
                texts.extend([decl.period.0, decl.period.1, decl.message]);
                texts.extend(decl.filter);
            }
            TableObjectKind::TemporalKey(decl) => {
                texts.extend(decl.fields);
                texts.extend([decl.range, decl.message]);
            }
        }
    }
    if let Some(bad) = texts.iter().find(|t| t.contains('\0')) {
        return Err(Error::programming(format!(
            "model '{name}' declares a text with a NUL byte: {bad:?}"
        )));
    }
    if table.len() > MAX_IDENTIFIER_LEN {
        return Err(Error::validation(format!(
            "Table name '{table}' is too long"
        )));
    }
    // typed exclusions and temporal keys name fields of this table
    crate::exclusion::check_model(name, &table_objects, &fields, &by_name)?;
    if class.kind == ModelKind::Transient && !log_access {
        return Err(Error::programming(
            "TransientModels must have log_access turned on, in order to implement their vacuum \
             policy",
        ));
    }

    if !mro
        .iter()
        .any(|d| d.field_parameters.contains(&"user_writeable"))
        && let Some(f) = fields.iter().find(|f| f.user_writeable)
    {
        return Err(Error::programming(format!(
            "Field {name}.{} sets user_writeable, which the model does not allow \
             (declare field_parameters = [\"user_writeable\"] on it)",
            f.name
        )));
    }
    let prefetch_fields = fields.iter().filter(|f| f.prefetch).map(|f| f.id).collect();
    let mut tracked: Vec<&FieldSpec> = fields.iter().filter(|f| f.tracking.is_some()).collect();
    if tracked.len() > MAX_TRACKED_FIELDS {
        return Err(Error::programming(format!(
            "model '{name}' tracks {} fields, more than the {MAX_TRACKED_FIELDS} a model may track",
            tracked.len()
        )));
    }
    tracked.sort_by_key(|f| (f.tracking, f.name));
    let tracked: Vec<FieldId> = tracked.into_iter().map(|f| f.id).collect();
    let row_version =
        row_version_origin.and_then(|_| by_name.get(crate::keys::ROW_VERSION).copied());
    let quoted_table = crate::ident::quote_arc(&table);
    let namespace = if class.event {
        crate::TableNamespace::Event
    } else {
        crate::TableNamespace::Current
    };
    // each identifier is quoted on its own: `"erp_event"."table"`, never one quoted `"a.b"`
    let sql_table = match namespace {
        crate::TableNamespace::Current => quoted_table.clone(),
        crate::TableNamespace::Event => std::sync::Arc::from(format!(
            "{}.{}",
            crate::ident::quote(crate::EVENT_SCHEMA),
            quoted_table
        )),
    };
    Ok(ModelSpec {
        id,
        name,
        description,
        table,
        quoted_table,
        namespace,
        sql_table,
        kind: class.kind,
        custom: class.custom,
        log_access,
        transient_policy,
        order,
        rec_name,
        rec_names_search,
        active_name,
        inherit,
        mro: mro.to_vec(),
        fields,
        by_name,
        table_objects,
        prefetch_fields,
        // resolved by `relations::finish`
        inherits: Vec::new(),
        check_company_auto,
        // resolved by `company::finish`
        company_scope: None,
        company_delegation: None,
        // resolved by `company_access::finish`; until then the refusing state
        company_access: crate::company_access::CompanyAccessState::Undeclared,
        allow_sudo_commands,
        inherits_access,
        parent_name: None,
        parent_path: None,
        fillfactor,
        id_sequence,
        partition,
        row_version,
        tracked,
        eventful,
        audit_reads,
        // checked and filled by `keys::finish`
        keys: Vec::new(),
    })
}

/// The most field names one outbox event may carry (capture limits). The registry refuses an eventful
/// model whose own columns could not fit in one event, and `erp-orm`'s `Envelope::new` uses the same
/// number as its run-time backstop.
pub const MAX_EVENT_FIELD_NAMES: usize = 128;

/// Refuses an eventful model with more columns than one event can name.
///
/// The ceiling is the set of fields with a column in the model's own table, `id` excluded:
/// a create lists the given column fields plus the insert-time columns (`create_date`, `write_date`,
/// server-set fields, `row_version`), and a write lists the changed column fields. The
/// `inherited.is_none()` and `name != "id"` filters are defensive: a delegated copy has no column
/// of its own (its column lives in the parent table) and `id` has no column type, so today neither
/// changes the count; they keep the rule right if either ever does.
///
/// # Errors
///
/// `Validation` naming the model, the opting-in addon, the count and the limit.
fn check_event_field_names(models: &[ModelSpec]) -> Result<()> {
    for model in models.iter().filter(|m| m.eventful) {
        let count = model
            .fields
            .iter()
            .filter(|f| f.has_column() && f.inherited.is_none() && f.name != "id")
            .count();
        if count > MAX_EVENT_FIELD_NAMES {
            // same rule as `build_model`: the first declaration in the MRO that opts in
            let addon = model
                .mro
                .iter()
                .find(|decl| decl.eventful)
                .map(|decl| decl.addon)
                .ok_or_else(|| {
                    Error::programming(format!(
                        "eventful model '{}' has no opting-in declaration in its MRO",
                        model.name
                    ))
                })?;
            return Err(Error::validation(format!(
                "model '{}' of addon '{addon}' cannot be eventful: {count} fields with a column exceed the {MAX_EVENT_FIELD_NAMES} field names one event may carry (a later release)",
                model.name,
            )));
        }
    }
    Ok(())
}

/// The most tracked fields one model may have. The flush `UPDATE` returns the row id plus the old
/// and the new value of every tracked column it writes in its `RETURNING` list, and PostgreSQL
/// limits a target list to 1,664 entries (`MaxTupleAttributeNumber`): 800 tracked columns use
/// `1 + 2 * 800 = 1,601` entries, which leaves room for the other returned values (this is a
/// target-list limit, not a limit on bound parameters).
pub const MAX_TRACKED_FIELDS: usize = 800;

fn resolve_tracking(
    model: &str,
    name: &str,
    declared: Option<Option<u32>>,
    ty: &FieldType,
    store: bool,
    p: &FieldDecl,
) -> Result<Option<u32>> {
    let Some(Some(sequence)) = declared else {
        return Ok(None);
    };
    if sequence == 0 {
        return Err(Error::programming(format!(
            "field '{model}.{name}': tracking = 0 is refused (the reference ERP reads it as untracked in one \
             place and as tracked in another): use tracking = false to switch tracking off, or a \
             positive sequence"
        )));
    }
    if matches!(ty, FieldType::One2many { .. }) {
        return Err(Error::programming(format!(
            "field '{model}.{name}': tracking is not supported on a one2many field (it has no \
             value of its own: track the many2one of its lines)"
        )));
    }
    if !store {
        return Err(Error::programming(format!(
            "field '{model}.{name}': tracking needs a stored field (this field is not stored)"
        )));
    }
    if matches!(p.generated, Some(Some(_))) {
        return Err(Error::programming(format!(
            "field '{model}.{name}': tracking a database-generated field is not supported (the \
             database computes its value, no write of the record captures it)"
        )));
    }
    if p.compute.is_some() || p.related.is_some() {
        return Err(Error::programming(format!(
            "field '{model}.{name}': tracking a computed field is not supported yet \
             (a later release tracks fields written by write, create and upsert; tracking \
             recomputed fields belongs to a later slice)"
        )));
    }
    if p.translate == Some(true) || p.company_dependent == Some(true) {
        return Err(Error::programming(format!(
            "field '{model}.{name}': tracking a translated or company-dependent field is not \
             supported yet (a later release tracks plain stored fields; resolving the \
             language or company belongs to a later slice)"
        )));
    }
    Ok(Some(sequence))
}

fn resolve_field(model: &str, id: FieldId, merged: Merged) -> Result<FieldSpec> {
    let p = merged.params;
    let name = p.name;
    let mut copy_default = !matches!(p.kind, KindDecl::Id | KindDecl::One2many { .. });
    if name == "state" {
        copy_default = false;
    }
    let is_generated = matches!(p.generated, Some(Some(_)));
    let (store, readonly, copy, compute_sudo);
    if is_generated {
        // The per-declaration checks in `apply` see one `fd` at a time; a base `compute`,
        // `default` or `required` plus an extension's `.generated(..)` only meets here, in the
        // merged parameters.
        let conflict = if p.readonly == Some(false) {
            Some("attribute 'readonly' conflicts with generation")
        } else if p.copy == Some(true) {
            Some("attribute 'copy' conflicts with generation")
        } else if p.store == Some(false) {
            Some("attribute 'store' conflicts with generation")
        } else if p.required == Some(true) {
            Some("required is not supported")
        } else if p
            .default
            .as_ref()
            .is_some_and(|d| !matches!(d, DefaultDecl::Remove))
        {
            Some("attribute 'default' conflicts with generation")
        } else if p.compute.is_some() {
            Some("attribute 'compute' conflicts with generation")
        } else if p.related.is_some() {
            Some("attribute 'related' conflicts with generation")
        } else if p.inverse.is_some() {
            Some("attribute 'inverse' conflicts with generation")
        } else if p.precompute.is_some() {
            Some("attribute 'precompute' conflicts with generation")
        } else if p.compute_sudo.is_some() {
            Some("attribute 'compute_sudo' conflicts with generation")
        } else if p.depends.is_some() {
            Some("attribute 'depends' conflicts with generation")
        } else if p.depends_context.is_some() {
            Some("attribute 'depends_context' conflicts with generation")
        } else {
            None
        };
        if let Some(reason) = conflict {
            return Err(Error::programming(format!(
                "Invalid generated field '{model}.{name}': {reason}"
            )));
        }
        store = true;
        readonly = true;
        copy = false;
        compute_sudo = false;
        if !matches!(
            p.kind,
            KindDecl::Boolean
                | KindDecl::Integer
                | KindDecl::Float { .. }
                | KindDecl::Monetary { .. }
                | KindDecl::Char { .. }
                | KindDecl::Text
        ) {
            return Err(Error::programming(format!(
                "Invalid generated field '{model}.{name}': unsupported expression"
            )));
        }
    } else if p.related.is_some() {
        store = p.store.unwrap_or(false);
        compute_sudo = p.compute_sudo.unwrap_or(true);
        copy = p.copy.unwrap_or(false);
        readonly = p.readonly.unwrap_or(true);
    } else if p.compute.is_some() {
        store = p.store.unwrap_or(false);
        compute_sudo = p.compute_sudo.unwrap_or(store);
        copy = if store && p.readonly == Some(false) {
            p.copy.unwrap_or(copy_default)
        } else {
            p.copy.unwrap_or(false)
        };
        readonly = p.readonly.unwrap_or(p.inverse.is_none());
    } else {
        store = p.store.unwrap_or(true);
        compute_sudo = p.compute_sudo.unwrap_or(false);
        copy = p.copy.unwrap_or(copy_default);
        readonly = p.readonly.unwrap_or(matches!(p.kind, KindDecl::Id));
    }
    // filled by the ORM at insert, never by a caller: readonly and not copied
    let server_set = p.server_set.unwrap_or(false);
    if server_set
        && !(matches!(p.kind, KindDecl::Datetime | KindDecl::TimestampTz)
            && store
            && p.compute.is_none()
            && p.related.is_none()
            && matches!(p.default, None | Some(DefaultDecl::Remove)))
    {
        return Err(Error::programming(format!(
            "field '{model}.{name}': server_set is for stored datetime fields without a compute, \
             related path or default"
        )));
    }
    let readonly = readonly || server_set;
    let copy = copy && !server_set;
    let required = p.required.unwrap_or(false);
    // Asserting it on any other field is an error, never dropped; an
    // opt-out there has nothing to opt out of and is dropped.
    let computed_and_stored = (p.compute.is_some() || p.related.is_some()) && store;
    if p.precompute == Some(true) && !computed_and_stored {
        return Err(Error::programming(format!(
            "field '{model}.{name}': precompute = true is for stored computed or related fields; \
             this field is {}",
            if p.compute.is_some() || p.related.is_some() {
                "not stored"
            } else {
                "not computed"
            }
        )));
    }
    let precompute = p.precompute.filter(|_| computed_and_stored);
    if store && p.search_method.is_some() {
        return Err(Error::programming(format!(
            "field '{model}.{name}': search_method is only for nonstored fields"
        )));
    }

    let mut selection_ondelete = FxHashMap::default();
    let ty = match p.kind {
        KindDecl::Id => FieldType::Id,
        KindDecl::Boolean => FieldType::Boolean,
        KindDecl::Integer => FieldType::Integer,
        KindDecl::Float { digits } => FieldType::Float { digits },
        KindDecl::Monetary { currency_field } => FieldType::Monetary {
            currency_field: currency_field.map(str::to_owned),
        },
        KindDecl::Char { size } => FieldType::Char { size },
        KindDecl::Text => FieldType::Text,
        KindDecl::Html => FieldType::Html,
        KindDecl::Selection { .. } => {
            let sel = merged.selection.unwrap_or_default();
            if sel.values.is_none() && sel.method.is_none() && p.related.is_none() {
                return Err(Error::programming(format!(
                    "{model}.{name}: selection is mandatory unless the field is related or extended"
                )));
            }
            let has_default = matches!(
                p.default,
                Some(DefaultDecl::Value(_)) | Some(DefaultDecl::Fn(_))
            );
            let existing_keys: FxHashSet<&str> = match &sel.values {
                Some(vs) => vs.iter().map(|(k, _)| k.as_str()).collect(),
                None => FxHashSet::default(),
            };

            for pol in sel.ondelete.values() {
                match pol {
                    SelectionOnDeleteDecl::SetDefault if !has_default => {
                        return Err(Error::programming(format!(
                            "{model}.{name}: ondelete policy of type 'set default' is invalid for this field as it does not define a default! Either define one in the base field, or change the chosen ondelete policy"
                        )));
                    }
                    SelectionOnDeleteDecl::SetValue(target) if !existing_keys.contains(target) => {
                        return Err(Error::programming(format!(
                            "{model}.{name}: ondelete policy of type 'set %' must be either 'set null', 'set default', or 'set value' where value is a valid selection value."
                        )));
                    }
                    SelectionOnDeleteDecl::SetNull if required => {
                        return Err(Error::programming(format!(
                            "{model}.{name}: required selection fields must define an ondelete policy that implements the proper cleanup of the corresponding records upon module uninstallation. Please use one or more of the following policies: 'set default' (if the field has a default defined), 'cascade', or a single-argument callable where the argument is the recordset containing the specified option."
                        )));
                    }
                    _ => {}
                }
            }

            for val in &sel.new_values {
                let policy = sel
                    .ondelete
                    .get(val)
                    .copied()
                    .unwrap_or(SelectionOnDeleteDecl::SetNull);
                if required && policy == SelectionOnDeleteDecl::SetNull {
                    return Err(Error::programming(format!(
                        "{model}.{name}: required selection fields must define an ondelete policy that implements the proper cleanup of the corresponding records upon module uninstallation. Please use one or more of the following policies: 'set default' (if the field has a default defined), 'cascade', or a single-argument callable where the argument is the recordset containing the specified option."
                    )));
                }
            }

            for (k, pol) in sel.ondelete {
                let owned = match pol {
                    SelectionOnDeleteDecl::SetNull => erp_fields::SelectionOnDelete::SetNull,
                    SelectionOnDeleteDecl::Cascade => erp_fields::SelectionOnDelete::Cascade,
                    SelectionOnDeleteDecl::SetDefault => erp_fields::SelectionOnDelete::SetDefault,
                    SelectionOnDeleteDecl::SetValue(v) => {
                        erp_fields::SelectionOnDelete::SetValue((*v).to_owned())
                    }
                    SelectionOnDeleteDecl::Callback(c) => {
                        erp_fields::SelectionOnDelete::Callback((*c).to_owned())
                    }
                };
                selection_ondelete.insert(k, owned);
            }

            FieldType::Selection {
                values: sel.values,
                method: sel.method,
            }
        }
        KindDecl::Date => FieldType::Date,
        KindDecl::Datetime => FieldType::Datetime,
        KindDecl::Uuid => FieldType::Uuid,
        KindDecl::Xid8 => FieldType::Xid8,
        KindDecl::TimestampTz => FieldType::TimestampTz,
        KindDecl::Json => FieldType::Json,
        // the default `ondelete` depends on the model kinds: settled by `relations::finish`
        KindDecl::Many2one { comodel, ondelete } => FieldType::Many2one {
            comodel: comodel.to_owned(),
            ondelete: ondelete.unwrap_or(if required {
                OnDelete::Restrict
            } else {
                OnDelete::SetNull
            }),
        },
        KindDecl::One2many { comodel, inverse } => FieldType::One2many {
            comodel: comodel.to_owned(),
            inverse: inverse.to_owned(),
        },
        // the relation table is named by `relations::finish`, once every table is known
        KindDecl::Many2many {
            comodel, ondelete, ..
        } => FieldType::Many2many {
            comodel: comodel.to_owned(),
            relation: None,
            ondelete: ondelete.unwrap_or(OnDelete::Cascade),
        },
        KindDecl::Reference { models, method } => FieldType::Reference {
            models: models.map(|list| {
                list.iter()
                    .map(|(m, l)| ((*m).to_owned(), (*l).to_owned()))
                    .collect()
            }),
            method: method.map(str::to_owned),
        },
        KindDecl::Many2oneReference { model_field } => FieldType::Many2oneReference {
            model_field: model_field.to_owned(),
        },
        KindDecl::RowVersion => FieldType::RowVersion,
        KindDecl::DateRange { bounds } => FieldType::DateRange { bounds },
        KindDecl::DatetimeRange { bounds } => FieldType::DatetimeRange { bounds },
    };
    let tracking = resolve_tracking(model, name, p.tracking, &ty, store, &p)?;
    let html_policy = if matches!(ty, FieldType::Html) {
        Some(
            p.html_policy
                .unwrap_or(erp_fields::html::FieldPolicy::Sanitize(
                    erp_fields::html::SanitizePolicy::Default(
                        erp_fields::html::DefaultPolicy::new(),
                    ),
                )),
        )
    } else {
        if p.html_policy.is_some() {
            return Err(Error::validation(format!(
                "field '{model}.{name}': HTML policy requires an Html field"
            )));
        }
        None
    };
    let translate = p.translate.unwrap_or(false);
    if translate {
        if !ty.supports_translation() {
            return Err(Error::programming(format!(
                "field '{model}.{name}': only Char, Text, and Html fields can be translated (this field is {})",
                ty.type_name()
            )));
        }
        if p.company_dependent == Some(true) {
            return Err(Error::programming(format!(
                "field '{model}.{name}': a field cannot be both translated and company-dependent"
            )));
        }
    }
    if p.company_dependent == Some(true) {
        let supported = matches!(
            ty,
            FieldType::Char { .. }
                | FieldType::Text
                | FieldType::Html
                | FieldType::Boolean
                | FieldType::Integer
                | FieldType::Float { .. }
                | FieldType::Monetary { .. }
                | FieldType::Selection { .. }
                | FieldType::Many2one { .. }
                | FieldType::Date
                | FieldType::Datetime
        );
        if !supported
            || !store
            || p.compute.is_some()
            || p.inverse.is_some()
            || p.related.is_some()
            || is_generated
            || tracking.is_some()
        {
            return Err(Error::programming(format!(
                "field '{model}.{name}': this company-dependent declaration is not supported until a later release"
            )));
        }
    }
    let column = if store {
        if p.company_dependent == Some(true) || translate {
            Some(ColumnType::Jsonb)
        } else {
            ty.column_type()
        }
    } else {
        None
    };
    let index = match p.index {
        Some(explicit) => explicit,
        // Many2one columns are indexed by default, except the audit fields.
        None if matches!(ty, FieldType::Many2one { .. }) && !merged.magic => Some(IndexKind::Btree),
        None => None,
    }
    .filter(|_| column.is_some());
    if let Some(Some(kind)) = p.index
        && column.is_none()
        && kind.field_types().is_some()
    {
        return Err(Error::programming(format!(
            "field '{model}.{name}': index = \"{}\" needs a stored field (this field has no \
             column)",
            kind.as_str()
        )));
    }
    if let Some(kind) = index {
        crate::typed_index::check_field_index(
            model,
            name,
            kind,
            &ty,
            p.company_dependent.unwrap_or(false),
            translate,
        )?;
    }
    let prefetch = p.prefetch.unwrap_or(true) && column.is_some();
    // Native literal defaults need a reviewed ORM/schema-backfill contract. Until
    // then they must not compose successfully while sql_default omits their fill.
    // Explicit null/removal and runtime default functions retain their own policy.
    if matches!(
        ty,
        FieldType::Uuid | FieldType::Xid8 | FieldType::TimestampTz
    ) && matches!(
        p.default,
        Some(DefaultDecl::Value(value))
            if !matches!(value, StaticValue::False | StaticValue::Bool(false))
    ) {
        return Err(Error::programming(format!(
            "field '{model}.{name}': literal defaults for native {} fields require the \
             future native static-default slice; no schema backfill default is inferred",
            ty.type_name()
        )));
    }
    let default = match p.default {
        None | Some(DefaultDecl::Remove) => None,
        Some(DefaultDecl::Value(v)) => Some(FieldDefault::Value(v)),
        Some(DefaultDecl::Fn(f)) => Some(FieldDefault::Fn(f)),
    };
    // a persistent column default only where schema sync can state it exactly and the
    // ORM always writes the column itself (a required field with a static default)
    let db_default = p.db_default.unwrap_or(false);
    if db_default {
        let why = if !matches!(ty, FieldType::Boolean) {
            Some(format!("is {}", ty.type_name()))
        } else if !store || column.is_none() {
            Some("is not stored".to_owned())
        } else if p.company_dependent == Some(true) {
            Some("is company-dependent".to_owned())
        } else if is_generated {
            Some("is generated".to_owned())
        } else if !required {
            Some("is not required".to_owned())
        } else if !matches!(default, Some(FieldDefault::Value(StaticValue::Bool(_)))) {
            Some("has no static boolean default".to_owned())
        } else {
            None
        };
        if let Some(why) = why {
            return Err(Error::programming(format!(
                "field '{model}.{name}': db_default is for a stored, required boolean \
                 field with a static boolean default, not company-dependent and not generated; \
                 this field {why}"
            )));
        }
    }
    let quoted_name = crate::ident::quote_arc(name);
    Ok(FieldSpec {
        id,
        name,
        quoted_name,
        ty,
        string: p.string.map_or_else(|| default_label(name), str::to_owned),
        help: p.help,
        required,
        readonly,
        store,
        copy,
        index,
        default,
        prefetch,
        compute: p.compute,
        search_method: p.search_method,
        inverse: p.inverse,
        related: p.related,
        precompute,
        compute_read: if !compute_sudo {
            ComputeRead::AsCaller
        } else if p.related.is_some() && p.compute_sudo.is_none() && !store {
            ComputeRead::CompanyBound
        } else {
            ComputeRead::Superuser
        },
        groups: p.groups,
        depends: p.depends,
        reads: p.reads.unwrap_or_default(),
        depends_context: p.depends_context.unwrap_or_default(),
        modules: merged.modules,
        magic: merged.magic,
        manual: merged.manual,
        domain: p.domain,
        context: p.context,
        // refined by `company::finish`
        check_company: if p.check_company == Some(true) {
            CheckCompany::Declared
        } else {
            CheckCompany::Off
        },
        check_company_declared: p.check_company,
        bypass_search_access: p.bypass_search_access.unwrap_or(false),
        delegate: p.delegate.unwrap_or(false),
        company_dependent: p.company_dependent.unwrap_or(false),
        translate,
        html_policy,
        change_default: p.change_default.unwrap_or(false),
        user_writeable: p.user_writeable.unwrap_or(false),
        secret: p.secret.unwrap_or(false),
        db_default,
        tracking,
        inherited: None,
        server_set,
        selection_ondelete,
        generated: match p.generated {
            Some(Some(source)) => {
                let storage = p
                    .generated_storage
                    .unwrap_or(crate::generated::GeneratedStorage::Virtual);
                Some(crate::generated::GeneratedSpec::raw(source, storage))
            }
            _ => None,
        },
    })
}

/// The static value of a default as a dynamic value, if it is static.
pub fn static_default(default: &DefaultDecl) -> Option<StaticValue> {
    match default {
        DefaultDecl::Value(v) => Some(*v),
        _ => None,
    }
}

/// Validate actual relation targets through aliases and delegated fields. A worklist
/// follows every path segment so an intermediate alias cannot hide a comodel mismatch.
fn monetary_currency_relation(
    models: &[ModelSpec],
    by_name: &FxHashMap<&'static str, ModelId>,
    model: ModelId,
    field: FieldId,
) -> bool {
    let mut pending = vec![(model, field, "res.currency".to_owned())];
    let mut seen = FxHashSet::default();
    while let Some((model, field, expected)) = pending.pop() {
        let spec = &models[model.index()];
        let fs = spec.field_by_id(field);
        if !matches!(&fs.ty, FieldType::Many2one { comodel, .. } if comodel == &expected) {
            return false;
        }
        if !seen.insert((model, field)) {
            continue;
        }
        if let Some(inherited) = fs.inherited {
            pending.push((inherited.parent, inherited.parent_field, expected));
        } else if let Some(path) = fs.related {
            let mut current = model;
            let mut parts = path.split('.').peekable();
            while let Some(name) = parts.next() {
                let Some(segment) = models[current.index()].field(name) else {
                    return false;
                };
                if parts.peek().is_none() {
                    pending.push((current, segment.id, expected.clone()));
                } else {
                    let FieldType::Many2one { comodel, .. } = &segment.ty else {
                        return false;
                    };
                    let Some(next) = by_name.get(comodel.as_str()) else {
                        return false;
                    };
                    pending.push((current, segment.id, comodel.clone()));
                    current = *next;
                }
            }
        }
    }
    true
}

/// A shared id sequence must not take the name of a relation in the registry: a
/// model or many2many table, or the identity sequence PostgreSQL generates for a model table.
/// `CREATE SEQUENCE IF NOT EXISTS` would otherwise find that relation and keep it.
fn check_id_sequences(models: &[ModelSpec]) -> Result<()> {
    for model in models {
        let Some(seq) = model.id_sequence else {
            continue;
        };
        for other in models.iter().filter(|m| m.has_table()) {
            let identity = pg_make_object_name(&other.table, Some("id"), "seq");
            let clash = if other.table == seq {
                Some("the table")
            } else if identity == seq && other.id_sequence.is_none() {
                Some("the identity sequence of the table")
            } else {
                other
                    .fields
                    .iter()
                    .any(|f| {
                        matches!(&f.ty, FieldType::Many2many { relation: Some(r), .. } if r.table == seq)
                    })
                    .then_some("a many2many relation table")
            };
            if let Some(what) = clash {
                return Err(Error::programming(format!(
                    "model '{}' sets id_sequence '{seq}', which is {what} of model '{}': a shared \
                     id sequence needs a name of its own",
                    model.name, other.name
                )));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod manual_extension_tests {
    use super::*;
    use crate::company_access::CompanyAccessDecl;

    static FIELDS: [FieldDecl; 1] = [FieldDecl::char("x_note")];
    static GONE: ModelDecl = ModelDecl::extension(MANUAL_ADDON, "x_gone", &["x_gone"])
        .kind_follows_model(true)
        .fields(&FIELDS);
    static GONE_WITH_ACCESS: ModelDecl = ModelDecl::extension(MANUAL_ADDON, "x_gone", &["x_gone"])
        .kind_follows_model(true)
        .fields(&FIELDS)
        .company_access(CompanyAccessDecl::unrestricted("kept"));

    fn compose_with(decl: &'static ModelDecl) -> Result<Registry> {
        let mut sink = Sink::new(ManualFieldPolicy::DropBroken);
        compose(
            vec![&BASE_ADDON],
            &FxHashSet::default(),
            &[&BASE_DECL, decl],
            &[],
            &mut sink,
        )
    }

    /// Custom fields of a model that is gone are dropped; a company access declared with them is
    /// not dropped silently: the model's absence is the error (review F5).
    #[test]
    fn a_manual_extension_of_a_missing_model_keeps_its_company_access_error() {
        assert!(
            compose_with(&GONE).is_ok(),
            "the custom field alone is dropped"
        );
        let err = compose_with(&GONE_WITH_ACCESS).unwrap_err();
        assert_eq!(err.kind(), erp_fields::ErrorKind::Programming);
        assert_eq!(err.message(), "Model 'x_gone' does not exist in registry.");
    }
}
