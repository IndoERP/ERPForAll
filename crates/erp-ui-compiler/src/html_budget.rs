//! Bounded HTML parsing: the shared admission budget handed to the vendored
//! html5ever (`vendor/html5ever-bounded`, feature `budget`) and the driver loop that stops at the
//! first refusal.
//!
//! The parser asks [`ParseBudget`] before it retains more data. The first refusal is sticky and the
//! driver returns it as a typed [`ParseError`]; the partial tree is dropped, never returned.

use std::borrow::Cow;
use std::cell::Cell;
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard};

use html5ever::budget::{Budget, BudgetExceeded, BudgetKind, Limit, LimitError};
use html5ever::driver::{self, ParseOpts};
use html5ever::tendril::{StrTendril, TendrilSink};
use html5ever::tokenizer::TokenizerOpts;
use html5ever::tree_builder::{
    ElemName, ElementFlags, NodeOrText, QuirksMode, TreeBuilderOpts, TreeSink,
};
use html5ever::{Attribute, LocalName, Namespace, QualName};

/// Validated limits for one parse. A zero or overflowing value is an error, never "unlimited".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParseLimits {
    bytes: Limit,
    work: Limit,
    depth: Limit,
    nodes: Limit,
    parse_errors: Limit,
}

/// Parse errors the sink keeps unless [`ParseLimits::with_max_parse_errors`] says otherwise. The
/// rest are counted in [`ParseReport::parse_errors_dropped`], never silently ignored.
pub const DEFAULT_MAX_PARSE_ERRORS: u64 = 1000;

impl ParseLimits {
    /// Validate the four limits.
    ///
    /// # Errors
    /// [`LimitError`] when any value is zero or above [`Limit::MAX`].
    pub fn new(bytes: u64, work: u64, depth: u64, nodes: u64) -> Result<ParseLimits, LimitError> {
        Ok(ParseLimits {
            bytes: Limit::new(bytes)?,
            work: Limit::new(work)?,
            depth: Limit::new(depth)?,
            nodes: Limit::new(nodes)?,
            parse_errors: Limit::new(DEFAULT_MAX_PARSE_ERRORS)?,
        })
    }

    /// Replace the cap on parse errors the sink keeps.
    ///
    /// # Errors
    /// [`LimitError`] when `max` is zero or above [`Limit::MAX`].
    pub fn with_max_parse_errors(mut self, max: u64) -> Result<ParseLimits, LimitError> {
        self.parse_errors = Limit::new(max)?;
        Ok(self)
    }
}

#[derive(Default)]
struct Counters {
    live_bytes: u64,
    peak_bytes: u64,
    work: u64,
    nodes: u64,
    tripped: Option<BudgetExceeded>,
}

/// The production [`Budget`]: exact byte, work, depth and node counters with a sticky first
/// refusal, shared by the tokenizer and the tree builder.
pub struct ParseBudget {
    limits: ParseLimits,
    state: Mutex<Counters>,
}

impl ParseBudget {
    /// A fresh budget with nothing charged.
    #[must_use]
    pub fn new(limits: ParseLimits) -> Arc<ParseBudget> {
        Arc::new(ParseBudget {
            limits,
            state: Mutex::new(Counters::default()),
        })
    }

    /// Live charged bytes right now (what the parser still retains).
    #[must_use]
    pub fn live_bytes(&self) -> u64 {
        self.lock().live_bytes
    }

    /// Nodes admitted so far (tree builder and sink together).
    #[must_use]
    pub fn nodes(&self) -> u64 {
        self.lock().nodes
    }

    /// Highest number of live charged bytes seen so far (input to the envelope measurement).
    #[must_use]
    pub fn peak_bytes(&self) -> u64 {
        self.lock().peak_bytes
    }

    // A poisoned lock only means another thread panicked while holding plain counters; the data
    // is still consistent, so keep going rather than panic in the parser.
    fn lock(&self) -> MutexGuard<'_, Counters> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn refuse(
        st: &mut Counters,
        kind: BudgetKind,
        requested: u64,
        limit: Limit,
    ) -> Result<(), BudgetExceeded> {
        let err = *st
            .tripped
            .get_or_insert(BudgetExceeded::new(kind, requested, limit.get()));
        Err(err)
    }
}

impl Budget for ParseBudget {
    fn admit_bytes(&self, n: usize) -> Result<(), BudgetExceeded> {
        let mut st = self.lock();
        if let Some(e) = st.tripped {
            return Err(e);
        }
        let want = st.live_bytes.saturating_add(n as u64);
        if want > self.limits.bytes.get() {
            return Self::refuse(&mut st, BudgetKind::WorkingBytes, want, self.limits.bytes);
        }
        st.live_bytes = want;
        st.peak_bytes = st.peak_bytes.max(want);
        Ok(())
    }

    // Releasing more than is live means a charge/release pair is out of balance. That is a
    // parser accounting bug, so it is never hidden: debug builds panic (tests catch it), and
    // release builds record `BudgetKind::Accounting` as the sticky refusal so the parse fails
    // with a typed error. Once the budget has tripped for another reason the parse is already
    // lost and a refused charge may legitimately be released, so that case only clamps at zero.
    fn release_bytes(&self, n: usize) {
        let mut st = self.lock();
        let n = n as u64;
        if n > st.live_bytes {
            if st.tripped.is_none() {
                let live = st.live_bytes;
                st.tripped = Some(BudgetExceeded::new(BudgetKind::Accounting, n, live));
                debug_assert!(
                    false,
                    "released more bytes than were charged: released {n}, live {live}"
                );
            }
            st.live_bytes = 0;
            return;
        }
        st.live_bytes -= n;
    }

    fn admit_work(&self, n: u64) -> Result<(), BudgetExceeded> {
        let mut st = self.lock();
        if let Some(e) = st.tripped {
            return Err(e);
        }
        let want = st.work.saturating_add(n);
        if want > self.limits.work.get() {
            return Self::refuse(&mut st, BudgetKind::Work, want, self.limits.work);
        }
        st.work = want;
        Ok(())
    }

    fn admit_depth(&self, depth: usize) -> Result<(), BudgetExceeded> {
        let mut st = self.lock();
        if let Some(e) = st.tripped {
            return Err(e);
        }
        if depth as u64 > self.limits.depth.get() {
            return Self::refuse(&mut st, BudgetKind::Depth, depth as u64, self.limits.depth);
        }
        Ok(())
    }

    fn admit_node(&self) -> Result<(), BudgetExceeded> {
        let mut st = self.lock();
        if let Some(e) = st.tripped {
            return Err(e);
        }
        let want = st.nodes.saturating_add(1);
        if want > self.limits.nodes.get() {
            return Self::refuse(&mut st, BudgetKind::Nodes, want, self.limits.nodes);
        }
        st.nodes = want;
        Ok(())
    }

    fn tripped(&self) -> Option<BudgetExceeded> {
        self.lock().tripped
    }
}

/// Why a bounded parse produced no tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ParseError {
    /// The input is larger than the hard input cap (checked before any byte is queued).
    InputTooLarge {
        /// Bytes offered.
        len: u64,
        /// The cap.
        cap: u64,
    },
    /// The admission budget refused; the partial tree was dropped.
    ///
    /// A refusal that is not an input limit (an accounting fault) is [`ParseError::Internal`] and
    /// cannot be wrapped here:
    ///
    /// ```compile_fail
    /// use erp_ui_compiler::html_budget::ParseError;
    /// use html5ever::budget::{BudgetExceeded, BudgetKind};
    /// let _ = ParseError::Budget(BudgetExceeded::new(BudgetKind::Accounting, 1, 1));
    /// ```
    Budget(BudgetRefusal),
    /// The parser's own accounting is inconsistent. This is a bug in the parser or its budget,
    /// never a limit the input exceeded; report it, do not show it as "document too large".
    Internal(InternalFault),
}

/// A refusal by the admission budget because the input exceeded a limit. Only the conversion from
/// [`BudgetExceeded`] builds one, and that conversion sends accounting faults and unknown kinds to
/// [`ParseError::Internal`], so a `BudgetRefusal` always names a limit the input went over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BudgetRefusal(BudgetExceeded);

impl BudgetRefusal {
    /// Which limit was exceeded.
    #[must_use]
    pub fn kind(&self) -> BudgetKind {
        self.0.kind
    }

    /// The amount that was asked for when the limit refused.
    #[must_use]
    pub fn requested(&self) -> u64 {
        self.0.requested
    }

    /// The limit that was exceeded.
    #[must_use]
    pub fn limit(&self) -> u64 {
        self.0.limit
    }
}

impl fmt::Display for BudgetRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// A parser fault that is not caused by the input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum InternalFault {
    /// More bytes were released than were charged.
    Accounting {
        /// Bytes the parser tried to release.
        released: u64,
        /// Bytes that were live at that moment.
        live: u64,
    },
    /// The budget reported a [`BudgetKind`] this crate does not know.
    UnknownBudgetKind,
}

impl From<BudgetExceeded> for ParseError {
    fn from(e: BudgetExceeded) -> ParseError {
        match e.kind {
            BudgetKind::Accounting => ParseError::Internal(InternalFault::Accounting {
                released: e.requested,
                live: e.limit,
            }),
            BudgetKind::WorkingBytes | BudgetKind::Work | BudgetKind::Depth | BudgetKind::Nodes => {
                ParseError::Budget(BudgetRefusal(e))
            }
            // `BudgetKind` is non-exhaustive: a kind added later is an internal fault until it is
            // listed above on purpose, never silently a user-facing limit.
            _ => ParseError::Internal(InternalFault::UnknownBudgetKind),
        }
    }
}

impl ParseError {
    /// True when the input exceeded a limit (the caller may show a "too large or too deep"
    /// message); false for an internal fault.
    #[must_use]
    pub fn is_limit(&self) -> bool {
        matches!(
            self,
            ParseError::InputTooLarge { .. } | ParseError::Budget(_)
        )
    }
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParseError::InputTooLarge { len, cap } => {
                write!(f, "html input of {len} bytes exceeds the {cap} byte cap")
            }
            ParseError::Budget(e) => e.fmt(f),
            ParseError::Internal(InternalFault::UnknownBudgetKind) => {
                f.write_str("internal html parser fault: unknown budget kind; report this as a bug")
            }
            ParseError::Internal(InternalFault::Accounting { released, live }) => write!(
                f,
                "internal html parser fault: released {released} bytes with {live} live; \
                 report this as a bug"
            ),
        }
    }
}

impl std::error::Error for ParseError {}

/// What the sink kept and dropped; returned with the parse output.
///
/// Only the parser builds a report: it cannot be constructed outside this crate, and an ignored
/// report is a lint warning.
///
/// ```compile_fail
/// use erp_ui_compiler::html_budget::ParseReport;
/// let _ = ParseReport { parse_errors_kept: 0, parse_errors_dropped: 0 };
/// ```
///
/// ```compile_fail
/// #![deny(unused_must_use)]
/// use erp_ui_compiler::html_budget::ParseReport;
/// fn ignore(report: ParseReport) {
///     std::convert::identity(report);
/// }
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
#[must_use]
pub struct ParseReport {
    /// Parse errors forwarded to the inner sink.
    pub parse_errors_kept: u64,
    /// Parse errors beyond the cap, counted but not forwarded.
    pub parse_errors_dropped: u64,
}

/// Element name the sink reports for the document handle after a refusal (the tree builder asks
/// for the name of its stand-in handle before it stops; the inner sink must not see that call).
pub struct BoundedName<'a, S: TreeSink + 'a> {
    inner: Option<S::ElemName<'a>>,
    stub: &'a (Namespace, LocalName),
}

impl<'a, S: TreeSink + 'a> fmt::Debug for BoundedName<'a, S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.inner {
            Some(n) => n.fmt(f),
            None => f.write_str("<refused>"),
        }
    }
}

impl<'a, S: TreeSink + 'a> ElemName for BoundedName<'a, S> {
    fn ns(&self) -> &Namespace {
        match &self.inner {
            Some(n) => n.ns(),
            None => &self.stub.0,
        }
    }

    fn local_name(&self) -> &LocalName {
        match &self.inner {
            Some(n) => n.local_name(),
            None => &self.stub.1,
        }
    }
}

/// A [`TreeSink`] wrapper that enforces the parts of the node budget the parser cannot see
/// (text and doctype nodes), bounds the parse-error list, and keeps the inner sink safe after a
/// refusal.
///
/// The fork already admits elements and comments, so those are not counted again. Adjacent text
/// that the inner sink would merge is still counted per append, so the node count is an upper
/// bound: it fails closed.
pub struct BoundedSink<S: TreeSink> {
    inner: S,
    budget: Arc<ParseBudget>,
    doc: S::Handle,
    stub: (Namespace, LocalName),
    max_errors: u64,
    kept: Cell<u64>,
    dropped: Cell<u64>,
}

impl<S: TreeSink> BoundedSink<S> {
    /// Wrap `inner`; the sink must share `budget` with the parser, which only
    /// [`parse_document_bounded`] guarantees, so this is not public.
    fn new(inner: S, budget: Arc<ParseBudget>) -> BoundedSink<S> {
        let doc = inner.get_document();
        let max_errors = budget.limits.parse_errors.get();
        BoundedSink {
            inner,
            budget,
            doc,
            stub: (Namespace::default(), LocalName::default()),
            max_errors,
            kept: Cell::new(0),
            dropped: Cell::new(0),
        }
    }

    fn refused(&self) -> bool {
        self.budget.tripped().is_some()
    }

    /// Admit one text or doctype node; false means refused (the budget keeps the first refusal).
    fn admit_node(&self) -> bool {
        self.budget.admit_node().is_ok()
    }

    fn admit_child(&self, child: &NodeOrText<S::Handle>) -> bool {
        match child {
            NodeOrText::AppendNode(_) => !self.refused(),
            NodeOrText::AppendText(_) => self.admit_node(),
        }
    }
}

impl<S: TreeSink> TreeSink for BoundedSink<S> {
    type Handle = S::Handle;
    type Output = (S::Output, ParseReport);
    type ElemName<'a>
        = BoundedName<'a, S>
    where
        Self: 'a;

    fn finish(self) -> Self::Output {
        let report = ParseReport {
            parse_errors_kept: self.kept.get(),
            parse_errors_dropped: self.dropped.get(),
        };
        (self.inner.finish(), report)
    }

    fn parse_error(&self, msg: Cow<'static, str>) {
        if self.kept.get() < self.max_errors {
            self.kept.set(self.kept.get() + 1);
            self.inner.parse_error(msg);
        } else {
            self.dropped.set(self.dropped.get().saturating_add(1));
        }
    }

    fn get_document(&self) -> Self::Handle {
        self.doc.clone()
    }

    fn elem_name<'a>(&'a self, target: &'a Self::Handle) -> Self::ElemName<'a> {
        let inner = if self.refused() && self.inner.same_node(target, &self.doc) {
            None
        } else {
            Some(self.inner.elem_name(target))
        };
        BoundedName {
            inner,
            stub: &self.stub,
        }
    }

    fn create_element(
        &self,
        name: QualName,
        attrs: Vec<Attribute>,
        flags: ElementFlags,
    ) -> Self::Handle {
        self.inner.create_element(name, attrs, flags)
    }

    fn create_comment(&self, text: html5ever::tendril::StrTendril) -> Self::Handle {
        self.inner.create_comment(text)
    }

    fn create_pi(
        &self,
        target: html5ever::tendril::StrTendril,
        data: html5ever::tendril::StrTendril,
    ) -> Self::Handle {
        self.inner.create_pi(target, data)
    }

    fn append(&self, parent: &Self::Handle, child: NodeOrText<Self::Handle>) {
        if self.admit_child(&child) {
            self.inner.append(parent, child);
        }
    }

    fn append_based_on_parent_node(
        &self,
        element: &Self::Handle,
        prev_element: &Self::Handle,
        child: NodeOrText<Self::Handle>,
    ) {
        if self.admit_child(&child) {
            self.inner
                .append_based_on_parent_node(element, prev_element, child);
        }
    }

    fn append_doctype_to_document(
        &self,
        name: html5ever::tendril::StrTendril,
        public_id: html5ever::tendril::StrTendril,
        system_id: html5ever::tendril::StrTendril,
    ) {
        if self.admit_node() {
            self.inner
                .append_doctype_to_document(name, public_id, system_id);
        }
    }

    fn mark_script_already_started(&self, node: &Self::Handle) {
        if !self.refused() {
            self.inner.mark_script_already_started(node);
        }
    }

    fn pop(&self, node: &Self::Handle) {
        if !self.refused() {
            self.inner.pop(node);
        }
    }

    fn get_template_contents(&self, target: &Self::Handle) -> Self::Handle {
        if self.refused() && self.inner.same_node(target, &self.doc) {
            return self.doc.clone();
        }
        self.inner.get_template_contents(target)
    }

    fn same_node(&self, x: &Self::Handle, y: &Self::Handle) -> bool {
        self.inner.same_node(x, y)
    }

    fn set_quirks_mode(&self, mode: QuirksMode) {
        self.inner.set_quirks_mode(mode);
    }

    fn append_before_sibling(&self, sibling: &Self::Handle, new_node: NodeOrText<Self::Handle>) {
        if self.admit_child(&new_node) {
            self.inner.append_before_sibling(sibling, new_node);
        }
    }

    fn add_attrs_if_missing(&self, target: &Self::Handle, attrs: Vec<Attribute>) {
        if !self.refused() {
            self.inner.add_attrs_if_missing(target, attrs);
        }
    }

    fn associate_with_form(
        &self,
        target: &Self::Handle,
        form: &Self::Handle,
        nodes: (&Self::Handle, Option<&Self::Handle>),
    ) {
        if !self.refused() {
            self.inner.associate_with_form(target, form, nodes);
        }
    }

    fn remove_from_parent(&self, target: &Self::Handle) {
        if !self.refused() {
            self.inner.remove_from_parent(target);
        }
    }

    fn reparent_children(&self, node: &Self::Handle, new_parent: &Self::Handle) {
        if !self.refused() {
            self.inner.reparent_children(node, new_parent);
        }
    }

    fn is_mathml_annotation_xml_integration_point(&self, handle: &Self::Handle) -> bool {
        self.inner
            .is_mathml_annotation_xml_integration_point(handle)
    }

    fn set_current_line(&self, line_number: u64) {
        self.inner.set_current_line(line_number);
    }

    // Declarative shadow roots are not built: the inner sink would create an uncounted subtree.
    // The template is then parsed as an ordinary `<template>` element.
    fn allow_declarative_shadow_roots(&self, _intended_parent: &Self::Handle) -> bool {
        false
    }

    fn attach_declarative_shadow(
        &self,
        location: &Self::Handle,
        template: &Self::Handle,
        attrs: &[Attribute],
    ) -> bool {
        !self.refused()
            && self
                .inner
                .attach_declarative_shadow(location, template, attrs)
    }

    // Not forwarded: the inner sink would clone the option's whole subtree into `<selectedcontent>`
    // (RcDom does), nodes this budget cannot count. `<selectedcontent>` stays empty.
    fn maybe_clone_an_option_into_selectedcontent(&self, _option: &Self::Handle) {}
}

/// Parse `chunks` as a document into `sink` under `budget`. The total input size is checked
/// against `input_cap` before the first chunk is queued; the budget is checked after every chunk
/// and after the end of input, and the first refusal stops the parse.
///
/// # Errors
/// [`ParseError::InputTooLarge`], [`ParseError::Budget`] or [`ParseError::Internal`]; the sink's
/// output is dropped.
pub fn parse_document_bounded<S: TreeSink>(
    sink: S,
    chunks: &[&str],
    input_cap: u64,
    budget: &Arc<ParseBudget>,
) -> Result<(S::Output, ParseReport), ParseError> {
    let len: u64 = chunks.iter().map(|c| c.len() as u64).sum();
    if len > input_cap {
        return Err(ParseError::InputTooLarge {
            len,
            cap: input_cap,
        });
    }
    let shared: Arc<dyn Budget> = budget.clone();
    let opts = ParseOpts {
        tokenizer: TokenizerOpts {
            budget: Some(shared.clone()),
            ..TokenizerOpts::default()
        },
        tree_builder: TreeBuilderOpts {
            budget: Some(shared),
            ..TreeBuilderOpts::default()
        },
    };
    let mut parser = driver::parse_document(BoundedSink::new(sink, budget.clone()), opts);
    for c in chunks {
        parser.process(StrTendril::from_slice(c));
        if let Some(e) = budget.tripped() {
            return Err(ParseError::from(e));
        }
    }
    let out = parser.finish();
    match budget.tripped() {
        Some(e) => Err(ParseError::from(e)),
        None => Ok(out),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use markup5ever_rcdom::RcDom;

    fn budget(bytes: u64, depth: u64) -> Arc<ParseBudget> {
        let limits = ParseLimits::new(bytes, 1_000_000, depth, 1_000_000).unwrap();
        ParseBudget::new(limits)
    }

    #[test]
    fn zero_limit_is_a_typed_error() {
        let err = ParseLimits::new(0, 1, 1, 1).unwrap_err();
        assert_eq!(err, LimitError::Zero);
        assert_eq!(
            err.to_string(),
            "parser budget limit must be greater than zero"
        );
    }

    #[test]
    fn small_document_parses_and_charges() {
        let b = budget(1 << 20, 256);
        let dom = parse_document_bounded(RcDom::default(), &["<p>hi</p>"], 1024, &b);
        assert!(dom.is_ok());
        assert_eq!(b.tripped(), None);
    }

    #[test]
    fn input_over_cap_is_refused_before_parsing() {
        let b = budget(1 << 20, 256);
        let err = parse_document_bounded(RcDom::default(), &["<p>hi</p>"], 4, &b).err();
        assert_eq!(err, Some(ParseError::InputTooLarge { len: 9, cap: 4 }));
        assert_eq!(
            err.map(|e| e.to_string()).as_deref(),
            Some("html input of 9 bytes exceeds the 4 byte cap")
        );
        assert_eq!(b.peak_bytes(), 0, "nothing was charged");
    }

    #[test]
    fn deep_nesting_is_refused_with_depth_kind() {
        let b = budget(1 << 24, 64);
        let html = "<div>".repeat(200);
        let err = parse_document_bounded(RcDom::default(), &[&html], 1 << 20, &b).err();
        match err {
            Some(ParseError::Budget(e)) => {
                assert_eq!(e.kind(), BudgetKind::Depth);
                assert_eq!(e.limit(), 64);
            }
            other => panic!("expected depth refusal, got {other:?}"),
        }
    }

    #[test]
    fn attribute_flood_is_refused_with_bytes_kind() {
        let b = budget(4096, 256);
        let html = format!(
            "<p {}>",
            (0..5000)
                .map(|i| format!("a{i}=1"))
                .collect::<Vec<_>>()
                .join(" ")
        );
        let err = parse_document_bounded(RcDom::default(), &[&html], 1 << 20, &b).err();
        match err {
            Some(ParseError::Budget(e)) => assert_eq!(e.kind(), BudgetKind::WorkingBytes),
            other => panic!("expected bytes refusal, got {other:?}"),
        }
    }

    /// Review M2: tiny but valid limits give a typed refusal, never a panic.
    #[test]
    fn tiny_limits_refuse_without_panic() {
        for html in ["x", "<b><i>x</i></b>", "<table> x </table>"] {
            for (limits, kind) in [
                (
                    ParseLimits::new(1 << 20, 1 << 20, 256, 1).unwrap(),
                    BudgetKind::Nodes,
                ),
                (
                    ParseLimits::new(1, 1 << 20, 256, 1 << 20).unwrap(),
                    BudgetKind::WorkingBytes,
                ),
            ] {
                let b = ParseBudget::new(limits);
                let err = parse_document_bounded(RcDom::default(), &[html], 1024, &b).err();
                match err {
                    Some(ParseError::Budget(e)) => assert_eq!(e.kind(), kind, "{html}"),
                    other => panic!("expected a typed budget refusal for {html}, got {other:?}"),
                }
            }
        }
    }

    /// Review M3: bytes charged for table text and formatting entries come back, so the live
    /// charge at the end does not depend on how many of them the document had.
    #[test]
    fn live_bytes_do_not_grow_with_released_state() {
        let live = |html: &str| {
            let b = budget(1 << 30, 256);
            let _ = parse_document_bounded(RcDom::default(), &[html], 1 << 24, &b).expect("parses");
            b.live_bytes()
        };
        let table = |n: usize| format!("<table>{}</table>", " <!--c-->".repeat(n));
        assert_eq!(live(&table(10)), live(&table(2000)));
        let closed = |n: usize| "<b class=n id=m>x</b>".repeat(n);
        assert_eq!(live(&closed(10)), live(&closed(2000)));
    }

    /// Review M3: releasing more than was charged is an accounting fault. Debug builds assert;
    /// release builds record it so `tripped()` reports a typed error and the parse fails.
    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "released more bytes than were charged")]
    fn over_release_asserts_in_debug_builds() {
        let b = budget(1 << 20, 256);
        b.release_bytes(1);
    }

    #[cfg(not(debug_assertions))]
    #[test]
    fn over_release_is_a_typed_failure_in_release_builds() {
        let b = budget(1 << 20, 256);
        b.admit_bytes(10).unwrap();
        b.release_bytes(11);
        let e = b.tripped().expect("over-release is visible");
        assert_eq!(e.kind, BudgetKind::Accounting);
        assert_eq!((e.requested, e.limit), (11, 10));
        assert!(
            b.admit_bytes(1).is_err(),
            "the sticky fault refuses further charges"
        );
    }

    fn nodes_budget(nodes: u64) -> Arc<ParseBudget> {
        ParseBudget::new(ParseLimits::new(1 << 30, 1 << 30, 256, nodes).unwrap())
    }

    /// Nodes the whole document needs: run once with a huge budget and read the counter back.
    fn nodes_needed(html: &str) -> u64 {
        let b = nodes_budget(1 << 30);
        let _ = parse_document_bounded(RcDom::default(), &[html], 1 << 20, &b).expect("parses");
        b.nodes()
    }

    fn refusal(html: &str, nodes: u64) -> Option<ParseError> {
        let b = nodes_budget(nodes);
        parse_document_bounded(RcDom::default(), &[html], 1 << 20, &b).err()
    }

    /// The WASM probe's node limit (`html_budget_probe`) is this count; a document that adds only
    /// text is then refused by the sink's text-node admission, as one more node.
    #[test]
    fn empty_document_has_three_element_nodes() {
        let elements = nodes_needed("");
        assert_eq!(elements, 3, "html, head and body");
        match refusal("x", elements).expect("a text node is one more") {
            ParseError::Budget(e) => {
                assert_eq!(e.kind(), BudgetKind::Nodes);
                assert_eq!((e.requested(), e.limit()), (elements + 1, elements));
            }
            other => panic!("expected node refusal, got {other:?}"),
        }
    }

    /// text nodes count against the node budget (the fork only admits elements
    /// and comments). The needed count comes from our own run, never from a literal.
    #[test]
    fn text_nodes_count_against_node_budget() {
        let html = "<p>a</p><p>b</p>";
        let n = nodes_needed(html);
        assert!(refusal(html, n).is_none(), "exactly n nodes is admitted");
        let err = refusal(html, n - 1).expect("one fewer node is refused");
        match err {
            ParseError::Budget(e) => {
                assert_eq!(e.kind(), BudgetKind::Nodes);
                assert_eq!((e.requested(), e.limit()), (n, n - 1));
            }
            other => panic!("expected node refusal, got {other:?}"),
        }
        assert_eq!(
            err.to_string(),
            format!(
                "parser budget exceeded: nodes requested {n}, limit {}",
                n - 1
            )
        );
        // The same document without text needs fewer nodes: text is what was added.
        assert!(nodes_needed("<p></p><p></p>") < n);
    }

    #[test]
    fn doctype_counts_as_a_node() {
        assert!(nodes_needed("<!DOCTYPE html>x") > nodes_needed("x"));
        let n = nodes_needed("<!DOCTYPE html>x");
        assert!(matches!(
            refusal("<!DOCTYPE html>x", n - 1),
            Some(ParseError::Budget(e)) if e.kind() == BudgetKind::Nodes
        ));
    }

    /// Parse errors beyond the cap are counted, not forwarded, and are not a refusal.
    #[test]
    fn parse_errors_are_bounded() {
        let html = "</x>".repeat(10_000);
        let limits = ParseLimits::new(1 << 30, 1 << 30, 256, 1 << 30)
            .unwrap()
            .with_max_parse_errors(64)
            .unwrap();
        let b = ParseBudget::new(limits);
        let (dom, report) =
            parse_document_bounded(RcDom::default(), &[&html], 1 << 20, &b).expect("parses");
        assert_eq!(report.parse_errors_kept, 64);
        assert_eq!(dom.errors.borrow().len() as u64, report.parse_errors_kept);
        // The default-capped run saw the same errors: kept + dropped is the parser's total.
        let (_, all) = parse_document_bounded(
            RcDom::default(),
            &[&html],
            1 << 20,
            &ParseBudget::new(ParseLimits::new(1 << 30, 1 << 30, 256, 1 << 30).unwrap()),
        )
        .expect("parses");
        let total = all.parse_errors_kept + all.parse_errors_dropped;
        assert!(total > 64, "the document really has more than the cap");
        assert_eq!(report.parse_errors_dropped, total - 64);
    }

    #[test]
    fn zero_parse_error_cap_is_a_typed_error() {
        let limits = ParseLimits::new(1, 1, 1, 1).unwrap();
        assert_eq!(
            limits.with_max_parse_errors(0).unwrap_err(),
            LimitError::Zero
        );
    }

    /// After a refusal the parser still calls the sink (document stand-in handle). Refusing at
    /// every node count must give a typed refusal and never a panic from the inner sink.
    #[test]
    fn sink_tolerates_calls_after_refusal() {
        let inputs = [
            "x",
            "<table> x </table>",
            "<template><p>",
            "<form><input>",
            "<b><p>x</b>y",
            "<select><option>a<option>b",
            "<svg><foreignObject><p>x",
            "<!--c--><html><!--d--><body><!--e-->",
        ];
        for html in inputs {
            let n = nodes_needed(html);
            for k in 1..n {
                let r = std::panic::catch_unwind(|| refusal(html, k));
                match r {
                    Ok(Some(ParseError::Budget(e))) => assert_eq!(e.kind(), BudgetKind::Nodes),
                    other => panic!("{html:?} with {k} nodes of {n}: {other:?}"),
                }
            }
        }
    }

    #[test]
    fn accounting_fault_is_internal_not_a_limit() {
        let e = ParseError::from(BudgetExceeded::new(BudgetKind::Accounting, 11, 10));
        assert_eq!(
            e,
            ParseError::Internal(InternalFault::Accounting {
                released: 11,
                live: 10
            })
        );
        assert_eq!(
            e.to_string(),
            "internal html parser fault: released 11 bytes with 10 live; report this as a bug"
        );
        assert!(!e.is_limit());
        let n = ParseError::from(BudgetExceeded::new(BudgetKind::Nodes, 2, 1));
        assert!(n.is_limit());
        assert!(ParseError::InputTooLarge { len: 2, cap: 1 }.is_limit());
    }

    /// After a refusal the document stand-in is answered by the sink itself; the inner sink never
    /// sees the call (RcDom would panic on `elem_name` of the document).
    #[test]
    fn document_stand_in_is_answered_after_refusal() {
        let b = nodes_budget(1);
        assert!(b.admit_node().is_ok());
        assert!(b.admit_node().is_err(), "second node trips the budget");
        let sink = BoundedSink::new(RcDom::default(), b);
        let doc = sink.get_document();
        let name = sink.elem_name(&doc);
        assert_eq!(*name.ns(), Namespace::default());
        assert_eq!(*name.local_name(), LocalName::default());
        assert!(sink.same_node(&sink.get_template_contents(&doc), &doc));
        // Mutating calls are no-ops: nothing was appended to the document.
        sink.append(&doc, NodeOrText::AppendText("x".into()));
        assert!(doc.children.borrow().is_empty());
    }

    #[test]
    fn unrefused_document_name_is_not_stubbed() {
        let sink = BoundedSink::new(RcDom::default(), nodes_budget(10));
        let doc = sink.get_document();
        let html = sink.create_element(
            QualName::new(None, html5ever::ns!(html), html5ever::local_name!("p")),
            Vec::new(),
            ElementFlags::default(),
        );
        assert_eq!(sink.elem_name(&html).local_name().as_ref(), "p");
        let _ = doc;
    }
}
