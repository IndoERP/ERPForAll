//! Arena-based arch tree.
//!
//! An empty string means "no
//! text" (the serializer writes `<a/>` for both of lxml's `None` and `''`).
//!
//! Every allocation is charged against a node cap and a byte cap (tag names, attributes, text),
//! so a combination that copies or builds too much fails with
//! [`ViewError::LimitExceeded`] instead of growing without bound.

use crate::error::{LimitKind, ViewError, ViewResult};
use crate::limits::{Budget, ViewLimits};
use std::collections::HashMap;
use std::num::NonZeroU32;

/// Index of a node in its [`Arch`]. Only meaningful for the arch that created it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct NodeId(u32);

impl NodeId {
    #[cfg(feature = "host-access")]
    pub(crate) fn from_idx(index: NodeIdx) -> Self {
        Self(index.0)
    }
    pub(crate) fn index(self) -> usize {
        self.0 as usize
    }

    /// The public handle of this node.
    pub(crate) fn to_idx(self) -> NodeIdx {
        NodeIdx(self.0)
    }
}

/// An opaque node index in an [`Arch`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NodeIdx(pub(crate) u32);

impl NodeIdx {
    /// The 0-based index of this node in the arch's node arena.
    #[must_use]
    pub fn index(self) -> usize {
        self.0 as usize
    }
}

/// What to do with an element during filtered arch serialization.
///
/// Crate-private: only the `host-access` bridge ([`crate::host_access`]) drives it, from verdicts
/// an evaluation decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
#[cfg_attr(not(any(test, feature = "host-access")), allow(dead_code))]
pub(crate) enum FilterDecision {
    /// Render this element and its subtree normally.
    Allow,
    /// Skip this element and its subtree entirely; its tail text is preserved.
    Deny,
    /// Unwrap this element: omit its start and end tags, but render its text, children and tail.
    Unwrap,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NodeKind {
    Element,
    Sentinel,
}

/// Where an element of an arch was written: the view whose arch held it and the 1-based line of
/// its start tag in that arch.
///
/// Parsing records the line of every element; content a spec inserts keeps its spec's view and
/// line, a copy (`$0`, a moved node) keeps the origin of what it copies. An error found in a
/// combined arch can so name the view and line that wrote the faulty node, not only the view
/// being validated.
///
/// ```
/// use erp_views::{ViewCatalogue, ViewDescriptor};
///
/// let catalogue = ViewCatalogue::new(vec![
///     ViewDescriptor::new(1, "base", "m", "<form>\n  <a/>\n</form>"),
///     ViewDescriptor::new(2, "ext", "m", "<a position=\"after\">\n\n<b/></a>").extension_of(1),
/// ])?;
/// let combined = catalogue.combine(1)?;
/// let root = combined.arch().root();
/// let origins: Vec<(i64, u32)> = root
///     .children()
///     .map(|c| (c.origin().view_id(), c.origin().line()))
///     .collect();
/// assert_eq!(origins, [(1, 2), (2, 3)]);
/// # Ok::<(), erp_views::ViewError>(())
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NodeOrigin {
    view_id: i64,
    line: u32,
}

impl NodeOrigin {
    /// The origin of a node of view `view_id` at `line` (1-based: a line 0 cannot be written,
    /// review T-7, so no caller can make an origin that looks like the engine's own).
    pub const fn new(view_id: i64, line: NonZeroU32) -> Self {
        Self {
            view_id,
            line: line.get(),
        }
    }

    /// The view whose arch holds the node.
    pub const fn view_id(&self) -> i64 {
        self.view_id
    }

    /// The 1-based line of the node's start tag in that view's arch.
    pub const fn line(&self) -> u32 {
        self.line
    }
}

/// The origin of a node the engine builds itself (a sentinel), which never reaches a finished
/// arch: view 0, line 0 ([`NodeOrigin::new`] cannot make line 0).
const ENGINE_ORIGIN: NodeOrigin = NodeOrigin {
    view_id: 0,
    line: 0,
};

#[derive(Debug, Clone)]
pub(crate) struct NodeData {
    kind: NodeKind,
    origin: NodeOrigin,
    /// Original parsed element ordinal in origin.view_id; copies retain it.
    source_node: Option<u32>,
    tag: String,
    attrs: Vec<(String, String)>,
    text: String,
    tail: String,
    parent: Option<NodeId>,
    first_child: Option<NodeId>,
    last_child: Option<NodeId>,
    prev: Option<NodeId>,
    next: Option<NodeId>,
}

/// Node, byte and per-element attribute caps of one arena, with the limit each one reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Caps {
    nodes: usize,
    node_kind: LimitKind,
    bytes: usize,
    byte_kind: LimitKind,
    attrs: usize,
}

impl Caps {
    /// Caps of one parsed arch.
    pub(crate) fn parsed(limits: &ViewLimits) -> Self {
        Self {
            nodes: limits.get(LimitKind::ArchElements),
            node_kind: LimitKind::ArchElements,
            bytes: limits.get(LimitKind::ArchBytes),
            byte_kind: LimitKind::ArchBytes,
            attrs: limits.get(LimitKind::ElementAttributes),
        }
    }

    /// Caps of the arch being combined.
    pub(crate) fn combined(limits: &ViewLimits) -> Self {
        Self {
            nodes: limits.get(LimitKind::CombinedNodes),
            node_kind: LimitKind::CombinedNodes,
            bytes: limits.get(LimitKind::CombinedBytes),
            byte_kind: LimitKind::CombinedBytes,
            attrs: limits.get(LimitKind::ElementAttributes),
        }
    }
}

/// A parsed or combined view architecture.
///
/// Read it through [`Arch::root`] or serialize it with [`Arch::to_xml`]. Two archs are equal when
/// they serialize to the same XML.
#[derive(Debug, Clone)]
pub struct Arch {
    nodes: Vec<NodeData>,
    root: NodeId,
    bytes: usize,
    caps: Caps,
}

/// One node of the subtree being copied, captured before the copy is allocated.
struct Snapshot {
    depth: usize,
    src: NodeId,
    kind: NodeKind,
    origin: NodeOrigin,
    /// Original parsed element ordinal in origin.view_id; copies retain it.
    source_node: Option<u32>,
    tag: String,
    attrs: Vec<(String, String)>,
    text: String,
    tail: String,
}

fn attrs_len(attrs: &[(String, String)]) -> usize {
    attrs.iter().map(|(k, v)| k.len() + v.len()).sum()
}

impl Arch {
    /// An arch with a single root element and no attributes.
    pub(crate) fn with_root(
        tag: String,
        attrs: Vec<(String, String)>,
        caps: Caps,
    ) -> ViewResult<Self> {
        let mut arch = Self {
            nodes: Vec::new(),
            root: NodeId(0),
            bytes: 0,
            caps,
        };
        arch.root = arch.alloc(NodeKind::Element, tag, attrs, String::new(), String::new())?;
        Ok(arch)
    }

    pub(crate) fn set_caps(&mut self, caps: Caps) {
        self.caps = caps;
    }

    fn charge(&mut self, bytes: usize) -> ViewResult<()> {
        let total = self.bytes.checked_add(bytes);
        match total {
            Some(total) if total <= self.caps.bytes => {
                self.bytes = total;
                Ok(())
            }
            _ => Err(ViewError::LimitExceeded {
                limit: self.caps.byte_kind,
                max: self.caps.bytes,
                actual: total,
            }),
        }
    }

    /// Allocates a detached node, charging its strings and the node itself.
    pub(crate) fn alloc(
        &mut self,
        kind: NodeKind,
        tag: String,
        attrs: Vec<(String, String)>,
        text: String,
        tail: String,
    ) -> ViewResult<NodeId> {
        self.check_attr_count(attrs.len())?;
        let len = self.nodes.len();
        if len >= self.caps.nodes {
            return Err(ViewError::LimitExceeded {
                limit: self.caps.node_kind,
                max: self.caps.nodes,
                actual: len.checked_add(1),
            });
        }
        let id = u32::try_from(len).map_err(|_| ViewError::LimitExceeded {
            limit: self.caps.node_kind,
            max: self.caps.nodes,
            actual: len.checked_add(1),
        })?;
        self.charge(tag.len() + attrs_len(&attrs) + text.len() + tail.len())?;
        self.nodes.push(NodeData {
            kind,
            origin: ENGINE_ORIGIN,
            source_node: match kind {
                NodeKind::Element => Some(id),
                NodeKind::Sentinel => None,
            },
            tag,
            attrs,
            text,
            tail,
            parent: None,
            first_child: None,
            last_child: None,
            prev: None,
            next: None,
        });
        Ok(NodeId(id))
    }

    /// Rejects an element that would carry `count` attributes when that is over the cap.
    fn check_attr_count(&self, count: usize) -> ViewResult<()> {
        if count > self.caps.attrs {
            return Err(ViewError::LimitExceeded {
                limit: LimitKind::ElementAttributes,
                max: self.caps.attrs,
                actual: Some(count),
            });
        }
        Ok(())
    }

    pub(crate) fn alloc_sentinel(&mut self) -> ViewResult<NodeId> {
        self.alloc(
            NodeKind::Sentinel,
            String::new(),
            Vec::new(),
            String::new(),
            String::new(),
        )
    }

    fn data(&self, id: NodeId) -> &NodeData {
        &self.nodes[id.index()]
    }

    fn data_mut(&mut self, id: NodeId) -> &mut NodeData {
        &mut self.nodes[id.index()]
    }

    // --- reading -------------------------------------------------------------------------------

    pub(crate) fn root_id(&self) -> NodeId {
        self.root
    }

    /// Number of slots in the arena (live and removed nodes); sizes per-node scratch tables.
    pub(crate) fn slots(&self) -> usize {
        self.nodes.len()
    }

    /// Owned heap storage retained by this arena, including spare container capacity.
    /// The caller charges the inline `Arch` in its containing cache/output slab separately.
    pub(crate) fn retained_bytes(&self) -> Option<usize> {
        let base = self
            .nodes
            .capacity()
            .checked_mul(std::mem::size_of::<NodeData>())?;
        self.nodes.iter().try_fold(base, |total, node| {
            let attrs = node
                .attrs
                .capacity()
                .checked_mul(std::mem::size_of::<(String, String)>())?;
            let total = total
                .checked_add(attrs)?
                .checked_add(node.tag.capacity())?
                .checked_add(node.text.capacity())?
                .checked_add(node.tail.capacity())?;
            node.attrs.iter().try_fold(total, |sum, (name, value)| {
                sum.checked_add(name.capacity())?
                    .checked_add(value.capacity())
            })
        })
    }

    pub(crate) fn is_sentinel(&self, id: NodeId) -> bool {
        self.data(id).kind == NodeKind::Sentinel
    }

    pub(crate) fn tag(&self, id: NodeId) -> &str {
        &self.data(id).tag
    }

    pub(crate) fn origin(&self, id: NodeId) -> NodeOrigin {
        self.data(id).origin
    }

    pub(crate) fn source_node(&self, id: NodeId) -> Option<u32> {
        self.data(id).source_node
    }

    pub(crate) fn set_source_node(&mut self, id: NodeId, source: Option<u32>) {
        self.data_mut(id).source_node = source;
    }

    /// Records where the node `id` was written.
    pub(crate) fn set_origin(&mut self, id: NodeId, origin: NodeOrigin) {
        self.data_mut(id).origin = origin;
    }

    pub(crate) fn attrs(&self, id: NodeId) -> &[(String, String)] {
        &self.data(id).attrs
    }

    pub(crate) fn attr(&self, id: NodeId, name: &str) -> Option<&str> {
        self.data(id)
            .attrs
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    /// [`Arch::attr`] with the scan charged to `budget`: each attribute name compared costs one
    /// step plus its bytes when the lengths are equal.
    pub(crate) fn attr_charged(
        &self,
        id: NodeId,
        name: &str,
        budget: &mut Budget,
    ) -> ViewResult<Option<&str>> {
        for (k, v) in &self.data(id).attrs {
            if budget.str_eq(k, name)? {
                return Ok(Some(v));
            }
        }
        Ok(None)
    }

    pub(crate) fn text(&self, id: NodeId) -> &str {
        &self.data(id).text
    }

    pub(crate) fn tail(&self, id: NodeId) -> &str {
        &self.data(id).tail
    }

    pub(crate) fn parent(&self, id: NodeId) -> Option<NodeId> {
        self.data(id).parent
    }

    pub(crate) fn first_child(&self, id: NodeId) -> Option<NodeId> {
        self.data(id).first_child
    }

    pub(crate) fn last_child(&self, id: NodeId) -> Option<NodeId> {
        self.data(id).last_child
    }

    pub(crate) fn next_sibling(&self, id: NodeId) -> Option<NodeId> {
        self.data(id).next
    }

    pub(crate) fn prev_sibling(&self, id: NodeId) -> Option<NodeId> {
        self.data(id).prev
    }

    /// The parent of a node that must be attached below the root.
    pub(crate) fn attached_parent(&self, id: NodeId) -> ViewResult<NodeId> {
        self.parent(id).ok_or(ViewError::InvariantViolation(
            "node expected below the root has no parent",
        ))
    }

    /// Child elements, sentinels skipped.
    pub(crate) fn element_children(&self, id: NodeId) -> impl Iterator<Item = NodeId> + '_ {
        std::iter::successors(self.first_child(id), move |&c| self.next_sibling(c))
            .filter(move |&c| !self.is_sentinel(c))
    }

    /// Pre-order walk of the subtree of `top` (sentinels included) with depths relative to `top`.
    pub(crate) fn walk(&self, top: NodeId) -> Walk<'_> {
        Walk {
            arch: self,
            top,
            next: Some((top, 0)),
        }
    }

    /// `true` when `ancestor` is `node` or one of its ancestors.
    pub(crate) fn is_ancestor_or_self(&self, ancestor: NodeId, node: NodeId) -> bool {
        std::iter::successors(Some(node), |&n| self.parent(n)).any(|n| n == ancestor)
    }

    /// Element nesting depth of the live tree (the root alone is 1). The engine itself measures
    /// depth through the evaluation budget (`apply::charged_depth`).
    #[cfg(test)]
    pub(crate) fn max_depth(&self) -> usize {
        self.walk(self.root)
            .map(|(_, depth)| depth)
            .max()
            .map_or(1, |d| d + 1)
    }

    // --- mutation ------------------------------------------------------------------------------

    /// Renames an existing element without changing its children or text. The same retained
    /// byte cap as insertion applies; callers charge evaluation work separately.
    pub(crate) fn set_tag(&mut self, id: NodeId, tag: &str) -> ViewResult<()> {
        self.charge(tag.len().saturating_sub(self.data(id).tag.len()))?;
        self.data_mut(id).tag = tag.to_owned();
        Ok(())
    }

    pub(crate) fn set_text(&mut self, id: NodeId, text: String) -> ViewResult<()> {
        self.charge(text.len().saturating_sub(self.data(id).text.len()))?;
        self.data_mut(id).text = text;
        Ok(())
    }

    pub(crate) fn set_tail(&mut self, id: NodeId, tail: String) -> ViewResult<()> {
        self.charge(tail.len().saturating_sub(self.data(id).tail.len()))?;
        self.data_mut(id).tail = tail;
        Ok(())
    }

    /// Appends to the text of `id` in place (amortized, no copy of the existing text).
    pub(crate) fn append_text(&mut self, id: NodeId, more: &str) -> ViewResult<()> {
        self.charge(more.len())?;
        self.data_mut(id).text.push_str(more);
        Ok(())
    }

    /// Appends to the tail of `id` in place.
    pub(crate) fn append_tail(&mut self, id: NodeId, more: &str) -> ViewResult<()> {
        self.charge(more.len())?;
        self.data_mut(id).tail.push_str(more);
        Ok(())
    }

    /// Strips trailing whitespace from the text of `id` (Python's `str.rstrip()`), in place.
    pub(crate) fn rstrip_text(&mut self, id: NodeId) {
        let t = &mut self.data_mut(id).text;
        let keep = t.trim_end().len();
        t.truncate(keep);
    }

    /// Strips trailing whitespace from the tail of `id`, in place.
    pub(crate) fn rstrip_tail(&mut self, id: NodeId) {
        let t = &mut self.data_mut(id).tail;
        let keep = t.trim_end().len();
        t.truncate(keep);
    }

    /// Moves the tail of `from` to `to` (which must have none); nothing new is built.
    pub(crate) fn move_tail(&mut self, from: NodeId, to: NodeId) {
        let tail = std::mem::take(&mut self.data_mut(from).tail);
        self.data_mut(to).tail = tail;
    }

    pub(crate) fn take_tail(&mut self, id: NodeId) -> String {
        std::mem::take(&mut self.data_mut(id).tail)
    }

    pub(crate) fn set_attr(&mut self, id: NodeId, name: &str, value: String) -> ViewResult<()> {
        let existing = self.data(id).attrs.iter().position(|(k, _)| k == name);
        match existing {
            Some(i) => {
                let old = self.data(id).attrs[i].1.len();
                self.charge(value.len().saturating_sub(old))?;
                self.data_mut(id).attrs[i].1 = value;
            }
            None => {
                self.check_attr_count(self.data(id).attrs.len().saturating_add(1))?;
                self.charge(name.len() + value.len())?;
                self.data_mut(id).attrs.push((name.to_owned(), value));
            }
        }
        Ok(())
    }

    pub(crate) fn remove_attr(&mut self, id: NodeId, name: &str) {
        self.data_mut(id).attrs.retain(|(k, _)| k != name);
    }

    /// Removes attribute `name` from `node`. Crate-private: a finished arch has no
    /// public mutator.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn remove_attribute(&mut self, node: NodeIdx, name: &str) {
        self.remove_attr(NodeId(node.0), name);
    }

    /// Unlinks `id` from its parent and siblings; its subtree and tail stay with it.
    pub(crate) fn detach(&mut self, id: NodeId) {
        let (parent, prev, next) = {
            let d = self.data(id);
            (d.parent, d.prev, d.next)
        };
        match prev {
            Some(p) => self.data_mut(p).next = next,
            None => {
                if let Some(par) = parent {
                    self.data_mut(par).first_child = next;
                }
            }
        }
        match next {
            Some(n) => self.data_mut(n).prev = prev,
            None => {
                if let Some(par) = parent {
                    self.data_mut(par).last_child = prev;
                }
            }
        }
        let d = self.data_mut(id);
        d.parent = None;
        d.prev = None;
        d.next = None;
    }

    /// Appends the detached node `child` as the last child of `parent`.
    pub(crate) fn append_child(&mut self, parent: NodeId, child: NodeId) {
        let last = self.data(parent).last_child;
        {
            let c = self.data_mut(child);
            c.parent = Some(parent);
            c.prev = last;
            c.next = None;
        }
        match last {
            Some(l) => self.data_mut(l).next = Some(child),
            None => self.data_mut(parent).first_child = Some(child),
        }
        self.data_mut(parent).last_child = Some(child);
    }

    /// Inserts the detached node `child` right before the attached node `anchor`.
    pub(crate) fn insert_before(&mut self, anchor: NodeId, child: NodeId) -> ViewResult<()> {
        let parent = self.attached_parent(anchor)?;
        let prev = self.data(anchor).prev;
        {
            let c = self.data_mut(child);
            c.parent = Some(parent);
            c.prev = prev;
            c.next = Some(anchor);
        }
        self.data_mut(anchor).prev = Some(child);
        match prev {
            Some(p) => self.data_mut(p).next = Some(child),
            None => self.data_mut(parent).first_child = Some(child),
        }
        Ok(())
    }

    /// Inserts the detached node `child` right after the attached node `anchor`.
    pub(crate) fn insert_after(&mut self, anchor: NodeId, child: NodeId) -> ViewResult<()> {
        match self.data(anchor).next {
            Some(next) => self.insert_before(next, child),
            None => {
                let parent = self.attached_parent(anchor)?;
                self.append_child(parent, child);
                Ok(())
            }
        }
    }

    /// Makes the detached node `id` the root; the old tree becomes unreachable.
    pub(crate) fn set_root(&mut self, id: NodeId) {
        self.data_mut(id).tail.clear();
        self.root = id;
    }

    // --- copying -------------------------------------------------------------------------------

    fn snapshot(&self, top: NodeId) -> Vec<Snapshot> {
        self.walk(top)
            .map(|(id, depth)| {
                let d = self.data(id);
                Snapshot {
                    depth,
                    src: id,
                    kind: d.kind,
                    origin: d.origin,
                    source_node: d.source_node,
                    tag: d.tag.clone(),
                    attrs: d.attrs.clone(),
                    text: d.text.clone(),
                    tail: if id == top {
                        String::new()
                    } else {
                        d.tail.clone()
                    },
                }
            })
            .collect()
    }

    /// Allocates `plan` as a detached subtree whose root gets `top_tail`; returns its root and
    /// the copies of the `track`ed source nodes, in `track` order. Every tracked node must be in
    /// the plan, once.
    ///
    /// `track` is indexed once, so the cost is linear in plan plus `track` (a spec can hold
    /// thousands of `$0` locations).
    fn instantiate(
        &mut self,
        plan: Vec<Snapshot>,
        top_tail: String,
        track: &[NodeId],
    ) -> ViewResult<(NodeId, Vec<NodeId>)> {
        let mut found: Vec<Option<NodeId>> = vec![None; track.len()];
        let mut slot_of: HashMap<NodeId, usize> = HashMap::with_capacity(track.len());
        for (i, &t) in track.iter().enumerate() {
            if slot_of.insert(t, i).is_some() {
                return Err(ViewError::InvariantViolation(
                    "node tracked twice in a copy",
                ));
            }
        }
        let mut ancestors: Vec<NodeId> = Vec::new();
        let mut top = None;
        for snap in plan {
            let id = self.alloc(snap.kind, snap.tag, snap.attrs, snap.text, snap.tail)?;
            self.set_origin(id, snap.origin);
            self.set_source_node(id, snap.source_node);
            ancestors.truncate(snap.depth);
            match ancestors.last() {
                Some(&parent) => self.append_child(parent, id),
                None => top = Some(id),
            }
            ancestors.push(id);
            if let Some(&i) = slot_of.get(&snap.src) {
                found[i] = Some(id);
            }
        }
        let top = top.ok_or(ViewError::InvariantViolation("copied subtree is empty"))?;
        self.set_tail(top, top_tail)?;
        let copies = found
            .into_iter()
            .map(|copy| {
                copy.ok_or(ViewError::InvariantViolation(
                    "tracked node is not in the copied subtree",
                ))
            })
            .collect::<ViewResult<Vec<NodeId>>>()?;
        Ok((top, copies))
    }

    /// Copies the subtree of `src_id` from another arch as a detached subtree whose root gets
    /// `tail`. Returns the copy and the copies of the `track`ed source nodes, which must all be
    /// in that subtree (in `track` order).
    pub(crate) fn import(
        &mut self,
        src: &Arch,
        src_id: NodeId,
        tail: String,
        track: &[NodeId],
    ) -> ViewResult<(NodeId, Vec<NodeId>)> {
        self.instantiate(src.snapshot(src_id), tail, track)
    }

    /// Copies a subtree of this arch as a detached subtree, its tail included when `with_tail`
    /// (lxml's `deepcopy` keeps the tail).
    pub(crate) fn copy_subtree(&mut self, id: NodeId, with_tail: bool) -> ViewResult<NodeId> {
        let tail = if with_tail {
            self.tail(id).to_owned()
        } else {
            String::new()
        };
        let plan = self.snapshot(id);
        Ok(self.instantiate(plan, tail, &[])?.0)
    }

    // --- serialization -------------------------------------------------------------------------

    /// Serializes the subtree of `top` (its tail too when `with_tail`), escaping like lxml.
    pub(crate) fn write_xml(&self, top: NodeId, with_tail: bool, out: &mut String) {
        let mut cur = top;
        'node: loop {
            let d = self.data(cur);
            if d.kind == NodeKind::Element {
                open_tag(d, out);
                if d.first_child.is_none() && d.text.is_empty() {
                    out.push_str("/>");
                } else {
                    out.push('>');
                    escape_text(&d.text, out);
                    if let Some(child) = d.first_child {
                        cur = child;
                        continue 'node;
                    }
                    close_tag(&d.tag, out);
                }
            }
            // `cur` is complete: write its tail, then continue with its next sibling or close
            // its parent.
            loop {
                if cur == top {
                    if with_tail {
                        escape_text(self.tail(cur), out);
                    }
                    return;
                }
                escape_text(self.tail(cur), out);
                if let Some(next) = self.next_sibling(cur) {
                    cur = next;
                    continue 'node;
                }
                match self.parent(cur) {
                    Some(parent) => {
                        cur = parent;
                        if self.data(cur).kind == NodeKind::Element {
                            close_tag(self.tag(cur), out);
                        }
                    }
                    None => return,
                }
            }
        }
    }

    /// Serializes the subtree of `top` applying `filter` to each element.
    #[cfg_attr(not(any(test, feature = "host-access")), allow(dead_code))]
    pub(crate) fn write_xml_filtered<F>(
        &self,
        top: NodeIdx,
        with_tail: bool,
        filter: &F,
        out: &mut String,
    ) where
        F: Fn(NodeIdx) -> FilterDecision,
    {
        let top_id = NodeId(top.0);
        let mut cur = top_id;
        'node: loop {
            let d = self.data(cur);
            if d.kind == NodeKind::Element {
                match filter(NodeIdx(cur.0)) {
                    FilterDecision::Deny => loop {
                        if cur == top_id {
                            if with_tail {
                                escape_text(self.tail(cur), out);
                            }
                            return;
                        }
                        escape_text(self.tail(cur), out);
                        if let Some(next) = self.next_sibling(cur) {
                            cur = next;
                            continue 'node;
                        }
                        match self.parent(cur) {
                            Some(parent) => {
                                cur = parent;
                                if self.data(cur).kind == NodeKind::Element
                                    && filter(NodeIdx(cur.0)) != FilterDecision::Unwrap
                                {
                                    close_tag(self.tag(cur), out);
                                }
                            }
                            None => return,
                        }
                    },
                    FilterDecision::Unwrap => {
                        escape_text(&d.text, out);
                        if let Some(child) = d.first_child {
                            cur = child;
                            continue 'node;
                        }
                    }
                    FilterDecision::Allow => {
                        open_tag(d, out);
                        if d.first_child.is_none() && d.text.is_empty() {
                            out.push_str("/>");
                        } else {
                            out.push('>');
                            escape_text(&d.text, out);
                            if let Some(child) = d.first_child {
                                cur = child;
                                continue 'node;
                            }
                            close_tag(&d.tag, out);
                        }
                    }
                }
            }
            loop {
                if cur == top_id {
                    if with_tail {
                        escape_text(self.tail(cur), out);
                    }
                    return;
                }
                escape_text(self.tail(cur), out);
                if let Some(next) = self.next_sibling(cur) {
                    cur = next;
                    continue 'node;
                }
                match self.parent(cur) {
                    Some(parent) => {
                        cur = parent;
                        if self.data(cur).kind == NodeKind::Element
                            && filter(NodeIdx(cur.0)) != FilterDecision::Unwrap
                        {
                            close_tag(self.tag(cur), out);
                        }
                    }
                    None => return,
                }
            }
        }
    }

    pub(crate) fn node_xml(&self, id: NodeId) -> String {
        let mut out = String::new();
        self.write_xml(id, false, &mut out);
        out
    }

    // --- public API ----------------------------------------------------------------------------

    #[cfg(feature = "host-access")]
    pub(crate) fn node_ref(&self, index: NodeIdx) -> Option<NodeRef<'_>> {
        (index.index() < self.nodes.len()).then_some(NodeRef {
            arch: self,
            id: NodeId::from_idx(index),
        })
    }

    /// The root element.
    pub fn root(&self) -> NodeRef<'_> {
        NodeRef {
            arch: self,
            id: self.root,
        }
    }

    /// The index of the root node.
    #[must_use]
    pub fn root_node_idx(&self) -> NodeIdx {
        NodeIdx(self.root.0)
    }

    /// Number of nodes in the arch arena.
    #[must_use]
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// Tag name of `node`.
    #[must_use]
    pub fn tag_of(&self, node: NodeIdx) -> &str {
        self.tag(NodeId(node.0))
    }

    /// Attributes of `node` in document order.
    #[must_use]
    pub fn attrs_of(&self, node: NodeIdx) -> &[(String, String)] {
        self.attrs(NodeId(node.0))
    }

    /// Serializes the arch applying `filter` to each element.
    #[must_use]
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn to_xml_filtered<F>(&self, filter: F) -> String
    where
        F: Fn(NodeIdx) -> FilterDecision,
    {
        let mut out = String::with_capacity(self.to_xml().len());
        self.write_xml_filtered(NodeIdx(self.root.0), false, &filter, &mut out);
        out
    }

    /// Serializes the arch (lxml-compatible escaping; empty elements as `<a/>`).
    pub fn to_xml(&self) -> String {
        self.node_xml(self.root)
    }
}

impl PartialEq for Arch {
    fn eq(&self, other: &Self) -> bool {
        self.to_xml() == other.to_xml()
    }
}

impl Eq for Arch {}

/// `<tag a="v" ...` without the closing `>` or `/>`.
fn open_tag(d: &NodeData, out: &mut String) {
    out.push('<');
    out.push_str(&d.tag);
    for (k, v) in &d.attrs {
        out.push(' ');
        out.push_str(k);
        out.push_str("=\"");
        escape_attribute(v, out);
        out.push('"');
    }
}

fn close_tag(tag: &str, out: &mut String) {
    out.push_str("</");
    out.push_str(tag);
    out.push('>');
}

fn escape_text(s: &str, out: &mut String) {
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '\r' => out.push_str("&#13;"),
            c => out.push(c),
        }
    }
}

fn escape_attribute(s: &str, out: &mut String) {
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\n' => out.push_str("&#10;"),
            '\t' => out.push_str("&#9;"),
            '\r' => out.push_str("&#13;"),
            c => out.push(c),
        }
    }
}

/// Pre-order iterator over a subtree, yielding `(node, depth below the subtree root)`.
pub(crate) struct Walk<'a> {
    arch: &'a Arch,
    top: NodeId,
    next: Option<(NodeId, usize)>,
}

impl Iterator for Walk<'_> {
    type Item = (NodeId, usize);

    fn next(&mut self) -> Option<Self::Item> {
        let (cur, depth) = self.next?;
        self.next = match self.arch.first_child(cur) {
            Some(child) => Some((child, depth + 1)),
            None => {
                let mut node = cur;
                let mut d = depth;
                loop {
                    if node == self.top {
                        break None;
                    }
                    if let Some(sibling) = self.arch.next_sibling(node) {
                        break Some((sibling, d));
                    }
                    match self.arch.parent(node) {
                        Some(parent) => {
                            node = parent;
                            d = d.saturating_sub(1);
                        }
                        None => break None,
                    }
                }
            }
        };
        Some((cur, depth))
    }
}

/// A read-only handle on one element of an [`Arch`].
#[derive(Debug, Clone, Copy)]
pub struct NodeRef<'a> {
    arch: &'a Arch,
    id: NodeId,
}

impl<'a> NodeRef<'a> {
    /// The node's index in its arch.
    #[must_use]
    pub fn id(&self) -> NodeIdx {
        NodeIdx(self.id.0)
    }

    /// Tag name.
    pub fn tag(&self) -> &'a str {
        self.arch.tag(self.id)
    }

    /// The view and line that wrote this element.
    pub fn origin(&self) -> NodeOrigin {
        self.arch.origin(self.id)
    }

    /// The 0-based element ordinal in the original parsed view identified by
    /// [`Self::origin`]. Imports, moves and `$0` copies retain it, so same-line peers stay
    /// distinguishable after combination. Engine-generated markers have no source ordinal.
    /// This identifies the original element; attribute edits do not claim its new values
    /// came from that source. Consumers compare those values before attributing a diagnostic.
    ///
    /// ```
    /// use erp_views::{ViewCatalogue, ViewDescriptor};
    /// let catalogue = ViewCatalogue::new(vec![
    ///     ViewDescriptor::new(1, "pair", "ui.template", "<div><span/><span/></div>"),
    /// ])?;
    /// let combined = catalogue.combine(1)?;
    /// let sources: Vec<_> = combined.arch().root().children()
    ///     .map(|node| node.source_node_index()).collect();
    /// assert_eq!(sources, [Some(1), Some(2)]);
    /// # Ok::<(), erp_views::ViewError>(())
    /// ```
    pub fn source_node_index(&self) -> Option<u32> {
        self.arch.source_node(self.id)
    }

    /// Value of one attribute.
    pub fn attr(&self, name: &str) -> Option<&'a str> {
        self.arch.attr(self.id, name)
    }

    /// Attributes in document order.
    pub fn attributes(&self) -> impl Iterator<Item = (&'a str, &'a str)> + 'a + use<'a> {
        self.arch
            .attrs(self.id)
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
    }

    /// Text before the first child (empty when there is none).
    pub fn text(&self) -> &'a str {
        self.arch.text(self.id)
    }

    /// Text after the end tag (empty when there is none).
    pub fn tail(&self) -> &'a str {
        self.arch.tail(self.id)
    }

    /// Child elements in document order.
    pub fn children(&self) -> impl Iterator<Item = NodeRef<'a>> + 'a + use<'a> {
        let arch = self.arch;
        arch.element_children(self.id)
            .map(move |id| NodeRef { arch, id })
    }

    /// Parent element (`None` for the root).
    pub fn parent(&self) -> Option<NodeRef<'a>> {
        self.arch.parent(self.id).map(|id| NodeRef {
            arch: self.arch,
            id,
        })
    }

    /// Serializes this element and its content (without its tail).
    pub fn to_xml(&self) -> String {
        self.arch.node_xml(self.id)
    }
}
