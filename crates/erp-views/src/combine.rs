//! View catalogue and combination order (
//! inheriting views `:733-790`).
//!
//! Only the views of the requested inheritance tree are looked at: a broken view elsewhere in the
//! catalogue (missing parent, bad arch, cycle) does not affect other combinations.

use crate::apply::{apply_inheritance_specs, apply_primary_template, apply_template_specs};
use crate::arch::{Arch, Caps};
use crate::error::{LimitKind, ViewError, ViewResult};
use crate::limits::{Budget, ViewLimits};
use crate::parse::parse_arch;
use std::borrow::Cow;
use std::collections::{HashMap, HashSet, VecDeque};

/// Where a view sits in the inheritance tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Inheritance {
    /// A view without `inherit_id` (always primary).
    Root,
    /// `mode="extension"`: modifies its parent in place for every view built on it.
    Extension {
        /// The inherited view.
        parent: i64,
    },
    /// `mode="primary"` with `inherit_id`: a new view built on its parent's combined arch.
    Primary {
        /// The inherited view.
        parent: i64,
    },
}

impl Inheritance {
    /// The inherited view, if any.
    pub fn parent(&self) -> Option<i64> {
        match self {
            Inheritance::Root => None,
            Inheritance::Extension { parent } | Inheritance::Primary { parent } => Some(*parent),
        }
    }
}

/// One `ir.ui.view` record as the engine needs it.
///
/// ```
/// use erp_views::{Inheritance, ViewDescriptor};
///
/// let ext = ViewDescriptor::new(2, "partner.ext", "res.partner", "<form position=\"inside\"/>")
///     .extension_of(1)
///     .with_priority(20);
/// assert_eq!(ext.inheritance(), Inheritance::Extension { parent: 1 });
/// assert_eq!(ext.priority(), 20);
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewDescriptor {
    id: i64,
    name: String,
    model: String,
    arch: String,
    inheritance: Inheritance,
    priority: i32,
    active: bool,
    merge_primary_root: bool,
    template_replacements: bool,
}

impl ViewDescriptor {
    pub fn new(
        id: i64,
        name: impl Into<String>,
        model: impl Into<String>,
        arch: impl Into<String>,
    ) -> Self {
        Self {
            id,
            name: name.into(),
            model: model.into(),
            arch: arch.into(),
            inheritance: Inheritance::Root,
            priority: 16,
            active: true,
            merge_primary_root: false,
            template_replacements: false,
        }
    }

    /// Makes this view an extension of `parent`.
    pub fn extension_of(mut self, parent: i64) -> Self {
        self.inheritance = Inheritance::Extension { parent };
        self.merge_primary_root = false;
        self
    }

    /// Makes this view a primary view inheriting from `parent`.
    pub fn primary_of(mut self, parent: i64) -> Self {
        self.inheritance = Inheritance::Primary { parent };
        self.merge_primary_root = false;
        self
    }

    /// Makes this an Elang primary template inheriting from `parent`.
    ///
    /// Unlike [`Self::primary_of`], `arch` is the complete child root, whose children are
    /// inheritance specs. After applying them, the root tag and attributes replace/override
    /// those of the combined parent, except `t-inherit` and `t-inherit-mode`. Subsequent
    /// extensions and primary descendants see the merged root. The root records this view's
    /// provenance; descendants retain their own. Parsing, evaluation work and retained bytes
    /// use the catalogue's ordinary [`ViewLimits`].
    ///
    /// This opt-in leaves server-view primary inheritance unchanged. Stray text among specs
    /// is refused, just as in a `<data>` container. Errors are returned by combination.
    ///
    /// ```
    /// use erp_views::{ViewCatalogue, ViewDescriptor};
    /// let catalogue = ViewCatalogue::new(vec![
    ///     ViewDescriptor::new(1, "parent", "ui.template", "<div><span/></div>"),
    ///     ViewDescriptor::new(2, "child", "ui.template", "<section/>")
    ///         .primary_template_of(1),
    /// ])?;
    /// assert_eq!(catalogue.combine(2)?.to_xml(), "<section><span/></section>");
    /// # Ok::<(), erp_views::ViewError>(())
    /// ```
    pub fn primary_template_of(mut self, parent: i64) -> Self {
        self.inheritance = Inheritance::Primary { parent };
        self.merge_primary_root = true;
        self
    }

    /// Uses Elang's positional `$0` replacement for this view's specs.
    ///
    /// Each text node equal to `$0` is replaced at its position by a copy of the target,
    /// without copying the target's following text. Ordinary server-view specs retain their
    /// append behavior. Copies still occur after moves and obey the same arena
    /// and evaluation limits. This does not change primary-root behavior: use
    /// [`Self::primary_template_of`] for an Elang primary template.
    ///
    /// ```
    /// use erp_views::{ViewCatalogue, ViewDescriptor};
    /// let catalogue = ViewCatalogue::new(vec![
    ///     ViewDescriptor::new(1, "parent", "ui.template", "<div><span/></div>"),
    ///     ViewDescriptor::new(2, "wrap", "ui.template",
    ///         "<xpath expr=\"//span\" position=\"replace\"><section>$0<i/></section></xpath>")
    ///         .extension_of(1).with_template_replacements(),
    /// ])?;
    /// assert_eq!(catalogue.combine(1)?.to_xml(), "<div><section><span/><i/></section></div>");
    /// # Ok::<(), erp_views::ViewError>(())
    /// ```
    pub fn with_template_replacements(mut self) -> Self {
        self.template_replacements = true;
        self
    }

    /// Sets the priority (lower applies earlier; ties by id).
    pub fn with_priority(mut self, priority: i32) -> Self {
        self.priority = priority;
        self
    }

    /// Sets the active flag.
    pub fn with_active(mut self, active: bool) -> Self {
        self.active = active;
        self
    }

    /// Record id.
    pub fn id(&self) -> i64 {
        self.id
    }

    /// Record name, used in error context.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Model name (empty for views without a model, such as QWeb templates).
    pub fn model(&self) -> &str {
        &self.model
    }

    /// Raw arch.
    pub fn arch(&self) -> &str {
        &self.arch
    }

    /// Position in the inheritance tree.
    pub fn inheritance(&self) -> Inheritance {
        self.inheritance
    }

    /// Priority.
    pub fn priority(&self) -> i32 {
        self.priority
    }

    /// Active flag.
    pub fn active(&self) -> bool {
        self.active
    }
}

/// A combined view: the arch and the ids of the views applied to build it, in application order
/// (the root first). The ids are the dependency set a cache of combined archs is invalidated by.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CombinedView {
    arch: Arch,
    applied_view_ids: Vec<i64>,
    evaluation_steps: usize,
}

impl CombinedView {
    /// The combined arch.
    pub fn arch(&self) -> &Arch {
        &self.arch
    }

    /// Takes the combined arch.
    pub fn into_arch(self) -> Arch {
        self.arch
    }

    /// Views applied, in order, the root first.
    pub fn applied_view_ids(&self) -> &[i64] {
        &self.applied_view_ids
    }

    /// Serialized combined arch.
    pub fn to_xml(&self) -> String {
        self.arch.to_xml()
    }

    /// Evaluation steps the combination used, in total across every view applied, out of the
    /// [`LimitKind::EvaluationSteps`](crate::LimitKind::EvaluationSteps) limit: nodes visited
    /// while locating spec targets and by the depth check after each spec, tags, attribute names
    /// and values compared, predicate tests, attribute edits, and XPath mark tables (
    /// §6). A host can log it to see how close its views come to the limit.
    ///
    /// ```
    /// use erp_views::{LimitKind, ViewCatalogue, ViewDescriptor, ViewLimits};
    ///
    /// let catalogue = ViewCatalogue::new(vec![
    ///     ViewDescriptor::new(1, "base", "m", "<form><a/><b/></form>"),
    ///     ViewDescriptor::new(2, "ext", "m", "<b position=\"inside\"><c/></b>").extension_of(1),
    /// ])?;
    /// let combined = catalogue.combine(1)?;
    /// // Locating <b> visits form, a, b and compares their tags (6 steps); the depth check then
    /// // visits form, a, b, c (4 steps). Reading the root and spec adds 4 byte-work steps.
    /// assert_eq!(combined.evaluation_steps(), 14);
    /// assert!(combined.evaluation_steps() < ViewLimits::default().get(LimitKind::EvaluationSteps));
    /// # Ok::<(), erp_views::ViewError>(())
    /// ```
    #[must_use]
    pub fn evaluation_steps(&self) -> usize {
        self.evaluation_steps
    }
}

/// An immutable set of views, indexed for combination.
///
/// The engine is pure: the caller (the `ir.ui.view` layer) builds the catalogue from its records
/// and keeps any cache of combined results, keyed by view id and catalogue version (
/// warm target).
///
/// ```
/// use erp_views::{ViewCatalogue, ViewDescriptor};
///
/// let catalogue = ViewCatalogue::new(vec![
///     ViewDescriptor::new(1, "base", "res.partner", "<form><field name=\"name\"/></form>"),
///     ViewDescriptor::new(2, "ext", "res.partner",
///         "<field name=\"name\" position=\"after\"><field name=\"email\"/></field>")
///         .extension_of(1),
/// ])?;
/// let combined = catalogue.combine(1)?;
/// assert_eq!(combined.to_xml(), "<form><field name=\"name\"/><field name=\"email\"/></form>");
/// assert_eq!(combined.applied_view_ids(), &[1, 2]);
/// # Ok::<(), erp_views::ViewError>(())
/// ```
#[derive(Debug, Clone)]
pub struct ViewCatalogue {
    views: Vec<ViewDescriptor>,
    by_id: HashMap<i64, usize>,
    children: HashMap<i64, Vec<usize>>,
    limits: ViewLimits,
}

impl ViewCatalogue {
    /// Indexes `views` with the default limits.
    ///
    /// # Errors
    ///
    /// [`ViewError::DuplicateViewId`] when two views share an id.
    pub fn new(views: Vec<ViewDescriptor>) -> ViewResult<Self> {
        Self::with_limits(views, ViewLimits::default())
    }

    /// Indexes `views` with the given limits.
    ///
    /// # Errors
    ///
    /// [`ViewError::DuplicateViewId`] when two views share an id; [`ViewError::LimitExceeded`]
    /// for [`LimitKind::CatalogueViews`](crate::LimitKind::CatalogueViews) when there are more views than
    /// the limit.
    pub fn with_limits(views: Vec<ViewDescriptor>, limits: ViewLimits) -> ViewResult<Self> {
        if views.len() > limits.get(LimitKind::CatalogueViews) {
            return Err(limits.exceeded(LimitKind::CatalogueViews, Some(views.len())));
        }
        let mut bytes = 0usize;
        for view in &views {
            let next = bytes
                .checked_add(view.name.len())
                .and_then(|n| n.checked_add(view.model.len()))
                .and_then(|n| n.checked_add(view.arch.len()));
            bytes = next
                .filter(|&n| n <= limits.get(LimitKind::CatalogueBytes))
                .ok_or_else(|| limits.exceeded(LimitKind::CatalogueBytes, next))?;
        }
        let mut by_id = HashMap::with_capacity(views.len());
        let mut children: HashMap<i64, Vec<usize>> = HashMap::new();
        for (i, view) in views.iter().enumerate() {
            if by_id.insert(view.id, i).is_some() {
                return Err(ViewError::DuplicateViewId(view.id));
            }
            if let Some(parent) = view.inheritance.parent() {
                children.entry(parent).or_default().push(i);
            }
        }
        for list in children.values_mut() {
            list.sort_by_key(|&i| (views[i].priority, views[i].id));
        }
        Ok(Self {
            views,
            by_id,
            children,
            limits,
        })
    }

    /// The view with `id`.
    pub fn get(&self, id: i64) -> Option<&ViewDescriptor> {
        self.by_id.get(&id).map(|&i| &self.views[i])
    }

    /// The limits the catalogue combines with.
    pub fn limits(&self) -> &ViewLimits {
        &self.limits
    }

    /// The views of the catalogue, in the order they were given.
    pub fn views(&self) -> &[ViewDescriptor] {
        &self.views
    }

    /// The views whose parent is `id`, active or not, in combination order (priority, then id).
    ///
    /// ```
    /// use erp_views::{ViewCatalogue, ViewDescriptor};
    ///
    /// let catalogue = ViewCatalogue::new(vec![
    ///     ViewDescriptor::new(1, "base", "m", "<form/>"),
    ///     ViewDescriptor::new(3, "late", "m", "<form/>").extension_of(1).with_priority(20),
    ///     ViewDescriptor::new(2, "early", "m", "<form/>").extension_of(1).with_active(false),
    /// ])?;
    /// let ids: Vec<i64> = catalogue.children(1).map(|v| v.id()).collect();
    /// assert_eq!(ids, [2, 3]);
    /// # Ok::<(), erp_views::ViewError>(())
    /// ```
    pub fn children(&self, id: i64) -> impl Iterator<Item = &ViewDescriptor> + '_ {
        self.children
            .get(&id)
            .map(Vec::as_slice)
            .into_iter()
            .flatten()
            .map(|&i| &self.views[i])
    }

    /// # Errors
    ///
    /// [`ViewError::ViewNotFound`], [`ViewError::ParentNotFound`], [`ViewError::InheritanceCycle`],
    /// [`ViewError::InactiveViewOnPath`] for the path; [`ViewError::InView`] wrapping the parse or
    /// apply error of the view that failed.
    pub fn combine(&self, id: i64) -> ViewResult<CombinedView> {
        self.combine_with(id, &mut ArchSource::Fresh, Budget::new(&self.limits))
    }

    /// Combines several views; equal to combining each one alone, but each arch is parsed once
    /// and the batch shares one budget: each combination keeps its own
    /// [`LimitKind::EvaluationSteps`] and draws from the batch's [`LimitKind::BatchSteps`]
    /// (review S-2), so a batch costs at most that many steps whatever its length.
    ///
    /// # Errors
    ///
    /// The first error of [`ViewCatalogue::combine`] among `ids`; [`ViewError::LimitExceeded`]
    /// for [`LimitKind::BatchSteps`] (inside the [`ViewError::InView`] of the view whose spec
    /// crossed it) when the batch's steps pass that limit. [`LimitKind::BatchBytes`] bounds
    /// retained cache and outputs, including container capacity, before root copying and
    /// retention. An active combined arch and the next spec being parsed add their respective
    /// per-arch working allowances; they are checked before joining the retained set.
    pub fn combine_many(&self, ids: &[i64]) -> ViewResult<Vec<CombinedView>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let reserved = self
            .views
            .len()
            .checked_mul(std::mem::size_of::<Option<Arch>>())
            .and_then(|n| {
                ids.len()
                    .checked_mul(std::mem::size_of::<CombinedView>())
                    .and_then(|outputs| n.checked_add(outputs))
            });
        let max = self.limits.get(LimitKind::BatchBytes);
        let used = batch_bytes(reserved, max)?;
        let mut cache = ArchSource::Cached {
            arches: (0..self.views.len()).map(|_| None).collect(),
            used,
            max,
        };
        let mut spent = 0usize;
        let mut combined = Vec::with_capacity(ids.len());
        for &id in ids {
            let budget = Budget::within_batch(&self.limits, spent);
            let view = self.combine_with(id, &mut cache, budget)?;
            spent = spent
                .checked_add(view.evaluation_steps)
                .ok_or_else(|| self.limits.exceeded(LimitKind::BatchSteps, None))?;
            let bytes = view.arch.retained_bytes().and_then(|n| {
                view.applied_view_ids
                    .capacity()
                    .checked_mul(std::mem::size_of::<i64>())
                    .and_then(|ids| n.checked_add(ids))
            });
            cache.retain(bytes)?;
            combined.push(view);
        }
        Ok(combined)
    }

    fn index(&self, id: i64) -> ViewResult<usize> {
        self.by_id
            .get(&id)
            .copied()
            .ok_or(ViewError::ViewNotFound(id))
    }

    /// Indices from `target` up to its root, checking parents, cycles and the active flag.
    fn path(&self, target: i64) -> ViewResult<Vec<usize>> {
        let mut path = vec![self.index(target)?];
        let mut seen = HashSet::from([target]);
        let max_depth = self.limits.get(LimitKind::InheritanceDepth);
        loop {
            if path.len() > max_depth {
                return Err(self
                    .limits
                    .exceeded(LimitKind::InheritanceDepth, Some(path.len())));
            }
            let Some(&last) = path.last() else {
                return Err(ViewError::InvariantViolation("empty inheritance path"));
            };
            let view = &self.views[last];
            if !view.active {
                return Err(ViewError::InactiveViewOnPath(view.id));
            }
            let Some(parent) = view.inheritance.parent() else {
                return Ok(path);
            };
            if !seen.insert(parent) {
                let mut ids: Vec<i64> = path.iter().map(|&i| self.views[i].id).collect();
                ids.push(parent);
                return Err(ViewError::InheritanceCycle(ids));
            }
            let parent_index =
                self.by_id
                    .get(&parent)
                    .copied()
                    .ok_or(ViewError::ParentNotFound {
                        view: view.id,
                        parent,
                    })?;
            path.push(parent_index);
        }
    }

    /// Children of `parent` that take part in the combination: active extensions of the same
    /// model, and views on the requested path (`_get_inheriting_views` and `get_hierarchy`).
    fn applied_children<'a>(
        &'a self,
        parent: usize,
        on_path: &'a HashSet<i64>,
    ) -> impl DoubleEndedIterator<Item = usize> + 'a {
        let parent_view = &self.views[parent];
        self.children
            .get(&parent_view.id)
            .map(Vec::as_slice)
            .into_iter()
            .flatten()
            .copied()
            .filter(move |&i| {
                let v = &self.views[i];
                on_path.contains(&v.id)
                    || (v.active
                        && matches!(v.inheritance, Inheritance::Extension { .. })
                        && v.model == parent_view.model)
            })
    }

    fn combine_with(
        &self,
        target: i64,
        source: &mut ArchSource,
        mut budget: Budget,
    ) -> ViewResult<CombinedView> {
        let path = self.path(target)?;
        let on_path: HashSet<i64> = path.iter().map(|&i| self.views[i].id).collect();
        let root = *path
            .last()
            .ok_or(ViewError::InvariantViolation("empty inheritance path"))?;
        let root_view = &self.views[root];
        budget
            .charge_bytes(root_view.arch.len())
            .map_err(|e| in_view(root_view, e))?;
        let mut arch = source
            .get(root, root_view, &self.limits, true)?
            .into_owned();
        arch.set_caps(Caps::combined(&self.limits));
        let mut applied = vec![self.views[root].id];

        // Depth-first from the root, extensions before primary views (`_combine`): children are
        // pushed to the front in order, primary ones to the back.
        let (extensions, primaries): (Vec<usize>, Vec<usize>) = self
            .applied_children(root, &on_path)
            .partition(|&i| matches!(self.views[i].inheritance, Inheritance::Extension { .. }));
        let mut queue: VecDeque<usize> = extensions.into_iter().chain(primaries).collect();
        while let Some(i) = queue.pop_front() {
            let view = &self.views[i];
            budget
                .charge_bytes(view.arch.len())
                .map_err(|e| in_view(view, e))?;
            let spec = source.get(i, view, &self.limits, false)?;
            arch = if view.merge_primary_root {
                apply_primary_template(arch, &spec, &self.limits, &mut budget)
            } else if view.template_replacements {
                apply_template_specs(arch, &spec, &self.limits, &mut budget)
            } else {
                apply_inheritance_specs(arch, &spec, &self.limits, &mut budget)
            }
            .map_err(|e| in_view(view, e))?;
            applied.push(view.id);
            for child in self.applied_children(i, &on_path).rev() {
                match self.views[child].inheritance {
                    Inheritance::Primary { .. } => queue.push_back(child),
                    Inheritance::Extension { .. } | Inheritance::Root => queue.push_front(child),
                }
            }
        }
        Ok(CombinedView {
            arch,
            applied_view_ids: applied,
            evaluation_steps: budget.spent(),
        })
    }
}

fn in_view(view: &ViewDescriptor, error: ViewError) -> ViewError {
    ViewError::InView {
        view_id: view.id,
        view_name: view.name.clone(),
        source: Box::new(error),
    }
}

/// Parsed archs: parsed on demand for one combination, or parsed once and shared by a batch.
/// The indexed cache has a charged slab, avoiding unaccounted hash-table capacity.
enum ArchSource {
    Fresh,
    Cached {
        arches: Vec<Option<Arch>>,
        used: usize,
        max: usize,
    },
}

fn batch_bytes(next: Option<usize>, max: usize) -> ViewResult<usize> {
    next.filter(|&n| n <= max).ok_or(ViewError::LimitExceeded {
        limit: LimitKind::BatchBytes,
        max,
        actual: next,
    })
}

impl ArchSource {
    fn retain(&mut self, bytes: Option<usize>) -> ViewResult<()> {
        if let Self::Cached { used, max, .. } = self {
            *used = batch_bytes(bytes.and_then(|n| used.checked_add(n)), *max)?;
        }
        Ok(())
    }

    fn get(
        &mut self,
        index: usize,
        view: &ViewDescriptor,
        limits: &ViewLimits,
        root_copy: bool,
    ) -> ViewResult<Cow<'_, Arch>> {
        let parse = || parse_arch(&view.arch, limits, view.id).map_err(|e| in_view(view, e));
        match self {
            ArchSource::Fresh => Ok(Cow::Owned(parse()?)),
            ArchSource::Cached { arches, used, max } => {
                let slot = arches.get_mut(index).ok_or(ViewError::InvariantViolation(
                    "view outside parsed-arch cache",
                ))?;
                if slot.is_none() {
                    let parsed = parse()?;
                    *used = batch_bytes(
                        parsed.retained_bytes().and_then(|n| used.checked_add(n)),
                        *max,
                    )?;
                    *slot = Some(parsed);
                }
                let arch = slot.as_ref().ok_or(ViewError::InvariantViolation(
                    "parsed-arch cache entry is empty",
                ))?;
                if root_copy {
                    // Reject before cloning the cached root. Subsequent in-progress edits have
                    // their own CombinedBytes/CombinedNodes caps and are checked before retention.
                    batch_bytes(
                        arch.retained_bytes().and_then(|n| used.checked_add(n)),
                        *max,
                    )?;
                }
                Ok(Cow::Borrowed(arch))
            }
        }
    }
}
