//! Guard capture (§5.1, §21.1): the pure
//! half of the guarded arch. [`GuardedArch::capture`] reads every `groups` attribute of a combined
//! arch, the groups the registry puts on the fields the arch shows, and the groups of the primary
//! view records, then removes the attributes. What is left is an arch that carries no access
//! information and a guard table that only this module can build.
//!
//! The table is private on purpose: there is no accessor and no constructor from parts, so a
//! caller cannot pair an arch with an edited table. A later slice (the `host-access` bridge of the
//! same crate) compares the table with the plan an evaluation was decided for.
//!
//! The walk is one iterative pre-order pass over the combined arch (`Arch::walk`), never
//! recursion. Guard text is canonical: the references of a clause are sorted and deduplicated, a
//! guard is a conjunction of clauses (the parent's guard, the node's own `groups` and the field
//! definition's groups intersect), and equal guards share one index. Guard indices follow the
//! first occurrence in pre-order, so they are the positions of the plan built from the table.
//!
//! The fields the arch reads are captured too: every `<field name>` element, and every field an
//! expression attribute reads (`invisible`, `readonly`, `required`, `column_invisible`,
//! `decoration-*`, `context`, a `domain`, `filter_domain`, `default_order`; see [`crate::exprs`]),
//! each on the model of the sub-view it sits in. The bridge turns them into the plan's field
//! references, so a node that shows or reads a field the user may not read is decided per user.
//!
//! Group references are checked as text only (`module.name`); whether a group exists is decided
//! when the guard is resolved against a policy snapshot, not here.

use std::collections::HashMap;
use std::fmt;

use crate::arch::{Arch, NodeId, NodeIdx};
use crate::combine::CombinedView;
use crate::error::{LimitKind, ViewError, ViewResult};
use crate::exprs::{AttrKind, Site, walk};
use crate::limits::ViewLimits;
use crate::validate::{ArchIssue, MAX_GROUP_REFS, SUB_VIEW_TAGS, split_groups};

/// Most record guards (primary views with groups) one capture accepts.
const MAX_RECORD_GUARDS: usize = 128;

/// Longest group reference accepted, in bytes (the rule of `QualifiedXmlId::parse` in `erp-orm`).
const MAX_GROUP_REF_LEN: usize = 256;

/// Characters of an attribute value echoed in an error message.
const ECHO_CHARS: usize = 64;

/// What the capture needs from the registry; implemented by the host over its model registry.
///
/// The answers are read once per capture. `registry_generation` names the registry state they
/// belong to, so a bundle captured under one generation is never served under another.
///
/// The guarantee is exactly this: the arch `host_access::filter` serves matches the
/// plan the evaluation decided. Whether that plan is the right one still depends on the answers
/// of the host's `GuardLookup` (field `groups`, comodels) and on the record guards it passes to
/// [`GuardedArch::capture`]; a host that lies there gets a bundle that follows the lie. Only the
/// `base` addon supplies a lookup today, over the transaction's registry.
pub trait GuardLookup {
    /// The generation of the registry the answers come from.
    fn registry_generation(&self) -> u64;
    /// The `groups` of the definition of `field` on `model` (a group spec text), when it has any.
    fn field_groups(&self, model: &str, field: &str) -> Option<&str>;
    /// The comodel of the relational field `field` on `model`; `None` for any other field.
    fn comodel(&self, model: &str, field: &str) -> Option<&str>;
    /// Whether `model` has the field `field`.
    fn has_field(&self, model: &str, field: &str) -> bool;
}

/// A validated `module.name` group reference.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct GroupRefText(pub(crate) Box<str>);

impl GroupRefText {
    /// The rule of `QualifiedXmlId::parse`: both parts non-empty, exactly one dot, no whitespace
    /// or control characters, module part ASCII letters, digits and `_`, at most 256 bytes.
    fn parse(value: &str) -> Result<Self, &'static str> {
        if value.len() > MAX_GROUP_REF_LEN {
            return Err("is longer than 256 bytes");
        }
        if value.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err("contains whitespace or control characters");
        }
        let Some((module, name)) = value.split_once('.') else {
            return Err("has no module part");
        };
        if module.is_empty() {
            return Err("has an empty module part");
        }
        if name.is_empty() {
            return Err("has an empty name part");
        }
        if name.contains('.') {
            return Err("has more than one dot");
        }
        if !module
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            return Err("has a module part that is not ASCII letters, digits and '_'");
        }
        Ok(Self(value.into()))
    }
}

/// One comma-separated group spec: allowed when the subject is in any positive group and in none
/// of the negative ones.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct ClauseText {
    pub(crate) positives: Box<[GroupRefText]>,
    pub(crate) negatives: Box<[GroupRefText]>,
}

impl ClauseText {
    fn refs(&self) -> usize {
        self.positives.len() + self.negatives.len()
    }
}

/// An effective guard: `never` (`.`), or a conjunction of clauses. Never open: an attribute that
/// parses to no restriction is refused at capture.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct GuardText {
    pub(crate) never: bool,
    pub(crate) clauses: Box<[ClauseText]>,
}

impl GuardText {
    fn never() -> Self {
        Self {
            never: true,
            clauses: Box::default(),
        }
    }

    /// The guard of `value`, or the problem with it.
    pub(crate) fn parse(value: &str) -> Result<Self, String> {
        if value == "." {
            return Ok(Self::never());
        }
        let mut positives = Vec::new();
        let mut negatives = Vec::new();
        for raw in value.split(',') {
            let term = raw.trim();
            let (negated, xmlid) = match term.strip_prefix('!') {
                Some(rest) => (true, rest),
                None => (false, term),
            };
            if xmlid.is_empty() {
                continue;
            }
            let parsed = GroupRefText::parse(xmlid).map_err(|problem| {
                format!(
                    "the term '{}' is not a group external id (module.name): it {problem}",
                    echo(term)
                )
            })?;
            if negated {
                negatives.push(parsed);
            } else {
                positives.push(parsed);
            }
        }
        if positives.is_empty() && negatives.is_empty() {
            return Err(
                "the group spec has no group reference; an open guard is not allowed".to_owned(),
            );
        }
        positives.sort();
        positives.dedup();
        negatives.sort();
        negatives.dedup();
        Ok(Self {
            never: false,
            clauses: Box::new([ClauseText {
                positives: positives.into(),
                negatives: negatives.into(),
            }]),
        })
    }

    /// Both guards must hold: `never` absorbs, otherwise the clauses join, sorted and deduplicated.
    pub(crate) fn intersect(&self, other: &Self) -> Self {
        if self.never || other.never {
            return Self::never();
        }
        let mut clauses: Vec<ClauseText> = self
            .clauses
            .iter()
            .chain(other.clauses.iter())
            .cloned()
            .collect();
        clauses.sort();
        clauses.dedup();
        Self {
            never: false,
            clauses: clauses.into(),
        }
    }

    fn refs(&self) -> usize {
        self.clauses.iter().map(ClauseText::refs).sum()
    }

    fn retained_bytes(&self) -> usize {
        let mut total = std::mem::size_of::<Self>().saturating_add(
            self.clauses
                .len()
                .saturating_mul(std::mem::size_of::<ClauseText>()),
        );
        for clause in &self.clauses {
            for r in clause.positives.iter().chain(clause.negatives.iter()) {
                total = total.saturating_add(std::mem::size_of::<GroupRefText>() + r.0.len());
            }
        }
        total
    }

    /// Canonical text, for tests: `.`, or clauses joined by ` & `, each `a,b,!c`.
    #[cfg(test)]
    fn render(&self) -> String {
        if self.never {
            return ".".to_owned();
        }
        let clauses: Vec<String> = self
            .clauses
            .iter()
            .map(|c| {
                let mut terms: Vec<String> = c.positives.iter().map(|r| r.0.to_string()).collect();
                terms.extend(c.negatives.iter().map(|r| format!("!{}", r.0)));
                terms.join(",")
            })
            .collect();
        clauses.join(" & ")
    }
}

/// `value` cut to [`ECHO_CHARS`] characters for a message.
pub(crate) fn echo(value: &str) -> String {
    match value.char_indices().nth(ECHO_CHARS) {
        Some((end, _)) => format!("{}...", &value[..end]),
        None => value.to_owned(),
    }
}

/// A primary view and the sorted, deduplicated ids of the groups it requires.
pub(crate) type RecordGuard = (i64, Box<[i64]>);

/// A node and the index of its effective guard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NodeGuard {
    pub(crate) node: NodeIdx,
    pub(crate) guard: u32,
}

/// What a field reference was read from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FieldRefKind {
    /// A `<field name>` element: the node itself shows the field.
    Node,
    /// A root category variable in a panel domain, distinct from its criterion scope.
    PanelDependency,
    /// An expression attribute of the node reads the field (`invisible`, a `domain`, ...); the
    /// attribute is named for the error a visible node reading an unreadable field raises.
    Expr { attribute: Box<str> },
    /// Whether the user may read the comodel of a relational `<field>` that holds a sub-view or a
    /// `domain` (the path is `<field>.id`): when it may not, the sub-view is removed and a
    /// `domain` on the field removes the field, never the whole request.
    Comodel,
}

/// A field the arch reads and the model it resolves against: a `<field name>` or a field named
/// by an expression attribute of `node`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FieldRefText {
    pub(crate) node: NodeIdx,
    pub(crate) model: Box<str>,
    /// A field name, or a dotted path of at most 8 hops (the left side of a domain leaf).
    pub(crate) path: Box<str>,
    pub(crate) kind: FieldRefKind,
    /// Position of this `(model, path)` among the distinct field references in order of first
    /// occurrence, which is its position in the plan built from the table (a plan shares one
    /// entry between equal references).
    #[cfg_attr(not(feature = "host-access"), allow(dead_code))]
    pub(crate) plan_index: u32,
}

/// The complete guard table of one captured arch. Its `Debug` prints counts only: the table is
/// never readable by a downstream crate, and a log line must not become a way to read it.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct GuardTable {
    pub(crate) node_guards: Box<[NodeGuard]>,
    pub(crate) guards: Box<[GuardText]>,
    pub(crate) record_guards: Box<[RecordGuard]>,
    pub(crate) fields: Box<[FieldRefText]>,
    pub(crate) root_model: Box<str>,
    pub(crate) applied_view_ids: Box<[i64]>,
    pub(crate) registry_generation: u64,
}

/// The immutable pair: a combined arch with every `groups` attribute removed, and the complete
/// guard table captured from those attributes, the registry's field groups and the primary views'
/// record groups before stripping. The fields are private and the only constructor is
/// [`GuardedArch::capture`]; there is no table accessor. What the type guarantees: the table was
/// captured from this arch (it cannot be edited or swapped afterwards) and the filtered output
/// matches the plan the evaluation decided. It does not vouch for the host's `GuardLookup` answers
/// or record guards given to `capture`, which decide what the plan contains.
#[derive(Clone, PartialEq, Eq)]
pub struct GuardedArch {
    arch: Arch,
    table: GuardTable,
}

impl fmt::Debug for GuardTable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GuardTable")
            .field("node_guards", &self.node_guards.len())
            .field("guards", &self.guards.len())
            .field("record_guards", &self.record_guards.len())
            .field("fields", &self.fields.len())
            .field("root_model", &self.root_model)
            .field("registry_generation", &self.registry_generation)
            .finish_non_exhaustive()
    }
}

impl fmt::Debug for GuardedArch {
    /// Counts only, not the stripped arch or the guard table.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GuardedArch")
            .field("table", &self.table)
            .finish_non_exhaustive()
    }
}

impl GuardedArch {
    /// The table, for the `host-access` bridge of this crate only (never public).
    #[cfg_attr(not(feature = "host-access"), allow(dead_code))]
    pub(crate) fn table(&self) -> &GuardTable {
        &self.table
    }
}

/// The context a node hands to its children during the walk.
#[derive(Clone, Copy)]
struct Ctx<'a> {
    /// Index of the effective guard in force, when there is one.
    guard: Option<u32>,
    /// The model `<field>` elements and expression attributes below resolve against.
    model: &'a str,
    /// The model that encloses the sub-view the node is in (what `parent.x` reads); `None` at the
    /// top of the view.
    parent_model: Option<&'a str>,
}

/// Interns distinct effective guards in first-occurrence order.
struct Interner {
    guards: Vec<GuardText>,
    index: HashMap<GuardText, u32>,
    max: usize,
}

impl Interner {
    fn intern(&mut self, guard: GuardText) -> ViewResult<u32> {
        if let Some(&i) = self.index.get(&guard) {
            return Ok(i);
        }
        if self.guards.len() >= self.max {
            return Err(ViewError::LimitExceeded {
                limit: LimitKind::CheckGroupRefs,
                max: self.max,
                actual: Some(self.guards.len() + 1),
            });
        }
        let i = u32::try_from(self.guards.len()).map_err(|_| ViewError::LimitExceeded {
            limit: LimitKind::CheckGroupRefs,
            max: self.max,
            actual: None,
        })?;
        self.guards.push(guard.clone());
        self.index.insert(guard, i);
        Ok(i)
    }
}

impl GuardedArch {
    /// Captures the guards of `combined` and strips its `groups` attributes.
    ///
    /// `record_guards` are `(view id, res.groups ids)` of the applied primary views that have
    /// groups; a view without groups is left out by the caller. `root_model` is the model of the
    /// view; `<field>` elements below a relational field's sub-view resolve against its comodel.
    ///
    /// # Errors
    ///
    /// [`ViewError::GuardCapture`] naming the view and line: a `groups` value (or field
    /// definition) that is empty, has an empty term or a term that is not `module.name`
    /// ([`ArchIssue::InvalidGroups`]); more than [`MAX_GROUP_REFS`] references in one attribute or
    /// in one effective guard ([`ArchIssue::TooManyGroups`]); a `<field>` without a name or
    /// unknown in its model ([`ArchIssue::FieldWithoutName`], [`ArchIssue::UnknownField`]); an
    /// expression attribute that cannot be read or names a field its model does not have
    /// ([`ArchIssue::InvalidExpression`], [`ArchIssue::UnknownFieldInExpression`]).
    /// [`ViewError::LimitExceeded`] for [`LimitKind::CheckGroupRefs`] when the arch has more
    /// distinct effective guards than the limit. [`ViewError::InvalidRecordGuards`] for a bad
    /// record guard. Nothing is returned partly captured.
    pub fn capture(
        combined: CombinedView,
        root_model: &str,
        record_guards: &[(i64, &[i64])],
        lookup: &dyn GuardLookup,
        limits: &ViewLimits,
    ) -> Result<GuardedArch, ViewError> {
        let record_guards = canonical_record_guards(record_guards, limits)?;
        let applied_view_ids: Box<[i64]> = combined.applied_view_ids().into();
        let mut arch = combined.into_arch();

        let mut interner = Interner {
            guards: Vec::new(),
            index: HashMap::new(),
            max: limits.get(LimitKind::CheckGroupRefs),
        };
        let mut node_guards: Vec<NodeGuard> = Vec::new();
        let mut fields: Vec<FieldRefText> = Vec::new();
        let mut to_strip: Vec<NodeId> = Vec::new();
        {
            let root_ctx = Ctx {
                guard: None,
                model: root_model,
                parent_model: None,
            };
            let mut stack: Vec<Ctx<'_>> = Vec::new();
            for (id, depth) in arch.walk(arch.root_id()) {
                if arch.is_sentinel(id) {
                    continue;
                }
                stack.truncate(depth);
                let parent = stack.last().copied().unwrap_or(root_ctx);
                let origin = arch.origin(id);
                let issue = |issue: ArchIssue| ViewError::GuardCapture(issue);

                let own = match arch.attr(id, "groups") {
                    Some(value) => {
                        to_strip.push(id);
                        let count = split_groups(value).count();
                        if count > MAX_GROUP_REFS {
                            return Err(issue(ArchIssue::TooManyGroups {
                                count,
                                max: MAX_GROUP_REFS,
                                origin,
                            }));
                        }
                        Some(GuardText::parse(value).map_err(|problem| {
                            issue(ArchIssue::InvalidGroups {
                                value: echo(value),
                                problem,
                                origin,
                            })
                        })?)
                    }
                    None => None,
                };

                let panel_section = arch
                    .parent(id)
                    .is_some_and(|parent| arch.tag(parent) == "searchpanel")
                    && arch.tag(id) == "field";
                let mut child_model = parent.model;
                let mut child_parent_model = parent.parent_model;
                let mut field_comodel = None;
                let mut definition = None;
                if arch.tag(id) == "field" {
                    let Some(name) = arch.attr(id, "name") else {
                        if panel_section {
                            return Err(ViewError::InvalidDescription(format!(
                                "Search panel section {} requires a root field.",
                                panel_ordinal(&arch, id)?
                            )));
                        }
                        return Err(issue(ArchIssue::FieldWithoutName { origin }));
                    };
                    if !lookup.has_field(parent.model, name) {
                        if panel_section {
                            return Err(ViewError::InvalidDescription(format!(
                                "Search panel section {} references an invalid root field.",
                                panel_ordinal(&arch, id)?
                            )));
                        }
                        return Err(issue(ArchIssue::UnknownField {
                            field: name.to_owned(),
                            model: parent.model.to_owned(),
                            origin,
                        }));
                    }
                    fields.push(FieldRefText {
                        node: id.to_idx(),
                        model: parent.model.into(),
                        path: name.into(),
                        kind: FieldRefKind::Node,
                        plan_index: 0,
                    });
                    if let Some(groups) = lookup.field_groups(parent.model, name) {
                        definition = Some(GuardText::parse(groups).map_err(|problem| {
                            issue(ArchIssue::InvalidGroups {
                                value: echo(groups),
                                problem: format!(
                                    "field '{}.{name}' definition: {problem}",
                                    parent.model
                                ),
                                origin,
                            })
                        })?);
                    }
                    field_comodel = lookup.comodel(parent.model, name);
                    match field_comodel {
                        Some(comodel) => {
                            if panel_section
                                || arch.attr(id, "domain").is_some()
                                || arch
                                    .element_children(id)
                                    .any(|c| SUB_VIEW_TAGS.contains(&arch.tag(c)))
                            {
                                fields.push(FieldRefText {
                                    node: id.to_idx(),
                                    model: parent.model.into(),
                                    path: format!("{name}.id").into(),
                                    kind: FieldRefKind::Comodel,
                                    plan_index: 0,
                                });
                            }
                            child_model = comodel;
                            child_parent_model = Some(parent.model);
                        }
                        None => {
                            let sub_view = arch
                                .element_children(id)
                                .find(|&c| SUB_VIEW_TAGS.contains(&arch.tag(c)));
                            if let Some(sub) = sub_view {
                                return Err(ViewError::GuardCapture(ArchIssue::InvalidGroups {
                                    value: String::new(),
                                    problem: format!(
                                        "field '{}.{name}' holds a <{}> sub-view but is not \
                                             a relational field",
                                        parent.model,
                                        arch.tag(sub)
                                    ),
                                    origin,
                                }));
                            }
                        }
                    }
                }

                if panel_section && let Some(group) = arch.attr(id, "groupby") {
                    let ordinal = panel_ordinal(&arch, id)?;
                    let model = field_comodel.ok_or_else(|| {
                        ViewError::InvalidDescription(format!(
                            "Search panel section {ordinal} has an invalid grouping scope."
                        ))
                    })?;
                    if !lookup.has_field(model, group) {
                        return Err(ViewError::InvalidDescription(format!(
                            "Search panel section {ordinal} references an invalid grouping field."
                        )));
                    }
                    fields.push(FieldRefText {
                        node: id.to_idx(),
                        model: model.into(),
                        path: group.into(),
                        kind: FieldRefKind::Expr {
                            attribute: "groupby".into(),
                        },
                        plan_index: 0,
                    });
                }
                for (name, value) in arch.attrs(id) {
                    if panel_section && name == "domain" {
                        let ordinal = panel_ordinal(&arch, id)?;
                        for PanelRead {
                            model,
                            path,
                            dependency,
                        } in panel_reads(parent.model, field_comodel, value, lookup).map_err(
                            |_| {
                                ViewError::InvalidDescription(format!(
                                    "Search panel section {ordinal} has an invalid domain."
                                ))
                            },
                        )? {
                            fields.push(FieldRefText {
                                node: id.to_idx(),
                                model,
                                path,
                                kind: if dependency {
                                    FieldRefKind::PanelDependency
                                } else {
                                    FieldRefKind::Expr {
                                        attribute: "domain".into(),
                                    }
                                },
                                plan_index: 0,
                            });
                        }
                        continue;
                    }
                    let Some(kind) = AttrKind::of(arch.tag(id), name) else {
                        continue;
                    };
                    let site = Site {
                        kind,
                        model: parent.model,
                        parent: parent.parent_model,
                        comodel: field_comodel,
                    };
                    let read = walk(&site, value, lookup)
                        .map_err(|problem| issue(problem.into_issue(name, value, origin)))?;
                    for r in read {
                        fields.push(FieldRefText {
                            node: id.to_idx(),
                            model: r.model,
                            path: r.path,
                            kind: FieldRefKind::Expr {
                                attribute: name.as_str().into(),
                            },
                            plan_index: 0,
                        });
                    }
                }

                let has_own = own.is_some() || definition.is_some();
                let mut guard = parent.guard;
                if has_own {
                    let mut effective = guard.map(|g| interner.guards[g as usize].clone());
                    for part in own.into_iter().chain(definition) {
                        effective = Some(match effective {
                            Some(e) => e.intersect(&part),
                            None => part,
                        });
                    }
                    let Some(effective) = effective else {
                        return Err(ViewError::InvariantViolation(
                            "a node with its own groups has an effective guard",
                        ));
                    };
                    let refs = effective.refs();
                    if refs > MAX_GROUP_REFS {
                        return Err(issue(ArchIssue::TooManyGroups {
                            count: refs,
                            max: MAX_GROUP_REFS,
                            origin,
                        }));
                    }
                    let index = interner.intern(effective)?;
                    node_guards.push(NodeGuard {
                        node: id.to_idx(),
                        guard: index,
                    });
                    guard = Some(index);
                }
                stack.push(Ctx {
                    guard,
                    model: child_model,
                    parent_model: child_parent_model,
                });
            }
        }

        for id in to_strip {
            arch.remove_attr(id, "groups");
        }
        node_guards.sort_by_key(|g| g.node);
        let mut seen: HashMap<(Box<str>, Box<str>), u32> = HashMap::new();
        for field in &mut fields {
            let next = u32::try_from(seen.len()).map_err(|_| {
                ViewError::InvariantViolation("a captured arch has fewer than 2^32 fields")
            })?;
            field.plan_index = *seen
                .entry((field.model.clone(), field.path.clone()))
                .or_insert(next);
        }

        Ok(GuardedArch {
            arch,
            table: GuardTable {
                node_guards: node_guards.into(),
                guards: interner.guards.into(),
                record_guards,
                fields: fields.into(),
                root_model: root_model.into(),
                applied_view_ids,
                registry_generation: lookup.registry_generation(),
            },
        })
    }

    /// The arch without its `groups` attributes.
    #[must_use]
    pub fn arch(&self) -> &Arch {
        &self.arch
    }

    /// The model the view is for.
    #[must_use]
    pub fn root_model(&self) -> &str {
        &self.table.root_model
    }

    /// The views applied to build the arch, in application order, the root first.
    #[must_use]
    pub fn applied_view_ids(&self) -> &[i64] {
        &self.table.applied_view_ids
    }

    /// The registry generation the table was captured under.
    #[must_use]
    pub fn registry_generation(&self) -> u64 {
        self.table.registry_generation
    }

    /// Nodes that carry a guard (own `groups` or a field definition with groups). Counts only:
    /// the table itself is not readable.
    #[must_use]
    pub fn node_guard_count(&self) -> usize {
        self.table.node_guards.len()
    }

    /// Owned heap storage retained by the arch and the table, saturating on overflow. A cache
    /// charges the inline struct separately.
    #[must_use]
    pub fn retained_bytes(&self) -> usize {
        let t = &self.table;
        let mut total = self.arch.retained_bytes().unwrap_or(usize::MAX);
        let mut add = |n: usize| total = total.saturating_add(n);
        add(t
            .node_guards
            .len()
            .saturating_mul(std::mem::size_of::<NodeGuard>()));
        add(t
            .guards
            .len()
            .saturating_mul(std::mem::size_of::<GuardText>()));
        for guard in &t.guards {
            add(guard.retained_bytes());
        }
        for (_, ids) in &t.record_guards {
            add(std::mem::size_of::<RecordGuard>());
            add(ids.len().saturating_mul(std::mem::size_of::<i64>()));
        }
        for f in &t.fields {
            add(std::mem::size_of::<FieldRefText>());
            add(f.model.len());
            add(f.path.len());
            if let FieldRefKind::Expr { attribute } = &f.kind {
                add(attribute.len());
            }
        }
        add(t.root_model.len());
        add(t
            .applied_view_ids
            .len()
            .saturating_mul(std::mem::size_of::<i64>()));
        total
    }

    /// The effective guards in index order, as canonical text (tests).
    #[cfg(test)]
    pub(crate) fn guard_texts(&self) -> Vec<String> {
        self.table.guards.iter().map(GuardText::render).collect()
    }

    /// `(node, guard index)` in node order (tests).
    #[cfg(test)]
    pub(crate) fn node_guard_entries(&self) -> Vec<(NodeIdx, usize)> {
        self.table
            .node_guards
            .iter()
            .map(|g| (g.node, g.guard as usize))
            .collect()
    }

    /// `(node, model, field)` of the `<field>` elements in document order (tests).
    #[cfg(test)]
    pub(crate) fn field_entries(&self) -> Vec<(NodeIdx, String, String)> {
        self.table
            .fields
            .iter()
            .filter(|f| f.kind == FieldRefKind::Node)
            .map(|f| (f.node, f.model.to_string(), f.path.to_string()))
            .collect()
    }

    /// `(node, model, path, attribute)` of the expression references in table order (tests).
    #[cfg(test)]
    pub(crate) fn expression_entries(&self) -> Vec<(NodeIdx, String, String, String)> {
        self.table
            .fields
            .iter()
            .filter_map(|f| match &f.kind {
                FieldRefKind::Expr { attribute } => Some((
                    f.node,
                    f.model.to_string(),
                    f.path.to_string(),
                    attribute.to_string(),
                )),
                FieldRefKind::PanelDependency => Some((
                    f.node,
                    f.model.to_string(),
                    f.path.to_string(),
                    "domain".to_owned(),
                )),
                FieldRefKind::Node | FieldRefKind::Comodel => None,
            })
            .collect()
    }

    /// The record guards in order (tests).
    #[cfg(test)]
    pub(crate) fn record_guard_entries(&self) -> Vec<(i64, Vec<i64>)> {
        self.table
            .record_guards
            .iter()
            .map(|(v, ids)| (*v, ids.to_vec()))
            .collect()
    }
}

/// Sorted by view id; ids sorted and deduplicated. A non-positive id, a duplicate view, a view
/// without groups or more than the bound is refused, never repaired.
fn canonical_record_guards(
    input: &[(i64, &[i64])],
    limits: &ViewLimits,
) -> ViewResult<Box<[RecordGuard]>> {
    let bad = |message: String| Err(ViewError::InvalidRecordGuards(message));
    if input.len() > MAX_RECORD_GUARDS {
        return bad(format!(
            "{} record guards, more than the {MAX_RECORD_GUARDS} allowed",
            input.len()
        ));
    }
    let max_ids = limits.get(LimitKind::CheckGroupRefs);
    let mut total_ids = 0usize;
    let mut out: Vec<RecordGuard> = Vec::with_capacity(input.len());
    for &(view, groups) in input {
        if view <= 0 {
            return bad(format!("view id {view} is not positive"));
        }
        if groups.is_empty() {
            return bad(format!(
                "view {view} has no groups; leave a view without groups out"
            ));
        }
        total_ids = total_ids.saturating_add(groups.len());
        if total_ids > max_ids {
            return Err(ViewError::LimitExceeded {
                limit: LimitKind::CheckGroupRefs,
                max: max_ids,
                actual: Some(total_ids),
            });
        }
        if let Some(&id) = groups.iter().find(|&&g| g <= 0) {
            return bad(format!("view {view} has the non-positive group id {id}"));
        }
        let mut ids = groups.to_vec();
        ids.sort_unstable();
        ids.dedup();
        out.push((view, ids.into()));
    }
    out.sort_by_key(|(view, _)| *view);
    if let Some(pair) = out.windows(2).find(|w| w[0].0 == w[1].0) {
        return bad(format!("view {} is listed twice", pair[0].0));
    }
    Ok(out.into())
}

fn panel_ordinal(arch: &Arch, node: NodeId) -> ViewResult<usize> {
    let parent = arch
        .parent(node)
        .ok_or(ViewError::InvariantViolation("panel section has no parent"))?;
    arch.element_children(parent)
        .position(|child| child == node)
        .map(|index| index + 1)
        .ok_or(ViewError::InvariantViolation(
            "panel section not in its parent",
        ))
}
struct PanelRead {
    model: Box<str>,
    path: Box<str>,
    dependency: bool,
}
fn panel_reads(
    model: &str,
    comodel: Option<&str>,
    source: &str,
    lookup: &dyn GuardLookup,
) -> ViewResult<Vec<PanelRead>> {
    if source.is_empty() {
        return Ok(Vec::new());
    }
    let parsed = erp_expr::parse_with_limits(source, erp_expr::EvalLimits::default())
        .map_err(|_| ViewError::InvalidDescription("Invalid panel domain".into()))?;
    let mut refs = Vec::new();
    crate::expression_references::walk_scoped_domain(
        &parsed,
        &mut crate::expression_references::NoMeter,
        |found| {
            let scope = if found.criterion {
                comodel.ok_or_else(|| {
                    ViewError::InvalidDescription("Panel domain requires a relation".into())
                })?
            } else {
                model
            };
            let path = found.parts.join(".");
            let mut current = scope;
            let mut segments = path.split('.').peekable();
            let mut hops = 0;
            while let Some(segment) = segments.next() {
                hops += 1;
                if hops > crate::exprs::MAX_PATH_HOPS || !lookup.has_field(current, segment) {
                    return Err(ViewError::InvalidDescription("Invalid panel field".into()));
                }
                if segments.peek().is_some() {
                    current = lookup.comodel(current, segment).ok_or_else(|| {
                        ViewError::InvalidDescription("Invalid panel relation".into())
                    })?;
                }
            }
            refs.push(PanelRead {
                model: scope.into(),
                path: path.into(),
                dependency: !found.criterion,
            });
            Ok(())
        },
    )?;
    Ok(refs)
}
