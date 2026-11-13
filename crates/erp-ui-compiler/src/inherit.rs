//! The `erp-views` adapter, first part (design note §5): template inheritance runs on
//! the same engine as server views. Every template and extension gets a synthetic id in set order
//! and the default priority 16, so the engine's `(priority, id)` order is the load order. An
//! extension is rewritten to `<data>`; a primary keeps its root and opts into the engine's
//! bounded primary-template merge, so later descendants can select the merged root.
//!
//! Only the templates and extensions that take part in an inheritance are handed to the engine;
//! the others are analysed from their source tree.

use crate::limits::{CompilerLimits, LimitKind};
use crate::names::TemplateName;
use crate::names::is_anchor_name;
use crate::source::Trust;
use crate::xml;
use std::collections::{BTreeMap, BTreeSet};

/// The model name every template descriptor carries: the engine only applies extensions whose
/// model equals their parent's.
const MODEL: &str = "ui.template";

pub(crate) const PRIORITY: i32 = 16;

/// How a top-level child of `<templates>` takes part in inheritance. The template's own name lives
/// in the role, so a name and a role can never disagree (an extension has none).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Role {
    /// A template without `t-inherit`.
    Root { name: TemplateName },
    /// `t-inherit="parent"` in primary mode: a new template built on its parent.
    Primary {
        name: TemplateName,
        parent: TemplateName,
    },
    /// `t-inherit-mode="extension"`: modifies its parent in place.
    Extension { parent: TemplateName },
}

impl Role {
    /// The template's name (`None` for an extension).
    pub(crate) fn name(&self) -> Option<&TemplateName> {
        match self {
            Role::Root { name } | Role::Primary { name, .. } => Some(name),
            Role::Extension { .. } => None,
        }
    }

    /// The parent named by `t-inherit`, if any.
    pub(crate) fn parent(&self) -> Option<&TemplateName> {
        match self {
            Role::Root { .. } => None,
            Role::Primary { parent, .. } | Role::Extension { parent } => Some(parent),
        }
    }

    pub(crate) fn is_extension(&self) -> bool {
        matches!(self, Role::Extension { .. })
    }
}

/// The index of an entry in set order. The `templates` limit (20 000 by default) bounds it, so it
/// fits a `u32` and every synthetic id is exact.
pub(crate) type EntryId = u32;

/// One top-level template or extension, in set order.
#[derive(Debug, Clone)]
pub(crate) struct Entry<'s> {
    /// `None` for an entry whose name or role could not be read: it never takes part.
    pub(crate) role: Option<&'s Role>,
    /// The trust mode of the entry's source.
    pub(crate) trust: Trust,
    /// The whole element's source text.
    pub(crate) element: &'s str,
    /// The source text between its start and end tags.
    pub(crate) inner: &'s str,
}

/// The engine's result for one template.
#[derive(Debug, Clone)]
pub(crate) struct Combined {
    pub(crate) xml: String,
    /// The entries applied, in order: the template's own chain of parents and every extension.
    pub(crate) applied: Vec<EntryId>,
    /// The least trusted mode of the applied entries: the combined template is analysed in it.
    pub(crate) trust: Trust,
    /// One engine origin for each element, in serialized document order.
    pub(crate) origins: Vec<(erp_views::NodeOrigin, Option<u32>)>,
}

/// Why a combination failed: the engine's error and the entry it points at, if any.
#[derive(Debug, Clone)]
pub(crate) struct CombineError {
    pub(crate) error: CombineProblem,
    /// The entry whose arch or spec failed.
    pub(crate) entry: Option<EntryId>,
}

/// Engine errors and adapter syntax errors remain distinct internally; neither is swallowed.
#[derive(Debug, Clone)]
pub(crate) enum CombineProblem {
    View(Box<erp_views::ViewError>),
    Rewrite(RewriteProblem),
    DuplicateAnchor {
        name: String,
        first_tag: String,
        first_origin: erp_views::NodeOrigin,
        second_tag: String,
        second_origin: erp_views::NodeOrigin,
    },
}

impl CombineProblem {
    pub(crate) fn spec_index(&self) -> Option<usize> {
        let Self::View(error) = self else { return None };
        let mut error = error.as_ref();
        // Engine context wrappers form a finite owned chain. Retain the original leaf kind.
        while let erp_views::ViewError::InView { source, .. } = error {
            error = source;
        }
        match error {
            erp_views::ViewError::InSpec { index, .. } => Some(*index),
            // Declaration, graph and root-only failures have no operation ordinal.
            _ => None,
        }
    }

    pub(crate) fn message(&self) -> String {
        match self {
            Self::View(error) => {
                let error = error.as_ref();
                let mut message = error.to_string();
                if let erp_views::ViewError::InView { source, .. } = error {
                    message.push_str(": ");
                    message.push_str(&source.to_string());
                }
                message
            }
            Self::Rewrite(problem) => problem.to_string(),
            Self::DuplicateAnchor {
                name,
                first_tag,
                first_origin,
                second_tag,
                second_origin,
            } => format!(
                "duplicate t-anchor '{name}': <{first_tag}> (view {}, line {}) and <{second_tag}> (view {}, line {}) declare the same anchor",
                first_origin.view_id(),
                first_origin.line(),
                second_origin.view_id(),
                second_origin.line(),
            ),
        }
    }
}

/// The synthetic id of entry `index` (ids start at 1 in set order).
pub(crate) fn synthetic_id(index: EntryId) -> i64 {
    i64::from(index) + 1
}

pub(crate) fn entry_index(id: i64) -> Option<EntryId> {
    EntryId::try_from(id.checked_sub(1)?).ok()
}

/// Combines the templates at `targets` (indices into `entries`), returning exactly one result per
/// target. `by_name` maps template names to their entry; parents are looked up there, and a
/// missing parent is reported by the caller before this runs. Extensions in `excluded` (the ones
/// with errors of their own) are left out, so their parent fails instead of compiling without
/// them (the caller reports it).
pub(crate) fn combine(
    entries: &[Entry<'_>],
    by_name: &BTreeMap<&TemplateName, EntryId>,
    targets: &[EntryId],
    excluded: &BTreeSet<EntryId>,
    limits: erp_views::ViewLimits,
    compiler_limits: &CompilerLimits,
) -> Vec<Result<Combined, CombineError>> {
    let entry = |i: EntryId| &entries[i as usize];
    // The entries taking part: the targets, every extension, and their parents up the chain.
    let mut involved: BTreeSet<EntryId> = BTreeSet::new();
    let mut stack: Vec<EntryId> = targets.to_vec();
    stack.extend((0..entries.len()).filter_map(|i| {
        let i = EntryId::try_from(i).ok()?;
        (entry(i).role.is_some_and(Role::is_extension) && !excluded.contains(&i)).then_some(i)
    }));
    let mut descriptors: Vec<erp_views::ViewDescriptor> = Vec::new();
    while let Some(i) = stack.pop() {
        let Some(role) = entry(i).role else { continue };
        if !involved.insert(i) {
            continue;
        }
        if let Some(parent) = role.parent()
            && let Some(&p) = by_name.get(parent)
        {
            stack.push(p);
        }
    }
    for &i in &involved {
        if let Some(role) = entry(i).role {
            match descriptor(role, entry(i), by_name, i, compiler_limits) {
                Ok(d) => descriptors.push(d),
                Err(message) => {
                    return targets
                        .iter()
                        .map(|_| {
                            Err(CombineError {
                                error: CombineProblem::Rewrite(message.clone()),
                                entry: Some(i),
                            })
                        })
                        .collect();
                }
            }
        }
    }
    let catalogue = match erp_views::ViewCatalogue::with_limits(descriptors, limits) {
        Ok(c) => c,
        Err(error) => {
            return targets
                .iter()
                .map(|_| {
                    Err(CombineError {
                        error: CombineProblem::View(Box::new(error.clone())),
                        entry: None,
                    })
                })
                .collect();
        }
    };
    targets
        .iter()
        .map(|&t| {
            catalogue
                .combine(synthetic_id(t))
                .map_err(|error| {
                    let entry = match &error {
                        erp_views::ViewError::InView { view_id, .. } => entry_index(*view_id),
                        erp_views::ViewError::ParentNotFound { view, .. } => entry_index(*view),
                        _ => None,
                    };
                    CombineError {
                        error: CombineProblem::View(Box::new(error)),
                        entry,
                    }
                })
                .and_then(|c| {
                    check_combined_anchors(c.arch())?;
                    let applied: Vec<EntryId> = c
                        .applied_view_ids()
                        .iter()
                        .filter_map(|&id| entry_index(id))
                        .collect();
                    let trust = applied
                        .iter()
                        .fold(entry(t).trust, |acc, &i| acc.least(entry(i).trust));
                    let mut origins = Vec::new();
                    let mut pending = vec![c.arch().root()];
                    while let Some(node) = pending.pop() {
                        origins.push((node.origin(), node.source_node_index()));
                        let children: Vec<_> = node.children().collect();
                        pending.extend(children.into_iter().rev());
                    }
                    Ok(Combined {
                        xml: c.to_xml(),
                        applied,
                        trust,
                        origins,
                    })
                })
        })
        .collect()
}

/// Duplicate diagnostics retain both engine origins. The caller resolves synthetic view ids
/// to addon/file locations; uniqueness is local to one complete template, never the whole set.
fn check_combined_anchors(arch: &erp_views::Arch) -> Result<(), CombineError> {
    let mut anchors: BTreeMap<&str, (&str, erp_views::NodeOrigin)> = BTreeMap::new();
    let mut pending = vec![arch.root()];
    while let Some(node) = pending.pop() {
        if let Some(name) = node.attr("t-anchor")
            && is_anchor_name(name)
            && let Some((tag, origin)) = anchors.insert(name, (node.tag(), node.origin()))
        {
            return Err(CombineError {
                entry: entry_index(node.origin().view_id()),
                error: CombineProblem::DuplicateAnchor {
                    name: name.to_owned(),
                    first_tag: tag.to_owned(),
                    first_origin: origin,
                    second_tag: node.tag().to_owned(),
                    second_origin: node.origin(),
                },
            });
        }
        let children: Vec<_> = node.children().collect();
        pending.extend(children.into_iter().rev());
    }
    Ok(())
}

fn descriptor(
    role: &Role,
    e: &Entry<'_>,
    by_name: &BTreeMap<&TemplateName, EntryId>,
    i: EntryId,
    limits: &CompilerLimits,
) -> Result<erp_views::ViewDescriptor, RewriteProblem> {
    let id = synthetic_id(i);
    let label = match role {
        Role::Root { name } | Role::Primary { name, .. } => name.as_str().to_owned(),
        Role::Extension { parent } => format!("extension of {parent}"),
    };
    let data = || format!("<data>{}</data>", e.inner);
    // A missing parent maps to id 0, which the engine reports as ParentNotFound.
    let parent_id = |parent: &TemplateName| by_name.get(parent).map_or(0, |&p| synthetic_id(p));
    let d = match role {
        Role::Root { .. } => erp_views::ViewDescriptor::new(id, label, MODEL, e.element),
        Role::Primary { parent, .. } => {
            let arch = rewrite_anchors(e.element, limits)?;
            erp_views::ViewDescriptor::new(id, label, MODEL, arch)
                .primary_template_of(parent_id(parent))
        }
        Role::Extension { parent } => {
            let arch = rewrite_anchors(&data(), limits)?;
            erp_views::ViewDescriptor::new(id, label, MODEL, arch).extension_of(parent_id(parent))
        }
    };
    Ok(d.with_priority(PRIORITY).with_template_replacements())
}

/// Specific shorthand failures, kept distinct from the engine's view errors.
#[derive(Debug, Clone, thiserror::Error)]
pub(crate) enum RewriteProblem {
    #[error("<anchor> requires a 'name' attribute")]
    MissingName,
    #[error("anchor name '{0}' must be [a-z0-9_.@-]+ and at most 128 bytes")]
    InvalidName(String),
    #[error("<anchor> does not accept attribute '{0}'")]
    UnknownAttribute(String),
    #[error("Invalid position attribute: '{0}'")]
    InvalidPosition(String),
    #[error("Invalid mode attribute: “{0}”")]
    InvalidMode(String),
    #[error("{message}")]
    Xml { message: String },
    #[error("anchor rewrite exceeds file_bytes limit: maximum {max}")]
    OutputLimit { max: usize },
    #[error("internal compiler error: {0}")]
    Invariant(&'static str),
}

/// Rewrites the locator shorthand without reserializing template text or attribute values.
/// The XML is parsed behind the same file/node/depth limits as the front end. Names contain
/// no quotes, so the generated XPath cannot acquire an extra predicate or string boundary.
fn rewrite_anchors(text: &str, limits: &CompilerLimits) -> Result<String, RewriteProblem> {
    if !text.contains("<anchor") {
        return Ok(text.to_owned());
    }
    let doc = xml::parse_document(text, limits)
        .map_err(|p| RewriteProblem::Xml { message: p.message })?;
    let mut edits: Vec<(std::ops::Range<usize>, String)> = Vec::new();
    let mut output_bytes = text.len();
    let max = limits.get(LimitKind::FileBytes);
    for node in doc
        .descendants()
        .filter(|n| n.is_element() && n.tag_name().name() == "anchor")
    {
        let name = node.attribute("name").ok_or(RewriteProblem::MissingName)?;
        if !is_anchor_name(name) {
            return Err(RewriteProblem::InvalidName(name.to_owned()));
        }
        for attr in node.attributes() {
            if !matches!(attr.name(), "name" | "position" | "mode") {
                return Err(RewriteProblem::UnknownAttribute(attr.name().to_owned()));
            }
        }
        if let Some(position) = node.attribute("position")
            && !matches!(
                position,
                "inside" | "before" | "after" | "replace" | "attributes" | "move"
            )
        {
            return Err(RewriteProblem::InvalidPosition(position.to_owned()));
        }
        if let Some(mode) = node.attribute("mode")
            && !matches!(mode, "outer" | "inner")
        {
            return Err(RewriteProblem::InvalidMode(mode.to_owned()));
        }
        let range = node.range();
        let open_end = opening_end(text, range.start)
            .ok_or(RewriteProblem::Invariant("anchor opening tag has no end"))?;
        let self_closing = text[range.start..open_end].ends_with("/>");
        let mut opening = format!("<xpath expr=\"//*[@t-anchor='{name}']\"");
        for attr in node.attributes().filter(|a| a.name() != "name") {
            // Position and mode values have just been checked against ASCII constants.
            opening.push(' ');
            opening.push_str(attr.name());
            opening.push_str("=\"");
            opening.push_str(attr.value());
            opening.push('"');
        }
        opening.push_str(if self_closing { "/>" } else { ">" });
        let mut add =
            |range: std::ops::Range<usize>, replacement: String| -> Result<(), RewriteProblem> {
                output_bytes = output_bytes
                    .checked_sub(range.len())
                    .and_then(|n| n.checked_add(replacement.len()))
                    .filter(|&n| n <= max)
                    .ok_or(RewriteProblem::OutputLimit { max })?;
                edits.push((range, replacement));
                Ok(())
            };
        add(range.start..open_end, opening)?;
        if !self_closing {
            let close = text[open_end..range.end]
                .rfind("</")
                .map(|i| open_end + i)
                .ok_or(RewriteProblem::Invariant("anchor closing tag missing"))?;
            add(close..range.end, "</xpath>".to_owned())?;
        }
    }
    edits.sort_by_key(|(r, _)| r.start);
    let mut out = String::with_capacity(output_bytes);
    let mut offset = 0;
    for (range, replacement) in edits {
        let unchanged = text
            .get(offset..range.start)
            .ok_or(RewriteProblem::Invariant("overlapping anchor rewrite"))?;
        out.push_str(unchanged);
        out.push_str(&replacement);
        offset = range.end;
    }
    out.push_str(&text[offset..]);
    Ok(out)
}

/// A quoted `>` is part of an attribute, not the end of the opening tag.
pub(crate) fn opening_end(text: &str, start: usize) -> Option<usize> {
    let mut quote = None;
    for (offset, byte) in text.as_bytes().get(start..)?.iter().copied().enumerate() {
        match (quote, byte) {
            (Some(wanted), found) if wanted == found => quote = None,
            (None, b'\'' | b'"') => quote = Some(byte),
            (None, b'>') => return start.checked_add(offset)?.checked_add(1),
            // Other bytes leave the quote state unchanged.
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(name: &str) -> TemplateName {
        TemplateName::new(name).unwrap()
    }

    fn entry<'s>(role: &'s Role, element: &'s str, inner: &'s str) -> Entry<'s> {
        Entry {
            role: Some(role),
            trust: Trust::Trusted,
            element,
            inner,
        }
    }

    fn ext(parent: &str) -> Role {
        Role::Extension { parent: n(parent) }
    }

    /// Design note §5 item 1: three addons, two files each, extensions written before and after
    /// their parent: the engine applies them in set order.
    #[test]
    fn synthetic_ids_give_load_order() {
        let roles = [
            ext("a.T"),
            Role::Root { name: n("a.T") },
            ext("a.T"),
            Role::Primary {
                name: n("b.P"),
                parent: n("a.T"),
            },
            ext("b.P"),
            ext("a.T"),
        ];
        let entries = vec![
            // addon a, file 1: an extension written before its parent
            entry(
                &roles[0],
                "",
                "<xpath expr=\"//div\" position=\"inside\"><i>1</i></xpath>",
            ),
            // addon a, file 2
            entry(&roles[1], "<t t-name=\"a.T\"><div/></t>", ""),
            // addon b, file 1
            entry(
                &roles[2],
                "",
                "<xpath expr=\"//div\" position=\"inside\"><i>2</i></xpath>",
            ),
            entry(
                &roles[3],
                "<t t-name=\"b.P\" t-inherit=\"a.T\"><xpath expr=\"//div\" position=\"inside\"><i>p</i></xpath></t>",
                "<xpath expr=\"//div\" position=\"inside\"><i>p</i></xpath>",
            ),
            // addon b, file 2
            entry(
                &roles[4],
                "",
                "<xpath expr=\"//div\" position=\"inside\"><i>3</i></xpath>",
            ),
            // addon c, file 1: extends the parent after the primary child was declared
            entry(
                &roles[5],
                "",
                "<xpath expr=\"//div\" position=\"inside\"><i>4</i></xpath>",
            ),
        ];
        let (a, b) = (n("a.T"), n("b.P"));
        let by_name: BTreeMap<&TemplateName, EntryId> = [(&a, 1), (&b, 3)].into_iter().collect();
        let out = combine(
            &entries,
            &by_name,
            &[1, 3],
            &BTreeSet::new(),
            erp_views::ViewLimits::default(),
            &CompilerLimits::default(),
        );
        assert_eq!(out.len(), 2);
        let parent = out[0].as_ref().unwrap();
        assert_eq!(parent.applied, vec![1, 0, 2, 5]);
        assert_eq!(parent.trust, Trust::Trusted);
        assert_eq!(
            parent.xml,
            "<t t-name=\"a.T\"><div><i>1</i><i>2</i><i>4</i></div></t>"
        );
        let child = out[1].as_ref().unwrap();
        // The primary child sees every extension of its parent, also the one declared after it,
        // then its own extension.
        assert_eq!(child.applied, vec![1, 0, 2, 5, 3, 4]);
        assert_eq!(
            child.xml,
            "<t t-name=\"b.P\"><div><i>1</i><i>2</i><i>4</i><i>p</i><i>3</i></div></t>"
        );
        // An excluded extension is not applied.
        let out = combine(
            &entries,
            &by_name,
            &[1],
            &[2].into_iter().collect(),
            erp_views::ViewLimits::default(),
            &CompilerLimits::default(),
        );
        assert_eq!(out[0].as_ref().unwrap().applied, vec![1, 0, 5]);
    }

    #[test]
    fn errors_point_at_the_failing_entry() {
        let roles = [Role::Root { name: n("a.T") }, ext("a.T")];
        let entries = vec![
            entry(&roles[0], "<t t-name=\"a.T\"><div/></t>", ""),
            entry(
                &roles[1],
                "",
                "<xpath expr=\"//span\" position=\"inside\"/>",
            ),
        ];
        let a = n("a.T");
        let by_name: BTreeMap<&TemplateName, EntryId> = [(&a, 0)].into_iter().collect();
        let out = combine(
            &entries,
            &by_name,
            &[0],
            &BTreeSet::new(),
            erp_views::ViewLimits::default(),
            &CompilerLimits::default(),
        );
        let err = out[0].as_ref().unwrap_err();
        assert_eq!(err.entry, Some(1));
        assert!(matches!(
            &err.error,
            CombineProblem::View(error) if matches!(error.as_ref(), erp_views::ViewError::InView { .. })
        ));
    }

    #[test]
    fn ids_are_exact() {
        assert_eq!(synthetic_id(0), 1);
        assert_eq!(synthetic_id(u32::MAX), i64::from(u32::MAX) + 1);
        assert_eq!(entry_index(synthetic_id(41)), Some(41));
        assert_eq!(entry_index(0), None);
    }
}
