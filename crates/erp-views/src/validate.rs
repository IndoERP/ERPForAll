//! The full per-tag validation (buttons,
//! labels, domains, modifiers), x2many sub-views and generated default views come with
//! : a sub-view inside a `<field>` is not read here and is returned as
//! [`ArchWarning::SubViewNotValidated`], never skipped without a word.

use std::collections::HashSet;
use std::fmt;
use std::rc::Rc;

use crate::arch::{Arch, NodeId, NodeOrigin};
use crate::exprs::{AttrKind, FieldSource, Site, is_constant, walk};
use crate::guard::{GuardText, echo};
use crate::{LimitKind, ViewLimits};

/// Most group references one `groups` attribute may list.
pub const MAX_GROUP_REFS: usize = 64;

/// Tags of the sub-views a `<field>` may hold.
pub(crate) const SUB_VIEW_TAGS: [&str; 5] = ["form", "list", "graph", "kanban", "calendar"];

/// The board task that brings the full view validation (x2many sub-views, per-tag checks).
pub const FULL_VALIDATION_TASK: &str = "a later release";

/// The view type of QWeb templates, which are combined but never validated
/// .
pub const QWEB: &str = "qweb";

/// What validation looks up outside the arch: the registry's models and fields, and which group
/// references exist. The `ir.ui.view` record layer implements it over the transaction's registry
/// and the group external ids it looked up in one statement.
pub trait RegistryLookup {
    /// Whether the registry has `model`.
    fn has_model(&self, model: &str) -> bool;
    /// Whether `model` has `field`.
    fn has_field(&self, model: &str, field: &str) -> bool;
    /// Whether the group external id `xmlid` (as written in a `groups` attribute, without `!`)
    /// names a group.
    fn has_group(&self, xmlid: &str) -> bool;
    /// The `groups` of the definition of `field` on `model` (a group spec text), when it has any.
    fn field_groups(&self, model: &str, field: &str) -> Option<&str>;
    /// The comodel of the relational field `field` on `model`; `None` for any other field.
    fn comodel(&self, model: &str, field: &str) -> Option<&str>;
    /// Whether membership of the group `group` implies membership of `implied` (both
    /// `module.name` external ids, transitively; the reflexive case is handled by the caller). A
    /// group the lookup does not know implies nothing: the answer is `false`, which makes the
    /// check refuse (fail closed).
    fn implies(&self, group: &str, implied: &str) -> bool;
}

/// The registry as the expression walk sees it.
struct Source<'a>(&'a dyn RegistryLookup);

impl FieldSource for Source<'_> {
    fn has_field(&self, model: &str, field: &str) -> bool {
        self.0.has_field(model, field)
    }
    fn comodel(&self, model: &str, field: &str) -> Option<&str> {
        self.0.comodel(model, field)
    }
}

/// Why a combined arch is invalid. Every issue carries the origin of the node it is about.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ArchIssue {
    #[error(
        "The root node of a {ty} view should be a <{ty}>, not a <{tag}>",
        ty = view_type.as_deref().unwrap_or("None")
    )]
    RootMismatch {
        /// The view's `type`, `None` when it has none (review T-6: no `"None"` sentinel).
        view_type: Option<String>,
        /// The root tag.
        tag: String,
        /// The root element.
        origin: NodeOrigin,
    },
    #[error("Model not found: {}", model.as_deref().unwrap_or("False"))]
    UnknownModel {
        /// The view's `model`, `None` when it has none.
        model: Option<String>,
        /// The root element.
        origin: NodeOrigin,
    },
    /// A `<field>` without `name`.
    #[error("Field tag must have a \"name\" attribute defined")]
    FieldWithoutName {
        /// The element.
        origin: NodeOrigin,
    },
    /// A `<field>` naming a field the model does not have.
    #[error("Field \"{field}\" does not exist in model \"{model}\"")]
    UnknownField {
        /// The field name.
        field: String,
        /// The model.
        model: String,
        /// The element.
        origin: NodeOrigin,
    },
    /// `attrs` or `states` on an element.
    #[error("Since 17.0, the \"attrs\" and \"states\" attributes are no longer used.")]
    ObsoleteAttribute {
        /// `attrs` or `states`.
        attribute: &'static str,
        /// The first element carrying one, in document order.
        origin: NodeOrigin,
    },
    /// A `groups` attribute listing more references than [`MAX_GROUP_REFS`].
    #[error("The groups attribute lists {count} groups, more than the {max} allowed")]
    TooManyGroups {
        /// References listed.
        count: usize,
        /// [`MAX_GROUP_REFS`].
        max: usize,
        /// The element.
        origin: NodeOrigin,
    },
    /// Distinct references across an arch exceed the host's configured check budget.
    #[error("The view lists {count} distinct groups, more than the {max} allowed")]
    TooManyGroupReferences {
        /// First distinct reference past the limit.
        count: usize,
        /// The configured CheckGroupRefs value.
        max: usize,
        /// The element whose references cross the limit.
        origin: NodeOrigin,
    },
    /// A `groups` attribute (or field definition) that is not a usable guard: no reference at
    /// all (an open guard is never captured), or a term that is not a `module.name` external id.
    /// Empty terms next to a real reference are ignored. Raised by install-time validation and by
    /// `GuardedArch::capture` alike.
    #[error("Invalid groups attribute '{value}': {problem}")]
    InvalidGroups {
        /// The attribute value, cut to 64 characters.
        value: String,
        /// What is wrong with it.
        problem: String,
        /// The element.
        origin: NodeOrigin,
    },
    /// An expression attribute (`invisible`, `readonly`, `required`, `column_invisible`,
    /// `decoration-*`, `context`, a `domain`, `filter_domain`, `default_order`) that cannot be
    /// read: it does not parse, uses a construct a view expression cannot use, uses `parent`
    /// outside a sub-view, or is not a domain where one is expected.
    #[error("Invalid expression in the {attribute} attribute '{value}': {problem}")]
    InvalidExpression {
        /// The attribute name.
        attribute: String,
        /// The attribute value, cut to 64 characters.
        value: String,
        /// What is wrong with it.
        problem: String,
        /// The element.
        origin: NodeOrigin,
    },
    /// A visible node reads, in an expression attribute, a field whose definition restricts it
    /// to groups the node's own guard does not imply: the user could see the node and not the
    /// field. Repeat the field's groups on the node (or an enclosing node). The rule is sound, not
    /// complete (5; A4, §23).
    #[error(
        "The {attribute} attribute reads the field \"{field}\" of model \"{model}\", which is \
         restricted to the groups \"{field_groups}\"; a visible node may not read a field it \
         does not guard: repeat the field's groups on the node"
    )]
    RestrictedFieldInExpression {
        /// The attribute name.
        attribute: String,
        /// The restricted field (the hop of a path that carries the groups).
        field: String,
        /// The model that defines it.
        model: String,
        /// The groups of its definition, as written.
        field_groups: String,
        /// The element.
        origin: NodeOrigin,
    },
    #[error(
        "Field \"{field}\" used in the {attribute} attribute does not exist in model \"{model}\""
    )]
    UnknownFieldInExpression {
        /// The attribute name.
        attribute: String,
        /// The field that is missing.
        field: String,
        /// The model that does not have it.
        model: String,
        /// The element.
        origin: NodeOrigin,
    },
}

impl ArchIssue {
    /// The view and line of the node the issue is about.
    pub fn origin(&self) -> NodeOrigin {
        match self {
            Self::RootMismatch { origin, .. }
            | Self::UnknownModel { origin, .. }
            | Self::FieldWithoutName { origin }
            | Self::UnknownField { origin, .. }
            | Self::ObsoleteAttribute { origin, .. }
            | Self::TooManyGroups { origin, .. }
            | Self::TooManyGroupReferences { origin, .. }
            | Self::InvalidGroups { origin, .. }
            | Self::InvalidExpression { origin, .. }
            | Self::UnknownFieldInExpression { origin, .. }
            | Self::RestrictedFieldInExpression { origin, .. } => *origin,
        }
    }
}

/// The warnings of one validation ([`validate_combined_in`]): what it did not refuse but the
/// caller must report. `#[must_use]`, so `validate_combined(..)?;` cannot drop them without a
/// compiler warning (review T-1; `Result`'s own `#[must_use]` does not reach the `Ok` value).
///
/// ```
/// use erp_views::validate::ArchWarnings;
///
/// let none = ArchWarnings::default();
/// assert!(none.is_empty());
/// assert_eq!(none.into_vec(), []);
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[must_use = "the warnings are the caller's to report (log or return)"]
pub struct ArchWarnings(Vec<ArchWarning>);

impl ArchWarnings {
    /// The warnings, in document order.
    pub fn as_slice(&self) -> &[ArchWarning] {
        &self.0
    }

    /// Whether there is none.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// How many there are.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// The warnings, in document order.
    pub fn into_vec(self) -> Vec<ArchWarning> {
        self.0
    }
}

impl IntoIterator for ArchWarnings {
    type Item = ArchWarning;
    type IntoIter = std::vec::IntoIter<ArchWarning>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

/// What validation did not refuse but the caller must know (returned, never only logged).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ArchWarning {
    UnknownGroup {
        /// The reference as written, without `!`.
        xmlid: String,
        /// The element.
        origin: NodeOrigin,
    },
    /// A sub-view inside a `<field>` (an x2many's `<list>`, `<form>`, ...) was not validated:
    /// its fields belong to the comodel, whose checks come with.
    SubViewNotValidated {
        /// The sub-view's tag.
        tag: String,
        /// The `<field>` holding it.
        field: String,
        /// The sub-view element.
        origin: NodeOrigin,
    },
}

impl ArchWarning {
    /// The view and line of the node the warning is about.
    pub fn origin(&self) -> NodeOrigin {
        match self {
            Self::UnknownGroup { origin, .. } | Self::SubViewNotValidated { origin, .. } => *origin,
        }
    }
}

impl fmt::Display for ArchWarning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownGroup { xmlid, origin } => write!(
                f,
                "unknown group '{xmlid}' in a groups attribute (view {}, line {}): it never \
                 matches",
                origin.view_id(),
                origin.line()
            ),
            Self::SubViewNotValidated { tag, field, origin } => write!(
                f,
                "the <{tag}> sub-view of field '{field}' (view {}, line {}) is not validated \
                 until {FULL_VALIDATION_TASK}",
                origin.view_id(),
                origin.line()
            ),
        }
    }
}

/// The group references of every `groups` attribute of `arch`, in document order, without `!`
/// and without duplicates: what the caller looks up before [`validate_combined`].
///
/// An attribute is counted before any of its references is collected, so an attribute over
/// [`MAX_GROUP_REFS`] is refused here, before the caller looks anything up; duplicates are folded
/// with a set, so the cost is linear in the references written (review R-1/S-1).
///
/// # Errors
///
/// [`ArchIssue::TooManyGroups`] for the first attribute, in document order, that lists more than
/// [`MAX_GROUP_REFS`] references (duplicates counted as written, as [`validate_combined`] does).
/// [`ArchIssue::TooManyGroupReferences`] when the default check budget is exceeded.
///
/// ```
/// use erp_views::{ViewCatalogue, ViewDescriptor, validate::group_refs};
///
/// let arch = "<form><a groups=\"base.group_user,!base.group_system\"/><b groups=\"base.group_user\"/></form>";
/// let combined = ViewCatalogue::new(vec![ViewDescriptor::new(1, "v", "m", arch)])?.combine(1)?;
/// assert_eq!(group_refs(combined.arch())?, ["base.group_user", "base.group_system"]);
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub fn group_refs(arch: &Arch) -> Result<Vec<&str>, ArchIssue> {
    group_refs_with_limits(arch, &ViewLimits::default())
}

/// Collects group references with the host's lowered [`LimitKind::CheckGroupRefs`] budget.
/// A caller checking several arches must also bound their union before a database lookup.
///
/// # Errors
///
/// [`ArchIssue::TooManyGroups`] for an attribute over [`MAX_GROUP_REFS`];
/// [`ArchIssue::TooManyGroupReferences`] before adding a distinct reference past the configured
/// bound. For example, `ViewLimits::default().lowered(LimitKind::CheckGroupRefs, 1)` permits
/// repeated mentions of one group, but rejects a second distinct group in any later attribute.
pub fn group_refs_with_limits<'a>(
    arch: &'a Arch,
    limits: &ViewLimits,
) -> Result<Vec<&'a str>, ArchIssue> {
    let max = limits.get(LimitKind::CheckGroupRefs);
    let mut refs: Vec<&str> = Vec::new();
    let mut seen: HashSet<&str> = HashSet::new();
    for (id, _) in arch.walk(arch.root_id()) {
        if arch.is_sentinel(id) {
            continue;
        }
        if let Some(groups) = arch.attr(id, "groups") {
            let count = split_groups(groups).count();
            if count > MAX_GROUP_REFS {
                return Err(ArchIssue::TooManyGroups {
                    count,
                    max: MAX_GROUP_REFS,
                    origin: arch.origin(id),
                });
            }
            for item in split_groups(groups) {
                if !seen.contains(item) {
                    if refs.len() >= max {
                        return Err(ArchIssue::TooManyGroupReferences {
                            count: refs.len() + 1,
                            max,
                            origin: arch.origin(id),
                        });
                    }
                    seen.insert(item);
                    refs.push(item);
                }
            }
        }
    }
    Ok(refs)
}

pub(crate) fn split_groups(value: &str) -> impl Iterator<Item = &str> {
    value
        .split(',')
        .map(str::trim)
        .map(|item| item.strip_prefix('!').map_or(item, str::trim))
        .filter(|item| !item.is_empty())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Scope<'a> {
    /// Every `<field>` (a write, a module's views, a created view that is the combination's root
    /// or carries a `replace`/`attributes` spec: [`crate::record::widens_validation`]).
    Whole,
    /// Only the `<field>`s whose own origin or an ancestor's origin is one of these views
    /// (the created ones), outside the deferred subviews.
    Views(&'a HashSet<i64>),
}

impl Scope<'_> {
    fn includes(self, origin: NodeOrigin) -> bool {
        match self {
            Self::Whole => true,
            Self::Views(views) => views.contains(&origin.view_id()),
        }
    }
}

/// [`validate_combined_in`] with [`Scope::Whole`].
///
/// # Errors
///
/// As [`validate_combined_in`].
pub fn validate_combined(
    arch: &Arch,
    view_type: Option<&str>,
    model: Option<&str>,
    fields: &dyn RegistryLookup,
) -> Result<ArchWarnings, ArchIssue> {
    validate_combined_in(arch, view_type, model, fields, Scope::Whole)
}

/// # Errors
///
/// ```
/// use erp_views::validate::{RegistryLookup, Scope, validate_combined_in};
/// use erp_views::{ViewCatalogue, ViewDescriptor};
///
/// struct Fields;
/// impl RegistryLookup for Fields {
///     fn has_model(&self, model: &str) -> bool { model == "res.partner" }
///     fn has_field(&self, _model: &str, field: &str) -> bool { field == "name" }
///     fn has_group(&self, _xmlid: &str) -> bool { true }
///     fn field_groups(&self, _model: &str, _field: &str) -> Option<&str> { None }
///     fn comodel(&self, _model: &str, _field: &str) -> Option<&str> { None }
///     fn implies(&self, _group: &str, _implied: &str) -> bool { false }
/// }
///
/// let arch = "<form>\n<field name=\"name\"/>\n<field name=\"nope\"/></form>";
/// let combined = ViewCatalogue::new(vec![ViewDescriptor::new(4, "v", "res.partner", arch)])?
///     .combine(4)?;
/// let issue =
///     validate_combined_in(combined.arch(), Some("form"), Some("res.partner"), &Fields, Scope::Whole)
///         .unwrap_err();
/// assert_eq!(issue.to_string(), "Field \"nope\" does not exist in model \"res.partner\"");
/// assert_eq!((issue.origin().view_id(), issue.origin().line()), (4, 3));
/// # Ok::<(), erp_views::ViewError>(())
/// ```
pub fn validate_combined_in(
    arch: &Arch,
    view_type: Option<&str>,
    model: Option<&str>,
    fields: &dyn RegistryLookup,
    scope: Scope<'_>,
) -> Result<ArchWarnings, ArchIssue> {
    if view_type == Some(QWEB) {
        return Ok(ArchWarnings::default());
    }
    let root = arch.root_id();
    let root_tag = arch.tag(root);
    if view_type != Some(root_tag) {
        return Err(ArchIssue::RootMismatch {
            view_type: view_type.map(str::to_owned),
            tag: root_tag.to_owned(),
            origin: arch.origin(root),
        });
    }
    let Some(model) = model.filter(|m| fields.has_model(m)) else {
        return Err(ArchIssue::UnknownModel {
            model: model.map(str::to_owned),
            origin: arch.origin(root),
        });
    };
    let mut warnings = Vec::new();
    // pre-order walk; the subtree of a sub-view is skipped by resuming after it, the field's
    // other children are checked
    // a set: a lookup per node, never a scan of the sub-views found so far (review A2-R10); it
    // allocates only when an arch has a sub-view
    let mut sub_views: HashSet<NodeId> = HashSet::new();
    let mut skip_until: Option<NodeId> = None;
    // A marked element brings its entire subtree into scope. The preorder depth lets us
    // leave that scope without walking ancestors or allocating state for every node.
    let mut scoped_depth: Option<usize> = None;
    // the effective guard of the visited ancestors, by depth (an unguarded chain holds `None`)
    let mut guard_stack: Vec<(usize, Option<Rc<GuardText>>)> = Vec::new();
    for (id, depth) in arch.walk(root) {
        if let Some(sub) = skip_until {
            if arch.is_ancestor_or_self(sub, id) {
                continue;
            }
            skip_until = None;
        }
        if sub_views.remove(&id) {
            skip_until = Some(id);
            continue;
        }
        if arch.is_sentinel(id) {
            continue;
        }
        if scoped_depth.is_some_and(|marked| depth <= marked) {
            scoped_depth = None;
        }
        if scoped_depth.is_none() && scope.includes(arch.origin(id)) {
            scoped_depth = Some(depth);
        }
        let own = check_groups(arch, id, fields, &mut warnings)?;
        let is_field = arch.tag(id) == "field";
        let origin = arch.origin(id);
        let mut name = "";
        let mut definition: Option<GuardText> = None;
        if is_field {
            name = match arch.attr(id, "name") {
                Some(name) if !name.is_empty() => name,
                _ => return Err(ArchIssue::FieldWithoutName { origin }),
            };
            if scoped_depth.is_some() && !fields.has_field(model, name) {
                return Err(ArchIssue::UnknownField {
                    field: name.to_owned(),
                    model: model.to_owned(),
                    origin,
                });
            }
            // A field node outside the validation scope keeps the guard its definition gives it
            // when that parses; a stale malformed definition there is not this check's business
            // (a scoped descendant that needed it is then refused for want of a guard).
            if let Some(groups) = fields.field_groups(model, name) {
                match GuardText::parse(groups) {
                    Ok(guard) => definition = Some(guard),
                    Err(problem) if scoped_depth.is_some() => {
                        return Err(definition_issue(model, name, groups, problem, origin));
                    }
                    Err(_) => {}
                }
            }
        }
        // the effective guard: enclosing nodes, the node's own groups, the field's definition
        while guard_stack.last().is_some_and(|(d, _)| *d >= depth) {
            guard_stack.pop();
        }
        let enclosing = guard_stack.last().and_then(|(_, g)| g.clone());
        let effective = [own, definition]
            .into_iter()
            .flatten()
            .fold(enclosing, |acc, part| {
                Some(Rc::new(match acc {
                    Some(acc) => acc.intersect(&part),
                    None => part,
                }))
            });
        guard_stack.push((depth, effective.clone()));
        if scoped_depth.is_some() {
            check_expressions(arch, id, model, fields, effective.as_deref())?;
        }
        if !is_field {
            continue;
        }
        for child in arch
            .element_children(id)
            .filter(|&c| SUB_VIEW_TAGS.contains(&arch.tag(c)))
        {
            warnings.push(ArchWarning::SubViewNotValidated {
                tag: arch.tag(child).to_owned(),
                field: name.to_owned(),
                origin: arch.origin(child),
            });
            // the walk skips the sub-view's subtree when it reaches it
            sub_views.insert(child);
        }
    }
    for (id, _) in arch.walk(root) {
        if arch.is_sentinel(id) {
            continue;
        }
        for attribute in ["attrs", "states"] {
            if arch.attr(id, attribute).is_some() {
                return Err(ArchIssue::ObsoleteAttribute {
                    attribute,
                    origin: arch.origin(id),
                });
            }
        }
    }
    Ok(ArchWarnings(warnings))
}

/// The `groups` attribute of `id`: at most [`MAX_GROUP_REFS`] references, each of them
/// well-formed ([`ArchIssue::InvalidGroups`], the capture's own rule), each unknown one a warning.
/// Returns the node's own guard, `None` when it has no `groups`.
fn check_groups(
    arch: &Arch,
    id: NodeId,
    fields: &dyn RegistryLookup,
    warnings: &mut Vec<ArchWarning>,
) -> Result<Option<GuardText>, ArchIssue> {
    let Some(groups) = arch.attr(id, "groups") else {
        return Ok(None);
    };
    let count = split_groups(groups).count();
    if count > MAX_GROUP_REFS {
        return Err(ArchIssue::TooManyGroups {
            count,
            max: MAX_GROUP_REFS,
            origin: arch.origin(id),
        });
    }
    // the serve-time capture parses the same value: a malformed reference fails the install
    // instead of every later get_view (review S1); unknown but well-formed is a warning
    let guard = GuardText::parse(groups).map_err(|problem| ArchIssue::InvalidGroups {
        value: echo(groups),
        problem,
        origin: arch.origin(id),
    })?;
    for xmlid in split_groups(groups) {
        if xmlid == "." {
            continue;
        }
        if !fields.has_group(xmlid) {
            warnings.push(ArchWarning::UnknownGroup {
                xmlid: xmlid.to_owned(),
                origin: arch.origin(id),
            });
        }
    }
    Ok(Some(guard))
}

fn definition_issue(
    model: &str,
    field: &str,
    groups: &str,
    problem: String,
    origin: NodeOrigin,
) -> ArchIssue {
    ArchIssue::InvalidGroups {
        value: echo(groups),
        problem: format!("field '{model}.{field}' definition: {problem}"),
        origin,
    }
}

/// Whether some attribute of `arch` holds an expression, domain or order that
/// [`validate_combined_in`] reads and that is more than a constant (`0`, `1`, `True`, `False`).
/// The caller loads the group implications the check needs only for such an arch. A sub-view's
/// attributes count too (the answer may be `true` for an arch whose reads are not checked).
pub fn reads_fields(arch: &Arch) -> bool {
    arch.walk(arch.root_id()).any(|(id, _)| {
        !arch.is_sentinel(id)
            && arch.attrs(id).iter().any(|(name, value)| {
                AttrKind::of(arch.tag(id), name).is_some() && !is_constant(value)
            })
    })
}

/// The expression attributes of `id`, read as `model`'s node: every name must exist, and a field
/// whose definition restricts it to groups must be covered by the node's effective guard.
fn check_expressions(
    arch: &Arch,
    id: NodeId,
    model: &str,
    fields: &dyn RegistryLookup,
    effective: Option<&GuardText>,
) -> Result<(), ArchIssue> {
    let tag = arch.tag(id);
    let origin = arch.origin(id);
    for (attribute, value) in arch.attrs(id) {
        let Some(kind) = AttrKind::of(tag, attribute) else {
            continue;
        };
        let comodel = match kind {
            AttrKind::FieldDomain => arch
                .attr(id, "name")
                .and_then(|name| fields.comodel(model, name)),
            _ => None,
        };
        let site = Site {
            kind,
            model,
            parent: None,
            comodel,
        };
        let read = walk(&site, value, &Source(fields))
            .map_err(|problem| problem.into_issue(attribute, value, origin))?;
        for r in &read {
            check_restricted(&r.model, &r.path, attribute, fields, effective, origin)?;
        }
    }
    Ok(())
}

/// Every hop of the read `path` from `model` whose field is restricted must be covered.
fn check_restricted(
    model: &str,
    path: &str,
    attribute: &str,
    fields: &dyn RegistryLookup,
    effective: Option<&GuardText>,
    origin: NodeOrigin,
) -> Result<(), ArchIssue> {
    let mut current = model;
    for hop in path.split('.') {
        if let Some(groups) = fields.field_groups(current, hop) {
            let required = GuardText::parse(groups)
                .map_err(|problem| definition_issue(current, hop, groups, problem, origin))?;
            if !covers(effective, &required, fields) {
                return Err(ArchIssue::RestrictedFieldInExpression {
                    attribute: attribute.to_owned(),
                    field: hop.to_owned(),
                    model: current.to_owned(),
                    field_groups: groups.to_owned(),
                    origin,
                });
            }
        }
        match fields.comodel(current, hop) {
            Some(next) => current = next,
            None => break,
        }
    }
    Ok(())
}

/// Whether being allowed by `node` guarantees being allowed by `required` (sound, not
/// complete; design §11 A4). `never` implies everything; an unguarded node implies nothing.
/// Each clause of `required` needs one clause of `node` that is positive, whose positives each
/// imply one of the required positives (when it has any), and whose negatives exclude every
/// required negative (being in a required-negative group implies being in a node-negative one).
fn covers(node: Option<&GuardText>, required: &GuardText, fields: &dyn RegistryLookup) -> bool {
    let implies = |group: &str, implied: &str| group == implied || fields.implies(group, implied);
    let Some(node) = node else {
        return false;
    };
    if node.never {
        return true;
    }
    if required.never {
        return false;
    }
    required.clauses.iter().all(|wanted| {
        node.clauses.iter().any(|have| {
            let positives = wanted.positives.is_empty()
                || (!have.positives.is_empty()
                    && have
                        .positives
                        .iter()
                        .all(|p| wanted.positives.iter().any(|q| implies(&p.0, &q.0))));
            let negatives = wanted
                .negatives
                .iter()
                .all(|x| have.negatives.iter().any(|y| implies(&x.0, &y.0)));
            positives && negatives
        })
    })
}
