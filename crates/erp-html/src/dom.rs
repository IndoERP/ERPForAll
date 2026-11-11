//! The arena DOM and the html5ever tree sink that builds it.
//!
//! The tree is a flat `Vec` of nodes addressed by [`NodeId`], a checked `u32` index. There is no
//! recursion over the tree anywhere in this crate, no `Rc`, and no indexing that can panic: a
//! handle that names no node is an internal fault ([`HtmlBudget::fail`]), never a panic.
//!
//! [`DomSink`] counts text and doctype nodes (the tree builder already admits elements and
//! comments), charges every byte it retains *before* the allocation grows, keeps only a saturating
//! count of parse errors, and after a refusal forwards nothing: it answers the stand-in handle the
//! tree builder keeps using (the document) without touching the tree.

use std::borrow::Cow;
use std::cell::{Cell, RefCell};
use std::mem::size_of;
use std::sync::Arc;

use html5ever::budget::Budget;
use html5ever::tendril::StrTendril;
use html5ever::tree_builder::{ElemName, ElementFlags, NodeOrText, QuirksMode, TreeSink};
use html5ever::{Attribute, LocalName, Namespace, QualName};

use crate::budget::HtmlBudget;

/// A node of the arena. Only the sink makes ids; they are never null, and an id from another
/// document or past the end simply names no node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct NodeId(u32);

impl NodeId {
    /// The document node, always the first node of an arena.
    const DOCUMENT: NodeId = NodeId(0);

    #[cfg(test)]
    pub(crate) fn from_raw(raw: u32) -> NodeId {
        NodeId(raw)
    }

    fn index(self) -> usize {
        usize::try_from(self.0).unwrap_or(usize::MAX)
    }
}

/// What a node is.
#[derive(Debug)]
pub(crate) enum NodeKind {
    /// The document; the only node without a parent.
    Document,
    /// The contents of a `<template>` element (a detached fragment).
    TemplateContents,
    /// An element with its attributes.
    Element {
        /// Namespace, prefix and local name.
        name: QualName,
        /// Attributes in source order.
        attrs: Vec<Attribute>,
        /// The detached fragment holding a `<template>`'s children.
        template_contents: Option<NodeId>,
        /// Whether this is a MathML `annotation-xml` integration point.
        mathml_annotation_xml: bool,
    },
    /// Character data.
    Text(String),
    /// A comment.
    Comment(String),
    /// A doctype; neither its name nor its identifiers are kept (the cleaner drops it).
    Doctype,
    /// A processing instruction (the HTML parser does not produce these); its text is not kept.
    ProcessingInstruction,
}

#[derive(Debug)]
struct Node {
    parent: Option<NodeId>,
    children: Vec<NodeId>,
    kind: NodeKind,
}

/// The parsed tree, read-only. All accessors are total: a bad id gives `None` or an empty slice.
#[derive(Debug)]
pub(crate) struct Dom {
    nodes: Vec<Node>,
    parse_errors: u64,
}

impl Dom {
    /// The document node.
    pub(crate) fn document(&self) -> NodeId {
        NodeId::DOCUMENT
    }

    /// The kind of `id`, or `None` when it names no node.
    pub(crate) fn kind(&self, id: NodeId) -> Option<&NodeKind> {
        self.nodes.get(id.index()).map(|n| &n.kind)
    }

    /// The children of `id`, empty when it names no node.
    pub(crate) fn children(&self, id: NodeId) -> &[NodeId] {
        self.nodes
            .get(id.index())
            .map_or(&[][..], |n| n.children.as_slice())
    }

    /// How many parse errors the parser reported (the messages are not kept).
    #[cfg(test)]
    pub(crate) fn parse_errors(&self) -> u64 {
        self.parse_errors
    }
}

/// The element name the sink reports to the tree builder: owned atoms, because the arena sits
/// behind a `RefCell` and cannot lend a reference that outlives its borrow.
#[derive(Debug)]
pub(crate) struct ArenaName {
    ns: Namespace,
    local: LocalName,
}

impl ElemName for ArenaName {
    fn ns(&self) -> &Namespace {
        &self.ns
    }

    fn local_name(&self) -> &LocalName {
        &self.local
    }
}

/// Bytes the heap holds for an attribute list: slots plus names and values.
fn attrs_bytes(attrs: &[Attribute]) -> usize {
    attrs.iter().fold(0usize, |sum, a| {
        sum.saturating_add(size_of::<Attribute>())
            .saturating_add(a.name.local.len())
            .saturating_add(a.value.len())
    })
}

/// The tree sink. Built over a shared [`HtmlBudget`]; the tokenizer and tree builder get the same
/// budget through the parse options.
pub(crate) struct DomSink {
    budget: Arc<HtmlBudget>,
    dom: RefCell<Dom>,
    parse_errors: Cell<u64>,
}

impl DomSink {
    /// An arena holding the document node. If the budget refuses even that, the arena stays empty
    /// and the budget is tripped, so the parse ends at its first check.
    pub(crate) fn new(budget: Arc<HtmlBudget>) -> DomSink {
        let sink = DomSink {
            budget,
            dom: RefCell::new(Dom {
                nodes: Vec::new(),
                parse_errors: 0,
            }),
            parse_errors: Cell::new(0),
        };
        sink.push_node(None, NodeKind::Document, 0);
        sink
    }

    fn refused(&self) -> bool {
        self.budget.refused()
    }

    fn charge(&self, n: usize) -> bool {
        self.budget.admit_bytes(n).is_ok()
    }

    fn work(&self, n: usize) -> bool {
        self.budget
            .admit_work(u64::try_from(n).unwrap_or(u64::MAX))
            .is_ok()
    }

    /// Make room for `additional` more elements in `v`: charge the capacity it will gain, then
    /// reserve exactly that. False means refused (the budget is tripped) or the allocator failed.
    fn reserve<T>(&self, v: &mut Vec<T>, additional: usize) -> bool {
        if v.capacity() - v.len() >= additional {
            return true;
        }
        let needed = v.len().saturating_add(additional);
        let new_cap = needed.max(v.capacity().saturating_mul(2)).max(4);
        let growth = new_cap - v.capacity();
        if !self.charge(growth.saturating_mul(size_of::<T>())) {
            return false;
        }
        if v.try_reserve_exact(new_cap - v.len()).is_err() {
            self.budget.fail("allocation failed");
            return false;
        }
        true
    }

    /// Add a node to the arena and return its id. `heap` is the byte size of the owned data in
    /// `kind`, charged before anything grows. `None` means refused or failed (budget tripped).
    fn push_node(&self, parent: Option<NodeId>, kind: NodeKind, heap: usize) -> Option<NodeId> {
        let mut dom = self.dom.borrow_mut();
        self.push_node_in(&mut dom, parent, kind, heap)
    }

    /// A handle that names no node: the sink's own fault, unless the parse is already refused
    /// (then the parser is only winding down with the stand-in).
    fn bad_handle(&self) {
        if !self.refused() {
            self.budget
                .fail("tree builder used a handle that names no node");
        }
    }

    /// True if `id` can hold children.
    fn is_container(dom: &Dom, id: NodeId) -> bool {
        matches!(
            dom.kind(id),
            Some(NodeKind::Document | NodeKind::TemplateContents | NodeKind::Element { .. })
        )
    }

    /// Detach `child` from its parent, if it has one. Scanning the parent's child list is charged
    /// as work.
    fn detach(&self, dom: &mut Dom, child: NodeId) -> bool {
        let Some(parent) = dom.nodes.get(child.index()).and_then(|n| n.parent) else {
            return true;
        };
        let Some(siblings) = dom.nodes.get(parent.index()).map(|p| p.children.len()) else {
            self.bad_handle();
            return false;
        };
        if !self.work(siblings) {
            return false;
        }
        let Some(p) = dom.nodes.get_mut(parent.index()) else {
            self.bad_handle();
            return false;
        };
        let Some(at) = p.children.iter().position(|c| *c == child) else {
            // The child names a parent that does not list it: the tree is inconsistent.
            self.budget
                .fail("node is missing from its parent's child list");
            return false;
        };
        p.children.remove(at);
        if let Some(c) = dom.nodes.get_mut(child.index()) {
            c.parent = None;
        }
        true
    }

    /// Does the chain of parents from `from` reach `ancestor`? Bounded by the arena size.
    fn reaches(dom: &Dom, from: NodeId, ancestor: NodeId) -> bool {
        let mut at = Some(from);
        for _ in 0..=dom.nodes.len() {
            match at {
                None => return false,
                Some(id) if id == ancestor => return true,
                Some(id) => at = dom.nodes.get(id.index()).and_then(|n| n.parent),
            }
        }
        true
    }

    /// Insert `child` under `parent`, at the end or before `before`.
    fn insert(&self, parent: NodeId, before: Option<NodeId>, child: NodeOrText<NodeId>) {
        if self.refused() || !self.work(1) {
            return;
        }
        let mut dom = self.dom.borrow_mut();
        if !Self::is_container(&dom, parent) {
            drop(dom);
            self.bad_handle();
            return;
        }
        // Where in the parent's child list the new child goes.
        let at = match before {
            None => dom.children(parent).len(),
            Some(sibling) => {
                let list = dom.children(parent);
                if !self.work(list.len()) {
                    return;
                }
                match list.iter().position(|c| *c == sibling) {
                    Some(at) => at,
                    None => {
                        drop(dom);
                        self.bad_handle();
                        return;
                    }
                }
            }
        };
        match child {
            NodeOrText::AppendNode(id) => {
                if dom.kind(id).is_none() || id == NodeId::DOCUMENT {
                    drop(dom);
                    self.bad_handle();
                    return;
                }
                if id == parent || (!dom.children(id).is_empty() && Self::reaches(&dom, parent, id))
                {
                    drop(dom);
                    self.budget
                        .fail("tree builder tried to make a node its own ancestor");
                    return;
                }
                if !self.detach(&mut dom, id) {
                    return;
                }
                // Removing the child from this very parent can move the insertion point.
                let at = at.min(dom.children(parent).len());
                self.link(&mut dom, parent, at, id);
            }
            NodeOrText::AppendText(text) => self.insert_text(&mut dom, parent, at, &text),
        }
    }

    /// Put `child` in `parent`'s child list at `at`.
    fn link(&self, dom: &mut Dom, parent: NodeId, at: usize, child: NodeId) {
        let Some(p) = dom.nodes.get_mut(parent.index()) else {
            self.bad_handle();
            return;
        };
        if !self.reserve(&mut p.children, 1) {
            return;
        }
        p.children.insert(at.min(p.children.len()), child);
        if let Some(c) = dom.nodes.get_mut(child.index()) {
            c.parent = Some(parent);
        }
    }

    /// Add text at `at` in `parent`: merged into the text node just before it when there is one,
    /// else as a new text node (which counts against the node limit).
    fn insert_text(&self, dom: &mut Dom, parent: NodeId, at: usize, text: &str) {
        let previous = at
            .checked_sub(1)
            .and_then(|i| dom.children(parent).get(i).copied());
        if let Some(prev) = previous
            && let Some(Node {
                kind: NodeKind::Text(existing),
                ..
            }) = dom.nodes.get_mut(prev.index())
        {
            let needed = existing.len().saturating_add(text.len());
            if needed > existing.capacity() {
                // Grow by doubling (as `reserve` does for vectors) so many small chunks cost
                // amortised constant time each; charge exactly the capacity gained.
                let new_cap = needed.max(existing.capacity().saturating_mul(2)).max(16);
                if !self.charge(new_cap - existing.capacity()) {
                    return;
                }
                if existing
                    .try_reserve_exact(new_cap - existing.len())
                    .is_err()
                {
                    self.budget.fail("allocation failed");
                    return;
                }
            }
            existing.push_str(text);
            return;
        }
        if self.budget.admit_node().is_err() {
            return;
        }
        let Some(node) = self.push_node_in(
            dom,
            Some(parent),
            NodeKind::Text(text.to_owned()),
            text.len(),
        ) else {
            return;
        };
        self.link(dom, parent, at, node);
    }

    /// `push_node` for a caller that already holds the arena borrow.
    fn push_node_in(
        &self,
        dom: &mut Dom,
        parent: Option<NodeId>,
        kind: NodeKind,
        heap: usize,
    ) -> Option<NodeId> {
        if self.refused() || !self.charge(heap) || !self.reserve(&mut dom.nodes, 1) {
            return None;
        }
        let Ok(raw) = u32::try_from(dom.nodes.len()) else {
            self.budget.fail("too many nodes for a 32-bit id");
            return None;
        };
        dom.nodes.push(Node {
            parent,
            children: Vec::new(),
            kind,
        });
        Some(NodeId(raw))
    }
}

impl TreeSink for DomSink {
    type Handle = NodeId;
    type Output = Dom;
    type ElemName<'a>
        = ArenaName
    where
        Self: 'a;

    fn finish(self) -> Dom {
        let mut dom = self.dom.into_inner();
        dom.parse_errors = self.parse_errors.get();
        dom
    }

    fn parse_error(&self, _msg: Cow<'static, str>) {
        self.parse_errors
            .set(self.parse_errors.get().saturating_add(1));
    }

    fn get_document(&self) -> NodeId {
        NodeId::DOCUMENT
    }

    fn elem_name<'a>(&'a self, target: &'a NodeId) -> ArenaName {
        let dom = self.dom.borrow();
        if let Some(NodeKind::Element { name, .. }) = dom.kind(*target) {
            return ArenaName {
                ns: name.ns.clone(),
                local: name.local.clone(),
            };
        }
        drop(dom);
        // After a refusal the tree builder asks for the name of its stand-in, the document, which
        // has none; that is answered with an empty name. Before a refusal, a node without a name
        // is the tree builder asking about something that is not an element: a fault.
        self.bad_handle();
        ArenaName {
            ns: Namespace::default(),
            local: LocalName::default(),
        }
    }

    fn create_element(&self, name: QualName, attrs: Vec<Attribute>, flags: ElementFlags) -> NodeId {
        if self.refused() || !self.work(1) {
            return NodeId::DOCUMENT;
        }
        let heap = attrs_bytes(&attrs).saturating_add(name.local.len());
        // A template keeps its children in a detached fragment, which is a node of its own.
        let template_contents = if flags.template {
            if self.budget.admit_node().is_err() {
                return NodeId::DOCUMENT;
            }
            match self.push_node(None, NodeKind::TemplateContents, 0) {
                Some(id) => Some(id),
                None => return NodeId::DOCUMENT,
            }
        } else {
            None
        };
        self.push_node(
            None,
            NodeKind::Element {
                name,
                attrs,
                template_contents,
                mathml_annotation_xml: flags.mathml_annotation_xml_integration_point,
            },
            heap,
        )
        .unwrap_or(NodeId::DOCUMENT)
    }

    // The tree builder admits the node before it calls this.
    fn create_comment(&self, text: StrTendril) -> NodeId {
        if self.refused() || !self.work(1) {
            return NodeId::DOCUMENT;
        }
        self.push_node(None, NodeKind::Comment(text.to_string()), text.len())
            .unwrap_or(NodeId::DOCUMENT)
    }

    // Never called by the HTML tree builder; counted here so a future caller cannot add nodes
    // the budget does not see.
    fn create_pi(&self, _target: StrTendril, _data: StrTendril) -> NodeId {
        if self.refused() || !self.work(1) || self.budget.admit_node().is_err() {
            return NodeId::DOCUMENT;
        }
        self.push_node(None, NodeKind::ProcessingInstruction, 0)
            .unwrap_or(NodeId::DOCUMENT)
    }

    fn append(&self, parent: &NodeId, child: NodeOrText<NodeId>) {
        self.insert(*parent, None, child);
    }

    fn append_based_on_parent_node(
        &self,
        element: &NodeId,
        prev_element: &NodeId,
        child: NodeOrText<NodeId>,
    ) {
        if self.refused() {
            return;
        }
        let has_parent = self
            .dom
            .borrow()
            .nodes
            .get(element.index())
            .is_some_and(|n| n.parent.is_some());
        if has_parent {
            self.append_before_sibling(element, child);
        } else {
            self.append(prev_element, child);
        }
    }

    fn append_doctype_to_document(
        &self,
        name: StrTendril,
        public_id: StrTendril,
        system_id: StrTendril,
    ) {
        // Nothing of a doctype is kept; it still counts as a node.
        let _ = (name, public_id, system_id);
        if self.refused() || !self.work(1) || self.budget.admit_node().is_err() {
            return;
        }
        if let Some(id) = self.push_node(None, NodeKind::Doctype, 0) {
            self.insert(NodeId::DOCUMENT, None, NodeOrText::AppendNode(id));
        }
    }

    fn get_template_contents(&self, target: &NodeId) -> NodeId {
        let contents = match self.dom.borrow().kind(*target) {
            Some(NodeKind::Element {
                template_contents, ..
            }) => *template_contents,
            _ => None,
        };
        match contents {
            Some(id) => id,
            None => {
                // After a refusal the stand-in (document) is asked; it answers itself.
                self.bad_handle();
                NodeId::DOCUMENT
            }
        }
    }

    fn same_node(&self, x: &NodeId, y: &NodeId) -> bool {
        x == y
    }

    fn set_quirks_mode(&self, _mode: QuirksMode) {}

    fn append_before_sibling(&self, sibling: &NodeId, new_node: NodeOrText<NodeId>) {
        if self.refused() {
            return;
        }
        let parent = self
            .dom
            .borrow()
            .nodes
            .get(sibling.index())
            .and_then(|n| n.parent);
        match parent {
            Some(parent) => self.insert(parent, Some(*sibling), new_node),
            None => self.bad_handle(),
        }
    }

    fn add_attrs_if_missing(&self, target: &NodeId, attrs: Vec<Attribute>) {
        if self.refused() {
            return;
        }
        let mut dom = self.dom.borrow_mut();
        let Some(Node {
            kind: NodeKind::Element { attrs: have, .. },
            ..
        }) = dom.nodes.get_mut(target.index())
        else {
            drop(dom);
            self.bad_handle();
            return;
        };
        if !self.work(have.len().saturating_mul(attrs.len()).max(1)) {
            return;
        }
        let missing: Vec<Attribute> = attrs
            .into_iter()
            .filter(|new| !have.iter().any(|old| old.name == new.name))
            .collect();
        if !self.charge(attrs_bytes(&missing)) || !self.reserve(have, missing.len()) {
            return;
        }
        have.extend(missing);
    }

    fn remove_from_parent(&self, target: &NodeId) {
        if self.refused() {
            return;
        }
        let mut dom = self.dom.borrow_mut();
        if dom.kind(*target).is_none() {
            drop(dom);
            self.bad_handle();
            return;
        }
        self.detach(&mut dom, *target);
    }

    fn reparent_children(&self, node: &NodeId, new_parent: &NodeId) {
        if self.refused() {
            return;
        }
        let mut dom = self.dom.borrow_mut();
        if !Self::is_container(&dom, *node) || !Self::is_container(&dom, *new_parent) {
            drop(dom);
            self.bad_handle();
            return;
        }
        let moved = match dom.nodes.get_mut(node.index()) {
            Some(n) => std::mem::take(&mut n.children),
            None => {
                drop(dom);
                self.bad_handle();
                return;
            }
        };
        if !self.work(moved.len()) {
            return;
        }
        for child in &moved {
            if let Some(c) = dom.nodes.get_mut(child.index()) {
                c.parent = Some(*new_parent);
            }
        }
        let Some(target) = dom.nodes.get_mut(new_parent.index()) else {
            self.bad_handle();
            return;
        };
        if !self.reserve(&mut target.children, moved.len()) {
            return;
        }
        target.children.extend(moved);
    }

    fn is_mathml_annotation_xml_integration_point(&self, handle: &NodeId) -> bool {
        matches!(
            self.dom.borrow().kind(*handle),
            Some(NodeKind::Element {
                mathml_annotation_xml: true,
                ..
            })
        )
    }

    // Declarative shadow roots are not built: they would add a subtree this budget does not
    // count. The template is parsed as an ordinary `<template>` element.
    fn allow_declarative_shadow_roots(&self, _intended_parent: &NodeId) -> bool {
        false
    }

    // Not built: an `<option>` is not cloned into `<selectedcontent>`, whose subtree would add
    // nodes the budget does not count. `<selectedcontent>` stays empty.
    fn maybe_clone_an_option_into_selectedcontent(&self, _option: &NodeId) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DefaultPolicy, HtmlSanitizeError, Limits};
    use html5ever::{local_name, ns};

    fn sink() -> (DomSink, Arc<HtmlBudget>) {
        let budget = HtmlBudget::new(&Limits::ceiling());
        (DomSink::new(Arc::clone(&budget)), budget)
    }

    fn element(sink: &DomSink, local: LocalName) -> NodeId {
        sink.create_element(
            QualName::new(None, ns!(html), local),
            Vec::new(),
            ElementFlags::default(),
        )
    }

    #[test]
    fn text_chunks_grow_the_text_node_by_doubling_and_are_charged() {
        let (sink, budget) = sink();
        let html = element(&sink, local_name!("html"));
        sink.append(&NodeId::DOCUMENT, NodeOrText::AppendNode(html));
        let before = budget.peak_bytes();
        let mut capacities = std::collections::BTreeSet::new();
        for _ in 0..5_000 {
            sink.append(&html, NodeOrText::AppendText(StrTendril::from("a")));
            let dom = sink.dom.borrow();
            let text = dom.children(html).first().copied().expect("a text node");
            if let Some(NodeKind::Text(t)) = dom.kind(text) {
                capacities.insert(t.capacity());
            }
        }
        assert!(budget.failure().is_none(), "{:?}", budget.failure());
        assert!(
            capacities.len() < 40,
            "{} distinct capacities for 5000 one-byte chunks",
            capacities.len()
        );
        let final_capacity = *capacities.last().expect("a capacity") as u64;
        assert!(
            budget.peak_bytes() - before >= final_capacity,
            "charged {} for a text node of capacity {final_capacity}",
            budget.peak_bytes() - before
        );
    }

    #[test]
    fn a_parent_that_does_not_list_its_child_is_an_internal_error() {
        let (sink, budget) = sink();
        let a = element(&sink, local_name!("div"));
        let b = element(&sink, local_name!("p"));
        // Corrupt on purpose: `b` names `a` as its parent but `a` does not list `b`.
        sink.dom.borrow_mut().nodes[b.index()].parent = Some(a);
        sink.remove_from_parent(&b);
        assert_eq!(
            budget.failure(),
            Some(HtmlSanitizeError::Internal {
                reason: "node is missing from its parent's child list"
            })
        );
    }

    #[test]
    fn a_document_or_template_fragment_as_a_child_is_an_internal_error() {
        let (sink, budget) = sink();
        let html = element(&sink, local_name!("html"));
        sink.append(&NodeId::DOCUMENT, NodeOrText::AppendNode(html));
        let fragment = sink
            .push_node(Some(html), NodeKind::TemplateContents, 0)
            .expect("node");
        {
            let mut dom = sink.dom.borrow_mut();
            sink.link(&mut dom, html, 0, fragment);
        }
        let dom = sink.finish();
        let result = crate::clean::clean(&dom, &DefaultPolicy::new(), &budget, None);
        assert_eq!(
            result.err(),
            Some(HtmlSanitizeError::Internal {
                reason: "a template fragment or document is listed as a child"
            })
        );
    }
}
