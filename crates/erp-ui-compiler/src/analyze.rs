//! The directive table (design note §6.3) and scoping (§6.4): one walk over a template's tree that
//! classifies every attribute, checks every expression and builds the code generator's IR.
//!
//! Directives are taken in OWL's precedence (`parseNode`, `owl.js:5309-5313`): custom directives,
//! `t-debug`/`t-log`, `t-foreach`, `t-if`, translation, `t-call`, `t-key`, `t-out`, `t-call-slot`,
//! components, DOM elements, `t-set`, plain `<t>`. Each stage consumes its attributes and builds
//! the rest of the element by calling [`Walker::element`] again, so a stage wraps what follows it
//! exactly as OWL's parser does. Error texts quoted by SPEC-UI cases are OWL's.

use crate::diag::{Diagnostic, DiagnosticKind, Location, Severity};
use crate::expr::{Expr, ExprContext, ExprEnv, ExprParser, LocalId};
use crate::ir::{
    AttrValue, Binding, CallParam, CallTarget, Component, ComponentTarget, Element, InterpPart,
    LocalInfo, LoopField, Node, Pos, PropIr, SlotIr, SlotTarget, TemplateIr, on_flags,
};
use crate::limits::{LimitHit, LimitKind};
use crate::names::{
    AddonName, TemplateName, is_anchor_name, is_attribute_name, is_component_name, is_element_name,
    is_event_name, is_plain_identifier, is_prop_name, is_slot_name,
};
use crate::source::{BuildMode, CompileOptions, Origin, Trust};
use crate::xml::{LineIndex, XAttr, XElement, XNode, is_xml_space};
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;

/// Tasks named by "not supported until" diagnostics.
pub(crate) const UI2_DONE: &str = "a later release";
pub(crate) const UI3_DONE: &str = "a later release";

/// Where the offsets of a tree point.
pub(crate) struct LocCtx<'a> {
    pub(crate) origin: &'a Origin,
    pub(crate) text: &'a str,
    /// `None` for a combined template, whose offsets are not source positions: its diagnostics
    /// carry no position (the caller names the combined parts in the message).
    pub(crate) lines: Option<&'a LineIndex>,
}

impl LocCtx<'_> {
    pub(crate) fn at(&self, offset: u32) -> Location {
        let position = self
            .lines
            .and_then(|lines| lines.position(self.text, offset as usize));
        Location::new(self.origin.clone(), position)
    }
}

/// OWL drops them (`parseTNode`,
/// `owl.js:5315`), so they are warnings; any other plain attribute on a `<t>` is an error
/// (`class`, `style`: BUG-065).
pub(crate) const T_MARKERS: [&str; 4] = ["owl", "name", "id", "data-id"];

/// A static `t-call` site.
#[derive(Debug, Clone)]
pub(crate) struct CallSite {
    pub(crate) callee: TemplateName,
    /// The caller's locals in scope at the site: a snapshot shared by every site with the same
    /// scope (rust review M1).
    pub(crate) in_scope: Rc<BTreeSet<Box<str>>>,
    /// The call's parameters and the body's `t-set`s (and `0` for a body).
    pub(crate) own: BTreeSet<Box<str>>,
    /// `t-call-context`: the callee reads the context object, every name counts as bound.
    pub(crate) binds_all: bool,
    /// Whether this call is inside a conditional guard (t-if, t-elif, t-else).
    pub(crate) guarded: bool,
    /// Start position in source.
    pub(crate) pos: Pos,
}

impl CallSite {
    /// Whether the site binds `name` for the callee.
    pub(crate) fn binds(&self, name: &str) -> bool {
        self.binds_all || self.in_scope.contains(name) || self.own.contains(name)
    }
}

/// The analysis of one template.
#[derive(Debug, Clone)]
pub(crate) struct TemplateAnalysis {
    pub(crate) ir: TemplateIr,
    pub(crate) diagnostics: Vec<Diagnostic>,
    /// Names the template reads that nothing in it binds, with where each is first read.
    pub(crate) free: BTreeMap<Box<str>, Location>,
    pub(crate) calls: Vec<CallSite>,
    /// Whether the template has text the translation pass would translate: static
    /// text or a `.translate` value.
    pub(crate) translatable: bool,
}

/// Analyses one template tree in trust mode `trust` (which the caller takes from the source kind
/// of every entry combined into the template, never from an option). `root` is the template
/// element (`t-name` and inheritance attributes are read by the caller and ignored here).
pub(crate) fn analyze_template(
    parser: &mut ExprParser,
    options: &CompileOptions,
    trust: Trust,
    templates: &BTreeSet<TemplateName>,
    name: &TemplateName,
    root: &XElement<'_>,
    loc: &LocCtx<'_>,
) -> TemplateAnalysis {
    let mut w = Walker {
        parser,
        options,
        trust,
        templates,
        loc,
        template: name,
        frames: vec![Vec::new()],
        scope: BTreeMap::new(),
        snapshot: None,
        locals: Vec::new(),
        max_locals: options.limits().get(LimitKind::Locals),
        locals_over: false,
        diags: Vec::new(),
        free: BTreeMap::new(),
        calls: Vec::new(),
        loops: 0,
        slots: 0,
        slot_targets: Vec::new(),
        translatable: false,
        if_depth: 0,
    };
    // A static anchor names one node in the complete template, regardless of branches. The
    // same check runs after inheritance, so two extensions cannot silently target an arbitrary
    // first match. The input tree already obeys the file-node and depth caps.
    let mut anchors: BTreeMap<&str, &XElement<'_>> = BTreeMap::new();
    let mut pending = vec![root];
    while let Some(node) = pending.pop() {
        if let Some(anchor) = node.attr("t-anchor")
            && is_anchor_name(anchor)
            && let Some(previous) = anchors.insert(anchor, node)
        {
            w.err(
                DiagnosticKind::InvalidDirectiveValue,
                "t-anchor",
                node.offset,
                format!(
                    "duplicate t-anchor '{anchor}': <{}> and <{}> declare the same anchor",
                    previous.name, node.name
                ),
            );
        }
        // Text nodes cannot declare anchor attributes.
        pending.extend(node.children.iter().rev().filter_map(|child| match child {
            XNode::Element(element) => Some(element),
            XNode::Text { .. } => None,
        }));
    }
    let mut at = Attrs::new(root);
    for root_only in ["t-name", "t-inherit", "t-inherit-mode"] {
        at.take(root_only);
    }
    // A branch directive on the root has no t-if before it (silent-failure review M-a): `children`
    // checks every other element's branches, the root never goes through it.
    if Walker::branch_count(root) > 1 {
        w.err(
            DiagnosticKind::MultipleBranchDirectives,
            "t-if",
            root.offset,
            "Only one conditional branching directive is allowed per node",
        );
    } else if root.has("t-elif") || root.has("t-else") {
        w.err(
            DiagnosticKind::BranchWithoutIf,
            "t-if",
            root.offset,
            "t-elif and t-else directives must be preceded by a t-if or t-elif directive",
        );
    }
    let body = w.element(root, at);
    TemplateAnalysis {
        ir: TemplateIr {
            name: name.clone(),
            body,
            locals: w.locals,
            free: w.free.keys().cloned().collect(),
        },
        diagnostics: w.diags,
        free: w.free,
        calls: w.calls,
        translatable: w.translatable,
    }
}

/// The attributes of one element and which of them a stage has consumed.
struct Attrs<'e, 's> {
    el: &'e XElement<'s>,
    used: Vec<bool>,
}

impl<'e, 's> Attrs<'e, 's> {
    fn new(el: &'e XElement<'s>) -> Self {
        Self {
            el,
            used: vec![false; el.attrs.len()],
        }
    }

    /// Takes attribute `name` if present and not consumed.
    fn take(&mut self, name: &str) -> Option<&'e XAttr<'s>> {
        let i = self
            .el
            .attrs
            .iter()
            .enumerate()
            .position(|(i, a)| !self.used[i] && a.name == name)?;
        self.used[i] = true;
        Some(&self.el.attrs[i])
    }

    fn has(&self, name: &str) -> bool {
        self.el
            .attrs
            .iter()
            .enumerate()
            .any(|(i, a)| !self.used[i] && a.name == name)
    }

    /// The first unconsumed attribute whose name satisfies `pred`, consumed.
    fn take_where(&mut self, pred: impl Fn(&str) -> bool) -> Option<&'e XAttr<'s>> {
        let i = self
            .el
            .attrs
            .iter()
            .enumerate()
            .position(|(i, a)| !self.used[i] && pred(a.name))?;
        self.used[i] = true;
        Some(&self.el.attrs[i])
    }

    /// Takes every unconsumed attribute, in source order.
    fn take_rest(&mut self) -> Vec<&'e XAttr<'s>> {
        let mut out = Vec::new();
        for (i, a) in self.el.attrs.iter().enumerate() {
            if !self.used[i] {
                self.used[i] = true;
                out.push(a);
            }
        }
        out
    }
}

/// Collects the `t-set-slot` children of the component being analysed.
#[derive(Default)]
struct SlotCollector {
    slots: Vec<SlotIr>,
}

struct Walker<'a, 'p> {
    parser: &'p mut ExprParser,
    options: &'a CompileOptions,
    trust: Trust,
    templates: &'a BTreeSet<TemplateName>,
    loc: &'a LocCtx<'a>,
    template: &'a TemplateName,
    /// Scopes that `t-set` declares into: the template, each loop body, slot body and call body;
    /// each holds the names it declared, in order.
    frames: Vec<Vec<Box<str>>>,
    /// Every name in scope with its declarations, innermost last: a lookup is one map search
    /// (rust review M1).
    scope: BTreeMap<Box<str>, Vec<LocalId>>,
    /// The names in scope as shared by call sites; dropped whenever the scope changes.
    snapshot: Option<Rc<BTreeSet<Box<str>>>>,
    locals: Vec<LocalInfo>,
    /// The `locals` limit, and whether it was reported.
    max_locals: usize,
    locals_over: bool,
    diags: Vec<Diagnostic>,
    free: BTreeMap<Box<str>, Location>,
    calls: Vec<CallSite>,
    loops: u32,
    slots: u32,
    slot_targets: Vec<SlotCollector>,
    translatable: bool,
    if_depth: usize,
}

const MODS: [(&str, u32); 6] = [
    ("stop", on_flags::STOP),
    ("prevent", on_flags::PREVENT),
    ("self", on_flags::SELF),
    ("capture", on_flags::CAPTURE),
    ("synthetic", on_flags::SYNTHETIC),
    ("passive", on_flags::PASSIVE),
];

/// IDL properties set as properties, not attributes (`isProp`, `owl.js:5968-5983`), with the
/// property name.
fn idl_property(tag: &str, attr: &str) -> Option<&'static str> {
    let readonly = || match attr {
        "readonly" | "readOnly" => Some("readOnly"),
        _ => None,
    };
    match tag {
        "input" => match attr {
            "checked" => Some("checked"),
            "indeterminate" => Some("indeterminate"),
            "value" => Some("value"),
            "disabled" => Some("disabled"),
            _ => readonly(),
        },
        "option" => match attr {
            "selected" => Some("selected"),
            "disabled" => Some("disabled"),
            _ => None,
        },
        "textarea" => match attr {
            "value" => Some("value"),
            "disabled" => Some("disabled"),
            _ => readonly(),
        },
        "select" => match attr {
            "value" => Some("value"),
            "disabled" => Some("disabled"),
            _ => None,
        },
        "button" | "optgroup" => (attr == "disabled").then_some("disabled"),
        _ => None,
    }
}

fn is_component_tag(tag: &str) -> bool {
    tag.as_bytes().first().is_some_and(u8::is_ascii_uppercase)
}

fn is_blank(text: &str) -> bool {
    text.chars().all(is_xml_space)
}

/// Why a format string could not be split.
#[derive(Debug, Clone, PartialEq, Eq)]
enum SplitError {
    /// Longer than the `expression_bytes` limit.
    TooLong(LimitHit),
    /// An opener without its closer.
    Unterminated(String),
}

/// The first `{{` or `#{` in `s`, found in one pass.
fn find_opener(s: &str) -> Option<usize> {
    s.as_bytes()
        .windows(2)
        .position(|w| w[1] == b'{' && matches!(w[0], b'{' | b'#'))
}

/// Splits a `t-attf-*` value (or a dynamic `t-call`/slot name) into literal parts and `{{…}}` /
/// `#{…}` expressions (OWL's `INTERP_REGEXP`, `owl.js:5220`). The length is checked against
/// `max_bytes` first, and every byte is scanned a bounded number of times (rust review H2).
fn split_interpolation(s: &str, max_bytes: usize) -> Result<Vec<(bool, &str)>, SplitError> {
    if s.len() > max_bytes {
        return Err(SplitError::TooLong(LimitHit::new(
            LimitKind::ExpressionBytes,
            max_bytes,
            s.len(),
        )));
    }
    let mut parts = Vec::new();
    let mut rest = s;
    loop {
        let Some(start) = find_opener(rest) else {
            if !rest.is_empty() {
                parts.push((false, rest));
            }
            return Ok(parts);
        };
        if start > 0 {
            parts.push((false, &rest[..start]));
        }
        let (open, close) = if rest[start..].starts_with("{{") {
            (2, "}}")
        } else {
            (2, "}")
        };
        let body_start = start + open;
        let Some(len) = rest[body_start..].find(close) else {
            return Err(SplitError::Unterminated(format!(
                "unterminated interpolation '{}' (expected '{close}')",
                &rest[start..]
            )));
        };
        parts.push((true, &rest[body_start..body_start + len]));
        rest = &rest[body_start + len + close.len()..];
    }
}

impl<'s> Walker<'_, '_> {
    // --- diagnostics and expressions -----------------------------------------------------------

    fn diag(
        &mut self,
        severity: Severity,
        kind: DiagnosticKind,
        directive: &str,
        offset: u32,
        message: impl Into<Box<str>>,
    ) {
        let d = Diagnostic::new(severity, kind, message)
            .with_directive(directive)
            .with_template(Some(self.template))
            .with_location(self.loc.at(offset))
            .with_source_offset(offset);
        self.diags.push(d);
    }

    fn err(
        &mut self,
        kind: DiagnosticKind,
        directive: &str,
        offset: u32,
        message: impl Into<Box<str>>,
    ) {
        self.diag(Severity::Error, kind, directive, offset, message);
    }

    fn limit(&mut self, hit: LimitHit, directive: &str, offset: u32) {
        let d = Diagnostic::limit_hit(hit)
            .with_directive(directive)
            .with_template(Some(self.template))
            .with_location(self.loc.at(offset))
            .with_source_offset(offset);
        self.diags.push(d);
    }

    /// Splits a format string or dynamic name; on failure records the diagnostic.
    fn split<'v>(&mut self, a: &'v XAttr<'_>, directive: &str) -> Option<Vec<(bool, &'v str)>> {
        match split_interpolation(&a.value, self.parser.max_bytes()) {
            Ok(parts) => Some(parts),
            Err(SplitError::TooLong(hit)) => {
                self.limit(hit, directive, a.offset);
                None
            }
            Err(SplitError::Unterminated(message)) => {
                self.err(
                    DiagnosticKind::InvalidDirectiveValue,
                    directive,
                    a.offset,
                    message,
                );
                None
            }
        }
    }

    fn not_until(&mut self, directive: &str, offset: u32, feature: &str, task: &str) {
        self.err(
            DiagnosticKind::NotSupportedUntil,
            directive,
            offset,
            format!("{feature} is not supported until {task}"),
        );
    }

    fn lookup(scope: &BTreeMap<Box<str>, Vec<LocalId>>, name: &str) -> Option<LocalId> {
        scope.get(name).and_then(|ids| ids.last().copied())
    }

    /// Parses an expression; on failure records the diagnostic and returns `None`.
    fn expr(
        &mut self,
        src: &str,
        context: ExprContext,
        directive: &str,
        offset: u32,
    ) -> Option<Expr> {
        let scope = &self.scope;
        let locals = |n: &str| Self::lookup(scope, n);
        let env = ExprEnv {
            locals: &locals,
            mode: self.options.mode(),
            context,
            trust: self.trust,
        };
        match self.parser.parse(src, &env) {
            Ok(e) => {
                // Past the locals limit, names that were not declared would read as free: the
                // template already failed, so they are not collected.
                if !self.locals_over {
                    let mut names = Vec::new();
                    e.for_each_free(&mut |n| names.push(n));
                    for n in names {
                        if !self.free.contains_key(n) {
                            let location = self.loc.at(offset);
                            self.free.insert(n.into(), location);
                        }
                    }
                }
                Some(e)
            }
            Err(e) => {
                match e.limit {
                    Some(hit) if e.kind == DiagnosticKind::Limit => {
                        let d = Diagnostic::limit_hit(hit);
                        let d = if d.message() == e.message {
                            d
                        } else {
                            d.with_message(e.message)
                        };
                        self.diags.push(
                            d.with_directive(directive)
                                .with_template(Some(self.template))
                                .with_location(self.loc.at(offset)),
                        );
                    }
                    _ => self.err(e.kind, directive, offset, e.message),
                }
                None
            }
        }
    }

    fn attr_expr(&mut self, a: &XAttr<'_>, context: ExprContext, directive: &str) -> Option<Expr> {
        self.expr(&a.value, context, directive, a.offset)
    }

    /// Declares a template local in the innermost frame. Past the `locals` limit the template
    /// fails once and later declarations are not recorded.
    fn declare(&mut self, name: &str, info: LocalInfo, offset: u32) {
        if self.locals_over {
            return;
        }
        let count = self.locals.len() + 1;
        let id = match LocalId::try_from(self.locals.len()) {
            Ok(id) if count <= self.max_locals => id,
            _ => {
                self.locals_over = true;
                self.limit(
                    LimitHit::new(LimitKind::Locals, self.max_locals, count),
                    "t-set",
                    offset,
                );
                return;
            }
        };
        self.locals.push(info);
        let Some(frame) = self.frames.last_mut() else {
            self.diags.push(
                Diagnostic::internal("a local was declared outside every scope")
                    .with_template(Some(self.template))
                    .with_location(self.loc.at(offset)),
            );
            return;
        };
        frame.push(name.into());
        self.scope.entry(name.into()).or_default().push(id);
        self.snapshot = None;
    }

    /// Declares a `t-set` local and returns its ID.
    fn declare_set(&mut self, name: &str, offset: u32) -> Option<LocalId> {
        if self.locals_over {
            return None;
        }
        let count = self.locals.len() + 1;
        let id = match LocalId::try_from(self.locals.len()) {
            Ok(id) if count <= self.max_locals => id,
            _ => {
                self.locals_over = true;
                self.limit(
                    LimitHit::new(LimitKind::Locals, self.max_locals, count),
                    "t-set",
                    offset,
                );
                return None;
            }
        };
        self.locals.push(LocalInfo::Set { id });
        let Some(frame) = self.frames.last_mut() else {
            self.diags.push(
                Diagnostic::internal("a local was declared outside every scope")
                    .with_template(Some(self.template))
                    .with_location(self.loc.at(offset)),
            );
            return None;
        };
        frame.push(name.into());
        self.scope.entry(name.into()).or_default().push(id);
        self.snapshot = None;
        Some(id)
    }

    fn push_frame(&mut self) {
        self.frames.push(Vec::new());
    }

    /// Leaves the innermost frame and returns the names it declared.
    fn pop_frame(&mut self) -> Vec<Box<str>> {
        let names = self.frames.pop().unwrap_or_default();
        for n in &names {
            if let Some(ids) = self.scope.get_mut(n) {
                ids.pop();
                if ids.is_empty() {
                    self.scope.remove(n);
                }
            }
        }
        if !names.is_empty() {
            self.snapshot = None;
        }
        names
    }

    /// The names in scope, shared with every call site until the scope changes.
    fn in_scope(&mut self) -> Rc<BTreeSet<Box<str>>> {
        if let Some(s) = &self.snapshot {
            return Rc::clone(s);
        }
        let s: Rc<BTreeSet<Box<str>>> = Rc::new(self.scope.keys().cloned().collect());
        self.snapshot = Some(Rc::clone(&s));
        s
    }

    /// Checks a name that becomes a template local.
    fn local_name(&mut self, a: &XAttr<'_>, directive: &str) -> Option<String> {
        let name = a.value.trim();
        if is_plain_identifier(name) {
            Some(name.to_owned())
        } else {
            self.err(
                DiagnosticKind::InvalidName,
                directive,
                a.offset,
                format!("{directive}=\"{}\": a variable name must be a plain identifier that is not a reserved word", a.value),
            );
            None
        }
    }

    // --- children ------------------------------------------------------------------------------

    fn branch_count(el: &XElement<'_>) -> usize {
        ["t-if", "t-elif", "t-else"]
            .iter()
            .filter(|n| el.has(n))
            .count()
    }

    fn children(&mut self, nodes: &[XNode<'s>], out: &mut Vec<Node>) {
        let mut i = 0;
        while i < nodes.len() {
            let el = match &nodes[i] {
                XNode::Text { text, .. } => {
                    self.translatable |= !is_blank(text);
                    out.push(Node::Text(text.as_ref().into()));
                    i += 1;
                    continue;
                }
                XNode::Element(el) => el,
            };
            if Self::branch_count(el) > 1 {
                self.err(
                    DiagnosticKind::MultipleBranchDirectives,
                    "t-if",
                    el.offset,
                    "Only one conditional branching directive is allowed per node",
                );
                i += 1;
                continue;
            }
            if el.has("t-elif") || el.has("t-else") {
                self.err(
                    DiagnosticKind::BranchWithoutIf,
                    "t-if",
                    el.offset,
                    "t-elif and t-else directives must be preceded by a t-if or t-elif directive",
                );
                i += 1;
                continue;
            }
            if !el.has("t-if") {
                let nodes_out = self.element(el, Attrs::new(el));
                out.extend(nodes_out);
                i += 1;
                continue;
            }
            // A t-if chain: the following t-elif/t-else siblings, whitespace between them only.
            let mut chain: Vec<&XElement<'s>> = vec![el];
            let mut j = i + 1;
            loop {
                let mut k = j;
                let mut text_between = None;
                while let Some(XNode::Text { text, offset }) = nodes.get(k) {
                    if !is_blank(text) && text_between.is_none() {
                        text_between = Some(*offset);
                    }
                    k += 1;
                }
                let Some(XNode::Element(next)) = nodes.get(k) else {
                    break;
                };
                if !(next.has("t-elif") || next.has("t-else")) {
                    break;
                }
                if let Some(offset) = text_between {
                    self.err(
                        DiagnosticKind::TextBetweenBranches,
                        "t-if",
                        offset,
                        "text is not allowed between branching directives",
                    );
                }
                if Self::branch_count(next) > 1 {
                    self.err(
                        DiagnosticKind::MultipleBranchDirectives,
                        "t-if",
                        next.offset,
                        "Only one conditional branching directive is allowed per node",
                    );
                }
                let prev = chain[chain.len() - 1];
                if prev.has("t-foreach") {
                    self.err(
                        DiagnosticKind::IfWithForeachChained,
                        "t-if",
                        prev.offset,
                        "t-if cannot stay at the same level as t-foreach when using t-elif or t-else",
                    );
                }
                let is_else = next.has("t-else");
                chain.push(next);
                j = k + 1;
                if is_else {
                    break;
                }
            }
            if chain.len() == 1 {
                let nodes_out = self.element(el, Attrs::new(el));
                out.extend(nodes_out);
                i += 1;
                continue;
            }
            let pos = Pos(el.offset);
            let mut branches = Vec::with_capacity(chain.len());
            let mut ok = true;
            for branch in chain {
                let mut at = Attrs::new(branch);
                let cond = if let Some(a) = at.take("t-if") {
                    let c = self.attr_expr(a, ExprContext::Render, "t-if");
                    ok &= c.is_some();
                    c
                } else if let Some(a) = at.take("t-elif") {
                    let c = self.attr_expr(a, ExprContext::Render, "t-elif");
                    ok &= c.is_some();
                    c
                } else {
                    if let Some(a) = at.take("t-else")
                        && !a.value.is_empty()
                    {
                        self.err(
                            DiagnosticKind::InvalidDirectiveValue,
                            "t-else",
                            a.offset,
                            format!("t-else takes no value, found t-else=\"{}\"", a.value),
                        );
                    }
                    None
                };
                self.if_depth += 1;
                let content = self.element(branch, at);
                self.if_depth -= 1;
                branches.push((cond, content));
            }
            if ok {
                out.push(Node::If { branches, pos });
            }
            i = j;
        }
    }

    // --- one element ---------------------------------------------------------------------------

    fn element(&mut self, el: &XElement<'s>, mut at: Attrs<'_, 's>) -> Vec<Node> {
        let pos = Pos(el.offset);

        // t-set-slot: registered on the enclosing component, rendered nowhere here. Outside a
        // component, OWL's `parseTNode` (`owl.js:5315`) ignores the attribute on a `<t>` and renders
        // the content in place (`web.BadgeTagDot` relies on it): a warning, same output.
        if el.name == "t" && self.slot_targets.is_empty() {
            if let Some(a) = at.take("t-set-slot") {
                if is_slot_name(&a.value) {
                    self.diag(
                        Severity::Warning,
                        DiagnosticKind::IgnoredAttribute,
                        "t-set-slot",
                        a.offset,
                        "t-set-slot outside a component is ignored: its content renders in place",
                    );
                } else {
                    self.err(
                        DiagnosticKind::InvalidName,
                        "t-set-slot",
                        a.offset,
                        format!(
                            "t-set-slot=\"{}\": a slot name is [A-Za-z_][A-Za-z0-9_.-]*, not __proto__",
                            a.value
                        ),
                    );
                }
            }
        } else if let Some(a) = at.take("t-set-slot") {
            self.set_slot(el, a, at);
            return Vec::new();
        }

        // Custom directives (`owl.js:5336-5358`).
        if let Some(a) = at.take_where(|n| n == "t-custom" || n.starts_with("t-custom-")) {
            let name = a.name.strip_prefix("t-custom-").unwrap_or("");
            let directive_name = name.split('.').next().unwrap_or("");
            if directive_name.is_empty() {
                self.err(
                    DiagnosticKind::InvalidDirectiveValue,
                    "t-custom",
                    a.offset,
                    "Missing custom directive name with t-custom directive",
                );
            } else if !self.options.has_custom_directive(directive_name) {
                self.err(
                    DiagnosticKind::CustomDirectiveNotDefined,
                    "t-custom",
                    a.offset,
                    format!("Custom directive \"{directive_name}\" is not defined"),
                );
            } else {
                self.attr_expr(a, ExprContext::Handler, "t-custom");
            }
            // The rest of the element is analysed so its errors are reported; its IR is dropped,
            // because the whole node is deferred.
            let _rest_is_deferred = self.element(el, at);
            return vec![Node::Deferred {
                what: "t-custom-*",
                task: UI2_DONE,
                pos,
            }];
        }

        // t-anchor (design §4.3): validated; kept in dev output, stripped in prod.
        let mut anchor = None;
        if let Some(a) = at.take("t-anchor") {
            if is_anchor_name(&a.value) {
                if self.options.mode() == BuildMode::Dev {
                    anchor = Some(a.value.to_string());
                }
            } else {
                self.err(
                    DiagnosticKind::InvalidName,
                    "t-anchor",
                    a.offset,
                    format!("t-anchor=\"{}\": an anchor name is [a-z0-9_.@-]+", a.value),
                );
            }
        }

        // t-debug / t-log (`owl.js:5359-5388`): stripped in prod.
        let mut dev_only = false;
        if let Some(a) = at.take("t-debug") {
            if !a.value.is_empty() {
                self.err(
                    DiagnosticKind::InvalidDirectiveValue,
                    "t-debug",
                    a.offset,
                    "t-debug takes no value",
                );
            }
            dev_only = true;
        }
        if let Some(a) = at.take("t-log") {
            self.attr_expr(a, ExprContext::Render, "t-log");
            dev_only = true;
        }

        let mut nodes = self.element_structural(el, at, anchor);
        if dev_only && self.options.mode() == BuildMode::Dev {
            nodes = vec![Node::Deferred {
                what: "t-debug/t-log in dev mode",
                task: UI2_DONE,
                pos,
            }];
        }
        nodes
    }

    fn element_structural(
        &mut self,
        el: &XElement<'s>,
        mut at: Attrs<'_, 's>,
        anchor: Option<String>,
    ) -> Vec<Node> {
        let pos = Pos(el.offset);
        for (name, why) in [
            (
                "t-raw",
                "t-raw was removed in OWL 2: use t-out with a markup() value",
            ),
            (
                "t-call-block",
                "t-call-block is internal to OWL and not a template directive",
            ),
            (
                "t-valuef",
                "t-valuef is not a directive (the reference ERP's OWL ignores it and sets null, BUG-063): use t-value=\"'…'\"",
            ),
        ] {
            if let Some(a) = at.take(name) {
                self.err(DiagnosticKind::RejectedDirective, name, a.offset, why);
            }
        }

        // t-foreach (`owl.js:5518-5552`).
        if let Some(a) = at.take("t-foreach") {
            return self.foreach(el, a, at, anchor);
        }
        if let Some(a) = at.take("t-as") {
            self.err(
                DiagnosticKind::MissingCompanion,
                "t-as",
                a.offset,
                "t-as is only allowed with t-foreach",
            );
        }

        // t-if without a chain.
        if let Some(a) = at.take("t-if") {
            let cond = self.attr_expr(a, ExprContext::Render, "t-if");
            self.if_depth += 1;
            let content = self.element_structural(el, at, anchor);
            self.if_depth -= 1;
            return match cond {
                Some(cond) => vec![Node::If {
                    branches: vec![(Some(cond), content)],
                    pos,
                }],
                None => Vec::new(),
            };
        }
        // A branch attribute left here was reported already: by `children` (no t-if before it, or
        // several branch directives on one node) or by `analyze_template` (on the root).
        let _reported = (at.take("t-elif"), at.take("t-else"));

        // Translation (`owl.js:5828-5870`): checked; the translation pass is.
        if let Some(a) = at.take("t-translation")
            && a.value != "off"
        {
            self.err(
                DiagnosticKind::InvalidDirectiveValue,
                "t-translation",
                a.offset,
                format!("t-translation=\"{}\": the only value is \"off\"", a.value),
            );
        }
        if let Some(a) = at.take("t-translation-context")
            && AddonName::new(&a.value).is_err()
        {
            self.err(
                DiagnosticKind::InvalidDirectiveValue,
                "t-translation-context",
                a.offset,
                format!(
                    "t-translation-context=\"{}\": expected an addon name",
                    a.value
                ),
            );
        }

        // t-call (`owl.js:5573-5608`).
        if let Some(a) = at.take("t-call") {
            return self.call(el, a, at);
        }
        if let Some(a) = at.take("t-call-context") {
            self.err(
                DiagnosticKind::MissingCompanion,
                "t-call-context",
                a.offset,
                "t-call-context is only allowed with t-call",
            );
        }

        // Standalone t-key (`owl.js:5553-5572`).
        if let Some(a) = at.take("t-key") {
            let key = self.attr_expr(a, ExprContext::Render, "t-key");
            let body = self.element_structural(el, at, anchor);
            return match key {
                Some(key) => vec![Node::Keyed { key, body, pos }],
                None => Vec::new(),
            };
        }

        // t-out / t-esc (`owl.js:5489-5517`, components: `normalizeTOut` `:5935-5952`).
        let component = is_component_tag(el.name) || at.has("t-component");
        if at.has("t-out") || at.has("t-esc") {
            return self.out(el, at, component, anchor);
        }

        // t-call-slot / t-slot (`owl.js:5792-5827`).
        if at.has("t-call-slot") || at.has("t-slot") {
            return self.call_slot(el, at);
        }

        if component {
            return self.component(el, at, None);
        }

        if el.name != "t" || at.has("t-tag") {
            return self.dom(el, at, anchor, None);
        }

        // t-set (`owl.js:5653-5665`).
        if let Some(a) = at.take("t-set") {
            return self.set(el, a, at);
        }
        if let Some(a) = at.take("t-value") {
            self.err(
                DiagnosticKind::MissingCompanion,
                "t-value",
                a.offset,
                "t-value is only allowed with t-set",
            );
        }
        if let Some(a) = at.take("t-portal") {
            let target = self.attr_expr(a, ExprContext::Render, "t-portal");
            let mut children = Vec::new();
            self.children(&el.children, &mut children);
            return match target {
                Some(target) => vec![Node::Portal {
                    target,
                    children,
                    pos,
                }],
                None => Vec::new(),
            };
        }

        // Plain <t>: no attribute left.
        self.leftovers(&mut at, "<t>");
        let mut out = Vec::new();
        self.children(&el.children, &mut out);
        out
    }

    /// Reports every attribute no stage consumed on a `<t>` node (`on` names it). A misplaced
    /// `t-set` is reported once: its `t-value` companion is part of the same mistake. OWL's
    /// `parseTNode` (`owl.js:5315`) drops plain attributes: the markers of [`T_MARKERS`] get a
    /// warning (same output, the template compiles), any other is an error, since the author
    /// expected it to apply (silent-failure review M-c, BUG-065).
    fn leftovers(&mut self, at: &mut Attrs<'_, 's>, on: &str) {
        let rest = at.take_rest();
        let has_set = rest.iter().any(|a| a.name == "t-set");
        for a in rest {
            if has_set && a.name == "t-value" {
                continue;
            }
            if a.name.starts_with("t-") {
                self.unknown_attribute(a, on);
            } else if T_MARKERS.contains(&a.name) {
                self.diag(
                    Severity::Warning,
                    DiagnosticKind::IgnoredAttribute,
                    "attribute",
                    a.offset,
                    format!("attribute '{}' on {on} is ignored: a <t> node renders no element to carry it", a.name),
                );
            } else {
                self.err(
                    DiagnosticKind::DirectiveNotAllowedHere,
                    "attribute",
                    a.offset,
                    format!(
                        "attribute '{}' is not allowed on {on}: a <t> node renders no element to carry it (put it on an element; only the markers {} are accepted, and ignored)",
                        a.name,
                        T_MARKERS.join(", ")
                    ),
                );
            }
        }
    }

    fn unknown_attribute(&mut self, a: &XAttr<'_>, on: &str) {
        let name = a.name;
        if name.starts_with("t-") {
            let known = [
                "t-name",
                "t-inherit",
                "t-inherit-mode",
                "t-set",
                "t-value",
                "t-ref",
                "t-model",
                "t-props",
                "t-slot-scope",
                "t-component",
                "t-tag",
                "t-portal",
                "t-on",
            ];
            let base = name.split('.').next().unwrap_or(name);
            if known.contains(&base) || base.starts_with("t-on-") || base.starts_with("t-model") {
                self.err(
                    DiagnosticKind::DirectiveNotAllowedHere,
                    base,
                    a.offset,
                    format!("directive '{name}' is not allowed on {on}"),
                );
            } else {
                self.err(
                    DiagnosticKind::UnknownDirective,
                    base,
                    a.offset,
                    format!("Unknown QWeb directive: '{name}'"),
                );
            }
        } else {
            self.err(
                DiagnosticKind::DirectiveNotAllowedHere,
                "attribute",
                a.offset,
                format!("attribute '{name}' is not allowed on {on}"),
            );
        }
    }

    // --- stages --------------------------------------------------------------------------------

    fn foreach(
        &mut self,
        el: &XElement<'s>,
        a: &XAttr<'_>,
        mut at: Attrs<'_, 's>,
        anchor: Option<String>,
    ) -> Vec<Node> {
        let pos = Pos(el.offset);
        let collection = self.attr_expr(a, ExprContext::Render, "t-foreach");
        let as_attr = at.take("t-as");
        let key_attr = at.take("t-key");
        let as_value = as_attr.map_or("", |x| &*x.value);
        if key_attr.is_none_or(|k| k.value.is_empty()) {
            self.err(
                DiagnosticKind::MissingTKey,
                "t-foreach",
                a.offset,
                format!(
                    "\"Directive t-foreach should always be used with a t-key!\" (expression: t-foreach=\"{}\" t-as=\"{as_value}\")",
                    a.value
                ),
            );
        }
        let name = match as_attr {
            Some(x) => self.local_name(x, "t-as"),
            None => {
                self.err(
                    DiagnosticKind::MissingCompanion,
                    "t-foreach",
                    a.offset,
                    "t-foreach needs t-as naming the loop variable",
                );
                None
            }
        };
        let row = self.loops;
        self.loops += 1;
        self.push_frame();
        if let Some(name) = &name {
            for (suffix, field) in [
                ("", LoopField::Item),
                ("_index", LoopField::Index),
                ("_value", LoopField::Value),
                ("_first", LoopField::First),
                ("_last", LoopField::Last),
            ] {
                self.declare(
                    &format!("{name}{suffix}"),
                    LocalInfo::Loop { row, field },
                    a.offset,
                );
            }
        }
        let key = key_attr
            .filter(|k| !k.value.is_empty())
            .and_then(|k| self.attr_expr(k, ExprContext::Render, "t-key"));
        let body = self.element_structural(el, at, anchor);
        self.pop_frame();
        match (collection, key, name) {
            (Some(collection), Some(key), Some(_)) => vec![Node::For {
                row,
                collection,
                key,
                body,
                pos,
            }],
            _ => Vec::new(),
        }
    }

    fn call(&mut self, el: &XElement<'s>, a: &XAttr<'_>, mut at: Attrs<'_, 's>) -> Vec<Node> {
        let pos = Pos(el.offset);
        if el.name != "t" {
            self.err(
                DiagnosticKind::DirectiveNotAllowedHere,
                "t-call",
                a.offset,
                format!(
                    "Directive 't-call' can only be used on <t> nodes (used on a <{}>)",
                    el.name
                ),
            );
            return Vec::new();
        }
        let context_attr = at.take("t-call-context");
        let context =
            context_attr.and_then(|c| self.attr_expr(c, ExprContext::Render, "t-call-context"));
        let context_ok = context_attr.is_none() || context.is_some();
        let mut target = None;
        let mut callee = None;
        match self.split(a, "t-call") {
            None => {}
            Some(parts) if parts.iter().any(|(dynamic, _)| *dynamic) => {
                for (dynamic, part) in parts {
                    if dynamic
                        && let Some(expr) = self.expr(part, ExprContext::Render, "t-call", a.offset)
                    {
                        target = Some(CallTarget::Dynamic(expr));
                    }
                }
            }
            Some(_) => match TemplateName::new(&a.value) {
                Ok(name) if self.templates.contains(&name) => {
                    callee = Some(name.clone());
                    target = Some(CallTarget::Static(name));
                }
                Ok(name) => self.err(
                    DiagnosticKind::MissingTemplate,
                    "t-call",
                    a.offset,
                    format!("t-call: template '{name}' is not in the template set"),
                ),
                Err(e) => self.err(
                    DiagnosticKind::InvalidName,
                    "t-call",
                    a.offset,
                    e.to_string(),
                ),
            },
        }
        // The other attributes are the callee's locals.
        let in_scope = self.in_scope();
        let mut own: BTreeSet<Box<str>> = BTreeSet::new();
        let mut params = Vec::new();
        for p in at.take_rest() {
            if p.name.starts_with("t-translation-context-") {
                self.not_until(
                    "t-translation-context",
                    p.offset,
                    "t-translation-context-<attr>",
                    UI3_DONE,
                );
            } else if p.name.starts_with("t-") {
                self.unknown_attribute(p, "a t-call node");
            } else {
                // Parameters take the prop suffixes (OWL formats them with `formatProp`,
                // `owl.js:6753`, `:6882`): `.translate` is a literal string, `.bind` and `.alike`
                // an expression like no suffix; an empty value is `undefined`.
                let (base, suffix) = match p.name.split_once('.') {
                    Some((base, suffix)) => (base, Some(suffix)),
                    None => (p.name, None),
                };
                if !is_plain_identifier(base) {
                    self.err(
                        DiagnosticKind::InvalidName,
                        "t-call",
                        p.offset,
                        format!(
                            "t-call parameter '{base}' is not a plain identifier (nor __proto__)"
                        ),
                    );
                    continue;
                }
                let val_expr = match suffix {
                    None | Some("bind" | "alike") => self.prop_value(p),
                    Some("translate") => {
                        self.translatable = true;
                        Some(Expr::Str(p.value.as_ref().into()))
                    }
                    Some(other) => {
                        self.err(
                            DiagnosticKind::InvalidPropSuffix,
                            "t-call",
                            p.offset,
                            format!("Invalid prop suffix: {other}"),
                        );
                        continue;
                    }
                };
                own.insert(base.into());
                if let Some(val) = val_expr {
                    params.push(CallParam {
                        name: base.into(),
                        value: val,
                    });
                }
            }
        }
        // The body runs in a scope the callee sees: its t-sets are the callee's locals too.
        self.push_frame();
        let mut body = Vec::new();
        self.children(&el.children, &mut body);
        own.extend(self.pop_frame());
        if !body.is_empty() {
            own.insert("0".into());
        }
        if let Some(callee) = callee {
            self.calls.push(CallSite {
                callee,
                in_scope,
                own,
                binds_all: context_attr.is_some() && context_ok,
                guarded: self.if_depth > 0,
                pos,
            });
        }
        let body = (!body.is_empty()).then_some(body);
        if let Some(target) = target {
            vec![Node::Call {
                target,
                context,
                params,
                body,
                pos,
            }]
        } else {
            Vec::new()
        }
    }

    fn out(
        &mut self,
        el: &XElement<'s>,
        mut at: Attrs<'_, 's>,
        component: bool,
        anchor: Option<String>,
    ) -> Vec<Node> {
        let pos = Pos(el.offset);
        let esc = at.take("t-esc");
        let out = at.take("t-out");
        let (a, directive) = match (out, esc) {
            (Some(o), Some(e)) => {
                self.err(
                    DiagnosticKind::InvalidDirectiveValue,
                    "t-esc",
                    e.offset,
                    "t-out and t-esc on one node: use t-out only",
                );
                (o, "t-out")
            }
            (Some(o), None) => (o, "t-out"),
            (None, Some(e)) => {
                self.diag(
                    Severity::Warning,
                    DiagnosticKind::Deprecated,
                    "t-esc",
                    e.offset,
                    "t-esc has been deprecated in favor of t-out. If the value to render is not wrapped by the \"markup\" function, it will be escaped",
                );
                (e, "t-esc")
            }
            (None, None) => return Vec::new(),
        };
        let caller_body = a.value.trim() == "0";
        let expr = if caller_body {
            None
        } else {
            self.attr_expr(a, ExprContext::Render, directive)
        };
        if component {
            if directive == "t-esc" {
                self.err(
                    DiagnosticKind::ComponentDirective,
                    "t-esc",
                    a.offset,
                    "t-esc is not allowed on a component: use t-out (it becomes the default slot)",
                );
                return Vec::new();
            }
            if !el.children.is_empty() {
                self.err(
                    DiagnosticKind::ComponentDirective,
                    "t-out",
                    a.offset,
                    "Cannot have t-out on a component that already has content",
                );
                return Vec::new();
            }
            let slot_body = match (caller_body, expr) {
                (true, _) => vec![Node::CallerBody { pos }],
                (false, Some(expr)) => vec![Node::Out {
                    expr,
                    body: None,
                    pos,
                }],
                (false, None) => return Vec::new(),
            };
            return self.component(el, at, Some(slot_body));
        }
        let out_node = |body: Option<Vec<Node>>| -> Option<Node> {
            if caller_body {
                Some(Node::CallerBody { pos })
            } else {
                expr.clone().map(|expr| Node::Out { expr, body, pos })
            }
        };
        let mut body = Vec::new();
        self.children(&el.children, &mut body);
        let body = (!body.is_empty()).then_some(body);
        if el.name == "t" && !at.has("t-tag") {
            self.leftovers(&mut at, "a <t t-out> node");
            return out_node(body).into_iter().collect();
        }
        // On an element: the element with the output as its only child, its children the
        // fallback.
        let Some(node) = out_node(body) else {
            return Vec::new();
        };
        self.dom(el, at, anchor, Some(vec![node]))
    }

    fn call_slot(&mut self, el: &XElement<'s>, mut at: Attrs<'_, 's>) -> Vec<Node> {
        let pos = Pos(el.offset);
        let a = match (at.take("t-call-slot"), at.take("t-slot")) {
            (Some(a), Some(b)) => {
                self.err(
                    DiagnosticKind::InvalidDirectiveValue,
                    "t-slot",
                    b.offset,
                    "t-call-slot and t-slot on one node",
                );
                a
            }
            (Some(a), None) | (None, Some(a)) => a,
            (None, None) => return Vec::new(),
        };
        let directive = a.name;
        if el.name != "t" {
            self.err(
                DiagnosticKind::DirectiveNotAllowedHere,
                directive,
                a.offset,
                format!(
                    "Directive '{directive}' can only be used on <t> nodes (used on a <{}>)",
                    el.name
                ),
            );
            return Vec::new();
        }
        let mut deferred: Option<&'static str> = None;
        let mut target = None;
        match self.split(a, directive) {
            None => {}
            Some(parts) if parts.iter().any(|(dynamic, _)| *dynamic) => {
                for (dynamic, part) in parts {
                    if dynamic
                        && let Some(expr) =
                            self.expr(part, ExprContext::Render, directive, a.offset)
                    {
                        target = Some(SlotTarget::Dynamic(expr));
                    }
                }
            }
            Some(_) if is_slot_name(&a.value) => {
                target = Some(SlotTarget::Static(a.value.to_string().into()))
            }
            Some(_) => self.err(
                DiagnosticKind::InvalidName,
                directive,
                a.offset,
                format!(
                    "{directive}=\"{}\": a slot name is [A-Za-z_][A-Za-z0-9_.-]*, not __proto__",
                    a.value
                ),
            ),
        }
        let scope = at
            .take("t-props")
            .and_then(|p| self.attr_expr(p, ExprContext::Render, "t-props"));
        for p in at.take_rest() {
            if let Some(event) = p.name.strip_prefix("t-on-") {
                self.event_directive(p, event);
                deferred = Some("t-on-* on a slot");
            } else if p.name.starts_with("t-translation-context-") {
                self.not_until(
                    "t-translation-context",
                    p.offset,
                    "t-translation-context-<attr>",
                    UI3_DONE,
                );
            } else if p.name.starts_with("t-") {
                self.unknown_attribute(p, "a t-call-slot node");
            } else if is_prop_name(p.name) {
                self.expr(&p.value, ExprContext::Render, directive, p.offset);
                deferred = Some("slot parameters");
            } else {
                self.err(
                    DiagnosticKind::InvalidName,
                    directive,
                    p.offset,
                    format!("slot parameter '{}' is not a valid name", p.name),
                );
            }
        }
        let mut fallback = Vec::new();
        self.children(&el.children, &mut fallback);
        if let Some(what) = deferred {
            return vec![Node::Deferred {
                what,
                task: UI2_DONE,
                pos,
            }];
        }
        match target {
            Some(name) => vec![Node::Slot {
                name,
                scope,
                fallback,
                pos,
            }],
            None => Vec::new(),
        }
    }

    fn set_slot(&mut self, el: &XElement<'s>, a: &XAttr<'_>, mut at: Attrs<'_, 's>) {
        if el.name != "t" {
            self.err(
                DiagnosticKind::DirectiveNotAllowedHere,
                "t-set-slot",
                a.offset,
                format!(
                    "Directive 't-set-slot' can only be used on <t> nodes (used on a <{}>)",
                    el.name
                ),
            );
            return;
        }
        // Outside a component a `<t t-set-slot>` is handled by `element` (a warning).
        let name = if is_slot_name(&a.value) {
            Some(a.value.to_string())
        } else {
            self.err(
                DiagnosticKind::InvalidName,
                "t-set-slot",
                a.offset,
                format!(
                    "t-set-slot=\"{}\": a slot name is [A-Za-z_][A-Za-z0-9_.-]*, not __proto__",
                    a.value
                ),
            );
            None
        };
        let scope_attr = at.take("t-slot-scope");
        let mut params = Vec::new();
        while let Some(p) = at.take_where(|n| !n.starts_with("t-") || n.starts_with("t-on-")) {
            if let Some(event) = p.name.strip_prefix("t-on-") {
                self.event_directive(p, event);
            } else if is_prop_name(p.name) {
                if let Some(expr) = self.expr(&p.value, ExprContext::Render, "t-set-slot", p.offset)
                {
                    params.push(PropIr {
                        name: p.name.into(),
                        value: expr,
                        bind: false,
                    });
                }
            } else {
                self.err(
                    DiagnosticKind::InvalidName,
                    "t-set-slot",
                    p.offset,
                    format!("slot parameter '{}' is not a valid name", p.name),
                );
            }
        }
        self.push_frame();
        let mut scope = None;
        if let Some(s) = scope_attr
            && let Some(local) = self.local_name(s, "t-slot-scope")
        {
            let slot = self.slots;
            self.slots += 1;
            self.declare(&local, LocalInfo::SlotScope { slot }, s.offset);
            scope = Some(slot);
        }
        let body = self.element_structural(el, at, None);
        self.pop_frame();
        let Some(name) = name else {
            return;
        };
        let duplicate = self
            .slot_targets
            .last()
            .is_some_and(|c| c.slots.iter().any(|s| *s.name == *name));
        if duplicate {
            self.err(
                DiagnosticKind::InvalidDirectiveValue,
                "t-set-slot",
                a.offset,
                format!("slot '{name}' is defined twice for this component"),
            );
            return;
        }
        if let Some(target) = self.slot_targets.last_mut() {
            target.slots.push(SlotIr {
                name: name.into(),
                scope,
                params,
                body,
            });
        }
    }

    fn component(
        &mut self,
        el: &XElement<'s>,
        mut at: Attrs<'_, 's>,
        out_slot: Option<Vec<Node>>,
    ) -> Vec<Node> {
        let pos = Pos(el.offset);
        let mut target = None;
        if let Some(c) = at.take("t-component") {
            if el.name != "t" {
                self.err(
                    DiagnosticKind::DirectiveNotAllowedHere,
                    "t-component",
                    c.offset,
                    format!(
                        "Directive 't-component' can only be used on <t> nodes (used on a <{}>)",
                        el.name
                    ),
                );
                return Vec::new();
            }
            if let Some(expr) = self.attr_expr(c, ExprContext::Render, "t-component") {
                target = Some(ComponentTarget::Dynamic(expr));
            }
        } else if is_component_name(el.name) {
            target = Some(ComponentTarget::Static(el.name.into()));
        } else {
            self.err(
                DiagnosticKind::InvalidName,
                "component",
                el.offset,
                format!("component name '{}' is not [A-Z][A-Za-z0-9_]*", el.name),
            );
        }
        let spread = at
            .take("t-props")
            .and_then(|p| self.attr_expr(p, ExprContext::Render, "t-props"));
        let default_scope = at.take("t-slot-scope");
        let mut props: Vec<PropIr> = Vec::new();
        let mut events: Vec<Binding> = Vec::new();
        let mut ok = true;
        for p in at.take_rest() {
            let pname = p.name;
            if pname.starts_with("t-translation-context-") {
                self.not_until(
                    "t-translation-context",
                    p.offset,
                    "t-translation-context-<attr>",
                    UI3_DONE,
                );
                ok = false;
            } else if let Some(event) = pname.strip_prefix("t-on-") {
                if let Some((ev, flags, handler)) = self.event_directive(p, event) {
                    events.push(Binding::On {
                        event: ev.into(),
                        flags,
                        handler,
                    });
                } else {
                    ok = false;
                }
            } else if pname.starts_with("t-") {
                let family = pname.split('-').take(2).collect::<Vec<_>>().join("-");
                let (kind, message) = match family.as_str() {
                    "t-ref" => (
                        DiagnosticKind::RefOnComponent,
                        "t-ref is no longer supported on components. Consider exposing only the public part of the component's API through a callback prop.".to_owned(),
                    ),
                    "t-att" => (
                        DiagnosticKind::ComponentDirective,
                        "t-att makes no sense on component: props are already treated as expressions".to_owned(),
                    ),
                    "t-attf" => (
                        DiagnosticKind::ComponentDirective,
                        "t-attf is not supported on components: use template strings for string interpolation in props".to_owned(),
                    ),
                    _ => (
                        DiagnosticKind::ComponentDirective,
                        format!("unsupported directive on Component: {pname}"),
                    ),
                };
                let directive = if family == "t-attf" {
                    "t-attf".to_owned()
                } else {
                    pname.split('.').next().unwrap_or(pname).to_owned()
                };
                self.err(kind, &directive, p.offset, message);
                ok = false;
            } else {
                let mut parts = pname.split('.');
                let base = parts.next().unwrap_or("");
                let suffix = parts.next();
                if parts.next().is_some() {
                    self.err(
                        DiagnosticKind::InvalidPropSuffix,
                        "prop",
                        p.offset,
                        format!("Invalid prop suffix: {}", &pname[base.len() + 1..]),
                    );
                    ok = false;
                    continue;
                }
                if !is_prop_name(base) {
                    self.err(
                        DiagnosticKind::InvalidName,
                        "prop",
                        p.offset,
                        format!(
                            "prop name '{base}' is not [A-Za-z_$][A-Za-z0-9_$-]* (nor __proto__)"
                        ),
                    );
                    ok = false;
                    continue;
                }
                let (value, bind) = match suffix {
                    Some("translate") => {
                        self.translatable = true;
                        (Some(Expr::Str(p.value.as_ref().into())), false)
                    }
                    Some(s @ ("bind" | "alike")) => (self.prop_value(p), s == "bind"),
                    Some(other) => {
                        self.err(
                            DiagnosticKind::InvalidPropSuffix,
                            "prop",
                            p.offset,
                            format!("Invalid prop suffix: {other}"),
                        );
                        ok = false;
                        continue;
                    }
                    None => (self.prop_value(p), false),
                };
                match value {
                    Some(value) => props.push(PropIr {
                        name: base.into(),
                        value,
                        bind,
                    }),
                    None => ok = false,
                }
            }
        }
        // Slots: the t-set-slot descendants and the default content.
        self.slot_targets.push(SlotCollector::default());
        self.push_frame();
        let mut default_scope_slot = None;
        if let Some(s) = default_scope
            && let Some(local) = self.local_name(s, "t-slot-scope")
        {
            let slot = self.slots;
            self.slots += 1;
            self.declare(&local, LocalInfo::SlotScope { slot }, s.offset);
            default_scope_slot = Some(slot);
        }
        let default_body = match out_slot {
            Some(body) => body,
            None => {
                let mut body = Vec::new();
                self.children(&el.children, &mut body);
                body
            }
        };
        self.pop_frame();
        let mut slots = self.slot_targets.pop().unwrap_or_default().slots;
        if !default_body.is_empty() {
            if slots.iter().any(|s| &*s.name == "default") {
                self.err(
                    DiagnosticKind::InvalidDirectiveValue,
                    "t-set-slot",
                    el.offset,
                    "a component with t-set-slot=\"default\" cannot also have content outside its slots",
                );
                ok = false;
            } else {
                slots.push(SlotIr {
                    name: "default".into(),
                    scope: default_scope_slot,
                    params: Vec::new(),
                    body: default_body,
                });
            }
        }
        match (target, ok) {
            (Some(name), true) => vec![Node::Component(Component {
                name,
                props,
                spread,
                slots,
                events,
                pos,
            })],
            _ => Vec::new(),
        }
    }

    /// A prop value: an expression, or `undefined` when empty (`owl.js:6902`).
    fn prop_value(&mut self, p: &XAttr<'_>) -> Option<Expr> {
        if p.value.trim().is_empty() {
            return Some(Expr::Global("undefined"));
        }
        self.attr_expr(p, ExprContext::Render, "prop")
    }

    /// Checks a `t-on-<event>[.mods]` attribute; returns `(event, flags, handler)`.
    fn event_directive(
        &mut self,
        a: &XAttr<'_>,
        rest: &str,
    ) -> Option<(String, u32, Option<Expr>)> {
        let mut parts = rest.split('.');
        let event = parts.next().unwrap_or("");
        if event.is_empty() {
            self.err(
                DiagnosticKind::MissingEventName,
                "t-on",
                a.offset,
                "Missing event name with t-on directive",
            );
            return None;
        }
        let mut ok = true;
        if !is_event_name(event) {
            self.err(
                DiagnosticKind::InvalidName,
                "t-on",
                a.offset,
                format!("event name '{event}' is not [a-z][a-z0-9_:-]*"),
            );
            ok = false;
        }
        let mut flags = 0;
        for m in parts {
            match MODS.iter().find(|(n, _)| *n == m) {
                Some((_, f)) => flags |= f,
                None => {
                    self.err(
                        DiagnosticKind::UnknownEventModifier,
                        "t-on",
                        a.offset,
                        format!("Unknown event modifier: '{m}'"),
                    );
                    ok = false;
                }
            }
        }
        // OWL accepts the pair and the browser ignores `preventDefault()` in a passive listener
        // (BUG-070); the runtime's `on` refuses it too.
        let both = on_flags::PREVENT | on_flags::PASSIVE;
        if flags & both == both {
            self.err(
                DiagnosticKind::ConflictingEventModifiers,
                "t-on",
                a.offset,
                "the modifiers 'prevent' and 'passive' cannot be combined: a passive listener cannot prevent the default action; drop one of them",
            );
            ok = false;
        }
        let handler = if a.value.trim().is_empty() {
            None
        } else {
            Some(self.attr_expr(a, ExprContext::Handler, "t-on")?)
        };
        ok.then(|| (event.to_owned(), flags, handler))
    }

    fn set(&mut self, el: &XElement<'s>, a: &XAttr<'_>, mut at: Attrs<'_, 's>) -> Vec<Node> {
        let pos = Pos(el.offset);
        let name = self.local_name(a, "t-set");
        let value = if let Some(v) = at.take("t-value") {
            if v.value.trim().is_empty() {
                None
            } else {
                self.attr_expr(v, ExprContext::Render, "t-value")
            }
        } else {
            None
        };
        let body = if !el.children.is_empty() {
            let mut b = Vec::new();
            self.push_frame();
            self.children(&el.children, &mut b);
            self.pop_frame();
            Some(b)
        } else {
            None
        };
        self.leftovers(&mut at, "a t-set node");
        if let Some(name) = name
            && let Some(id) = self.declare_set(&name, a.offset)
        {
            return vec![Node::Set {
                local: id,
                name: name.into(),
                value,
                body,
                pos,
            }];
        }
        Vec::new()
    }

    fn dom(
        &mut self,
        el: &XElement<'s>,
        mut at: Attrs<'_, 's>,
        anchor: Option<String>,
        content: Option<Vec<Node>>,
    ) -> Vec<Node> {
        let pos = Pos(el.offset);
        let tag = el.name;
        let mut ok = true;
        let tag_expr = if let Some(t) = at.take("t-tag") {
            self.attr_expr(t, ExprContext::Render, "t-tag")
        } else {
            None
        };
        if tag.starts_with("block-") {
            self.err(
                DiagnosticKind::InvalidName,
                "t-tag",
                el.offset,
                format!("Invalid tag name: '{tag}'"),
            );
            return Vec::new();
        }
        if tag != "t" && !is_element_name(tag) {
            self.err(
                DiagnosticKind::InvalidName,
                "element",
                el.offset,
                format!("element name '{tag}' is not [A-Za-z][A-Za-z0-9-]*"),
            );
            ok = false;
        }
        let mut bindings = Vec::new();
        if let Some(r) = at.take("t-ref")
            && let Some(expr) = self.attr_expr(r, ExprContext::Render, "t-ref")
        {
            bindings.push(Binding::Ref(expr));
        }
        let portal = if let Some(p) = at.take("t-portal") {
            self.attr_expr(p, ExprContext::Render, "t-portal")
        } else {
            None
        };
        let mut attrs: Vec<(Box<str>, Box<str>)> = Vec::new();
        for a in at.take_rest() {
            let name = a.name;
            if name == "t-on" || name == "t-on-" {
                self.err(
                    DiagnosticKind::MissingEventName,
                    "t-on",
                    a.offset,
                    "Missing event name with t-on directive",
                );
                ok = false;
            } else if let Some(rest) = name.strip_prefix("t-on-") {
                match self.event_directive(a, rest) {
                    Some((event, flags, handler)) => bindings.push(Binding::On {
                        event: event.into(),
                        flags,
                        handler,
                    }),
                    None => ok = false,
                }
            } else if let Some(mods) = name.strip_prefix("t-model") {
                if let Some(binding) = self.model(el, a, mods) {
                    bindings.push(binding);
                } else {
                    ok = false;
                }
            } else if name.starts_with("block-") {
                self.err(
                    DiagnosticKind::InvalidName,
                    "attribute",
                    a.offset,
                    format!("Invalid attribute: '{name}'"),
                );
                ok = false;
            } else if name.starts_with("t-translation-context-") {
                self.not_until(
                    "t-translation-context",
                    a.offset,
                    "t-translation-context-<attr>",
                    UI3_DONE,
                );
                ok = false;
            } else if let Some(attr) = name.strip_prefix("t-attf-") {
                match self.binding(tag, a, attr, true) {
                    Some(b) => self.fold(b, &mut attrs, &mut bindings),
                    None => ok = false,
                }
            } else if let Some(attr) = name.strip_prefix("t-att-") {
                match self.binding(tag, a, attr, false) {
                    Some(b) => self.fold(b, &mut attrs, &mut bindings),
                    None => ok = false,
                }
            } else if name == "t-att" {
                match self.attr_expr(a, ExprContext::Render, "t-att") {
                    Some(e) => bindings.push(Binding::Attrs(e)),
                    None => ok = false,
                }
            } else if name == "t-value" && el.attrs.iter().any(|x| x.name == "t-set") {
                // Reported with the misplaced `t-set` it belongs to.
                ok = false;
            } else if name.starts_with("t-") {
                self.unknown_attribute(a, &format!("<{tag}>"));
                ok = false;
            } else if is_attribute_name(name) {
                attrs.push((name.into(), a.value.as_ref().into()));
            } else {
                self.err(
                    DiagnosticKind::InvalidName,
                    "attribute",
                    a.offset,
                    format!("attribute name '{name}' is not [A-Za-z_][A-Za-z0-9_.:-]*"),
                );
                ok = false;
            }
        }
        if let Some(anchor) = anchor
            && tag != "t"
        {
            attrs.push(("data-t-anchor".into(), anchor.into()));
        }
        let children = match content {
            Some(c) => c,
            None => {
                let mut c = Vec::new();
                self.children(&el.children, &mut c);
                c
            }
        };
        if !ok {
            return Vec::new();
        }
        let node = if let Some(tag) = tag_expr {
            Node::Tag {
                tag,
                attrs,
                bindings,
                children,
                pos,
            }
        } else if tag == "t" {
            if let Some(target) = portal {
                return vec![Node::Portal {
                    target,
                    children,
                    pos,
                }];
            }
            return children;
        } else {
            Node::Element(Element {
                tag: tag.into(),
                svg: el.svg,
                attrs,
                bindings,
                children,
                pos,
            })
        };
        if let Some(target) = portal {
            vec![Node::Portal {
                target,
                children: vec![node],
                pos,
            }]
        } else {
            vec![node]
        }
    }

    fn model(&mut self, el: &XElement<'s>, a: &XAttr<'_>, mods: &str) -> Option<Binding> {
        if !matches!(el.name, "input" | "select" | "textarea") {
            self.err(
                DiagnosticKind::ModelOnWrongElement,
                "t-model",
                a.offset,
                "The t-model directive only works with <input>, <textarea> and <select>",
            );
            return None;
        }
        let mut trim = false;
        let mut number = false;
        let mut lazy = false;
        let mut proxy = false;
        for m in mods.split('.').filter(|m| !m.is_empty()) {
            match m {
                "trim" => trim = true,
                "number" => number = true,
                "lazy" => lazy = true,
                "proxy" => proxy = true,
                other => {
                    self.err(
                        DiagnosticKind::InvalidDirectiveValue,
                        "t-model",
                        a.offset,
                        format!("Unknown t-model modifier: '{other}' (trim, lazy, number, proxy)"),
                    );
                    return None;
                }
            }
        }
        if !mods.is_empty() && !mods.starts_with('.') {
            self.unknown_attribute(a, &format!("<{}>", el.name));
            return None;
        }
        let expr = self.attr_expr(a, ExprContext::Render, "t-model")?;
        if proxy && !matches!(expr, Expr::Member(_, _, false) | Expr::Index(_, _, false)) {
            self.err(
                DiagnosticKind::InvalidDirectiveValue,
                "t-model",
                a.offset,
                "t-model.proxy needs a property to bind (this.state.value)",
            );
        }
        let input_type = if el.name == "input" {
            el.attr("type").map(Box::from)
        } else {
            None
        };
        Some(Binding::Model {
            target: expr,
            trim,
            number,
            lazy,
            proxy,
            tag: el.name.into(),
            input_type,
        })
    }

    /// A `t-att-<name>` / `t-attf-<name>` binding.
    fn binding(&mut self, tag: &str, a: &XAttr<'_>, attr: &str, format: bool) -> Option<Binding> {
        let directive = if format { "t-attf" } else { "t-att" };
        if !is_attribute_name(attr) {
            self.err(
                DiagnosticKind::InvalidName,
                directive,
                a.offset,
                format!("attribute name '{attr}' is not [A-Za-z_][A-Za-z0-9_.:-]*"),
            );
            return None;
        }
        let value = if format {
            let parts = self.split(a, directive)?;
            let mut out = Vec::with_capacity(parts.len());
            for (dynamic, part) in parts {
                if dynamic {
                    out.push(InterpPart::Expr(self.expr(
                        part,
                        ExprContext::Render,
                        directive,
                        a.offset,
                    )?));
                } else {
                    out.push(InterpPart::Lit(part.into()));
                }
            }
            AttrValue::Interp(out)
        } else {
            AttrValue::Expr(self.attr_expr(a, ExprContext::Render, directive)?)
        };
        Some(match attr {
            "class" => Binding::Class(value),
            "style" => Binding::Style(value),
            _ => match idl_property(tag, attr) {
                Some(prop) => Binding::Prop {
                    name: prop.into(),
                    value,
                },
                None => Binding::Attr {
                    name: attr.into(),
                    value,
                },
            },
        })
    }

    /// Folds a binding whose value is a literal into the static attributes (design note §7.1).
    fn fold(&self, b: Binding, attrs: &mut Vec<(Box<str>, Box<str>)>, bindings: &mut Vec<Binding>) {
        let literal = |v: &AttrValue| -> Option<Option<String>> {
            match v {
                AttrValue::Expr(e) => match e.constant()? {
                    crate::expr::Constant::Str(s) => Some(Some(s.to_owned())),
                    crate::expr::Constant::Bool(true) => Some(Some(String::new())),
                    crate::expr::Constant::Bool(false) | crate::expr::Constant::Null => Some(None),
                },
                AttrValue::Interp(parts) => {
                    let mut s = String::new();
                    for p in parts {
                        match p {
                            InterpPart::Lit(l) => s.push_str(l),
                            InterpPart::Expr(e) => match e.constant()? {
                                crate::expr::Constant::Str(x) => s.push_str(x),
                                _ => return None,
                            },
                        }
                    }
                    Some(Some(s))
                }
            }
        };
        match &b {
            Binding::Attr { name, value } if !attrs.iter().any(|(n, _)| n == name) => {
                if let Some(folded) = literal(value) {
                    if let Some(v) = folded {
                        attrs.push((name.clone(), v.into()));
                    }
                    return;
                }
            }
            Binding::Class(value) => {
                if let Some(Some(v)) = literal(value) {
                    if let Some((_, existing)) = attrs.iter_mut().find(|(n, _)| &**n == "class") {
                        let merged = format!("{existing} {v}");
                        *existing = merged.trim().into();
                    } else {
                        attrs.push(("class".into(), v.into()));
                    }
                    return;
                }
            }
            _ => {}
        }
        bindings.push(b);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::limits::CompilerLimits;

    #[test]
    fn interpolation_splits_like_owl() {
        let split = |s| split_interpolation(s, 4096);
        assert_eq!(
            split("a {{x}} b #{y}c").unwrap(),
            vec![
                (false, "a "),
                (true, "x"),
                (false, " b "),
                (true, "y"),
                (false, "c")
            ]
        );
        assert_eq!(split("{{x}}").unwrap(), vec![(true, "x")]);
        assert!(matches!(split("a {{x"), Err(SplitError::Unterminated(_))));
        assert_eq!(split("plain").unwrap(), vec![(false, "plain")]);
        assert_eq!(
            split("#{{a}}{{b}}").unwrap(),
            vec![(true, "{a"), (false, "}"), (true, "b")]
        );
        assert_eq!(
            split_interpolation("{{a}}", 4),
            Err(SplitError::TooLong(LimitHit::new(
                LimitKind::ExpressionBytes,
                4,
                5
            )))
        );
    }

    /// Rust review H2: a long run of interpolations (no `#{` anywhere) splits in linear time; a
    /// quadratic scan would take minutes here.
    #[test]
    fn interpolation_split_is_linear() {
        let s = "{{a}}".repeat(400_000);
        let parts = split_interpolation(&s, usize::MAX).unwrap();
        assert_eq!(parts.len(), 400_000);
        assert_eq!(find_opener("ab#{"), Some(2));
        assert_eq!(find_opener("a{b{"), None);
    }

    #[test]
    fn idl_properties_follow_owl() {
        assert_eq!(idl_property("input", "readonly"), Some("readOnly"));
        assert_eq!(idl_property("select", "value"), Some("value"));
        assert_eq!(idl_property("div", "value"), None);
        assert_eq!(idl_property("button", "disabled"), Some("disabled"));
    }

    fn analyze_with(xml: &str, limits: CompilerLimits) -> TemplateAnalysis {
        let origin = Origin::AddonFile {
            addon: AddonName::new("web").unwrap(),
            path: crate::names::TemplatePath::new("x.xml").unwrap(),
        };
        let tops = crate::xml::parse_templates_file(xml, &limits, 0).unwrap();
        let lines = LineIndex::new(xml);
        let loc = LocCtx {
            origin: &origin,
            text: xml,
            lines: Some(&lines),
        };
        let options = CompileOptions::new(BuildMode::Prod).with_limits(limits);
        let mut parser = ExprParser::new(&limits);
        let name = TemplateName::new("t").unwrap();
        let mut templates = BTreeSet::new();
        templates.insert(name.clone());
        analyze_template(
            &mut parser,
            &options,
            Trust::Trusted,
            &templates,
            &name,
            &tops[0].element,
            &loc,
        )
    }

    fn analyze_one(xml: &str) -> TemplateAnalysis {
        analyze_with(xml, CompilerLimits::DEFAULT)
    }

    fn kinds(xml: &str) -> Vec<DiagnosticKind> {
        analyze_one(xml)
            .diagnostics
            .iter()
            .map(Diagnostic::kind)
            .collect()
    }

    fn messages(xml: &str) -> Vec<String> {
        analyze_one(xml)
            .diagnostics
            .iter()
            .map(|d| d.message().to_owned())
            .collect()
    }

    #[test]
    fn folding_keeps_dynamic_bindings() {
        let a = analyze_one(
            "<templates><t t-name=\"t\"><div class=\"a\" t-att-class=\"'b'\" t-att-title=\"'x'\" t-att-hidden=\"false\" t-attf-id=\"i-{{'1'}}\" t-att-data-x=\"this.x\"/></t></templates>",
        );
        assert!(a.diagnostics.is_empty(), "{:?}", a.diagnostics);
        let Node::Element(div) = &a.ir.body[0] else {
            panic!()
        };
        assert_eq!(
            div.attrs,
            vec![
                ("class".into(), "a b".into()),
                ("title".into(), "x".into()),
                ("id".into(), "i-1".into()),
            ]
        );
        assert_eq!(div.bindings.len(), 1);
    }

    #[test]
    fn t_call_records_sites_and_bound_names() {
        let xml = "<templates><t t-name=\"t\"><t t-foreach=\"this.rows\" t-as=\"row\" t-key=\"row.id\"><t t-call=\"t\" extra=\"1\"><t t-set=\"inner\" t-value=\"2\"/></t></t></t></templates>";
        let a = analyze_one(xml);
        assert!(a.diagnostics.is_empty(), "{:?}", a.diagnostics);
        let site = &a.calls[0];
        let in_scope: Vec<&str> = site.in_scope.iter().map(|b| &**b).collect();
        assert_eq!(
            in_scope,
            ["row", "row_first", "row_index", "row_last", "row_value"]
        );
        let own: Vec<&str> = site.own.iter().map(|b| &**b).collect();
        assert_eq!(own, ["0", "extra", "inner"]);
        assert!(site.binds("row_index") && site.binds("inner") && !site.binds("other"));
        assert!(!site.binds_all);
    }

    /// Rust review M1: call sites in one scope share one snapshot of the names in scope; a new
    /// declaration makes a new one.
    #[test]
    fn call_sites_share_scope_snapshots() {
        let a = analyze_one(
            "<templates><t t-name=\"t\"><t t-set=\"a\" t-value=\"1\"/><t t-call=\"t\"/><t t-call=\"t\"/><t t-set=\"b\" t-value=\"2\"/><t t-call=\"t\"/></t></templates>",
        );
        assert!(a.diagnostics.is_empty(), "{:?}", a.diagnostics);
        assert!(Rc::ptr_eq(&a.calls[0].in_scope, &a.calls[1].in_scope));
        assert!(!Rc::ptr_eq(&a.calls[1].in_scope, &a.calls[2].in_scope));
        assert!(a.calls[2].binds("b") && !a.calls[0].binds("b"));
    }

    /// Rust review M1: the `locals` limit is checked once per declaration and reported once.
    #[test]
    fn locals_limit() {
        let limits = CompilerLimits::DEFAULT
            .lowered(LimitKind::Locals, 6)
            .unwrap();
        let xml = "<templates><t t-name=\"t\"><t t-set=\"a\" t-value=\"1\"/><t t-foreach=\"this.r\" t-as=\"x\" t-key=\"x\"><t t-set=\"b\" t-value=\"1\"/><t t-set=\"c\" t-value=\"1\"/></t></t></templates>";
        let a = analyze_with(xml, limits);
        let limit: Vec<_> = a.diagnostics.iter().filter_map(Diagnostic::limit).collect();
        assert_eq!(limit, [&LimitHit::new(LimitKind::Locals, 6, 7)]);
        assert_eq!(a.diagnostics.len(), 1, "{:?}", a.diagnostics);
        // `a`, the five `t-foreach` names of `x`, `b`, `c`: eight locals.
        let ok = CompilerLimits::DEFAULT
            .lowered(LimitKind::Locals, 8)
            .unwrap();
        let fits = analyze_with(xml, ok);
        assert!(fits.diagnostics.is_empty(), "{:?}", fits.diagnostics);
    }

    /// Rust review H2: dynamic t-call and slot names are checked against `expression_bytes`
    /// before they are split.
    #[test]
    fn dynamic_names_are_bounded() {
        let long = "{{a}}".repeat(1000);
        for xml in [
            format!("<templates><t t-name=\"t\"><t t-call=\"{long}\"/></t></templates>"),
            format!("<templates><t t-name=\"t\"><t t-slot=\"{long}\"/></t></templates>"),
            format!("<templates><t t-name=\"t\"><div t-attf-id=\"{long}\"/></t></templates>"),
        ] {
            let a = analyze_one(&xml);
            let hits: Vec<_> = a.diagnostics.iter().filter_map(Diagnostic::limit).collect();
            assert_eq!(
                hits,
                [&LimitHit::new(LimitKind::ExpressionBytes, 4096, 5000)],
                "{xml}"
            );
        }
    }

    /// Silent-failure review M-a: branch directives on the template root are errors.
    #[test]
    fn branches_on_the_root_are_errors() {
        assert_eq!(
            kinds("<templates><t t-name=\"t\" t-elif=\"this.a\"><div/></t></templates>"),
            [DiagnosticKind::BranchWithoutIf]
        );
        assert_eq!(
            kinds("<templates><t t-name=\"t\" t-else=\"\"><div/></t></templates>"),
            [DiagnosticKind::BranchWithoutIf]
        );
        assert_eq!(
            kinds("<templates><t t-name=\"t\" t-if=\"this.a\" t-else=\"\"><div/></t></templates>"),
            [DiagnosticKind::MultipleBranchDirectives]
        );
        assert!(
            kinds("<templates><t t-name=\"t\" t-if=\"this.a\"><div/></t></templates>").is_empty()
        );
    }

    /// Silent-failure review: the slot name of an ignored `t-set-slot` is still validated.
    #[test]
    fn ignored_set_slot_names_are_validated() {
        assert_eq!(
            kinds("<templates><t t-name=\"t\"><t t-set-slot=\"ok\">x</t></t></templates>"),
            [DiagnosticKind::IgnoredAttribute]
        );
        assert_eq!(
            kinds("<templates><t t-name=\"t\"><t t-set-slot=\"__proto__\">x</t></t></templates>"),
            [DiagnosticKind::InvalidName]
        );
    }

    /// Silent-failure review M-e: static text and `.translate` values mark the template.
    #[test]
    fn translatable_text_is_noticed() {
        assert!(
            analyze_one("<templates><t t-name=\"t\"><b>Hello</b></t></templates>").translatable
        );
        assert!(!analyze_one("<templates><t t-name=\"t\"><b>  </b></t></templates>").translatable);
        assert!(
            analyze_one("<templates><t t-name=\"t\"><Foo label.translate=\"Hi\"/></t></templates>")
                .translatable
        );
    }

    #[test]
    fn free_names_are_collected_not_reported() {
        let a = analyze_one(
            "<templates><t t-name=\"t\"><span t-out=\"label\"/><b t-if=\"label and other\"/></t></templates>",
        );
        assert!(a.diagnostics.is_empty());
        let free: Vec<&str> = a.free.keys().map(|k| &**k).collect();
        assert_eq!(free, ["label", "other"]);
        assert_eq!(a.free["label"].position().map(|p| p.line().get()), Some(1));
    }

    #[test]
    fn unknown_and_misplaced_directives() {
        assert_eq!(
            messages("<templates><t t-name=\"t\"><div t-foo=\"1\"/></t></templates>"),
            ["Unknown QWeb directive: 't-foo'"]
        );
        assert_eq!(
            messages("<templates><t t-name=\"t\"><div t-set=\"x\" t-value=\"1\"/></t></templates>"),
            ["directive 't-set' is not allowed on <div>"]
        );
        for marker in T_MARKERS {
            let a = analyze_one(&format!(
                "<templates><t t-name=\"t\"><t {marker}=\"1\"/></t></templates>"
            ));
            assert_eq!(a.diagnostics.len(), 1);
            assert_eq!(a.diagnostics[0].severity(), Severity::Warning);
            assert_eq!(a.diagnostics[0].kind(), DiagnosticKind::IgnoredAttribute);
            assert_eq!(
                a.diagnostics[0].message(),
                format!(
                    "attribute '{marker}' on <t> is ignored: a <t> node renders no element to carry it"
                )
            );
        }
    }

    /// `tests/spec_cases.rs`)
    ///
    /// Parity: divergent — bug fix BUG-065 (silent-failure review M-c).
    #[test]
    fn plain_attributes_on_t_are_errors() {
        let a = analyze_one(
            "<templates><t t-name=\"t\"><t t-out=\"this.a\" class=\"me-1\"/><t t-if=\"this.b\" style=\"x\">y</t></t></templates>",
        );
        let found: Vec<(Severity, DiagnosticKind, &str)> = a
            .diagnostics
            .iter()
            .map(|d| (d.severity(), d.kind(), d.message()))
            .collect();
        assert_eq!(found.len(), 2, "{found:?}");
        assert!(found.iter().all(
            |(s, k, _)| *s == Severity::Error && *k == DiagnosticKind::DirectiveNotAllowedHere
        ));
        assert!(
            found[0]
                .2
                .starts_with("attribute 'class' is not allowed on a <t t-out> node")
        );
        assert!(
            found[1]
                .2
                .starts_with("attribute 'style' is not allowed on <t>")
        );
    }
}
