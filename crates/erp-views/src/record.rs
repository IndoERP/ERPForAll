//! The record side of views: the `mode` rules of
//! `ir.ui.view`, a database row turned into a [`ViewDescriptor`], the validation
//! targets of a write and the typed errors of the record layer (
//! 059). Pure: the `ir.ui.view` model in addon `base` reads the rows and maps the errors.

use std::collections::HashSet;
use std::fmt;

use crate::arch::NodeOrigin;
use crate::combine::{CombinedView, Inheritance, ViewCatalogue, ViewDescriptor};
use crate::error::{LimitKind, ViewError};
use crate::validate::ArchIssue;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ViewMode {
    /// A view of its own: a root, or a new view built on its parent's combined arch.
    Primary,
    /// Modifies its parent in place for every view built on it.
    Extension,
}

impl ViewMode {
    /// The stored value (`primary`, `extension`).
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Primary => "primary",
            Self::Extension => "extension",
        }
    }

    /// The mode a stored value names.
    ///
    /// # Errors
    ///
    /// [`ViewRecordError::InvalidMode`] for any other text.
    pub fn parse(value: &str) -> Result<Self, ViewRecordError> {
        match value {
            "primary" => Ok(Self::Primary),
            "extension" => Ok(Self::Extension),
            other => Err(ViewRecordError::InvalidMode {
                value: other.to_owned(),
            }),
        }
    }
}

impl fmt::Display for ViewMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The mode of a view being created: the `mode` given, else `extension` with a parent and
/// `primary` without.
///
/// ```
/// use erp_views::record::{ViewMode, mode_on_create};
/// assert_eq!(mode_on_create(None, None), ViewMode::Primary);
/// assert_eq!(mode_on_create(Some(7), None), ViewMode::Extension);
/// assert_eq!(mode_on_create(Some(7), Some(ViewMode::Primary)), ViewMode::Primary);
/// ```
pub fn mode_on_create(inherit_id: Option<i64>, explicit: Option<ViewMode>) -> ViewMode {
    match (explicit, inherit_id) {
        (Some(mode), _) => mode,
        (None, Some(_)) => ViewMode::Extension,
        (None, None) => ViewMode::Primary,
    }
}

/// The mode a write of `inherit_id` gives one record that `had_parent` before, or `None` when the
/// record keeps its mode: the `mode` written, else `primary` when the parent is cleared,
/// `extension` when a view without parent gets one; a re-parented view keeps its mode.
/// Decided per record, never for a whole batch (BUG-074). Only whether
/// the record had a parent counts, so the old state is a `bool` and cannot be swapped with the new
/// parent.
///
/// ```
/// use erp_views::record::{ViewMode, mode_on_write};
/// assert_eq!(mode_on_write(true, None, None), Some(ViewMode::Primary));
/// assert_eq!(mode_on_write(false, Some(1), None), Some(ViewMode::Extension));
/// assert_eq!(mode_on_write(true, Some(2), None), None);
/// assert_eq!(mode_on_write(true, Some(2), Some(ViewMode::Extension)), Some(ViewMode::Extension));
/// ```
pub fn mode_on_write(
    had_parent: bool,
    new_inherit: Option<i64>,
    explicit: Option<ViewMode>,
) -> Option<ViewMode> {
    match (explicit, had_parent, new_inherit) {
        (Some(mode), _, _) => Some(mode),
        (None, _, None) => Some(ViewMode::Primary),
        (None, false, Some(_)) => Some(ViewMode::Extension),
        (None, true, Some(_)) => None,
    }
}

/// The engine's [`Inheritance`] of a stored row.
///
/// # Errors
///
/// [`ViewRecordError::InvariantViolation`] for `extension` without a parent: the table's
/// `_inheritance_mode` check makes that row impossible, so it is never read as a root.
pub fn inheritance(id_parent: Option<i64>, mode: ViewMode) -> Result<Inheritance, ViewRecordError> {
    match (id_parent, mode) {
        (None, ViewMode::Primary) => Ok(Inheritance::Root),
        (Some(parent), ViewMode::Primary) => Ok(Inheritance::Primary { parent }),
        (Some(parent), ViewMode::Extension) => Ok(Inheritance::Extension { parent }),
        (None, ViewMode::Extension) => Err(ViewRecordError::InvariantViolation(
            "an extension view without a parent (the _inheritance_mode check forbids it)",
        )),
    }
}

/// One `ir_ui_view` row as the record layer reads it, turned into a [`ViewDescriptor`] without
/// copying its strings.
///
/// ```
/// use erp_views::Inheritance;
/// use erp_views::record::{ViewMode, ViewRow};
///
/// let view = ViewRow::new(2, ViewMode::Extension)
///     .name("ext")
///     .model(Some("res.partner".to_owned()))
///     .arch("<form position=\"inside\"/>")
///     .inherit_id(Some(1))
///     .priority(20)
///     .into_descriptor()?;
/// assert_eq!(view.inheritance(), Inheritance::Extension { parent: 1 });
/// assert_eq!(view.priority(), 20);
/// # Ok::<(), erp_views::record::ViewRecordError>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use = "turn it into a descriptor with `into_descriptor`"]
pub struct ViewRow {
    id: i64,
    mode: ViewMode,
    name: String,
    model: Option<String>,
    arch: String,
    inherit_id: Option<i64>,
    priority: i32,
    active: bool,
}

impl ViewRow {
    /// Row `id` with `mode`; no parent, priority 16 and active until set.
    pub fn new(id: i64, mode: ViewMode) -> Self {
        Self {
            id,
            mode,
            name: String::new(),
            model: None,
            arch: String::new(),
            inherit_id: None,
            priority: 16,
            active: true,
        }
    }

    /// `name`.
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }

    /// `model` (`None` for a view without a model, review A-T4).
    pub fn model(mut self, model: Option<String>) -> Self {
        self.model = model;
        self
    }

    /// `arch_db`.
    pub fn arch(mut self, arch: impl Into<String>) -> Self {
        self.arch = arch.into();
        self
    }

    /// `inherit_id`.
    pub fn inherit_id(mut self, parent: Option<i64>) -> Self {
        self.inherit_id = parent;
        self
    }

    /// `priority`.
    pub fn priority(mut self, priority: i32) -> Self {
        self.priority = priority;
        self
    }

    /// `active`.
    pub fn active(mut self, active: bool) -> Self {
        self.active = active;
        self
    }

    /// The engine's view.
    ///
    /// # Errors
    ///
    /// As [`inheritance`].
    pub fn into_descriptor(self) -> Result<ViewDescriptor, ViewRecordError> {
        let inheritance = inheritance(self.inherit_id, self.mode)?;
        // the engine's descriptor names a view without model by an empty model
        // (`ViewDescriptor::model`): the conversion is the only place that knows it
        let model = self.model.unwrap_or_default();
        let view = ViewDescriptor::new(self.id, self.name, model, self.arch)
            .with_priority(self.priority)
            .with_active(self.active);
        Ok(match inheritance {
            Inheritance::Root => view,
            Inheritance::Primary { parent } => view.primary_of(parent),
            Inheritance::Extension { parent } => view.extension_of(parent),
        })
    }
}

/// What a write of the views `written` must check.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Targets {
    /// The written views that are active: each is combined and its arch validated.
    pub validated: Vec<i64>,
    /// The other views whose combination the write can break: every active primary view reached
    /// through extension children only from the nearest primary ancestor-or-self of each written
    /// view, and that nearest primary itself when only archived written views lead to it (an
    /// active written view's combination already combines it, review R-4).
    pub combined: Vec<i64>,
    /// The written views that are archived: each is parsed on its own ([`parse_alone`]) and, when
    /// it is a root primary view, its arch validated.
    pub standalone: Vec<i64>,
}

/// The [`Targets`] of a write of `written` (deduplicated, in first-seen order).
///
/// # Errors
///
/// [`ViewError::ViewNotFound`] for a written view that is not in the catalogue,
/// [`ViewError::ParentNotFound`] for a missing parent, [`ViewError::InheritanceCycle`] when the
/// parents of a written view loop, [`ViewError::LimitExceeded`] for
/// [`LimitKind::InheritanceDepth`] on a path too long.
///
/// ```
/// use erp_views::ViewCatalogue;
/// use erp_views::record::{ViewMode, ViewRow, validation_targets};
///
/// let row = |id, mode, parent| ViewRow::new(id, mode).arch("<div/>").inherit_id(parent);
/// let catalogue = ViewCatalogue::new(vec![
///     row(1, ViewMode::Primary, None).into_descriptor()?,
///     row(2, ViewMode::Extension, Some(1)).into_descriptor()?,
///     row(3, ViewMode::Primary, Some(2)).into_descriptor()?,
/// ])?;
/// let targets = validation_targets(&catalogue, &[2])?;
/// assert_eq!(targets.validated, [2]);
/// // combining 2 combines its root 1: only the sibling primary 3 is added (review R-4)
/// assert_eq!(targets.combined, [3]);
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub fn validation_targets(
    catalogue: &ViewCatalogue,
    written: &[i64],
) -> Result<Targets, ViewError> {
    let max_depth = catalogue_depth_limit(catalogue);
    let mut targets = Targets::default();
    let mut validated: HashSet<i64> = HashSet::new();
    let mut combined: HashSet<i64> = HashSet::new();
    let mut standalone: HashSet<i64> = HashSet::new();
    let mut roots: HashSet<i64> = HashSet::new();
    // the nearest primaries of active written views: combining the written view combines its
    // nearest primary's whole tree, so that primary is not combined again (review R-4)
    let mut covered: HashSet<i64> = HashSet::new();
    for &id in written {
        let view = catalogue.get(id).ok_or(ViewError::ViewNotFound(id))?;
        let root = nearest_primary(catalogue, id, max_depth)?;
        if view.active() {
            if validated.insert(id) {
                targets.validated.push(id);
            }
            covered.insert(root);
        } else if standalone.insert(id) {
            targets.standalone.push(id);
        }
        if !roots.insert(root) {
            continue;
        }
        let mut stack = vec![root];
        let mut seen: HashSet<i64> = HashSet::from([root]);
        let mut first = true;
        while let Some(current) = stack.pop() {
            let is_target = first
                || catalogue
                    .get(current)
                    .is_some_and(|v| matches!(v.inheritance(), Inheritance::Primary { .. }));
            if is_target
                && catalogue.get(current).is_some_and(ViewDescriptor::active)
                && !validated.contains(&current)
                && combined.insert(current)
            {
                targets.combined.push(current);
            }
            if !first && is_target {
                // a primary view starts its own tree: its children are not siblings of `root`
                continue;
            }
            first = false;
            let children: Vec<i64> = catalogue
                .children(current)
                .filter(|v| v.active())
                .map(|v| v.id())
                .collect();
            // pushed in reverse so the walk visits children in combination order
            for child in children.into_iter().rev() {
                if seen.insert(child) {
                    stack.push(child);
                }
            }
        }
    }
    // an archived written view keeps its nearest primary combined only while no active written
    // view of the same tree combines it (whatever the order of `written`)
    targets.combined.retain(|id| !covered.contains(id));
    Ok(targets)
}

/// The id of the nearest primary view at or above `id` (the root of its extension chain).
fn nearest_primary(catalogue: &ViewCatalogue, id: i64, max_depth: usize) -> Result<i64, ViewError> {
    let mut path = vec![id];
    let mut current = catalogue.get(id).ok_or(ViewError::ViewNotFound(id))?;
    loop {
        let parent = match current.inheritance() {
            Inheritance::Root | Inheritance::Primary { .. } => return Ok(current.id()),
            Inheritance::Extension { parent } => parent,
        };
        if path.contains(&parent) {
            path.push(parent);
            return Err(ViewError::InheritanceCycle(path));
        }
        path.push(parent);
        if path.len() > max_depth {
            return Err(ViewError::LimitExceeded {
                limit: LimitKind::InheritanceDepth,
                max: max_depth,
                actual: Some(path.len()),
            });
        }
        current = catalogue.get(parent).ok_or(ViewError::ParentNotFound {
            view: current.id(),
            parent,
        })?;
    }
}

fn catalogue_depth_limit(catalogue: &ViewCatalogue) -> usize {
    catalogue.limits().get(LimitKind::InheritanceDepth)
}

/// The arch of view `id` of `catalogue` parsed on its own, as if it were an active root (the
/// check of an archived written view, [`Targets::standalone`]): a parse error or a limit is
/// reported with the view's id; the node origins name the view. `validate` in the result says
/// whether the arch is a whole view (a root primary view, whose arch the caller validates) or a
/// list of specs (an extension or a primary view on a parent, only parsed).
///
/// # Errors
///
/// [`ViewError::ViewNotFound`] for a view that is not in the catalogue; the parse errors and
/// limits of [`ViewCatalogue::combine`].
///
/// ```
/// use erp_views::ViewCatalogue;
/// use erp_views::record::{Alone, ViewMode, ViewRow, parse_alone};
///
/// let catalogue = ViewCatalogue::new(vec![
///     ViewRow::new(1, ViewMode::Primary).arch("<form/>").active(false).into_descriptor()?,
///     ViewRow::new(2, ViewMode::Primary).arch("<form>").active(false).into_descriptor()?,
/// ])?;
/// let alone = parse_alone(&catalogue, 1)?;
/// assert!(matches!(alone, Alone::WholeView(_)));
/// assert_eq!(alone.view().arch().root().tag(), "form");
/// assert!(parse_alone(&catalogue, 2).is_err());
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub fn parse_alone(catalogue: &ViewCatalogue, id: i64) -> Result<Alone, ViewError> {
    let view = catalogue.get(id).ok_or(ViewError::ViewNotFound(id))?;
    let whole = view.inheritance() == Inheritance::Root;
    let alone = ViewDescriptor::new(view.id(), view.name(), view.model(), view.arch())
        .with_priority(view.priority());
    let parsed = ViewCatalogue::with_limits(vec![alone], *catalogue.limits())?.combine(id)?;
    Ok(if whole {
        Alone::WholeView(parsed)
    } else {
        Alone::Specs(parsed)
    })
}

/// Whether view `id` of `catalogue` has a spec with `position="replace"` or
/// `position="attributes"`, at any depth: a create of it is validated whole
/// ([`crate::validate::Scope::Whole`]).
/// An arch without either word or an entity reference is not parsed;
/// when references are present, the decision uses decoded attribute values (review CV-2).
///
/// # Errors
///
/// [`ViewError::ViewNotFound`]; the parse errors and limits of [`parse_alone`].
pub fn widens_validation(catalogue: &ViewCatalogue, id: i64) -> Result<bool, ViewError> {
    let view = catalogue.get(id).ok_or(ViewError::ViewNotFound(id))?;
    let text = view.arch();
    // An entity can spell part or all of either position. Only entity-free text can safely
    // take the negative fast path; parsing below decides from the decoded attributes.
    if !text.contains('&') && !text.contains("replace") && !text.contains("attributes") {
        return Ok(false);
    }
    let parsed = parse_alone(catalogue, id)?;
    let arch = parsed.view().arch();
    Ok(arch.walk(arch.root_id()).any(|(node, _)| {
        !arch.is_sentinel(node)
            && matches!(arch.attr(node, "position"), Some("replace" | "attributes"))
    }))
}

/// An archived view parsed on its own ([`parse_alone`]): which of the two it is decides whether
/// its arch is validated, so a list of specs cannot be validated as a whole view by mistake
/// (review T-8). Not `#[non_exhaustive]`: an arch is a whole view or specs, and a caller must
/// handle both.
#[derive(Debug)]
pub enum Alone {
    /// The arch of a root primary view: a whole view, validate it.
    WholeView(CombinedView),
    /// The specs of a view with a parent, parsed only (they apply to a parent's arch).
    Specs(CombinedView),
}

impl Alone {
    /// The parsed arch.
    pub fn view(&self) -> &CombinedView {
        match self {
            Self::WholeView(view) | Self::Specs(view) => view,
        }
    }
}

/// The parents of `id` loop (`_check_000_inheritance`): `Some` with the
/// ids of the loop (the first one repeated last), `None` when the path ends at a root. A missing
/// parent ends the path (the combination reports it).
///
/// # Errors
///
/// [`ViewError::LimitExceeded`] for [`LimitKind::InheritanceDepth`].
///
/// ```
/// use erp_views::ViewCatalogue;
/// use erp_views::record::{ViewMode, ViewRow, inheritance_cycle};
///
/// let ext = |id, parent| ViewRow::new(id, ViewMode::Extension).arch("<a/>").inherit_id(Some(parent));
/// let catalogue = ViewCatalogue::new(vec![ext(1, 2).into_descriptor()?, ext(2, 1).into_descriptor()?])?;
/// assert_eq!(inheritance_cycle(&catalogue, 1)?, Some(vec![1, 2, 1]));
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub fn inheritance_cycle(
    catalogue: &ViewCatalogue,
    id: i64,
) -> Result<Option<Vec<i64>>, ViewError> {
    let max_depth = catalogue_depth_limit(catalogue);
    let mut path = vec![id];
    let mut current = id;
    loop {
        let Some(parent) = catalogue
            .get(current)
            .and_then(|v| v.inheritance().parent())
        else {
            return Ok(None);
        };
        if let Some(start) = path.iter().position(|&v| v == parent) {
            let mut cycle = path.split_off(start);
            cycle.push(parent);
            return Ok(Some(cycle));
        }
        path.push(parent);
        if path.len() > max_depth {
            return Err(ViewError::LimitExceeded {
                limit: LimitKind::InheritanceDepth,
                max: max_depth,
                actual: Some(path.len()),
            });
        }
        current = parent;
    }
}

/// The view an error is reported against: the one whose arch holds the faulty node or spec.
///
/// ```
/// use erp_views::record::ViewLabel;
///
/// let label = ViewLabel::new(7, "partner.form")
///     .xmlid(Some("base.view_partner_form".into()))
///     .file(Some("base/views/res_partner_views.xml".into()))
///     .line(Some(12));
/// assert_eq!(
///     label.to_string(),
///     "partner.form (base.view_partner_form) in base/views/res_partner_views.xml, arch line 12"
/// );
/// assert_eq!(ViewLabel::new(7, "v").line(Some(3)).to_string(), "v, arch line 3");
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use]
pub struct ViewLabel {
    id: i64,
    name: String,
    model: Option<String>,
    xmlid: Option<String>,
    file: Option<String>,
    line: Option<u32>,
}

impl ViewLabel {
    /// View `id` named `name`, without external id, file or line.
    pub fn new(id: i64, name: impl Into<String>) -> Self {
        Self {
            id,
            name: name.into(),
            model: None,
            xmlid: None,
            file: None,
            line: None,
        }
    }

    /// Its model (`None` for a view without one, a QWeb template).
    pub fn model(mut self, model: Option<String>) -> Self {
        self.model = model;
        self
    }

    /// Its external id.
    pub fn xmlid(mut self, xmlid: Option<String>) -> Self {
        self.xmlid = xmlid;
        self
    }

    /// The data file it was loaded from (`arch_fs`: `<module>/<path>`).
    pub fn file(mut self, file: Option<String>) -> Self {
        self.file = file;
        self
    }

    /// The line of the faulty node in its arch.
    pub fn line(mut self, line: Option<u32>) -> Self {
        self.line = line;
        self
    }

    /// The view id.
    pub fn id(&self) -> i64 {
        self.id
    }

    /// The view name.
    pub fn view_name(&self) -> &str {
        &self.name
    }

    /// The view's model, if it has one.
    pub fn view_model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    /// The external id, if any.
    pub fn view_xmlid(&self) -> Option<&str> {
        self.xmlid.as_deref()
    }

    /// The data file, if known.
    pub fn view_file(&self) -> Option<&str> {
        self.file.as_deref()
    }

    /// The line in the arch, if known.
    pub fn view_line(&self) -> Option<u32> {
        self.line
    }
}

impl fmt::Display for ViewLabel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.name)?;
        if let Some(xmlid) = &self.xmlid {
            write!(f, " ({xmlid})")?;
        }
        // the line counts in the view's arch, not in its data file: the file does not record
        // where the arch starts
        if let Some(file) = &self.file {
            write!(f, " in {file}")?;
        }
        match self.line {
            Some(line) => write!(f, ", arch line {line}"),
            None => Ok(()),
        }
    }
}

/// Why a view's combination or arch is invalid.
///
/// The message is the innermost error's, and so is the [`std::error::Error::source`]: for
/// [`InvalidView::Combine`] the error under the [`ViewError::InView`] wrappers, not the wrapper
/// (review R-11), so a reader of the chain reaches the error that names what went wrong. The
/// wrapper, with the view it names, stays in the variant ([`InvalidView::origin_view`]).
///
/// ```
/// use std::error::Error as _;
/// use erp_views::ViewError;
/// use erp_views::record::InvalidView;
///
/// let leaf = ViewError::TargetNotFound("<e4>".into());
/// let invalid = InvalidView::Combine(ViewError::InView {
///     view_id: 2,
///     view_name: "ext".into(),
///     source: Box::new(leaf.clone()),
/// });
/// assert_eq!(invalid.source().and_then(|e| e.downcast_ref::<ViewError>()), Some(&leaf));
/// assert_eq!(invalid.to_string(), "Element '<e4>' cannot be located in parent view");
/// assert_eq!(invalid.origin_view(), Some(2));
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum InvalidView {
    /// The combination failed (a spec that matches nothing, a parse error, a limit, ...).
    Combine(ViewError),
    /// The combined arch fails a check of [`crate::validate::validate_combined`].
    Arch(ArchIssue),
}

impl fmt::Display for InvalidView {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Combine(error) => fmt::Display::fmt(leaf(error), f),
            Self::Arch(issue) => fmt::Display::fmt(issue, f),
        }
    }
}

impl std::error::Error for InvalidView {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Combine(error) => Some(leaf(error)),
            Self::Arch(issue) => Some(issue),
        }
    }
}

impl InvalidView {
    /// The view whose arch holds the faulty spec or node, when the error says: the innermost
    /// [`ViewError::InView`] of a combination error, the origin of an arch issue.
    pub fn origin_view(&self) -> Option<i64> {
        match self {
            Self::Combine(error) => innermost_view(error),
            Self::Arch(issue) => Some(issue.origin().view_id()),
        }
    }

    /// The origin of the faulty node of an arch issue (a combination error has no node).
    pub fn origin(&self) -> Option<NodeOrigin> {
        match self {
            Self::Combine(_) => None,
            Self::Arch(issue) => Some(issue.origin()),
        }
    }
}

/// The error under the [`ViewError::InView`] wrappers.
fn leaf(error: &ViewError) -> &ViewError {
    let mut current = error;
    while let ViewError::InView { source, .. } = current {
        current = source;
    }
    current
}

fn innermost_view(error: &ViewError) -> Option<i64> {
    let mut current = error;
    let mut found = None;
    while let ViewError::InView {
        view_id, source, ..
    } = current
    {
        found = Some(*view_id);
        current = source;
    }
    found
}

/// Errors of the `ir.ui.view` record layer. The `base` addon maps each to its ORM error kind:
/// `Validation` for every variant but [`ViewRecordError::InvariantViolation`] and
/// [`ViewRecordError::InvalidMode`] (`Programming`: a stored value the table cannot hold).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ViewRecordError {
    /// The `inherit_id` chain loops.
    #[error("You cannot create recursive inherited views.")]
    RecursiveInheritance {
        /// The loop, its first view repeated last.
        views: Vec<i64>,
    },
    /// An extension view with `group_ids`.
    #[error(
        "Inherited view cannot have 'groups' defined on the record. Use 'groups' attributes inside \
         the view definition"
    )]
    GroupsOnExtension,
    /// A view created without `type`, parent or arch.
    #[error("Missing view architecture.")]
    MissingArch,
    /// The arch's root tag is not a view type.
    #[error(
        "Invalid view type: '{tag}'.\nYou might have used an invalid starting tag in the \
         architecture.\nAllowed types are: {}",
        allowed.join(", ")
    )]
    InvalidType {
        /// The root tag.
        tag: String,
        /// The `type` selection values.
        allowed: Vec<String>,
    },
    /// A stored `mode` that is neither `primary` nor `extension`.
    #[error("invalid view mode '{value}': expected 'primary' or 'extension'")]
    InvalidMode {
        /// The stored text.
        value: String,
    },
    /// A view's combination or arch is invalid.
    #[error("Error while validating view {view}:\n{source}")]
    Invalid {
        /// The view whose arch holds the faulty node or spec.
        view: Box<ViewLabel>,
        /// The name of the view that was written (the error context's `name`, BUG-071).
        written: String,
        /// What is wrong.
        #[source]
        source: Box<InvalidView>,
    },
    /// A view type a request names that the `type` selection does not have (a client's
    /// `view_type`, not an arch's root tag: that is [`ViewRecordError::InvalidType`]).
    #[error("Unknown view type '{value}'. Allowed types are: {}", allowed.join(", "))]
    UnknownViewType {
        /// The requested type.
        value: String,
        /// The `type` selection values.
        allowed: Vec<String>,
    },
    /// A limit of the record layer.
    #[error("{limit} is {actual} {}, more than the {max} allowed", limit.unit())]
    Limit {
        /// What is too large.
        limit: RecordLimit,
        /// The size found.
        actual: usize,
        /// The limit.
        max: usize,
    },
    /// A record-layer invariant does not hold. Never expected; report it as a bug.
    #[error("internal error in the view records: {0}")]
    InvariantViolation(&'static str),
}

/// What a [`ViewRecordError::Limit`] bounds.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum RecordLimit {
    /// The bytes of one written arch ([`LimitKind::ArchBytes`] of the engine).
    ArchBytes,
    /// The views of the inheritance trees one check reads ([`LimitKind::CatalogueViews`]).
    CatalogueViews,
    /// The bytes of the metadata and archs of the inheritance trees one check reads
    /// ([`LimitKind::CatalogueBytes`] of the engine).
    CatalogueBytes,
    /// The distinct group references of the archs one check validates
    /// ([`LimitKind::CheckGroupRefs`] of the engine).
    GroupRefs,
    /// The groups with an external id that the guard-implication check loads (a bound of the
    /// record layer alone, like [`RecordLimit::ViewRefBytes`]).
    ImplicationGroups,
    /// External-id aliases fetched by one view check, bounded by CatalogueViews.
    Aliases,
    /// External-id alias text fetched by one view check, bounded by CatalogueBytes.
    AliasBytes,
    /// The bytes of a `<type>_view_ref` context value.
    ViewRefBytes {
        /// The context key (`form_view_ref`, ...).
        key: String,
    },
}

impl RecordLimit {
    /// The engine limit this record limit applies ([`crate::ViewLimits`] holds its value);
    /// `None` for [`RecordLimit::ViewRefBytes`], a limit of the record layer alone. The record
    /// layer keeps its own variants rather than wrapping [`LimitKind`] (review T-13): only these
    /// record-layer limits each keep their own message.
    pub fn engine_kind(&self) -> Option<LimitKind> {
        match self {
            Self::ArchBytes => Some(LimitKind::ArchBytes),
            Self::CatalogueViews => Some(LimitKind::CatalogueViews),
            Self::CatalogueBytes => Some(LimitKind::CatalogueBytes),
            Self::GroupRefs => Some(LimitKind::CheckGroupRefs),
            Self::Aliases => Some(LimitKind::CatalogueViews),
            Self::AliasBytes => Some(LimitKind::CatalogueBytes),
            Self::ViewRefBytes { .. } | Self::ImplicationGroups => None,
        }
    }

    /// The unit the sizes count.
    pub fn unit(&self) -> &'static str {
        match self {
            Self::ArchBytes | Self::ViewRefBytes { .. } => "bytes",
            Self::CatalogueViews => "views or more",
            Self::CatalogueBytes | Self::AliasBytes => "bytes or more",
            Self::Aliases => "aliases or more",
            Self::GroupRefs => "distinct group references",
            Self::ImplicationGroups => "groups or more",
        }
    }
}

impl fmt::Display for RecordLimit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ArchBytes => f.write_str("the view architecture"),
            // one check reads the trees of all its written views at once: the limits count them
            // together, and the message says so (review D-4)
            Self::CatalogueViews => f.write_str(
                "the size of the inheritance trees this check reads (the trees of all its \
                 written views together)",
            ),
            Self::CatalogueBytes => f.write_str(
                "the text of the inheritance trees this check reads (the trees of all its \
                 written views together)",
            ),
            Self::GroupRefs => {
                f.write_str("the distinct group-reference count of the checked views")
            }
            Self::ImplicationGroups => {
                f.write_str("the groups with an external id that the group-implication check loads")
            }
            Self::Aliases => f.write_str("the external-id aliases of the checked views"),
            Self::AliasBytes => f.write_str("the external-id text of the checked views"),
            Self::ViewRefBytes { key } => write!(f, "context['{key}']"),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

    use super::*;
    use crate::error::XpathErrorKind;

    fn catalogue(rows: Vec<ViewRow>) -> ViewCatalogue {
        ViewCatalogue::new(
            rows.into_iter()
                .map(|r| r.into_descriptor().unwrap())
                .collect(),
        )
        .unwrap()
    }

    fn row(id: i64, mode: ViewMode, parent: Option<i64>) -> ViewRow {
        ViewRow::new(id, mode).arch("<div/>").inherit_id(parent)
    }

    /// The tree of `test_check_primary_when_update_siblins_inherited_tree`: P1 with E1..E5 and a
    /// primary P2 on E2; a write of E4 reaches P1 and P2, a write of P2 only P2's own tree.
    #[test]
    fn targets_follow_extension_children_only() {
        use ViewMode::{Extension as E, Primary as P};
        let c = catalogue(vec![
            row(1, P, None),
            row(2, E, Some(1)),
            row(3, E, Some(1)),
            row(4, E, Some(1)),
            row(5, E, Some(1)),
            row(6, P, Some(3)),
            row(7, E, Some(1)),
            row(8, E, Some(6)),
            row(9, P, Some(6)),
        ]);
        let t = validation_targets(&c, &[5]).unwrap();
        assert_eq!(t.validated, [5]);
        // primaries below P2 (9) are P2's siblings, not P1's; P1 itself is not combined again:
        // combining 5 combines P1's tree (review R-4)
        assert_eq!(t.combined, [6]);
        let t = validation_targets(&c, &[6]).unwrap();
        assert_eq!(t.validated, [6]);
        assert_eq!(t.combined, [9]);
        // a batch deduplicates
        let t = validation_targets(&c, &[5, 4, 5]).unwrap();
        assert_eq!(t.validated, [5, 4]);
        assert_eq!(t.combined, [6]);
    }

    /// The nearest primary of an archived written view is combined, since the archived view is
    /// checked alone; it is left out as soon as an active written view of the same tree combines
    /// it, in either order (review R-4).
    #[test]
    fn the_root_is_combined_only_for_archived_written_views() {
        use ViewMode::{Extension as E, Primary as P};
        let c = catalogue(vec![
            row(1, P, None),
            row(2, E, Some(1)).active(false),
            row(3, E, Some(1)),
        ]);
        assert_eq!(validation_targets(&c, &[2]).unwrap().combined, [1]);
        assert!(validation_targets(&c, &[3]).unwrap().combined.is_empty());
        for written in [[2, 3], [3, 2]] {
            let t = validation_targets(&c, &written).unwrap();
            assert_eq!(t.validated, [3]);
            assert_eq!(t.standalone, [2]);
            assert!(t.combined.is_empty(), "{written:?}: {t:?}");
        }
    }

    #[test]
    fn inactive_views_are_not_targets() {
        use ViewMode::{Extension as E, Primary as P};
        let c = catalogue(vec![
            row(1, P, None),
            row(2, E, Some(1)).active(false),
            row(3, P, Some(2)).active(false),
        ]);
        let t = validation_targets(&c, &[2]).unwrap();
        assert!(t.validated.is_empty());
        assert_eq!(t.combined, [1]);
        // archived written views are checked alone (review A-S1), once each
        assert_eq!(t.standalone, [2]);
        let t = validation_targets(&c, &[3, 2, 3]).unwrap();
        assert_eq!(t.standalone, [3, 2]);
    }

    /// An archived view's arch is parsed alone; a root primary is a whole view to validate, an
    /// extension or a primary on a parent only a list of specs (review A-S1).
    #[test]
    fn archived_views_parse_alone() {
        use ViewMode::{Extension as E, Primary as P};
        let c = catalogue(vec![
            row(1, P, None).arch("<form/>").active(false),
            row(2, E, Some(1))
                .arch("<form position=\"inside\"/>")
                .active(false),
            row(3, P, Some(1))
                .arch("<xpath expr=\"//form\"/>")
                .active(false),
            row(4, P, None).arch("<form><div></form>").active(false),
        ]);
        let alone = parse_alone(&c, 1).unwrap();
        assert!(matches!(alone, Alone::WholeView(_)));
        assert_eq!(alone.view().arch().root().tag(), "form");
        assert!(matches!(parse_alone(&c, 2).unwrap(), Alone::Specs(_)));
        let specs = parse_alone(&c, 3).unwrap();
        assert!(matches!(specs, Alone::Specs(_)));
        assert_eq!(specs.view().arch().root().tag(), "xpath");
        let err = parse_alone(&c, 4).unwrap_err();
        assert_eq!(innermost_view(&err), Some(4), "{err:?}");
        assert_eq!(parse_alone(&c, 9).unwrap_err(), ViewError::ViewNotFound(9));
    }

    #[test]
    fn record_limits_name_what_is_too_large() {
        let error = ViewRecordError::Limit {
            limit: RecordLimit::ViewRefBytes {
                key: "form_view_ref".into(),
            },
            actual: 300,
            max: 256,
        };
        assert_eq!(
            error.to_string(),
            "context['form_view_ref'] is 300 bytes, more than the 256 allowed"
        );
        assert_eq!(RecordLimit::CatalogueViews.unit(), "views or more");
        // the tree limits count every written view's tree together: the message says so, since
        // the view it is reported against is only the first written one (review D-4)
        let error = ViewRecordError::Limit {
            limit: RecordLimit::CatalogueViews,
            actual: 10_001,
            max: 10_000,
        };
        assert_eq!(
            error.to_string(),
            "the size of the inheritance trees this check reads (the trees of all its written \
             views together) is 10001 views or more, more than the 10000 allowed"
        );
    }

    #[test]
    fn cycles_and_missing_parents_are_errors() {
        use ViewMode::Extension as E;
        let c = catalogue(vec![
            row(1, E, Some(2)),
            row(2, E, Some(3)),
            row(3, E, Some(1)),
        ]);
        assert_eq!(
            validation_targets(&c, &[1]),
            Err(ViewError::InheritanceCycle(vec![1, 2, 3, 1]))
        );
        assert_eq!(inheritance_cycle(&c, 2).unwrap(), Some(vec![2, 3, 1, 2]));
        // self loop
        let c = catalogue(vec![row(1, E, Some(1))]);
        assert_eq!(inheritance_cycle(&c, 1).unwrap(), Some(vec![1, 1]));
        // a cycle above a tail
        let c = catalogue(vec![
            row(1, E, Some(2)),
            row(2, E, Some(3)),
            row(3, E, Some(2)),
        ]);
        assert_eq!(inheritance_cycle(&c, 1).unwrap(), Some(vec![2, 3, 2]));
        let c = catalogue(vec![row(1, E, Some(9))]);
        assert_eq!(
            validation_targets(&c, &[1]),
            Err(ViewError::ParentNotFound { view: 1, parent: 9 })
        );
        assert_eq!(inheritance_cycle(&c, 1).unwrap(), None);
        assert_eq!(
            validation_targets(&c, &[5]),
            Err(ViewError::ViewNotFound(5))
        );
    }

    #[test]
    fn deep_paths_hit_the_depth_limit() {
        let mut rows = vec![row(1, ViewMode::Primary, None)];
        for id in 2..=200 {
            rows.push(row(id, ViewMode::Extension, Some(id - 1)));
        }
        let c = catalogue(rows);
        let limit = ViewError::LimitExceeded {
            limit: LimitKind::InheritanceDepth,
            max: 128,
            actual: Some(129),
        };
        assert_eq!(validation_targets(&c, &[200]), Err(limit.clone()));
        assert_eq!(inheritance_cycle(&c, 200), Err(limit.clone()));
        assert_eq!(c.combine(200).map(|_| ()), Err(limit));
        // within the limit: 128 is validated, which combines its root (review R-4)
        let t = validation_targets(&c, &[128]).unwrap();
        assert_eq!(t.validated, [128]);
        assert!(t.combined.is_empty());
    }

    #[test]
    fn extension_without_parent_is_an_invariant_violation() {
        assert_eq!(
            inheritance(None, ViewMode::Extension),
            Err(ViewRecordError::InvariantViolation(
                "an extension view without a parent (the _inheritance_mode check forbids it)"
            ))
        );
        assert_eq!(
            ViewMode::parse("other"),
            Err(ViewRecordError::InvalidMode {
                value: "other".into()
            })
        );
    }

    #[test]
    fn invalid_view_messages_show_the_leaf_error() {
        let error = ViewRecordError::Invalid {
            view: Box::new(ViewLabel::new(3, "ext").xmlid(Some("m.ext".into()))),
            written: "ext".into(),
            source: Box::new(InvalidView::Combine(ViewError::InView {
                view_id: 3,
                view_name: "ext".into(),
                source: Box::new(ViewError::TargetNotFound("<e4>".into())),
            })),
        };
        assert_eq!(
            error.to_string(),
            "Error while validating view ext (m.ext):\nElement '<e4>' cannot be located in parent \
             view"
        );
        let ViewRecordError::Invalid { source, .. } = &error else {
            unreachable!()
        };
        assert_eq!(source.origin_view(), Some(3));
        let xpath = InvalidView::Combine(ViewError::InvalidXpath {
            expr: "a|b".into(),
            kind: XpathErrorKind::Unsupported("'|'"),
        });
        assert_eq!(xpath.origin_view(), None);
    }

    /// The source of a combination error is the error under every [`ViewError::InView`] wrapper,
    /// not the outer wrapper (review R-11): a reader of the chain gets the error that says what
    /// is wrong, and its kind. An arch issue is the source of its own variant.
    #[test]
    fn invalid_view_source_is_the_innermost_error() {
        use std::error::Error as _;
        let leaf = ViewError::TargetNotFound("<e4>".into());
        let invalid = InvalidView::Combine(ViewError::InView {
            view_id: 3,
            view_name: "ext".into(),
            source: Box::new(ViewError::InView {
                view_id: 4,
                view_name: "deeper".into(),
                source: Box::new(leaf.clone()),
            }),
        });
        let source = invalid.source().expect("a source");
        assert_eq!(source.downcast_ref::<ViewError>(), Some(&leaf));
        assert!(source.source().is_none(), "{source:?}");
        assert_eq!(
            invalid.to_string(),
            "Element '<e4>' cannot be located in parent view"
        );
        // the wrappers stay in the variant: the view that holds the faulty spec
        assert_eq!(invalid.origin_view(), Some(4));
        // under a record error, the chain reaches the leaf in two steps
        let error = ViewRecordError::Invalid {
            view: Box::new(ViewLabel::new(4, "deeper")),
            written: "ext".into(),
            source: Box::new(invalid),
        };
        let below = error
            .source()
            .and_then(|e| e.source())
            .and_then(|e| e.downcast_ref::<ViewError>());
        assert_eq!(below, Some(&leaf));
        let issue = ArchIssue::FieldWithoutName {
            origin: NodeOrigin::new(9, std::num::NonZeroU32::MIN),
        };
        let arch = InvalidView::Arch(issue.clone());
        assert_eq!(
            arch.source().and_then(|e| e.downcast_ref::<ArchIssue>()),
            Some(&issue)
        );
        assert_eq!(
            arch.to_string(),
            "Field tag must have a \"name\" attribute defined"
        );
    }

    /// Each record limit names the engine limit whose value it applies, the `*_view_ref` bytes
    /// none (review T-13).
    #[test]
    fn record_limits_name_their_engine_limit() {
        let kinds: Vec<Option<LimitKind>> = [
            RecordLimit::ArchBytes,
            RecordLimit::CatalogueViews,
            RecordLimit::CatalogueBytes,
            RecordLimit::GroupRefs,
            RecordLimit::ImplicationGroups,
            RecordLimit::ViewRefBytes { key: "k".into() },
        ]
        .iter()
        .map(RecordLimit::engine_kind)
        .collect();
        assert_eq!(
            kinds,
            [
                Some(LimitKind::ArchBytes),
                Some(LimitKind::CatalogueViews),
                Some(LimitKind::CatalogueBytes),
                Some(LimitKind::CheckGroupRefs),
                None,
                None
            ]
        );
    }
}
