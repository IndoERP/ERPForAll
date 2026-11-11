//! Authority-bound, read-only input to pure H18 callbacks.
//!
//! Only the native checked host bridge constructs this value. Its lifetime is bounded by the
//! evaluation that admitted the arch, and every node handle borrows this input. There is no raw
//! arch, unrestricted node or decision-table accessor.
//!
//! ```compile_fail
//! let _ = erp_views::CheckedViewInput { };
//! ```
//! ```compile_fail
//! let _ = erp_views::CheckedViewInput::for_tests();
//! ```

use std::borrow::Cow;
use std::fmt;
use std::marker::PhantomData;

use erp_registry::{FieldSpec, ModelSpec};

use crate::arch::{Arch, FilterDecision, NodeId};
use crate::{NodeIdx, NodeOrigin};

/// A checked arch and descriptive registry metadata for one H18 invocation.
///
/// Registry metadata is descriptive only: a callback cannot use it to grant permission. The host
/// must independently authorize every schema-derived output reference before publication.
pub struct CheckedViewInput<'a> {
    arch: &'a Arch,
    decisions: Cow<'a, [FilterDecision]>,
    mobile: bool,
    model: &'a ModelSpec,
    // The constructor binds 'a to the actual GuardEvaluation borrow, even though the pure
    // callback interface deliberately cannot name the ORM type.
    _authority: PhantomData<&'a ()>,
}

impl fmt::Debug for CheckedViewInput<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Debug must not serialize the cached arch, which still contains denied nodes.
        f.debug_struct("CheckedViewInput")
            .field("root_model", &self.root_model())
            .field("allowed_nodes", &self.allowed_node_count())
            .finish_non_exhaustive()
    }
}

impl<'a> CheckedViewInput<'a> {
    #[cfg(any(test, feature = "host-access"))]
    pub(crate) fn from_parts(
        arch: &'a Arch,
        decisions: Box<[FilterDecision]>,
        model: &'a ModelSpec,
    ) -> Self {
        Self {
            arch,
            decisions: Cow::Owned(decisions.into_vec()),
            mobile: false,
            model,
            _authority: PhantomData,
        }
    }

    fn decision(&self, id: NodeId) -> FilterDecision {
        // Handles are minted from the same immutable arch. An impossible absent slot is denied
        // (the same fail-closed rule as FilteredArch), never treated as an allowed node.
        self.decisions
            .get(id.index())
            .copied()
            .unwrap_or(FilterDecision::Deny)
    }

    /// Whether the authenticated request selected mobile presentation. Defaults to `false`.
    /// This descriptive flag grants no access and does not change any checked node decision.
    pub fn mobile(&self) -> bool {
        self.mobile
    }

    /// Borrow this exact checked input with a request-selected mobile presentation flag.
    ///
    /// The projection shares its arch, registry metadata and evaluated decision slice. It borrows
    /// this input, so it cannot outlive this input or the guard evaluation that admitted it.
    /// The flag grants no authority; all output references still require host authorization.
    pub fn with_mobile(&self, mobile: bool) -> CheckedViewInput<'_> {
        CheckedViewInput {
            arch: self.arch,
            decisions: Cow::Borrowed(self.decisions.as_ref()),
            mobile,
            model: self.model,
            _authority: PhantomData,
        }
    }

    /// The model checked by the guard evaluation.
    pub fn root_model(&self) -> &str {
        self.model.name
    }

    /// Descriptive metadata from the exact environment registry checked by the host constructor.
    /// A field's presence here is not evidence that the principal may read it.
    pub fn model_spec(&self) -> &ModelSpec {
        self.model
    }

    /// Descriptive root-model field metadata; this does not grant field access.
    pub fn field_spec(&self, name: &str) -> Option<&FieldSpec> {
        self.model.field(name)
    }

    /// The allowed root, or `None` when its guard denies the entire arch.
    pub fn root(&self) -> Option<CheckedNode<'_>> {
        let id = self.arch.root_id();
        (self.decision(id) != FilterDecision::Deny).then_some(CheckedNode { input: self, id })
    }

    /// Number of reachable allowed elements, including unwrapped elements.
    pub fn allowed_node_count(&self) -> usize {
        let Some(root) = self.root() else { return 0 };
        root.walk().count()
    }

    /// Appends the exact filtered XML, preserving the engine's text and tail semantics.
    /// The XML adapter uses this; typed callbacks traverse nodes directly instead of reparsing it.
    pub fn write_xml(&self, out: &mut String) {
        self.arch.write_xml_filtered(
            self.arch.root_node_idx(),
            false,
            &|id| {
                self.decisions
                    .get(id.index())
                    .copied()
                    .unwrap_or(FilterDecision::Deny)
            },
            out,
        );
    }

    /// The exact filtered XML, for the legacy XML adapter.
    pub fn to_xml(&self) -> String {
        let mut output = String::new();
        self.write_xml(&mut output);
        output
    }
}

/// A reachable allowed node. It cannot outlive the checked input or navigate to a denied node.
#[derive(Clone, Copy)]
pub struct CheckedNode<'a> {
    input: &'a CheckedViewInput<'a>,
    id: NodeId,
}

impl fmt::Debug for CheckedNode<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CheckedNode")
            .field("tag", &self.tag())
            .field("origin", &self.origin())
            .finish_non_exhaustive()
    }
}

impl<'a> CheckedNode<'a> {
    /// Stable index in this checked input, for constructing a bounded output arena.
    pub fn id(&self) -> NodeIdx {
        self.id.to_idx()
    }
    /// Original element-child count before access filtering, without exposing denied children.
    ///
    /// Descriptive shape only: it grants no access and cannot be used to traverse denied nodes.
    /// Panel producers use it to preserve a sections contract when pruning leaves one category.
    ///
    /// ```
    /// fn original_sections(node: &erp_views::CheckedNode<'_>) -> usize {
    ///     node.source_child_count()
    /// }
    /// ```
    pub fn source_child_count(&self) -> usize {
        self.input.arch.element_children(self.id).count()
    }
    /// One-based element position among the original combined parent's children, before filtering.
    ///
    /// This exposes no denied node or text. Panel section IDs remain stable when earlier siblings
    /// are removed by access, groups or visibility. The root has ordinal one.
    ///
    /// # Errors
    /// An invariant failure if this private, reachable handle is absent from its parent.
    ///
    /// ```
    /// fn panel_id(node: &erp_views::CheckedNode<'_>) -> erp_views::ViewResult<usize> {
    ///     node.source_sibling_ordinal()
    /// }
    /// ```
    pub fn source_sibling_ordinal(&self) -> crate::ViewResult<usize> {
        let Some(parent) = self.input.arch.parent(self.id) else {
            return Ok(1);
        };
        self.input
            .arch
            .element_children(parent)
            .position(|child| child == self.id)
            .map(|index| index + 1)
            .ok_or(crate::ViewError::InvariantViolation(
                "checked node is absent from its parent",
            ))
    }
    /// Element name. Unwrapped elements remain traversable and report [`Self::is_unwrapped`].
    pub fn tag(&self) -> &'a str {
        self.input.arch.tag(self.id)
    }
    /// The view and source line that authored this element.
    pub fn origin(&self) -> NodeOrigin {
        self.input.arch.origin(self.id)
    }
    /// Value of one attribute on this allowed element.
    pub fn attr(&self, name: &str) -> Option<&'a str> {
        self.input.arch.attr(self.id, name)
    }
    /// Attributes in document order. Group attributes have already been captured and stripped.
    pub fn attributes(&self) -> impl Iterator<Item = (&'a str, &'a str)> + 'a + use<'a> {
        self.input
            .arch
            .attrs(self.id)
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str()))
    }
    /// Local text before this element's first child, never text inside a denied child.
    pub fn text(&self) -> &'a str {
        self.input.arch.text(self.id)
    }
    /// Text after this element's closing tag.
    pub fn tail(&self) -> &'a str {
        self.input.arch.tail(self.id)
    }
    /// Whether the XML projection omits this element's own tags but keeps its allowed content.
    pub fn is_unwrapped(&self) -> bool {
        self.input.decision(self.id) == FilterDecision::Unwrap
    }
    /// Allowed direct children in source order. A denied child and its subtree are absent;
    /// allowed unwrapped children are retained so a typed grammar may flatten them deliberately.
    pub fn children(&self) -> impl Iterator<Item = CheckedNode<'a>> + 'a + use<'a> {
        let input = self.input;
        input
            .arch
            .element_children(self.id)
            .filter(move |id| input.decision(*id) != FilterDecision::Deny)
            .map(move |id| CheckedNode { input, id })
    }
    /// Local text, allowed child elements and all direct-child tails in XML order. In particular,
    /// a denied child's tail is preserved without exposing that child's attributes or content.
    /// The `Element` item's tail is returned separately as a subsequent `Text` item.
    pub fn content(&self) -> impl Iterator<Item = CheckedContent<'a>> + 'a + use<'a> {
        let input = self.input;
        std::iter::once(CheckedContent::Text(self.text()))
            .chain(input.arch.element_children(self.id).flat_map(move |id| {
                let element = (input.decision(id) != FilterDecision::Deny)
                    .then_some(CheckedContent::Element(CheckedNode { input, id }));
                element
                    .into_iter()
                    .chain(std::iter::once(CheckedContent::Text(input.arch.tail(id))))
            }))
            .filter(|part| !matches!(part, CheckedContent::Text("")))
    }
    /// Pre-order traversal of the allowed subtree, without allocating a traversal stack.
    pub fn walk(&self) -> impl Iterator<Item = CheckedNode<'a>> + 'a + use<'a> {
        let input = self.input;
        let top = self.id;
        let mut next = Some(top);
        std::iter::from_fn(move || {
            let id = next?;
            next = if let Some(child) = input
                .arch
                .element_children(id)
                .find(|child| input.decision(*child) != FilterDecision::Deny)
            {
                Some(child)
            } else {
                let mut current = id;
                loop {
                    if current == top {
                        break None;
                    }
                    let mut sibling = input.arch.next_sibling(current);
                    while let Some(candidate) = sibling {
                        if !input.arch.is_sentinel(candidate)
                            && input.decision(candidate) != FilterDecision::Deny
                        {
                            break;
                        }
                        sibling = input.arch.next_sibling(candidate);
                    }
                    if sibling.is_some() {
                        break sibling;
                    }
                    match input.arch.parent(current) {
                        Some(parent) => current = parent,
                        None => break None,
                    }
                }
            };
            Some(CheckedNode { input, id })
        })
    }
}

/// One item of an allowed node's mixed content, with no access to denied child content.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub enum CheckedContent<'a> {
    /// Local text or a direct child's tail.
    Text(&'a str),
    /// An allowed child; an unwrapped child reports [`CheckedNode::is_unwrapped`].
    Element(CheckedNode<'a>),
}

#[cfg(test)]
impl CheckedViewInput<'_> {
    pub(crate) fn for_tests() -> Self {
        static MODEL: erp_registry::ModelDecl = erp_registry::ModelDecl::new("base", "h18.test");
        static REGISTRY: std::sync::OnceLock<erp_registry::Registry> = std::sync::OnceLock::new();
        static ARCH: std::sync::OnceLock<Arch> = std::sync::OnceLock::new();
        let registry = REGISTRY.get_or_init(|| {
            erp_registry::RegistryBuilder::new([])
                .with_decls([&MODEL])
                .build()
                .unwrap()
        });
        let arch = ARCH.get_or_init(|| crate::tests::arch("<form/>"));
        Self::from_parts(
            arch,
            vec![FilterDecision::Allow; arch.node_count()].into_boxed_slice(),
            registry.model("h18.test").unwrap(),
        )
    }
}

#[cfg(test)]
mod mobile_tests {
    use super::CheckedViewInput;

    #[test]
    fn checked_input_mobile_is_presentation_and_borrows_identical_authority() {
        let input = CheckedViewInput::for_tests();
        assert!(!input.mobile());
        let mobile = input.with_mobile(true);
        assert!(mobile.mobile());
        assert_eq!(mobile.to_xml(), input.to_xml());
        assert_eq!(mobile.allowed_node_count(), input.allowed_node_count());
        assert!(std::ptr::eq(mobile.model_spec(), input.model_spec()));
        assert_eq!(mobile.decisions.as_ptr(), input.decisions.as_ptr());
        let desktop = mobile.with_mobile(false);
        assert!(!desktop.mobile());
        assert_eq!(desktop.decisions.as_ptr(), input.decisions.as_ptr());
    }
}
