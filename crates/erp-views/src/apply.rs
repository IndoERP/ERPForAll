//! The source arch is taken by value and returned on success: when a spec fails, the partly
//! modified arch is dropped with the error, so no caller can observe a half-applied view.

use crate::arch::{Arch, NodeId};
use crate::attributes::apply_attributes;
use crate::error::{LimitKind, ViewError, ViewResult};
use crate::limits::{Budget, ViewLimits};
use crate::locate::{locate, spec_open_tag};
use crate::xpath::text_nodes;
use std::collections::VecDeque;

/// Locating and the depth check after each spec are charged to
/// `budget`, shared by every view of one combination.
pub(crate) fn apply_inheritance_specs(
    source: Arch,
    spec: &Arch,
    limits: &ViewLimits,
    budget: &mut Budget,
) -> ViewResult<Arch> {
    apply_specs(source, spec, limits, budget, false)
}

/// Template replacements preserve the position of a `$0` text node, while server views retain
/// their append policy. Every other position and safety rule uses the same implementation.
pub(crate) fn apply_template_specs(
    source: Arch,
    spec: &Arch,
    limits: &ViewLimits,
    budget: &mut Budget,
) -> ViewResult<Arch> {
    apply_specs(source, spec, limits, budget, true)
}

fn apply_specs(
    mut source: Arch,
    spec: &Arch,
    limits: &ViewLimits,
    budget: &mut Budget,
    template_replacements: bool,
) -> ViewResult<Arch> {
    let max_depth = limits.get(LimitKind::Depth);
    let mut queue = VecDeque::from([spec.root_id()]);
    let mut spec_index = 0usize;
    while let Some(spec_id) = queue.pop_front() {
        if spec.tag(spec_id) == "data" {
            reject_stray_text(spec, spec_id)?;
            queue.extend(spec.element_children(spec_id));
            continue;
        }
        spec_index = spec_index
            .checked_add(1)
            .ok_or(ViewError::InvariantViolation(
                "inheritance operation ordinal overflow",
            ))?;
        let applied = apply_spec(
            &mut source,
            spec,
            spec_id,
            limits,
            budget,
            template_replacements,
        )
        .and_then(|()| {
            let depth = charged_depth(&source, budget)?;
            if depth > max_depth {
                return Err(limits.exceeded(LimitKind::Depth, Some(depth)));
            }
            Ok(())
        });
        applied.map_err(|error| {
            if template_replacements {
                in_spec(error, spec, spec_id, spec_index)
            } else {
                error
            }
        })?;
    }
    Ok(source)
}

/// Applies the children of an Elang primary root as specs, then merges its root. Root merging
/// happens here, before the catalogue applies descendant views, so their selectors see it.
pub(crate) fn apply_primary_template(
    mut source: Arch,
    spec: &Arch,
    limits: &ViewLimits,
    budget: &mut Budget,
) -> ViewResult<Arch> {
    let root = spec.root_id();
    reject_stray_text(spec, root)?;
    let mut queue: VecDeque<_> = spec.element_children(root).collect();
    let mut spec_index = 0usize;
    while let Some(spec_id) = queue.pop_front() {
        if spec.tag(spec_id) == "data" {
            reject_stray_text(spec, spec_id)?;
            queue.extend(spec.element_children(spec_id));
            continue;
        }
        spec_index = spec_index
            .checked_add(1)
            .ok_or(ViewError::InvariantViolation(
                "inheritance operation ordinal overflow",
            ))?;
        apply_spec(&mut source, spec, spec_id, limits, budget, true)
            .and_then(|()| {
                let depth = charged_depth(&source, budget)?;
                let max_depth = limits.get(LimitKind::Depth);
                if depth > max_depth {
                    return Err(limits.exceeded(LimitKind::Depth, Some(depth)));
                }
                Ok(())
            })
            .map_err(|error| in_spec(error, spec, spec_id, spec_index))?;
    }
    let target = source.root_id();
    budget.charge_bytes(spec.tag(root).len())?;
    source.set_tag(target, spec.tag(root))?;
    for (name, value) in spec.attrs(root) {
        budget.charge_bytes(name.len())?;
        if matches!(name.as_str(), "t-inherit" | "t-inherit-mode") {
            continue;
        }
        // set_attr scans target attributes, then copies the value. Charge every name
        // comparison by its byte work too; long equal-length names must not be cheap.
        for (existing, _) in source.attrs(target) {
            budget.str_eq(existing, name)?;
        }
        budget.charge_bytes(value.len())?;
        source.set_attr(target, name, value.clone())?;
    }
    source.set_origin(target, spec.origin(root));
    source.set_source_node(target, spec.source_node(root));
    Ok(source)
}

fn in_spec(error: ViewError, arch: &Arch, spec_id: NodeId, index: usize) -> ViewError {
    ViewError::InSpec {
        index,
        line: arch.origin(spec_id).line(),
        spec: spec_open_tag(arch, spec_id),
        source: Box::new(error),
    }
}

/// Element nesting depth of the live tree (the root alone is 1), one budget step per node.
fn charged_depth(source: &Arch, budget: &mut Budget) -> ViewResult<usize> {
    let mut deepest = 0;
    for (_, depth) in source.walk(source.root_id()) {
        budget.charge()?;
        deepest = deepest.max(depth);
    }
    Ok(deepest + 1)
}

fn apply_spec(
    source: &mut Arch,
    spec: &Arch,
    spec_id: NodeId,
    limits: &ViewLimits,
    budget: &mut Budget,
    template_replacements: bool,
) -> ViewResult<()> {
    let target = locate(source, spec, spec_id, limits, budget)?
        .ok_or_else(|| ViewError::TargetNotFound(spec_open_tag(spec, spec_id)))?;
    let mut op = SpecOp {
        spec,
        spec_id,
        target,
        limits,
        budget,
        template_replacements,
    };
    // `position` defaults to `inside` and `mode` to `outer` when absent (template_inheritance.py
    // lines 159 and 161); present values must be valid.
    match spec.attr(spec_id, "position") {
        None | Some("inside") => op.inside(source),
        Some("after") => op.after(source),
        Some("before") => op.before(source),
        Some("replace") => match spec.attr(spec_id, "mode") {
            None | Some("outer") => op.replace_outer(source),
            Some("inner") => op.replace_inner(source),
            Some(other) => Err(ViewError::InvalidMode(other.to_owned())),
        },
        Some("attributes") => apply_attributes(source, spec, spec_id, target, op.budget),
        Some(other) => Err(ViewError::InvalidPosition(other.to_owned())),
    }
}

/// Non-whitespace text directly in a container whose children are the only meaningful content.
pub(crate) fn reject_stray_text(spec: &Arch, id: NodeId) -> ViewResult<()> {
    let stray = std::iter::once(spec.text(id))
        .chain(spec.element_children(id).map(|c| spec.tail(c)))
        .find(|t| !t.trim().is_empty());
    match stray {
        Some(text) => Err(ViewError::StrayText {
            container: spec.tag(id).to_owned(),
            text: text.to_owned(),
        }),
        None => Ok(()),
    }
}

/// Returns the matched text.
///
/// A match can only end at the end of `s` or just before a final newline, so the scan runs
/// backward over trailing whitespace only (which the caller strips right after): the cost does
/// not grow with the text before it. When `s` ends with a newline, a match ending before it
/// starts earlier than the final newline alone, so it wins.
fn rstrip_match(s: &str) -> &str {
    let b = s.as_bytes();
    let blank_run_start = |end: usize| {
        let mut j = end;
        while j > 0 && (b[j - 1] == b' ' || b[j - 1] == b'\t') {
            j -= 1;
        }
        j
    };
    if b.last() == Some(&b'\n') {
        let last = b.len() - 1;
        let j = blank_run_start(last);
        if j > 0 && b[j - 1] == b'\n' {
            return &s[j - 1..last];
        }
        // The final newline itself, followed by nothing.
        return &s[last..];
    }
    let j = blank_run_start(b.len());
    if j > 0 && b[j - 1] == b'\n' {
        &s[j - 1..]
    } else {
        ""
    }
}

fn add_text_before(source: &mut Arch, node: NodeId, text: &str) -> ViewResult<()> {
    if text.is_empty() {
        return Ok(());
    }
    match source.prev_sibling(node) {
        Some(prev) => source.append_tail(prev, text),
        None => {
            let parent = source.attached_parent(node)?;
            source.rstrip_text(parent);
            source.append_text(parent, text)
        }
    }
}

fn remove_element(source: &mut Arch, node: NodeId) -> ViewResult<()> {
    let tail = source.take_tail(node);
    add_text_before(source, node, &tail)?;
    source.detach(node);
    Ok(())
}

fn is_move(spec: &Arch, id: NodeId) -> bool {
    spec.attr(id, "position") == Some("move")
}

/// A `$0` location: an element with a text node equal to `$0`.
fn is_dollar_zero(spec: &Arch, id: NodeId) -> bool {
    text_nodes(spec, id).any(|t| t == "$0")
}

/// One located spec being applied.
struct SpecOp<'s, 'b> {
    spec: &'s Arch,
    spec_id: NodeId,
    target: NodeId,
    limits: &'s ViewLimits,
    budget: &'b mut Budget,
    template_replacements: bool,
}

impl SpecOp<'_, '_> {
    fn element_children(&self) -> Vec<NodeId> {
        self.spec.element_children(self.spec_id).collect()
    }

    fn extract(&mut self, source: &mut Arch, move_spec: NodeId) -> ViewResult<NodeId> {
        let has_content = self.spec.element_children(move_spec).next().is_some()
            || !self.spec.text(move_spec).trim().is_empty();
        if has_content {
            return Err(ViewError::InvalidMoveSpec(self.spec.node_xml(move_spec)));
        }
        let node = locate(source, self.spec, move_spec, self.limits, self.budget)?
            .ok_or_else(|| ViewError::MoveTargetNotFound(self.spec.node_xml(move_spec)))?;
        if node == source.root_id() {
            return Err(ViewError::MoveRootElement(self.spec.node_xml(move_spec)));
        }
        if source.is_ancestor_or_self(node, self.target) {
            return Err(ViewError::MoveTargetOrAncestor(
                self.spec.node_xml(move_spec),
            ));
        }
        remove_element(source, node)?;
        Ok(node)
    }

    fn add_stripped_items_before(&mut self, source: &mut Arch, anchor: NodeId) -> ViewResult<()> {
        let text = self.spec.text(self.spec_id);
        let before_text = match source.prev_sibling(anchor) {
            None => {
                let parent = source.attached_parent(anchor)?;
                let before = rstrip_match(source.text(parent)).to_owned();
                source.rstrip_text(parent);
                source.append_text(parent, text)?;
                before
            }
            Some(prev) => {
                let before = rstrip_match(source.tail(prev)).to_owned();
                source.rstrip_tail(prev);
                source.append_tail(prev, text)?;
                before
            }
        };
        let children = self.element_children();
        let last = children.len().checked_sub(1);
        for (i, &child) in children.iter().enumerate() {
            let spec_tail = self.spec.tail(child);
            let tail = if Some(i) == last {
                format!("{}{}", spec_tail.trim_end(), before_text)
            } else {
                spec_tail.to_owned()
            };
            let node = if is_move(self.spec, child) {
                let moved = self.extract(source, child)?;
                source.set_tail(moved, tail)?;
                moved
            } else {
                source.import(self.spec, child, tail, &[])?.0
            };
            source.insert_before(anchor, node)?;
        }
        Ok(())
    }

    fn inside(&mut self, source: &mut Arch) -> ViewResult<()> {
        let sentinel = source.alloc_sentinel()?;
        source.append_child(self.target, sentinel);
        self.add_stripped_items_before(source, sentinel)?;
        remove_element(source, sentinel)
    }

    fn after(&mut self, source: &mut Arch) -> ViewResult<()> {
        if self.target == source.root_id() {
            return Err(ViewError::RootSiblingPosition("after".to_owned()));
        }
        let sentinel = source.alloc_sentinel()?;
        source.insert_after(self.target, sentinel)?;
        source.move_tail(self.target, sentinel);
        self.add_stripped_items_before(source, sentinel)?;
        remove_element(source, sentinel)
    }

    fn before(&mut self, source: &mut Arch) -> ViewResult<()> {
        if self.target == source.root_id() {
            return Err(ViewError::RootSiblingPosition("before".to_owned()));
        }
        self.add_stripped_items_before(source, self.target)
    }

    /// `mode="inner"`: the spec content replaces the target's text and children; moves can take
    /// existing children (they are still in place while the spec is inserted before a sentinel).
    fn replace_inner(&mut self, source: &mut Arch) -> ViewResult<()> {
        let sentinel = source.alloc_sentinel()?;
        match source.first_child(self.target) {
            Some(first) => source.insert_before(first, sentinel)?,
            None => source.append_child(self.target, sentinel),
        }
        source.set_text(self.target, String::new())?;
        self.add_stripped_items_before(source, sentinel)?;
        // Drop the old content after the sentinel, then the sentinel, with their tails (lxml's
        // `remove` takes the tail along).
        while let Some(last) = source.last_child(self.target) {
            source.detach(last);
            if last == sentinel {
                break;
            }
        }
        Ok(())
    }

    /// Collected per child, so every location is in the subtree copied for it.
    fn dollar_zero_locations(&self, child: NodeId) -> Vec<NodeId> {
        self.spec
            .walk(child)
            .map(|(id, _)| id)
            .filter(|&id| is_dollar_zero(self.spec, id))
            .collect()
    }

    /// `mode="outer"` (default): the spec children replace the target.
    fn replace_outer(&mut self, source: &mut Arch) -> ViewResult<()> {
        let children = self.element_children();
        if let Some(&bad) = children
            .iter()
            .find(|&&c| is_move(self.spec, c) && is_dollar_zero(self.spec, c))
        {
            return Err(ViewError::InvalidMoveSpec(self.spec.node_xml(bad)));
        }
        if self.target == source.root_id() {
            return self.replace_root(source, &children);
        }
        let text = self.spec.text(self.spec_id);
        if !text.trim().is_empty() {
            return Err(ViewError::ReplaceTextDropped(text.to_owned()));
        }
        let mut copy_into = Vec::new();
        for &child in &children {
            let node = if is_move(self.spec, child) {
                let tail = self.spec.tail(child);
                if !tail.trim().is_empty() {
                    return Err(ViewError::ReplaceTextDropped(tail.to_owned()));
                }
                self.extract(source, child)?
            } else {
                let (node, mapped) = source.import(
                    self.spec,
                    child,
                    self.spec.tail(child).to_owned(),
                    &self.dollar_zero_locations(child),
                )?;
                copy_into.extend(mapped);
                node
            };
            source.insert_before(self.target, node)?;
        }
        for location in copy_into {
            self.fill_dollar_zero(source, location, self.target, true)?;
        }
        if self.template_replacements {
            // The tail is a sibling text node in the template DOM. Keep it at the original
            // replacement location; UI clones deliberately exclude it.
            remove_element(source, self.target)?;
        } else {
            // lxml's `getparent().remove(node)` drops the node with its tail.
            source.detach(self.target);
        }
        Ok(())
    }

    /// Fill only original spec locations, collected before copies. Template mode replaces
    /// exact text slots, so a sibling after `$0` stays after it; a target's tail belongs to
    /// its old parent and is not part of a template clone. Server mode retains append.
    fn fill_dollar_zero(
        &mut self,
        source: &mut Arch,
        location: NodeId,
        target: NodeId,
        server_with_tail: bool,
    ) -> ViewResult<()> {
        if !self.template_replacements {
            source.set_text(location, String::new())?;
            let copy = source.copy_subtree(target, server_with_tail)?;
            source.append_child(location, copy);
            return Ok(());
        }
        let mut tails = Vec::new();
        for child in source.element_children(location) {
            self.budget.charge()?;
            if self.budget.str_eq(source.tail(child), "$0")? {
                tails.push(child);
            }
        }
        if self.budget.str_eq(source.text(location), "$0")? {
            source.set_text(location, String::new())?;
            let copy = source.copy_subtree(target, false)?;
            match source.first_child(location) {
                Some(first) => source.insert_before(first, copy)?,
                None => source.append_child(location, copy),
            }
        }
        for child in tails {
            source.set_tail(child, String::new())?;
            let copy = source.copy_subtree(target, false)?;
            source.insert_after(child, copy)?;
        }
        Ok(())
    }

    /// Replacing the root: exactly one element becomes the new root and keeps the old root's
    /// `t-name`.
    fn replace_root(&mut self, source: &mut Arch, children: &[NodeId]) -> ViewResult<()> {
        let [child] = children else {
            return Err(ViewError::RootReplacementCount {
                found: children.len(),
            });
        };
        if is_move(self.spec, *child) {
            return Err(ViewError::MoveInRootReplacement);
        }
        for text in [self.spec.text(self.spec_id), self.spec.tail(*child)] {
            if !text.trim().is_empty() {
                return Err(ViewError::ReplaceTextDropped(text.to_owned()));
            }
        }
        let old_root = source.root_id();
        let locations = self.dollar_zero_locations(*child);
        let (node, mapped) = source.import(self.spec, *child, String::new(), &locations)?;
        for location in mapped {
            self.fill_dollar_zero(source, location, old_root, false)?;
        }
        if let Some(t_name) = source.attr(old_root, "t-name").map(str::to_owned) {
            source.set_attr(node, "t-name", t_name)?;
        }
        source.set_root(node);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::rstrip_match;
    use proptest::prelude::*;

    /// The forward scan this replaced: every newline tried from the left. Kept as the oracle.
    fn rstrip_match_forward(s: &str) -> &str {
        let b = s.as_bytes();
        for (i, &c) in b.iter().enumerate() {
            if c != b'\n' {
                continue;
            }
            let mut j = i + 1;
            while j < b.len() && (b[j] == b' ' || b[j] == b'\t') {
                j += 1;
            }
            if j == b.len() {
                return &s[i..];
            }
            if j + 1 == b.len() && b[j] == b'\n' {
                return &s[i..j];
            }
        }
        ""
    }

    proptest! {
        #[test]
        fn backward_rstrip_match_equals_the_forward_scan(s in "[a \t\né]{0,24}") {
            prop_assert_eq!(rstrip_match(&s), rstrip_match_forward(&s));
        }
    }

    #[test]
    fn rstrip_match_follows_python_regex() {
        assert_eq!(rstrip_match("abc\n    "), "\n    ");
        assert_eq!(rstrip_match("abc\n  \t"), "\n  \t");
        assert_eq!(rstrip_match("abc"), "");
        assert_eq!(rstrip_match("abc  "), "");
        assert_eq!(rstrip_match("\n"), "\n");
        // Python's `$` matches before a final newline: the leftmost match wins.
        assert_eq!(rstrip_match("abc\n  \n"), "\n  ");
        assert_eq!(rstrip_match("a\nb\n "), "\n ");
        assert_eq!(rstrip_match("é\n "), "\n ");
    }
}
