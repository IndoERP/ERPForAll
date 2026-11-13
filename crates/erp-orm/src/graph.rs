//! The compiled dependency graph of computed fields (COMP §4.3).
//!
//! Built once per registry, next to the method dispatch tables. Per model:
//!
//! - one [`FieldCompute`] per computed or related field: its compute chain (newest override
//!   first), its *compute group* (fields computed by the same method), its resolved dependencies
//!   (explicit `depends=` on the field wins, otherwise the union of the `depends` of every
//!   implementation in the chain, base first:/003), the fields it may read;
//! - the direct dependents of every field on the same model (the same-model trigger edges);
//! - a topological order of the model's computed fields;
//! - the constraint methods with the fields that trigger them.
//!
//! Across models (S4b):
//!
//! - **Paths.** A dependency `line_ids.price_total` or `partner_id.country_id.code` resolves to
//!   one `(model, field)` segment per step.
//!   Trees of the same changed field share their prefixes,
//!   so one traversal serves every dependent behind it. A segment that is a field of an
//!   `_inherits` parent expands to the delegate many2one and the parent's field. A transient
//!   model's path stops where it enters a regular model. Changing the inverse
//!   many2one of a one2many in a path is covered by the one2many's own notification (the inverse
//!   cache maintenance of REL §4.4.4 notifies the old and new owners).
//! - **Global order.** Every computed field of every model gets a rank: a field comes after every
//!   computed field it reads through any path (cycles across models, e.g. recursive fields, are
//!   broken deterministically). Recomputation and marking follow the ranks.
//! - **Upstream.** Per computed field, the computed fields (any model) it transitively depends on,
//!   in rank order, followed by itself: reading or searching a stored compute settles them first.
//! - **Reach.** Whether a change of a field can reach a stored computed field: the marking walk is
//!   pruned when every target behind an edge is non-stored, uncached and reaches nothing stored
//!   (`modified()` on an empty cache costs 0 statements).
//! - **Recursive fields.** A field reaching itself through a path is recursive;
//!   it is computed level by level.

use std::sync::Arc;

use erp_fields::{Error, FieldType, Result};
use erp_registry::{FieldId, FieldSpec, ModelId, ModelKind, ModelSpec, Registry};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::compute::{ComputeImpl, ConstraintImpl};
use crate::dispatch::{Dispatch, ImplKind, MethodFn};

/// A field of a model.
pub(crate) type FieldKey = (ModelId, FieldId);

/// A dependency path: one `(model, field)` per step, the first on the computed field's model.
pub(crate) type Path = Arc<[FieldKey]>;

fn package_compute_model<'a>(
    models: &'a [ModelSpec],
    compute: &ComputeImpl,
) -> Result<&'a ModelSpec> {
    models
        .iter()
        .find(|model| {
            model.name == compute.model
                || model
                    .mro
                    .iter()
                    .any(|decl| decl.name == compute.model && decl.addon == compute.addon)
        })
        .ok_or_else(|| {
            Error::programming(format!(
                "package compute '{}' from addon '{}' has no registered model '{}'",
                compute.method, compute.addon, compute.model
            ))
        })
}

/// Where a computed value comes from.
#[derive(Debug, Clone)]
pub(crate) enum Source {
    /// A compute chain, newest implementation first.
    Chain(Arc<[&'static ComputeImpl]>),
    /// A one-segment related field: the value of another field of the same record.
    Related(FieldId),
    /// A related field through relations (`partner_id.country_id.code`): the steps as declared
    /// (fields of `_inherits` parents are not expanded; reading them follows the delegate).
    RelatedPath(Path),
    /// The built-in `display_name` of `base`: computed by the ORM from the rec-name field on
    /// every read (never cached), so it only carries the trigger edge from that field.
    DisplayName,
}

/// A computed (or related) field.
#[derive(Debug)]
pub(crate) struct FieldCompute {
    pub(crate) method: &'static str,
    pub(crate) source: Source,
    /// Fields computed by the same method, in field order (includes this one).
    pub(crate) group: Arc<[FieldId]>,
    /// Resolved dependencies on fields of the same model (the first step of every path too).
    pub(crate) depends: Vec<FieldId>,
    /// Dependency paths of two steps or more (inherited steps expanded): prefetched for the
    /// compute and turned into trigger trees.
    pub(crate) paths: Vec<Path>,
    /// Paths read without a trigger (`reads = [...]` through relations).
    pub(crate) read_paths: Vec<Path>,
    /// The paths of `paths` that end on the company field of a hop of a company-bound related
    /// field (review RC3): they exist for invalidation only, so the registry
    /// checks their last link for that (review RD2) and not for the performance rules of
    /// ordinary dependencies.
    pub(crate) company_hops: Vec<Path>,
    /// What a sync compute may read on its own records: depends, reads and the group's fields.
    pub(crate) readable: Vec<FieldId>,
    /// Currency ordering prerequisites; these do not trigger amount recomputation.
    pub(crate) normalization: Vec<FieldId>,
    /// Context keys the value depends on (cache key, ENV §4.5).
    pub(crate) depends_context: Vec<&'static str>,
    pub(crate) stored: bool,
    /// `readonly=False`: a written value is kept until a dependency changes.
    pub(crate) editable: bool,
    /// Declared `precompute = true` (D7): protected for the rest of the create once
    /// precomputed, and a registry error when the graph cannot honour it. An implicitly
    /// precomputed field (no declaration) is not protected, so a later write to its inputs in the
    /// same create marks it again.
    pub(crate) precompute_declared: bool,
    /// Why the field is computed after the INSERT, when it is; `None` when it can be
    /// computed before it. Set when the registry is linked.
    pub(crate) late: Option<Late>,
    pub(crate) sudo: bool,
    pub(crate) is_async: bool,
    /// An async compute registered `candidate`: read-only view, joins same-call monetary
    /// admission.
    pub(crate) candidate: bool,
    /// Reaches itself through a path: computed level by level.
    pub(crate) recursive: bool,
    /// Inverse method name.
    pub(crate) inverse: Option<&'static str>,
}

impl FieldCompute {
    /// Every path the compute reads (dependencies and plain reads).
    pub(crate) fn input_paths(&self) -> impl Iterator<Item = &Path> {
        self.paths.iter().chain(self.read_paths.iter())
    }

    /// Computed before the INSERT of `create`: a stored field that is
    /// not [`late`](FieldCompute::late). Derived, so the two can never disagree.
    pub(crate) fn precompute(&self) -> bool {
        self.stored && self.late.is_none()
    }
}

/// Why a computed field cannot be computed before the INSERT of `create`. The
/// first blocking reason found, in this order: an opt-out, the kind of compute, a field of the same
/// compute method decided earlier, a late or later input of the same model, an input path through
/// a relation other than many2one.
#[derive(Debug, Clone)]
pub(crate) enum Late {
    /// Declared `precompute = false` on this field or on another field of its compute method (a
    /// compute group decides once).
    OptedOut(FieldId),
    /// An async compute: it may query, so it runs after the INSERT.
    Async,
    /// The built-in `display_name`, computed from the record name on every read.
    DisplayName,
    /// A database-generated value exists only after the record is inserted.
    Generated(FieldId),
    /// Its compute method reads a computed field of the same model that is itself late (that
    /// field has its own [`Late`]; [`Late::describe`] follows the chain to the field late on its
    /// own). The input comes earlier in the dependency order.
    Input(FieldId),
    /// Its compute method reads a computed field of the same model that comes **later** in the
    /// model's dependency order, whether or not that field is late itself: a `reads = [...]` entry
    /// (reads are not ordering edges), or a dependency of another field of the same compute method
    /// that sits between the two fields in the order. The method runs once, at the position of
    /// the first field of its group (`write::precompute_rows`), so that input has no value yet.
    /// Conservative on purpose: such a group is computed after the INSERT, and a declared
    /// `precompute = true` on it is a registry error.
    ReadsLater(FieldId),
    /// Reads a path whose step `step` (not the last) is not a many2one: its targets are not
    /// known before the INSERT.
    Path { path: Path, step: usize },
}

impl Late {
    /// The reason `field` is late, in words, for the `Programming` error of a declared precompute
    /// and the `Validation` error of a late required field. Follows a chain of late
    /// inputs to the first field that is late on its own, one clause per field (`it reads m.b,
    /// which is computed after the INSERT; m.b is declared precompute = false`).
    ///
    /// # Errors
    ///
    /// `Programming` when the graph contradicts itself: a [`Late::Input`] whose input has no
    /// reason, a chain longer than the model's fields (a cycle), or a [`Late::Path`] whose step is
    /// outside its path. These are ORM bugs, reported instead of a vague sentence.
    pub(crate) fn describe(
        &self,
        registry: &Registry,
        model: &ModelSpec,
        graph: &ModelGraph,
        field: FieldId,
    ) -> Result<String> {
        let name = |id: FieldId| format!("{}.{}", model.name, model.field_by_id(id).name);
        let mut text = String::new();
        let mut current = field;
        let mut subject = String::from("it");
        let mut late = self.clone();
        // the order is acyclic, so a chain visits each field of the model at most once
        for _ in 0..=model.fields.len() {
            let clause = match &late {
                Late::OptedOut(g) if *g == current => {
                    format!("{subject} is declared precompute = false")
                }
                Late::OptedOut(g) => format!(
                    "{subject} shares its compute method with {}, which is declared precompute = \
                     false",
                    name(*g)
                ),
                Late::Async => format!("{subject} has an async compute method"),
                Late::DisplayName => {
                    format!("{subject} is computed from the record name on every read")
                }
                Late::Generated(r) => format!(
                    "{subject} reads database-generated {}, which requires an inserted record",
                    name(*r)
                ),
                Late::ReadsLater(r) => format!(
                    "{} compute method reads {}, which comes later in the dependency order, so \
                     the method runs before it has a value",
                    possessive(&subject),
                    name(*r)
                ),
                Late::Path { path, step } => path_clause(registry, &subject, path, *step)?,
                Late::Input(r) => {
                    text.push_str(&format!(
                        "{subject} reads {}, which is computed after the INSERT; ",
                        name(*r)
                    ));
                    // the input's own reason is set before its dependents' (dependency order)
                    let next = graph
                        .compute(*r)
                        .and_then(|fc| fc.late.clone())
                        .ok_or_else(|| {
                            Error::programming(format!(
                                "precompute graph of {}: {} is recorded as late because of {}, \
                                 which has no reason of its own",
                                model.name,
                                name(current),
                                name(*r)
                            ))
                        })?;
                    subject = name(*r);
                    current = *r;
                    late = next;
                    continue;
                }
            };
            text.push_str(&clause);
            return Ok(text);
        }
        Err(Error::programming(format!(
            "precompute graph of {}: the chain of late inputs of {} does not end (a cycle)",
            model.name,
            name(field)
        )))
    }
}

/// `its` for the described field, `<model>.<field>'s` for a field further down the chain.
fn possessive(subject: &str) -> String {
    if subject == "it" {
        "its".to_owned()
    } else {
        format!("{subject}'s")
    }
}

/// The clause of [`Late::Path`]: the path, the step that is not a many2one, and what it is.
///
/// # Errors
///
/// `Programming` when `step` is outside `path` (an ORM bug: the step was found in that path).
fn path_clause(
    registry: &Registry,
    subject: &str,
    path: &[FieldKey],
    step: usize,
) -> Result<String> {
    let dotted: Vec<&str> = path
        .iter()
        .map(|(m, f)| registry.model_by_id(*m).field_by_id(*f).name)
        .collect();
    let dotted = dotted.join(".");
    let (m, f) = path.get(step).copied().ok_or_else(|| {
        Error::programming(format!(
            "precompute graph: step {step} is outside the dependency path {dotted}"
        ))
    })?;
    let hop_model = registry.model_by_id(m);
    let hop = hop_model.field_by_id(f);
    let hop_name = format!("{}.{}", hop_model.name, hop.name);
    Ok(match hop.ty {
        FieldType::One2many { .. } => format!(
            "{subject} reads {dotted} through the one2many {hop_name}, whose lines are created \
             after the INSERT; precompute over one2many lines is not supported until \
             PERF.PRECOMPUTE-X2M"
        ),
        FieldType::Many2many { .. } => format!(
            "{subject} reads {dotted} through the many2many {hop_name}; only many2one steps have \
             their targets before the INSERT"
        ),
        // any other relational step (a many2one_reference, a related relation): wording only
        _ => format!(
            "{subject} reads {dotted} through {hop_name}, which is not a many2one; only many2one \
             steps have their targets before the INSERT"
        ),
    })
}

/// A constraint method and the fields that trigger it.
#[derive(Debug, Clone)]
pub(crate) struct Constraint {
    pub(crate) method: &'static str,
    pub(crate) fields: Vec<FieldId>,
}

/// The graph of one model.
#[derive(Debug, Default)]
pub(crate) struct ModelGraph {
    /// By field index.
    pub(crate) fields: Vec<Option<Arc<FieldCompute>>>,
    /// Direct dependents on the same model, by field index.
    pub(crate) dependents: Vec<Vec<FieldId>>,
    /// Computed fields in dependency order.
    pub(crate) order: Vec<FieldId>,
    pub(crate) constraints: Vec<Constraint>,
}

impl ModelGraph {
    pub(crate) fn compute(&self, field: FieldId) -> Option<&Arc<FieldCompute>> {
        self.fields.get(field.index()).and_then(Option::as_ref)
    }

    /// Whether this field uses the framework's uncached record-name formatter.
    /// A registered `_compute_display_name` chain has the same method spelling,
    /// but must execute through the ordinary compute and context-cache machinery.
    pub(crate) fn is_builtin_display_name(&self, field: FieldId) -> bool {
        self.compute(field)
            .is_some_and(|compute| matches!(compute.source, Source::DisplayName))
    }

    pub(crate) fn dependents(&self, field: FieldId) -> &[FieldId] {
        self.dependents
            .get(field.index())
            .map(Vec::as_slice)
            .unwrap_or_default()
    }
}

#[derive(Debug, Default)]
pub(crate) struct TriggerTree {
    pub(crate) targets: Vec<FieldKey>,
    pub(crate) children: Vec<(FieldKey, TriggerTree)>,
    /// Every target of this node and below (pruning).
    pub(crate) all_targets: Vec<FieldKey>,
    /// Some target of this node or below is stored, or reaches a stored field.
    pub(crate) reaches_stored: bool,
}

impl TriggerTree {
    fn insert(&mut self, edges: &[FieldKey], target: FieldKey) {
        match edges.split_first() {
            None => {
                if !self.targets.contains(&target) {
                    self.targets.push(target);
                }
            }
            Some((edge, rest)) => {
                let i = match self.children.iter().position(|(e, _)| e == edge) {
                    Some(i) => i,
                    None => {
                        self.children.push((*edge, TriggerTree::default()));
                        self.children.len() - 1
                    }
                };
                self.children[i].1.insert(rest, target);
            }
        }
    }

    /// Fill `all_targets` and `reaches_stored` bottom-up.
    fn finish(&mut self, reaches: &FxHashSet<FieldKey>) {
        let mut all: Vec<FieldKey> = self.targets.clone();
        let mut stored = self.targets.iter().any(|t| reaches.contains(t));
        for (_, child) in &mut self.children {
            child.finish(reaches);
            stored |= child.reaches_stored;
            for t in &child.all_targets {
                if !all.contains(t) {
                    all.push(*t);
                }
            }
        }
        self.all_targets = all;
        self.reaches_stored = stored;
    }

    fn collect_targets(&self, out: &mut Vec<FieldKey>) {
        for t in &self.targets {
            if !out.contains(t) {
                out.push(*t);
            }
        }
        for (_, child) in &self.children {
            child.collect_targets(out);
        }
    }
}

/// A generated read and the alias/owner/source fields whose access it requires.
#[derive(Debug)]
pub(crate) struct GeneratedRead {
    pub(crate) fields: Vec<FieldKey>,
    pub(crate) path: Path,
}

/// The graphs of every model of a registry, and the cross-model structures.
#[derive(Debug, Default)]
pub(crate) struct ComputeGraph {
    models: Vec<ModelGraph>,
    /// Cross-model trigger trees by changed field.
    triggers: FxHashMap<FieldKey, TriggerTree>,
    /// Rank of every computed field in the global dependency order.
    rank: FxHashMap<FieldKey, u32>,
    /// Computed fields in global dependency order.
    order: Vec<FieldKey>,
    /// Per computed field: the computed fields it transitively depends on (any model) in rank
    /// order, followed by itself.
    upstream: FxHashMap<FieldKey, Vec<FieldKey>>,
    /// Computed fields whose change reaches a stored computed field (stored ones included).
    reaches_stored: FxHashSet<FieldKey>,
    generated_reads: FxHashMap<FieldKey, GeneratedRead>,
    generated_read_fields: FxHashMap<ModelId, Vec<FieldId>>,
    requires_persisted: FxHashSet<FieldKey>,
}

impl ComputeGraph {
    pub(crate) fn model(&self, model: ModelId) -> &ModelGraph {
        static EMPTY: ModelGraph = ModelGraph {
            fields: Vec::new(),
            dependents: Vec::new(),
            order: Vec::new(),
            constraints: Vec::new(),
        };
        self.models.get(model.index()).unwrap_or(&EMPTY)
    }

    pub(crate) fn requires_persisted(&self, key: FieldKey) -> bool {
        self.requires_persisted.contains(&key)
    }

    pub(crate) fn generated_read_fields(&self, model: ModelId) -> &[FieldId] {
        // No generated readers means no extra access work for this model.
        self.generated_read_fields
            .get(&model)
            .map_or(&[], Vec::as_slice)
    }

    pub(crate) fn generated_read(&self, key: FieldKey) -> Option<&GeneratedRead> {
        self.generated_reads.get(&key)
    }

    /// The computed field of `key`, if it is one.
    pub(crate) fn compute(&self, key: FieldKey) -> Option<&Arc<FieldCompute>> {
        self.model(key.0).compute(key.1)
    }

    /// The trigger tree of changes of `key` through relations.
    pub(crate) fn triggers(&self, key: FieldKey) -> Option<&TriggerTree> {
        self.triggers.get(&key)
    }

    /// The global rank of a computed field (`u32::MAX` for other fields).
    pub(crate) fn rank(&self, key: FieldKey) -> u32 {
        self.rank.get(&key).copied().unwrap_or(u32::MAX)
    }

    /// Computed fields in global dependency order.
    pub(crate) fn order(&self) -> &[FieldKey] {
        &self.order
    }

    /// The computed fields `key` depends on (any model), in rank order, then `key` itself.
    pub(crate) fn upstream(&self, key: FieldKey) -> &[FieldKey] {
        self.upstream
            .get(&key)
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    /// Whether a change of computed field `key` can reach a stored computed field.
    pub(crate) fn reaches_stored(&self, key: FieldKey) -> bool {
        self.reaches_stored.contains(&key)
    }

    /// Whether anything depends on `key` (on its model or through relations).
    pub(crate) fn has_dependents(&self, key: FieldKey) -> bool {
        !self.model(key.0).dependents(key.1).is_empty() || self.triggers.contains_key(&key)
    }
}

/// The built-in `display_name` of `base` (computed on read by the ORM itself).
pub(crate) fn is_builtin_display_name(field: &FieldSpec) -> bool {
    field.name == "display_name" && field.compute == Some("_compute_display_name")
}

type Owners<'a, T> = FxHashMap<(&'static str, &'static str), Vec<&'a T>>;

fn by_owner<'a, T>(
    items: impl Iterator<Item = &'a T>,
    owner: impl Fn(&T) -> (&'static str, &'static str),
) -> Owners<'a, T> {
    let mut map: Owners<'a, T> = FxHashMap::default();
    for item in items {
        map.entry(owner(item)).or_default().push(item);
    }
    map
}

/// The owners `(model, addon)` of a model's declarations, newest first.
fn owners(model: &ModelSpec) -> Vec<(&'static str, &'static str)> {
    let mut seen = Vec::new();
    for decl in &model.mro {
        let owner = (decl.name, decl.addon);
        if !seen.contains(&owner) {
            seen.push(owner);
        }
    }
    seen
}

fn check_compute_chain(model: &str, method: &str, chain: &[&'static ComputeImpl]) -> Result<()> {
    let defines: Vec<&&ComputeImpl> = chain
        .iter()
        .filter(|i| i.kind == ImplKind::Define)
        .collect();
    let Some(first) = chain.first() else {
        return Ok(());
    };
    if let [newer, older, ..] = defines.as_slice() {
        return Err(Error::programming(format!(
            "compute method '{method}' of model '{model}' is defined at {} and again at {}: the \
             later one must be #[compute(override, ...)]",
            older.location(),
            newer.location()
        )));
    }
    if chain.last().is_none_or(|l| l.kind != ImplKind::Define) {
        return Err(Error::programming(format!(
            "override of compute method '{method}' at {} has no definition below it in model \
             '{model}'",
            first.location()
        )));
    }
    if let Some(other) = chain
        .iter()
        .find(|i| i.entry.is_async() != first.entry.is_async())
    {
        return Err(Error::programming(format!(
            "compute method '{method}' of model '{model}' mixes sync and async implementations: \
             {} and {}",
            first.location(),
            other.location()
        )));
    }
    let candidate = first.entry.is_candidate();
    if let Some(other) = chain.iter().find(|i| i.entry.is_candidate() != candidate) {
        return Err(Error::programming(format!(
            "compute method '{method}' of model '{model}' mixes candidate and non-candidate \
             implementations: {} and {}",
            first.location(),
            other.location()
        )));
    }
    if candidate && chain.len() > 1 {
        return Err(Error::programming(format!(
            "candidate compute method '{method}' of model '{model}' is overridden or defined \
             twice ({} and {}): a candidate compute has no override chain",
            first.location(),
            chain[chain.len() - 1].location()
        )));
    }
    Ok(())
}

/// A method with signature `(this, tx) -> Result<()>` (inverse and constraint methods). A chain
/// whose newest entry is WebAssembly has no typed signature to compare: it is called in the wire
/// form with `null` arguments and must answer `null` (checked at the call).
fn check_unit_method(
    dispatch: &Dispatch,
    model: &ModelSpec,
    method: &str,
    role: &str,
) -> Result<()> {
    let targets = dispatch.targets(model.id, method);
    let Some(newest) = targets.first() else {
        return Err(Error::programming(format!(
            "{role} method '{method}' of model '{}' is not implemented: add an async #[method] \
             fn {method}(this, tx) -> Result<()>",
            model.name
        )));
    };
    if newest.is_wasm() {
        return Ok(());
    }
    let chain = dispatch.implementations(model.id, method);
    let Some(first) = chain.first() else {
        return Err(Error::programming(format!(
            "{role} method '{method}' of model '{}' is not implemented natively",
            model.name
        )));
    };
    if (first.entry)().downcast_ref::<MethodFn<(), ()>>().is_none() {
        return Err(Error::programming(format!(
            "{role} method '{method}' of model '{}' at {}:{} must take no arguments and return \
             Result<()>",
            model.name, first.file, first.line
        )));
    }
    Ok(())
}

impl ComputeGraph {
    /// Build the graph of every model, then the cross-model structures.
    pub(crate) fn build(
        registry: &Registry,
        dispatch: &Dispatch,
        computes: &[&'static ComputeImpl],
        constraints: &[&'static ConstraintImpl],
    ) -> Result<Self> {
        let installed: FxHashSet<&str> = registry.addons().iter().map(|a| a.name).collect();
        let computes: Vec<&'static ComputeImpl> = computes
            .iter()
            .copied()
            .filter(|c| installed.contains(c.addon))
            .collect();
        let constraints: Vec<&'static ConstraintImpl> = constraints
            .iter()
            .copied()
            .filter(|c| installed.contains(c.addon))
            .collect();
        let compute_owners = by_owner(computes.iter().copied(), |c| (c.model, c.addon));
        let constraint_owners = by_owner(constraints.iter().copied(), |c| (c.model, c.addon));

        // Every implementation needs a declaration of its model in its addon (as for methods).
        let declared: FxHashSet<(&str, &str)> = registry
            .models()
            .iter()
            .flat_map(|m| m.mro.iter().map(|d| (d.name, d.addon)))
            .collect();
        for c in &computes {
            if !declared.contains(&(c.model, c.addon)) {
                return Err(Error::programming(format!(
                    "#[compute] '{}' at {} has no declaration of model '{}' in addon '{}'",
                    c.method,
                    c.location(),
                    c.model,
                    c.addon
                )));
            }
        }

        // A compute of a package receives its inputs as data and its result is readable by anyone
        // who can read the computed field, so it must not copy a field that is kept from some
        // readers (security review r1 H2).
        for c in computes.iter().filter(|c| c.file == "<wasm>") {
            let model = package_compute_model(registry.models(), c)?;
            for name in c.depends.iter().chain(c.reads) {
                if let Some(groups) = model.field(name).and_then(|f| f.groups) {
                    return Err(Error::programming(format!(
                        "compute '{}' of the package addon '{}' names the field {}.{name}, which \
                         is restricted to groups ({groups}): a package compute cannot read a \
                         protected field",
                        c.method, c.addon, model.name
                    )));
                }
            }
        }

        let mut used_methods: FxHashSet<(&str, &str)> = FxHashSet::default();
        let mut models = Vec::with_capacity(registry.models().len());
        for model in registry.models() {
            let graph = build_model(
                registry,
                model,
                dispatch,
                &compute_owners,
                &constraint_owners,
                &mut used_methods,
            )?;
            models.push(graph);
        }
        if let Some(c) = computes
            .iter()
            .find(|c| !used_methods.contains(&(c.model, c.method)))
        {
            return Err(Error::programming(format!(
                "#[compute] '{}' at {} is not the compute method of any field of '{}'",
                c.method,
                c.location(),
                c.model
            )));
        }
        let mut graph = Self {
            models,
            ..Self::default()
        };
        graph.link(registry)?;
        Ok(graph)
    }

    /// Cross-model structures: trigger trees, global order, upstream lists, reach, recursion and
    /// precompute eligibility through paths.
    fn link(&mut self, registry: &Registry) -> Result<()> {
        // trigger trees: for every path prefix of two steps or more, the change of its last step
        // reaches the computed field through the reversed earlier steps
        let mut triggers: FxHashMap<FieldKey, TriggerTree> = FxHashMap::default();
        let mut recursive: FxHashSet<FieldKey> = FxHashSet::default();
        for model in registry.models() {
            let graph = &self.models[model.id.index()];
            for field in &model.fields {
                let Some(fc) = graph.compute(field.id) else {
                    continue;
                };
                let target = (model.id, field.id);
                for path in &fc.paths {
                    if path.iter().skip(1).any(|step| *step == target) {
                        recursive.insert(target);
                    }
                    let original = path;
                    let path = trigger_path(registry, model, path);
                    // the last link of a company hop path is checked for invalidation only
                    let hop_link = fc
                        .company_hops
                        .iter()
                        .any(|hop| **hop == **original)
                        .then(|| original.len().saturating_sub(2));
                    for (i, step) in path.iter().take(path.len().saturating_sub(1)).enumerate() {
                        if hop_link == Some(i) {
                            check_hop_edge(registry, model, field, *step)?;
                        } else {
                            check_edge(registry, model, field, *step)?;
                        }
                    }
                    for k in 1..path.len() {
                        let edges: Vec<FieldKey> = path[..k].iter().rev().copied().collect();
                        triggers.entry(path[k]).or_default().insert(&edges, target);
                    }
                }
            }
        }

        // dependencies between computed fields, across models
        let mut computed: Vec<FieldKey> = Vec::new();
        let mut deps: FxHashMap<FieldKey, Vec<FieldKey>> = FxHashMap::default();
        for model in registry.models() {
            let graph = &self.models[model.id.index()];
            for f in &graph.order {
                let Some(fc) = graph.compute(*f) else {
                    continue;
                };
                let key = (model.id, *f);
                computed.push(key);
                let mut list: Vec<FieldKey> = Vec::new();
                let add = |k: FieldKey, list: &mut Vec<FieldKey>| {
                    if k != key && !list.contains(&k) {
                        list.push(k);
                    }
                };
                for d in &fc.depends {
                    add((model.id, *d), &mut list);
                    add_o2m_inverse(registry, (model.id, *d), &mut list);
                }
                for r in &fc.readable {
                    if !fc.group.contains(r) {
                        add((model.id, *r), &mut list);
                    }
                }
                for path in fc.input_paths() {
                    for step in path.iter() {
                        add(*step, &mut list);
                        add_o2m_inverse(registry, *step, &mut list);
                    }
                }
                deps.insert(key, list);
            }
        }
        // Preserve ordinary input edges for unsaved serializer availability before the
        // recompute ordering graph drops non-callback columns such as generated outputs.
        let declared_inputs = deps.clone();
        let is_computed = |k: &FieldKey, models: &[ModelGraph]| {
            models
                .get(k.0.index())
                .and_then(|m| m.compute(k.1))
                .is_some()
        };
        for list in deps.values_mut() {
            list.retain(|k| is_computed(k, &self.models));
        }
        self.scope_label_input_caches(registry, &computed, &deps)?;

        // global order (Kahn; a remaining cycle is broken at its smallest key)
        let mut indegree: FxHashMap<FieldKey, usize> = computed.iter().map(|k| (*k, 0)).collect();
        let mut users: FxHashMap<FieldKey, Vec<FieldKey>> = FxHashMap::default();
        for (k, list) in &deps {
            for d in list {
                *indegree.entry(*k).or_default() += 1;
                users.entry(*d).or_default().push(*k);
            }
        }
        let mut order: Vec<FieldKey> = Vec::with_capacity(computed.len());
        let mut done: FxHashSet<FieldKey> = FxHashSet::default();
        let mut ready: std::collections::BTreeSet<FieldKey> = computed
            .iter()
            .copied()
            .filter(|k| indegree.get(k) == Some(&0))
            .collect();
        while done.len() < computed.len() {
            let next = match ready.pop_first() {
                Some(k) => k,
                None => match computed.iter().filter(|k| !done.contains(k)).min() {
                    Some(k) => *k,
                    None => break,
                },
            };
            if !done.insert(next) {
                continue;
            }
            order.push(next);
            for u in users.get(&next).map(Vec::as_slice).unwrap_or_default() {
                if let Some(d) = indegree.get_mut(u) {
                    *d = d.saturating_sub(1);
                    if *d == 0 && !done.contains(u) {
                        ready.insert(*u);
                    }
                }
            }
        }
        let rank: FxHashMap<FieldKey, u32> = order
            .iter()
            .enumerate()
            .map(|(i, k)| (*k, u32::try_from(i).unwrap_or(u32::MAX)))
            .collect();

        // upstream lists
        let mut upstream: FxHashMap<FieldKey, Vec<FieldKey>> = FxHashMap::default();
        for key in &order {
            let mut seen: FxHashSet<FieldKey> = FxHashSet::default();
            let mut stack: Vec<FieldKey> = deps.get(key).cloned().unwrap_or_default();
            while let Some(k) = stack.pop() {
                if k == *key || !seen.insert(k) {
                    continue;
                }
                stack.extend(deps.get(&k).map(Vec::as_slice).unwrap_or_default());
            }
            let mut list: Vec<FieldKey> = seen.into_iter().collect();
            list.sort_by_key(|k| rank.get(k).copied().unwrap_or(u32::MAX));
            list.push(*key);
            upstream.insert(*key, list);
        }

        // reach: stored computed fields, then every field with a dependent that reaches one
        let mut reaches: FxHashSet<FieldKey> = computed
            .iter()
            .copied()
            .filter(|k| self.compute(*k).is_some_and(|fc| fc.stored))
            .collect();
        loop {
            let mut changed = false;
            for key in &computed {
                if reaches.contains(key) {
                    continue;
                }
                let mut dependents: Vec<FieldKey> = self
                    .model(key.0)
                    .dependents(key.1)
                    .iter()
                    .map(|d| (key.0, *d))
                    .collect();
                if let Some(tree) = triggers.get(key) {
                    tree.collect_targets(&mut dependents);
                }
                if dependents.iter().any(|d| reaches.contains(d)) {
                    reaches.insert(*key);
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        for tree in triggers.values_mut() {
            tree.finish(&reaches);
        }

        // recursion, and which fields are computed after the INSERT (D7)
        for (model_index, graph) in self.models.iter_mut().enumerate() {
            let Some(model) = registry.models().get(model_index) else {
                continue;
            };
            let model_order = graph.order.clone();
            // position of every computed field in the model's dependency order, by field index
            let mut position = vec![usize::MAX; model.fields.len()];
            for (pos, f) in model_order.iter().enumerate() {
                if let Some(slot) = position.get_mut(f.index()) {
                    *slot = pos;
                }
            }
            let mut declared_late: Option<FieldId> = None;
            for (pos, f) in model_order.iter().enumerate() {
                let late = match graph.compute(*f) {
                    Some(fc) => late_reason(registry, model, graph, fc, &position, pos),
                    None => continue,
                };
                let fc = graph.fields[f.index()]
                    .as_mut()
                    .and_then(Arc::get_mut)
                    .ok_or_else(|| {
                        Error::programming(format!(
                            "the compute graph of {} is shared while the registry is linked",
                            model.name
                        ))
                    })?;
                fc.recursive = recursive.contains(&(model.id, *f));
                if fc.precompute_declared && late.is_some() && declared_late.is_none() {
                    declared_late = Some(*f);
                }
                fc.late = late;
            }
            if let Some(f) = declared_late
                && let Some(late) = graph.compute(f).and_then(|fc| fc.late.as_ref())
            {
                return Err(Error::programming(format!(
                    "Field {}.{} declares precompute = true, but it cannot be computed before the \
                     INSERT: {}",
                    model.name,
                    model.field_by_id(f).name,
                    late.describe(registry, model, graph, f)?
                )));
            }
        }
        self.triggers = triggers;
        self.rank = rank;
        self.order = order;
        self.upstream = upstream;
        self.reaches_stored = reaches;
        self.generated_reads = generated_reads(registry, self)?;
        for (model, field) in self.generated_reads.keys() {
            self.generated_read_fields
                .entry(*model)
                .or_default()
                .push(*field);
        }
        for fields in self.generated_read_fields.values_mut() {
            fields.sort_unstable();
        }
        self.requires_persisted
            .extend(self.generated_reads.keys().copied());
        loop {
            let mut changed = false;
            for (field, inputs) in &declared_inputs {
                if !self.requires_persisted.contains(field)
                    && inputs
                        .iter()
                        .any(|input| self.requires_persisted.contains(input))
                {
                    self.requires_persisted.insert(*field);
                    changed = true;
                }
            }
            // Delegated exposures have their compute/dependency declarations cleared by the
            // registry. Availability still follows the owner, including ordinary native computes
            // that need a generated value. Iterate with input edges for multi-level inheritance.
            for model in registry.models() {
                for field in &model.fields {
                    if let Some(inherited) = field.inherited
                        && self
                            .requires_persisted
                            .contains(&(inherited.parent, inherited.parent_field))
                        && self.requires_persisted.insert((model.id, field.id))
                    {
                        changed = true;
                    }
                }
            }
            if !changed {
                break;
            }
        }
        Ok(())
    }

    /// A privileged custom label must not reuse an unbound nonstored intermediary.
    /// Walk the already resolved dependency graph, including read-only paths across models,
    /// and scope every nonstored input group to the same original-reader company boundary.
    /// Stored values retain their existing write-time contract. The visited set bounds cycles
    /// by the registry's finite computed-field graph; no runtime relation traversal is added.
    fn scope_label_input_caches(
        &mut self,
        registry: &Registry,
        computed: &[FieldKey],
        deps: &FxHashMap<FieldKey, Vec<FieldKey>>,
    ) -> Result<()> {
        let mut pending: Vec<FieldKey> = computed
            .iter()
            .copied()
            .filter(|key| {
                self.compute(*key).is_some_and(|compute| {
                    !compute.stored
                        && matches!(compute.source, Source::Chain(_))
                        && compute.group.iter().any(|field| {
                            registry.model_by_id(key.0).field_by_id(*field).name == "display_name"
                        })
                })
            })
            .collect();
        let mut visited = FxHashSet::default();
        let mut scoped = Vec::new();
        while let Some(key) = pending.pop() {
            if !visited.insert(key) {
                continue;
            }
            let compute = self.compute(key).ok_or_else(|| {
                Error::programming(format!(
                    "Custom label input {}.{} has no compute entry",
                    registry.model_by_id(key.0).name,
                    registry.model_by_id(key.0).field_by_id(key.1).name
                ))
            })?;
            if compute.stored {
                continue;
            }
            // The built-in formatter is never cached; its computed inputs still need scoping.
            if !matches!(compute.source, Source::DisplayName) {
                scoped.push(key);
            }
            pending.extend(compute.group.iter().map(|field| (key.0, *field)));
            if let Some(inputs) = deps.get(&key) {
                pending.extend(inputs.iter().copied());
            }
        }
        for (model, field) in scoped {
            let compute = self.models[model.index()].fields[field.index()]
                .as_mut()
                .and_then(Arc::get_mut)
                .ok_or_else(|| {
                    Error::programming(format!(
                        "the label-input compute graph of {} is shared while the registry is linked",
                        registry.model_by_id(model).name
                    ))
                })?;
            if !compute.depends_context.contains(&"company_bound") {
                compute.depends_context.push("company_bound");
            }
        }
        Ok(())
    }
}

/// Compile alias access paths once. Native compute chains retain their existing sudo contract;
/// only related aliases and delegated exposures transparently forward generated outputs.
fn generated_reads(
    registry: &Registry,
    graph: &ComputeGraph,
) -> Result<FxHashMap<FieldKey, GeneratedRead>> {
    let mut out = FxHashMap::default();
    for model in registry.models() {
        for field in &model.fields {
            if !field.is_generated() && field.related.is_none() && field.inherited.is_none() {
                continue;
            }
            let start = (model.id, field.id);
            let mut cursor = start;
            let mut visited = FxHashSet::default();
            let mut fields = Vec::new();
            let mut path = Vec::new();
            loop {
                if !visited.insert(cursor) {
                    // A closed ordinary related cycle cannot reach a generated terminal;
                    // keep the existing recursive-related contract for these unrelated fields.
                    break;
                }
                fields.push(cursor);
                let current = registry.model_by_id(cursor.0).field_by_id(cursor.1);
                if let Some(inherited) = current.inherited {
                    let delegate = (cursor.0, inherited.delegate);
                    fields.push(delegate);
                    path.push(delegate);
                    cursor = (inherited.parent, inherited.parent_field);
                    continue;
                }
                if let Some(generated) = current.generated() {
                    fields.extend(
                        generated
                            .dependencies()
                            .iter()
                            .map(|source| (cursor.0, *source)),
                    );
                    path.push(cursor);
                    out.insert(
                        start,
                        GeneratedRead {
                            fields,
                            path: Arc::from(path),
                        },
                    );
                    break;
                }
                if current.related.is_none() {
                    break;
                }
                let compute = graph.compute(cursor).ok_or_else(|| {
                    Error::programming(format!(
                        "Generated access path for '{}.{}' has no related-field resolution",
                        model.name, field.name
                    ))
                })?;
                match &compute.source {
                    Source::Related(target) => cursor = (cursor.0, *target),
                    Source::RelatedPath(steps) => {
                        let (last, preceding) = steps.split_last().ok_or_else(|| {
                            Error::programming(format!(
                                "Generated access path for '{}.{}' has an empty related path",
                                model.name, field.name
                            ))
                        })?;
                        fields.extend_from_slice(preceding);
                        path.extend_from_slice(preceding);
                        cursor = *last;
                    }
                    Source::Chain(_) | Source::DisplayName => {
                        return Err(Error::programming(format!(
                            "Generated access path for '{}.{}' has an ambiguous related resolution",
                            model.name, field.name
                        )));
                    }
                }
            }
        }
    }
    Ok(out)
}

/// Anything else would leave dependents stale
/// without notice, so the registry refuses it.
fn check_edge(
    registry: &Registry,
    model: &ModelSpec,
    field: &FieldSpec,
    edge: FieldKey,
) -> Result<()> {
    let edge_model = registry.model_by_id(edge.0);
    let step = edge_model.field_by_id(edge.1);
    if matches!(&step.ty, FieldType::Many2one { .. })
        && step.has_column()
        && step.index.is_none()
        && !step.magic
    {
        return Err(Error::programming(format!(
            "Field {}.{} depends on a path through {}.{}, which has index = false: the \
             ORM finds the records to recompute by that column, keep it indexed",
            model.name, field.name, edge_model.name, step.name
        )));
    }
    if is_traceable(step) {
        return Ok(());
    }
    Err(Error::programming(format!(
        "Field {}.{} depends on a path through {}.{}, a computed field that is neither stored \
         nor related: the records to recompute when the rest of the path changes cannot be \
         found (the reference ERP needs it searchable); depend on the stored fields it is computed from",
        model.name, field.name, edge_model.name, step.name
    )))
}

/// Whether the ORM can find the records that point to a changed record through `step`.
fn is_traceable(step: &FieldSpec) -> bool {
    match &step.ty {
        FieldType::Many2one { .. } if step.has_column() => true,
        FieldType::One2many { .. } | FieldType::Many2many { .. } => {
            step.store || step.related.is_some()
        }
        _ => step.related.is_some(),
    }
}

/// [`check_edge`] for the last link of the path to the company of a hop of a company-bound
/// related `field` (review RD2). The path exists so that a hop moving to another company drops
/// the cached value: invalidation correctness is never dropped, but a rule that only keeps the
/// lookup fast (the index of a many2one) is not applied, the lookup is merely slower.
///
/// # Errors
///
/// `Programming` when the link cannot be traced, naming the field and the link.
fn check_hop_edge(
    registry: &Registry,
    model: &ModelSpec,
    field: &FieldSpec,
    edge: FieldKey,
) -> Result<()> {
    let edge_model = registry.model_by_id(edge.0);
    let step = edge_model.field_by_id(edge.1);
    if is_traceable(step) {
        return Ok(());
    }
    let related = field.related.ok_or_else(|| {
        Error::programming(format!(
            "Field {}.{} has company hop paths but is not a related field",
            model.name, field.name
        ))
    })?;
    Err(Error::programming(format!(
        "Field {}.{} is company-bound related to '{}' (compute_sudo is not declared): the \
         company of the record it reaches through {}.{} cannot be watched, because {}.{} is a \
         computed field that is neither stored nor related; make that field stored or related, \
         or declare compute_sudo on the related field",
        model.name, field.name, related, edge_model.name, step.name, edge_model.name, step.name
    )))
}

/// A one2many's content depends on its inverse field: a computed inverse is upstream of whoever
/// reads the one2many.
fn add_o2m_inverse(registry: &Registry, key: FieldKey, list: &mut Vec<FieldKey>) {
    let model = registry.model_by_id(key.0);
    let field = model.field_by_id(key.1);
    if let FieldType::One2many { comodel, inverse } = &field.ty
        && let Some(co) = registry.model(comodel)
        && let Some(inv) = co.field_id(inverse)
        && !list.contains(&(co.id, inv))
    {
        list.push((co.id, inv));
    }
}

/// The first step of a path, before the last, that is not a many2one field (its targets are not
/// known before the INSERT, so precompute through it is impossible); `None` when every such step
/// is a many2one.
fn first_non_many2one_step(registry: &Registry, path: &[FieldKey]) -> Option<usize> {
    path.iter()
        .take(path.len().saturating_sub(1))
        .position(|(m, f)| {
            !matches!(
                registry.model_by_id(*m).field_by_id(*f).ty,
                FieldType::Many2one { .. }
            )
        })
}

/// Why `fc`, at `pos` in its model's dependency order, is computed after the INSERT, from the
/// reasons already set on the fields before it; `None` when it can be computed
/// before. A compute group decides once: an opt-out, a late input or a blocked path of any of its
/// fields makes every field of the group late.
fn late_reason(
    registry: &Registry,
    model: &ModelSpec,
    graph: &ModelGraph,
    fc: &FieldCompute,
    position: &[usize],
    pos: usize,
) -> Option<Late> {
    if let Some(g) = fc.group.iter().copied().find(|g| {
        let spec = model.field_by_id(*g);
        spec.store && spec.precompute == Some(false)
    }) {
        return Some(Late::OptedOut(g));
    }
    if fc.is_async {
        return Some(Late::Async);
    }
    if matches!(fc.source, Source::DisplayName) {
        return Some(Late::DisplayName);
    }
    // a field of the group decided earlier in the order decides for the whole group
    if let Some(late) = fc
        .group
        .iter()
        .filter(|g| position.get(g.index()).is_some_and(|p| *p < pos))
        .find_map(|g| graph.compute(*g).and_then(|m| m.late.clone()))
    {
        return Some(late);
    }
    let members: Vec<&FieldCompute> = fc
        .group
        .iter()
        .filter_map(|g| graph.compute(*g).map(AsRef::as_ref))
        .collect();
    for member in &members {
        for r in &member.readable {
            if fc.group.contains(r) {
                continue;
            }
            if model.field_by_id(*r).is_generated() {
                return Some(Late::Generated(*r));
            }
            let Some(input) = graph.compute(*r) else {
                // a plain field of the record: known at the INSERT
                continue;
            };
            if position.get(r.index()).is_none_or(|p| *p > pos) {
                return Some(Late::ReadsLater(*r));
            }
            if input.late.is_some() {
                return Some(Late::Input(*r));
            }
        }
    }
    for member in &members {
        for path in member.input_paths() {
            if let Some(step) = first_non_many2one_step(registry, path) {
                return Some(Late::Path {
                    path: Arc::clone(path),
                    step,
                });
            }
        }
    }
    None
}

fn trigger_path(registry: &Registry, model: &ModelSpec, path: &[FieldKey]) -> Vec<FieldKey> {
    if model.kind != ModelKind::Transient {
        return path.to_vec();
    }
    path.iter()
        .copied()
        .take_while(|(m, _)| registry.model_by_id(*m).kind == ModelKind::Transient)
        .collect()
}

/// A resolved dependency: the steps as declared and with `_inherits` steps expanded.
struct Resolved {
    declared: Vec<FieldKey>,
    expanded: Vec<FieldKey>,
}

/// The company field of the company-scoped model `step` points to (`None` for a model without
/// company scope, a non-relational field, or an `_inherits` delegate, which is the same record).
///
/// # Errors
///
/// `Programming` for a company scope this version does not know (review SF2).
fn hop_company_field(
    registry: &erp_registry::Registry,
    step: &FieldSpec,
) -> Result<Option<FieldKey>> {
    use erp_registry::CompanyScope;
    if step.delegate {
        return Ok(None);
    }
    let Some(hop) = step.ty.comodel().and_then(|c| registry.model(c)) else {
        return Ok(None);
    };
    match hop.company_scope {
        Some(CompanyScope::Single { field, .. } | CompanyScope::Many { field, .. }) => {
            Ok(Some((hop.id, field)))
        }
        // no scope, or `res.company` itself: no company to bound by
        None | Some(CompanyScope::Itself) => Ok(None),
        Some(other) => Err(crate::related_company::unknown_scope(other, hop.name)),
    }
}

/// Whether a related path goes through a record of a company-scoped model, the records its last
/// field points to included (review SC4); a hop through `_inherits` is the same
/// record, not another.
///
/// # Errors
///
/// As [`hop_company_field`].
fn crosses_company_scope(registry: &erp_registry::Registry, path: &[FieldKey]) -> Result<bool> {
    for (model, field) in path {
        let step = registry.model_by_id(*model).field_by_id(*field);
        if hop_company_field(registry, step)?.is_some() {
            return Ok(true);
        }
    }
    Ok(false)
}

/// The trigger paths of the company fields of the hops of a company-bound related `path`: when a
/// hop moves to another company, the cached value no longer holds (review RC3). One path per
/// company-scoped hop: the steps up to it, then the company field of the model it points to.
///
/// # Errors
///
/// As [`hop_company_field`].
fn hop_company_paths(
    registry: &erp_registry::Registry,
    path: &[FieldKey],
) -> Result<Vec<Vec<FieldKey>>> {
    let mut paths = Vec::new();
    for (i, (model, field)) in path.iter().enumerate() {
        let step = registry.model_by_id(*model).field_by_id(*field);
        if let Some(company) = hop_company_field(registry, step)? {
            let mut steps = path[..=i].to_vec();
            steps.push(company);
            paths.push(steps);
        }
    }
    Ok(paths)
}

/// Whether reading the company-bound related `field` of `model` passes a company-scoped record:
/// on its own path, or on the path of a company-bound related field that path goes through
/// (read with the same bound, review SC1). `stack` holds the fields being looked at.
///
/// # Errors
///
/// `Programming` when company-bound related fields lead back to themselves: such a value can
/// never be read.
fn bound_crossing(
    registry: &Registry,
    model: &ModelSpec,
    field: &FieldSpec,
    stack: &mut Vec<FieldKey>,
) -> Result<bool> {
    let Some(path) = field.related.filter(|_| field.company_bound()) else {
        return Ok(false);
    };
    if stack.contains(&(model.id, field.id)) {
        return Err(Error::programming(format!(
            "Field {}.{} is company-bound and related to itself through its own path",
            model.name, field.name
        )));
    }
    // a related path that resolves to nothing (a bare self-reference) is refused where the graph
    // is built; reaching here with one is a broken invariant, never "no bound" (review SF5)
    let Some(resolved) = resolve_path(registry, model, field, "", path, "related")? else {
        return Err(Error::programming(format!(
            "company-bound related field {}.{} has a path that resolves to no field",
            model.name, field.name
        )));
    };
    stack.push((model.id, field.id));
    let mut crossing = false;
    for keys in [&resolved.declared, &resolved.expanded] {
        crossing |= crosses_company_scope(registry, keys)?;
        for (m, f) in keys.iter().copied() {
            let inner_model = registry.model_by_id(m);
            if (m, f) != (model.id, field.id)
                && bound_crossing(registry, inner_model, inner_model.field_by_id(f), stack)?
            {
                crossing = true;
            }
        }
    }
    stack.pop();
    Ok(crossing)
}

fn not_found(method: &str, owner: &ModelSpec, field: &FieldSpec, model: &str, name: &str) -> Error {
    Error::programming(format!(
        "Wrong @depends on '{method}' (compute method of field {}.{}). Dependency field \
         '{name}' not found in model {model}.",
        owner.name, field.name
    ))
}

fn resolve_path(
    registry: &Registry,
    model: &ModelSpec,
    field: &FieldSpec,
    method: &str,
    path: &str,
    what: &str,
) -> Result<Option<Resolved>> {
    let segments: Vec<&str> = path.split('.').collect();
    if segments.contains(&"id") {
        return Err(Error::programming(
            "Compute method cannot depend on field 'id'.",
        ));
    }
    let mut declared: Vec<FieldKey> = Vec::with_capacity(segments.len());
    let mut expanded: Vec<FieldKey> = Vec::with_capacity(segments.len());
    let mut current: &ModelSpec = model;
    for (i, name) in segments.iter().enumerate() {
        let Some(step) = current.field(name) else {
            return Err(if what == "depends" {
                not_found(method, model, field, current.name, name)
            } else {
                Error::programming(format!(
                    "Wrong @{what} on '{method}' (compute method of field {}.{}). Field '{name}' \
                     not found in model {}.",
                    model.name, field.name, current.name
                ))
            });
        };
        declared.push((current.id, step.id));
        // a field of an `_inherits` parent: the delegate, then the parent's field
        let mut target_model = current;
        let mut target = step;
        while let Some(inherited) = target.inherited {
            expanded.push((target_model.id, inherited.delegate));
            target_model = registry.model_by_id(inherited.parent);
            target = target_model.field_by_id(inherited.parent_field);
        }
        expanded.push((target_model.id, target.id));
        let last = i + 1 == segments.len();
        if !last {
            let comodel = match &target.ty {
                FieldType::Many2one { comodel, .. }
                | FieldType::One2many { comodel, .. }
                | FieldType::Many2many { comodel, .. } => registry.model(comodel),
                _ => None,
            };
            current = comodel.ok_or_else(|| {
                Error::programming(format!(
                    "Field {}.{}: {what} '{path}' goes through {}.{}, which is not a many2one, \
                     one2many or many2many field (the reference ERP: \"Field {} in {}.{}'s dependencies is \
                     not reachable.\")",
                    model.name,
                    field.name,
                    current.name,
                    name,
                    segments[i + 1],
                    model.name,
                    field.name
                ))
            })?;
        }
    }
    if what == "depends" && declared.len() == 1 && declared[0] == (model.id, field.id) {
        return Ok(None);
    }
    Ok(Some(Resolved { declared, expanded }))
}

fn push_unique(list: &mut Vec<FieldId>, id: FieldId) {
    if !list.contains(&id) {
        list.push(id);
    }
}

fn push_path(list: &mut Vec<Path>, path: Vec<FieldKey>) {
    if path.len() >= 2 && !list.iter().any(|p| **p == *path) {
        list.push(Arc::from(path));
    }
}

/// Add one resolved dependency or read to the lists of a field.
fn add_resolved(
    model: &ModelSpec,
    resolved: Resolved,
    same: &mut Vec<FieldId>,
    readable: &mut Vec<FieldId>,
    paths: &mut Vec<Path>,
) {
    // the step as declared stays readable on the records themselves
    if let Some((m, f)) = resolved.declared.first()
        && *m == model.id
    {
        push_unique(readable, *f);
    }
    if let Some((m, f)) = resolved.expanded.first()
        && *m == model.id
    {
        push_unique(same, *f);
        push_unique(readable, *f);
    }
    push_path(paths, resolved.expanded);
}

fn build_model(
    registry: &Registry,
    model: &ModelSpec,
    dispatch: &Dispatch,
    compute_owners: &Owners<'static, ComputeImpl>,
    constraint_owners: &Owners<'static, ConstraintImpl>,
    used: &mut FxHashSet<(&'static str, &'static str)>,
) -> Result<ModelGraph> {
    let owners = owners(model);
    // compute chains by method, newest first
    let mut chains: FxHashMap<&'static str, Vec<&'static ComputeImpl>> = FxHashMap::default();
    for owner in &owners {
        for imp in compute_owners
            .get(owner)
            .map(Vec::as_slice)
            .unwrap_or_default()
        {
            chains.entry(imp.method).or_default().push(imp);
        }
    }
    for (method, chain) in &chains {
        check_compute_chain(model.name, method, chain)?;
    }

    let n = model.fields.len();
    let mut fields: Vec<Option<Arc<FieldCompute>>> = (0..n).map(|_| None).collect();
    let mut groups: FxHashMap<&'static str, Vec<FieldId>> = FxHashMap::default();
    for field in &model.fields {
        if let Some(method) = field.compute
            && !(is_builtin_display_name(field) && !chains.contains_key(method))
        {
            groups.entry(method).or_default().push(field.id);
        }
    }

    for field in &model.fields {
        let mut depends: Vec<FieldId> = Vec::new();
        let mut readable: Vec<FieldId> = Vec::new();
        let mut paths: Vec<Path> = Vec::new();
        let mut read_paths: Vec<Path> = Vec::new();
        let mut company_hops: Vec<Path> = Vec::new();
        let (method, source, mut context, is_async) = if let Some(path) = field.related {
            let resolved =
                resolve_path(registry, model, field, "", path, "related")?.ok_or_else(|| {
                    Error::programming(format!(
                        "Field {}.{} cannot be related to itself",
                        model.name, field.name
                    ))
                })?;
            if resolved.declared == [(model.id, field.id)] {
                return Err(Error::programming(format!(
                    "Field {}.{} cannot be related to itself",
                    model.name, field.name
                )));
            }
            // a field of an `_inherits` parent is followed through the delegate, like a path
            let source = match (resolved.declared.as_slice(), resolved.expanded.len()) {
                ([(_, target)], 1) => Source::Related(*target),
                ([_], _) => Source::RelatedPath(Arc::from(resolved.expanded.clone())),
                (steps, _) => Source::RelatedPath(Arc::from(steps.to_vec())),
            };
            // a company-bound related value that passes a company-scoped hop depends on the
            // reader's companies; one that does not is cached as any other
            let bound_hops = bound_crossing(registry, model, field, &mut Vec::new())?;
            // built from the steps with `_inherits` expanded, the ones the trigger trees follow
            // (review RD1): a step the child only inherits has no stored column to trace
            let hop_paths = if bound_hops {
                hop_company_paths(registry, &resolved.expanded)?
            } else {
                Vec::new()
            };
            add_resolved(model, resolved, &mut depends, &mut readable, &mut paths);
            for hop_path in hop_paths {
                // a path the dependencies already hold keeps the checks of a dependency
                let before = paths.len();
                push_path(&mut paths, hop_path);
                if paths.len() > before
                    && let Some(added) = paths.last()
                {
                    company_hops.push(Arc::clone(added));
                }
            }
            let context = if bound_hops {
                vec!["company_bound"]
            } else {
                Vec::new()
            };
            ("", source, context, false)
        } else if let Some(method) = field.compute {
            if is_builtin_display_name(field) && !chains.contains_key(method) {
                // display_name depends on the rec-name field (RSET §4.15)
                if let Some(rec) = model.rec_name.filter(|r| *r != field.id) {
                    depends.push(rec);
                    readable.push(rec);
                }
                (method, Source::DisplayName, Vec::new(), false)
            } else {
                let chain = chains.get(method).ok_or_else(|| {
                    Error::programming(format!(
                        "Field {}.{}: compute method '{method}' not found; declare it with \
                     #[compute(...)] in a #[methods] block of the model",
                        model.name, field.name
                    ))
                })?;
                used.insert((model.name, method));
                for owner in &owners {
                    // overrides declared for a parent model are used by this model too
                    used.insert((owner.0, method));
                }
                let mut context: Vec<&'static str> = field.depends_context.to_vec();
                let declared: Vec<&'static str> = match field.depends {
                    Some(explicit) => explicit.to_vec(),
                    None => chain
                        .iter()
                        .rev()
                        .flat_map(|imp| imp.depends.iter().copied())
                        .collect(),
                };
                for path in declared {
                    if let Some(resolved) =
                        resolve_path(registry, model, field, method, path, "depends")?
                    {
                        add_resolved(model, resolved, &mut depends, &mut readable, &mut paths);
                    }
                }
                let mut reads: Vec<&'static str> = Vec::new();
                for imp in chain.iter().rev() {
                    reads.extend(imp.reads.iter().copied());
                    for key in imp.depends_context {
                        if !context.contains(key) {
                            context.push(key);
                        }
                    }
                }
                reads.extend(field.reads.iter().copied());
                for name in reads {
                    if let Some(resolved) =
                        resolve_path(registry, model, field, method, name, "reads")?
                    {
                        let mut ignored = Vec::new();
                        add_resolved(
                            model,
                            resolved,
                            &mut ignored,
                            &mut readable,
                            &mut read_paths,
                        );
                    }
                }
                let is_async = chain.first().is_some_and(|c| c.entry.is_async());
                let candidate = chain.first().is_some_and(|c| c.entry.is_candidate());
                if candidate
                    && paths
                        .iter()
                        .chain(read_paths.iter())
                        .any(|path| path.len() > 1)
                {
                    return Err(Error::programming(format!(
                        "Field {}.{}: candidate compute '{method}' declares a dependency or read \
                         through a relation; its read-only view shows staged values of direct \
                         fields of the batch only",
                        model.name, field.name
                    )));
                }
                (
                    method,
                    Source::Chain(Arc::from(chain.as_slice())),
                    context,
                    is_async,
                )
            }
        } else {
            continue;
        };
        for d in &depends {
            push_unique(&mut readable, *d);
        }
        let group: Arc<[FieldId]> = match (&source, field.related) {
            (Source::DisplayName, _) | (_, Some(_)) => Arc::from([field.id]),
            _ => Arc::from(groups.get(method).cloned().unwrap_or_default()),
        };
        for g in group.iter() {
            push_unique(&mut readable, *g);
        }
        let mut normalization = Vec::new();
        for &input in &readable {
            if let FieldType::Monetary {
                currency_field: Some(name),
            } = &model.field_by_id(input).ty
                && let Some(currency) = model.field_id(name)
                && !group.contains(&currency)
                && !depends.contains(&currency)
            {
                push_unique(&mut normalization, currency);
            }
        }
        for &currency in &normalization {
            push_unique(&mut readable, currency);
        }
        if !field.store
            && matches!(source, Source::Chain(_))
            && group
                .iter()
                .any(|field| model.field_by_id(*field).name == "display_name")
        {
            // A custom label may be computed with privileged target permissions on behalf
            // of a reader whose company-bound inputs remain restricted. Separate that
            // reader's result from an explicit sudo result and from other active companies.
            // Every output of the same compute group needs the same cache boundary.
            if !context.contains(&"company_bound") {
                context.push("company_bound");
            }
        }
        context.dedup();
        if field.store && !context.is_empty() {
            return Err(Error::programming(format!(
                "Field {}.{} is stored but depends on the context ({}): a stored value cannot \
                 differ per context",
                model.name,
                field.name,
                context.join(", ")
            )));
        }
        if is_async && field.precompute == Some(true) {
            return Err(Error::programming(format!(
                "Field {}.{}: async computes cannot be precomputed; drop precompute",
                model.name, field.name
            )));
        }
        depends.retain(|d| *d != field.id);
        let candidate = matches!(
            &source,
            Source::Chain(chain) if chain.first().is_some_and(|c| c.entry.is_candidate())
        );
        fields[field.id.index()] = Some(Arc::new(FieldCompute {
            method,
            source,
            group,
            depends,
            paths,
            read_paths,
            company_hops,
            readable,
            normalization,
            depends_context: context,
            stored: field.store,
            editable: !field.readonly,
            precompute_declared: field.store && field.precompute == Some(true),
            // decided when the registry is linked (`ComputeGraph::link`)
            late: None,
            sudo: field.compute_sudo(),
            is_async,
            candidate,
            recursive: false,
            inverse: field.inverse,
        }));
    }

    // Groups must agree on how they are stored and computed.
    for (method, members) in &groups {
        let specs: Vec<&FieldSpec> = members.iter().map(|f| model.field_by_id(*f)).collect();
        if let Some(first) = specs.first() {
            if specs.iter().any(|s| s.store != first.store) {
                return Err(Error::programming(format!(
                    "{}: fields {} share compute method '{method}' but are not all stored or \
                     all non-stored",
                    model.name,
                    specs.iter().map(|s| s.name).collect::<Vec<_>>().join(", ")
                )));
            }
            if specs
                .iter()
                .any(|s| s.compute_sudo() != first.compute_sudo())
            {
                return Err(Error::programming(format!(
                    "{}: fields {} share compute method '{method}' but have different \
                     compute_sudo",
                    model.name,
                    specs.iter().map(|s| s.name).collect::<Vec<_>>().join(", ")
                )));
            }
        }
    }

    // Topological order (Kahn) of the model's own dependencies.
    let computed: Vec<FieldId> = model
        .fields
        .iter()
        .filter(|f| fields[f.id.index()].is_some())
        .map(|f| f.id)
        .collect();
    let mut dependents: Vec<Vec<FieldId>> = vec![Vec::new(); n];
    let mut indegree: Vec<usize> = vec![0; n];
    for g in &computed {
        if let Some(fc) = &fields[g.index()] {
            for d in &fc.depends {
                push_unique(&mut dependents[d.index()], *g);
                if fields[d.index()].is_some() {
                    indegree[g.index()] += 1;
                }
            }
        }
    }
    let mut scheduled_users = dependents.clone();
    for &g in &computed {
        if let Some(fc) = &fields[g.index()] {
            for &currency in &fc.normalization {
                if fields[currency.index()].is_some() {
                    push_unique(&mut scheduled_users[currency.index()], g);
                    indegree[g.index()] += 1;
                }
            }
        }
    }
    let mut order: Vec<FieldId> = Vec::with_capacity(computed.len());
    let mut ready: Vec<FieldId> = computed
        .iter()
        .copied()
        .filter(|g| indegree[g.index()] == 0)
        .collect();
    ready.reverse();
    while let Some(f) = ready.pop() {
        order.push(f);
        for g in scheduled_users[f.index()].iter().rev() {
            indegree[g.index()] -= 1;
            if indegree[g.index()] == 0 {
                ready.push(*g);
            }
        }
    }
    if order.len() != computed.len() {
        let path = cycle_path(&computed, &order, &fields);
        let names: Vec<&str> = path.iter().map(|g| model.field_by_id(*g).name).collect();
        return Err(Error::programming(format!(
            "{}: computed fields depend on each other in a cycle: {}",
            model.name,
            names.join(" -> ")
        )));
    }

    // Inverse methods must exist with the right signature.
    for fc in fields.iter().flatten() {
        if let Some(inverse) = fc.inverse {
            check_unit_method(dispatch, model, inverse, "inverse")?;
        }
    }

    // Constraints: newest declaration of a method name wins.
    let mut constraints: Vec<Constraint> = Vec::new();
    for owner in &owners {
        for c in constraint_owners
            .get(owner)
            .map(Vec::as_slice)
            .unwrap_or_default()
        {
            if constraints.iter().any(|k| k.method == c.method) {
                continue;
            }
            let mut ids = Vec::new();
            for name in c.fields {
                let id = model.field_id(name).ok_or_else(|| {
                    Error::programming(format!(
                        "method {}.{}: @constrains parameter '{name}' is not a field name ({}:{})",
                        model.name, c.method, c.file, c.line
                    ))
                })?;
                push_unique(&mut ids, id);
            }
            check_unit_method(dispatch, model, c.method, "constraint")?;
            constraints.push(Constraint {
                method: c.method,
                fields: ids,
            });
        }
    }

    Ok(ModelGraph {
        fields,
        dependents,
        order,
        constraints,
    })
}

/// One cycle among the computed fields that the topological sort could not order, as a path that
/// starts and ends with the same field.
fn cycle_path(
    computed: &[FieldId],
    order: &[FieldId],
    fields: &[Option<Arc<FieldCompute>>],
) -> Vec<FieldId> {
    let left: Vec<FieldId> = computed
        .iter()
        .copied()
        .filter(|g| !order.contains(g))
        .collect();
    let Some(&start) = left.first() else {
        return Vec::new();
    };
    // Every remaining field has a remaining dependency: follow them until one repeats.
    let mut path = vec![start];
    let mut current = start;
    loop {
        let next = fields
            .get(current.index())
            .and_then(Option::as_ref)
            .and_then(|fc| {
                fc.depends
                    .iter()
                    .chain(&fc.normalization)
                    .copied()
                    .find(|d| left.contains(d))
            });
        let Some(next) = next else {
            return path;
        };
        if let Some(pos) = path.iter().position(|p| *p == next) {
            let mut cycle = path.split_off(pos);
            cycle.push(next);
            cycle.reverse();
            return cycle;
        }
        path.push(next);
        current = next;
    }
}

#[cfg(test)]
mod package_model_tests {
    use super::*;

    #[test]
    fn missing_package_compute_model_is_an_error() {
        let compute = crate::compute::wasm_compute_impl(&crate::compute::WasmComputeSpec {
            addon: "test_package".to_owned(),
            model: "missing.model".to_owned(),
            method: "compute_value".to_owned(),
            depends: vec!["value".to_owned()],
            reads: Vec::new(),
            depends_context: Vec::new(),
        })
        .expect("valid compute specification");
        let error = package_compute_model(&[], compute).expect_err("missing model must fail");
        assert_eq!(error.kind(), erp_fields::ErrorKind::Programming);
        assert_eq!(
            error.message(),
            "package compute 'compute_value' from addon 'test_package' has no registered model 'missing.model'"
        );
    }
}
