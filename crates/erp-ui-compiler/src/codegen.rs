//! Code generation for the benchmark subset (design note §7): one ES module per template that
//! clones static HTML once per block, walks to the dynamic positions and binds them through the
//! runtime helpers of §7.2 (the contract frozen's `abi.d.ts`).
//!
//! Static HTML is serialised here from the IR, so it re-parses to the same tree: markup the HTML
//! parser would restructure is an error (`HtmlStructure`), never silently different DOM. The
//! unit tests check that claim against a real HTML5 parser (html5ever).

use crate::analyze::{LocCtx, UI2_DONE};
use crate::diag::{Diagnostic, DiagnosticKind};
use crate::expr::{Expr, GLOBALS, LocalId, Names, print};
use crate::ir::{
    AttrValue, Binding, Component, Element, InterpPart, LocalInfo, LoopField, Node, Pos, SlotIr,
    TemplateIr,
};
use crate::jsstr::push_js_string;
use crate::limits::{LimitHit, LimitKind};
use crate::names::is_plain_identifier;
use crate::source::BuildMode;
use crate::sourcemap::SourceMap;
use std::cell::Cell;
use std::fmt::Write as _;

/// The module specifier of the runtime helper ABI as a literal, so the header below can be built at
/// compile time; [`ABI_MODULE`] is the same text.
macro_rules! abi_module {
    () => {
        "@erpforall/elang/abi"
    };
}

/// The module every generated module imports the 17 helpers from (the runtime package's `./abi`
/// export, naming note). Changing it changes only this import string; the helper list,
/// order and signatures stay frozen.
pub const ABI_MODULE: &str = abi_module!();

/// The module header every generated module starts with.
pub(crate) const MODULE_HEADER: &str = concat!(
    "/*! SPDX-License-Identifier: MIT OR Apache-2.0 */\nimport * as $ from \"",
    abi_module!(),
    "\";\n"
);

/// The members of the ABI namespace (design note §7.2, frozen's `abi.d.ts`):
/// the only `$.<member>` names a module may use.
pub(crate) const ABI: [&str; 17] = [
    "template",
    "effect",
    "insert",
    "out",
    "attr",
    "cls",
    "style",
    "prop",
    "attrs",
    "interp",
    "on",
    "callHandler",
    "cond",
    "list",
    "keyed",
    "component",
    "slot",
];

/// SVG elements a template may use inside `<svg>`: shapes, text, structure, paint servers. A
/// closed list: anything else inside SVG is refused, so nothing reaches an HTML integration point
/// or a raw-text element (security review S1, S6).
const SVG_ELEMENTS: [&str; 22] = [
    "svg",
    "g",
    "defs",
    "symbol",
    "use",
    "path",
    "rect",
    "circle",
    "ellipse",
    "line",
    "polyline",
    "polygon",
    "text",
    "tspan",
    "image",
    "clipPath",
    "mask",
    "pattern",
    "marker",
    "linearGradient",
    "radialGradient",
    "stop",
];

/// SVG elements refused with their reason (the rest of the SVG vocabulary is refused as "not in
/// the supported list").
const SVG_REFUSED: [(&str, &str); 10] = [
    (
        "style",
        "its text is read as raw text, outside the escaping of the static HTML",
    ),
    ("script", "scripts are never part of a template"),
    (
        "foreignObject",
        "its content is HTML inside SVG (an HTML integration point)",
    ),
    ("title", "it is an HTML integration point"),
    ("desc", "it is an HTML integration point"),
    (
        "a",
        "its href is a navigation target the compiler does not check",
    ),
    (
        "animate",
        "an animation can change any attribute, href included",
    ),
    ("set", "it can change any attribute, href included"),
    (
        "animateTransform",
        "animations are not supported in templates",
    ),
    ("animateMotion", "animations are not supported in templates"),
];

/// The longest `.nextSibling` chain a module walks before it names an anchor node: keeps every
/// generated expression shallow, whatever the number of siblings (security review S4).
pub(crate) const MAX_CHAIN: usize = 16;

const VOID: [&str; 13] = [
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "source", "track",
    "wbr",
];

/// The HTML elements a template may use (sorted): the ones whose parsing the structure check
/// models (round-two review M1). Any other name, unless it is a custom element name, is refused:
/// the HTML parser treats several of them specially (`body`, `html`, `head`, `frame`,
/// `frameset` are dropped and their children moved up, `image` becomes `img`, `param`,
/// `keygen`, `basefont`, `bgsound` are void, `nobr`, `rb`, `rt`, `rp`, `rtc` close open
/// elements, `object`, `applet`, `marquee` are scope boundaries, `base` rebases every URL).
const HTML_ELEMENTS: [&str; 114] = [
    "a",
    "abbr",
    "address",
    "area",
    "article",
    "aside",
    "audio",
    "b",
    "bdi",
    "bdo",
    "blockquote",
    "br",
    "button",
    "canvas",
    "caption",
    "center",
    "cite",
    "code",
    "col",
    "colgroup",
    "data",
    "datalist",
    "dd",
    "del",
    "details",
    "dfn",
    "dialog",
    "dir",
    "div",
    "dl",
    "dt",
    "em",
    "embed",
    "fieldset",
    "figcaption",
    "figure",
    "font",
    "footer",
    "form",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "header",
    "hgroup",
    "hr",
    "i",
    "iframe",
    "img",
    "input",
    "ins",
    "kbd",
    "label",
    "legend",
    "li",
    "link",
    "listing",
    "main",
    "map",
    "mark",
    "menu",
    "meta",
    "meter",
    "nav",
    "noembed",
    "noframes",
    "noscript",
    "ol",
    "optgroup",
    "option",
    "output",
    "p",
    "picture",
    "plaintext",
    "pre",
    "progress",
    "q",
    "ruby",
    "s",
    "samp",
    "script",
    "search",
    "section",
    "select",
    "slot",
    "small",
    "source",
    "span",
    "strong",
    "style",
    "sub",
    "summary",
    "sup",
    "svg",
    "table",
    "tbody",
    "td",
    "template",
    "textarea",
    "tfoot",
    "th",
    "thead",
    "time",
    "title",
    "tr",
    "track",
    "u",
    "ul",
    "var",
    "video",
    "wbr",
    "xmp",
];

/// Names the custom-element grammar reserves (SVG and MathML names with a hyphen).
const RESERVED_CUSTOM: [&str; 8] = [
    "annotation-xml",
    "color-profile",
    "font-face",
    "font-face-format",
    "font-face-name",
    "font-face-src",
    "font-face-uri",
    "missing-glyph",
];

/// A custom element name: starts with `a-z`, holds a `-`, and is not reserved. The element-name
/// grammar of the front end already limits the characters; the parser treats these as ordinary
/// elements.
fn is_custom_element(tag: &str) -> bool {
    tag.starts_with(|c: char| c.is_ascii_lowercase())
        && tag.contains('-')
        && !RESERVED_CUSTOM.contains(&tag)
}

/// Elements that close an open `<p>` (the HTML parser's "close a p element" start tags).
const CLOSES_P: [&str; 40] = [
    "address",
    "article",
    "aside",
    "blockquote",
    "center",
    "dd",
    "details",
    "dialog",
    "dir",
    "div",
    "dl",
    "dt",
    "fieldset",
    "figcaption",
    "figure",
    "footer",
    "form",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "header",
    "hgroup",
    "hr",
    "li",
    "listing",
    "main",
    "menu",
    "nav",
    "ol",
    "p",
    "pre",
    "search",
    "section",
    "summary",
    "table",
    "ul",
    "xmp",
];

fn has_caller_body(nodes: &[Node]) -> bool {
    nodes.iter().any(|n| match n {
        Node::CallerBody { .. } => true,
        Node::Element(el) => has_caller_body(&el.children),
        Node::If { branches, .. } => branches.iter().any(|(_, b)| has_caller_body(b)),
        Node::For { body, .. } | Node::Keyed { body, .. } => has_caller_body(body),
        Node::Set { body, .. } => body.as_ref().is_some_and(|b| has_caller_body(b)),
        Node::Call { body, .. } => body.as_ref().is_some_and(|b| has_caller_body(b)),
        Node::Portal { children, .. } => has_caller_body(children),
        Node::Tag { children, .. } => has_caller_body(children),
        Node::Component(c) => c.slots.iter().any(|s| has_caller_body(&s.body)),
        _ => false,
    })
}

fn has_t_call(nodes: &[Node]) -> bool {
    nodes.iter().any(|n| match n {
        Node::Call { .. } => true,
        Node::Element(el) => has_t_call(&el.children),
        Node::If { branches, .. } => branches.iter().any(|(_, b)| has_t_call(b)),
        Node::For { body, .. } | Node::Keyed { body, .. } => has_t_call(body),
        Node::Set { body, .. } => body.as_ref().is_some_and(|b| has_t_call(b)),
        Node::Portal { children, .. } => has_t_call(children),
        Node::Tag { children, .. } => has_t_call(children),
        Node::Component(c) => c.slots.iter().any(|s| has_t_call(&s.body)),
        _ => false,
    })
}

fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        None => String::new(),
        Some(f) => f.to_uppercase().collect::<String>() + chars.as_str(),
    }
}

/// Generates the module of one template, or the diagnostics explaining why it cannot be.
/// Generation stops as soon as the text written passes `max_module` bytes (the `module_bytes`
/// limit), so a huge template never builds a huge string.
pub(crate) fn generate(
    ir: &TemplateIr,
    loc: &LocCtx<'_>,
    limits: &crate::limits::CompilerLimits,
) -> Result<(String, SourceMap), Vec<Diagnostic>> {
    let max_module = limits.get(LimitKind::ModuleBytes);
    let mut g = Gen {
        ir,
        loc,
        limits,
        effects: 0,
        statics: String::new(),
        templates: 0,
        nodes: 0,
        errors: Vec::new(),
        key_of: None,
        internal: Cell::new(None),
        emitted: MODULE_HEADER.len(),
        precharged: 0,
        max_module,
        over: false,
    };
    let mut body = String::new();
    let root = g.block(&ir.body, &mut body, 1);
    if g.over {
        let hit = LimitHit::new(LimitKind::ModuleBytes, max_module, g.emitted);
        return Err(vec![
            Diagnostic::limit_hit(hit)
                .with_message(format!(
                    "{} (generation stopped once the module passed the limit)",
                    hit.message()
                ))
                .with_template(Some(&ir.name))
                .with_location(loc.at(0)),
        ]);
    }
    if let Some(what) = g.internal.get() {
        g.errors.push(
            Diagnostic::internal(format!("code generation reached {what}"))
                .with_template(Some(&ir.name))
                .with_location(loc.at(0)),
        );
    }
    if !g.errors.is_empty() {
        return Err(g.errors);
    }
    let mut out = String::with_capacity(MODULE_HEADER.len() + g.statics.len() + body.len() + 128);
    out.push_str(MODULE_HEADER);
    out.push_str(&g.statics);
    out.push_str("export const name = ");
    push_js_string(&mut out, ir.name.as_str());
    out.push_str(";\n");

    let needs_context = has_t_call(&ir.body) || has_caller_body(&ir.body) || !ir.free.is_empty();
    if needs_context {
        out.push_str("export function render($self, $c, $b, $d = 0) {\n");
        let _ = writeln!(
            out,
            "  if ($d > 64) {{ throw Error(\"t-call depth limit (64) exceeded: {}\"); }}",
            ir.name
        );
    } else {
        out.push_str("export function render($self) {\n");
    }

    out.push_str(&body);
    match root {
        Some(var) => {
            let _ = writeln!(out, "  return {var};");
        }
        None => out.push_str("  return null;\n"),
    }
    out.push_str("}\n");

    let source_path = format!("{}/{}", loc.origin.addon(), loc.origin.path());
    let mut source_map = SourceMap::new(format!("{}.js", ir.name));
    source_map.sources.push(source_path.clone());
    for (line_idx, _) in out.lines().enumerate() {
        let gen_line = (line_idx + 1) as u32;
        source_map.add_mapping(
            gen_line,
            0,
            1,
            0,
            0,
            loc.origin.addon().as_str(),
            loc.origin.path().as_str(),
            ir.name.as_str(),
        );
    }

    Ok((out, source_map))
}

/// A node of a block's static tree.
enum SNode<'i> {
    Element {
        el: &'i Element,
        children: Vec<SNode<'i>>,
    },
    Text(String),
    /// A dynamic position: a `<!>` comment, except when it is the last child of an element, where
    /// the helper appends (`marker` null, §7.2) and no comment node or walk is needed.
    Marker {
        node: &'i Node,
        trailing: bool,
    },
}

struct Gen<'i, 'l> {
    ir: &'i TemplateIr,
    loc: &'l LocCtx<'l>,
    limits: &'l crate::limits::CompilerLimits,
    effects: usize,
    /// Module-level `const $tN = $.template(…)` lines.
    statics: String,
    templates: u32,
    nodes: u32,
    errors: Vec<Diagnostic>,
    /// The loop whose key function is being generated.
    key_of: Option<u32>,
    /// Set when printing reached a name code generation cannot bind (an undeclared local or a
    /// free name, whose templates are refused before code generation): the module is not produced
    /// and the set fails with an internal error (rust review M2).
    internal: Cell<Option<&'static str>>,
    /// Bytes written so far, the `module_bytes` limit, and whether it was passed.
    emitted: usize,
    /// Bytes of nested block functions already charged by their own lines and about to be
    /// embedded in the next line: that line charges only the rest (round-two review L1).
    precharged: usize,
    max_module: usize,
    over: bool,
}

impl Names for Gen<'_, '_> {
    fn local(&self, id: LocalId, out: &mut String) {
        match self.ir.locals.get(id as usize) {
            Some(LocalInfo::Loop { row, field }) if self.key_of == Some(*row) => {
                let _ = match field {
                    LoopField::Item => write!(out, "$ki{row}"),
                    LoopField::Value => write!(out, "$kv{row}"),
                    LoopField::Index => write!(out, "$kx{row}"),
                    LoopField::First => write!(out, "($kx{row} === 0)"),
                    LoopField::Last => write!(out, "($kx{row} === $kc{row} - 1)"),
                };
            }
            Some(LocalInfo::Loop { row, field }) => {
                let _ = match field {
                    LoopField::Item => write!(out, "$r{row}.item()"),
                    LoopField::Value => write!(out, "$r{row}.value()"),
                    LoopField::Index => write!(out, "$r{row}.index()"),
                    LoopField::First => write!(out, "($r{row}.index() === 0)"),
                    LoopField::Last => write!(out, "($r{row}.index() === $r{row}.count() - 1)"),
                };
            }
            Some(LocalInfo::SlotScope { slot }) => {
                let _ = write!(out, "$s{slot}");
            }
            Some(LocalInfo::Set { id: set_id }) => {
                let _ = write!(out, "$v{set_id}");
            }
            None => {
                self.internal.set(Some("an undeclared local"));
                out.push_str("null");
            }
        }
    }

    fn free(&self, name: &str, out: &mut String) {
        let _ = write!(out, "($c ? $c.{name} : undefined)");
    }
}

impl<'i> Gen<'i, '_> {
    fn error(&mut self, kind: DiagnosticKind, directive: &str, pos: Pos, message: String) {
        self.errors.push(
            Diagnostic::error(kind, message)
                .with_directive(directive)
                .with_template(Some(&self.ir.name))
                .with_location(self.loc.at(pos.0))
                .with_source_offset(pos.0),
        );
    }

    /// Writes one indented line of code and charges it to the module size, less the nested
    /// block functions it embeds, whose lines were charged when they were written.
    fn line(&mut self, code: &mut String, depth: usize, args: std::fmt::Arguments<'_>) {
        let before = code.len();
        indent(code, depth);
        let _ = code.write_fmt(args);
        code.push('\n');
        let written = code.len() - before;
        let nested = std::mem::take(&mut self.precharged);
        self.charge(written.saturating_sub(nested));
    }

    fn charge(&mut self, bytes: usize) {
        self.emitted = self.emitted.saturating_add(bytes);
        if self.emitted > self.max_module {
            self.over = true;
        }
    }

    fn var(&mut self) -> String {
        let v = format!("$n{}", self.nodes);
        self.nodes += 1;
        v
    }

    fn expr(&self, e: &Expr) -> String {
        let mut s = String::new();
        print(e, self, &mut s);
        s
    }

    fn check_effect_limit(&mut self, pos: Pos) {
        self.effects += 1;
        let max_effects = self.limits.get(LimitKind::Effects);
        if self.effects > max_effects {
            self.errors.push(
                Diagnostic::over(LimitKind::Effects, max_effects, self.effects)
                    .with_template(Some(&self.ir.name))
                    .with_location(self.loc.at(pos.0))
                    .with_source_offset(pos.0),
            );
        }
    }

    fn emit_set(
        &mut self,
        local: LocalId,
        value: Option<&Expr>,
        body: Option<&'i [Node]>,
        _pos: Pos,
        code: &mut String,
        depth: usize,
    ) {
        match (value, body) {
            (Some(val), None) => {
                let v = self.expr(val);
                self.line(code, depth, format_args!("const $v{local} = {v};"));
            }
            (None, Some(body)) => {
                let body_fn = self.block_fn("", body, depth);
                self.line(code, depth, format_args!("const $v{local} = {body_fn};"));
            }
            (Some(val), Some(body)) => {
                let v = self.expr(val);
                let body_fn = self.block_fn("", body, depth);
                self.line(
                    code,
                    depth,
                    format_args!(
                        "const $v{local} = ({v} !== undefined && {v} !== null) ? {v} : ({body_fn})();"
                    ),
                );
            }
            (None, None) => {
                self.line(code, depth, format_args!("const $v{local} = null;"));
            }
        }
    }

    // --- static tree ---------------------------------------------------------------------------

    fn static_tree(&mut self, nodes: &'i [Node], out: &mut Vec<SNode<'i>>) {
        for n in nodes {
            match n {
                Node::Set { .. } => {}
                Node::Element(el) => {
                    let mut children = Vec::new();
                    self.static_tree(&el.children, &mut children);
                    if let Some(SNode::Marker { trailing, .. }) = children.last_mut() {
                        *trailing = true;
                    }
                    out.push(SNode::Element { el, children });
                }
                Node::Text(t) => match out.last_mut() {
                    Some(SNode::Text(prev)) => prev.push_str(t),
                    _ => out.push(SNode::Text(t.to_string())),
                },
                other => out.push(SNode::Marker {
                    node: other,
                    trailing: false,
                }),
            }
        }
    }

    /// Checks that the HTML parser builds exactly this tree from the serialised string.
    /// `parent_svg` tells whether the parent element is an SVG element.
    fn check_structure(
        &mut self,
        nodes: &[SNode<'i>],
        ancestors: &mut Vec<&'i str>,
        parent_svg: bool,
    ) {
        let parent = ancestors.last().copied();
        if parent.is_none() {
            self.check_block_top(nodes);
        }
        for n in nodes {
            match n {
                SNode::Text(t) => {
                    if let Some(p) = parent
                        && matches!(
                            p,
                            "table" | "thead" | "tbody" | "tfoot" | "tr" | "colgroup" | "select"
                        )
                        && !t.chars().all(crate::xml::is_xml_space)
                    {
                        self.structure(
                            None,
                            format!(
                                "text directly inside <{p}> is moved out of it by the HTML parser"
                            ),
                        );
                    }
                }
                SNode::Marker { .. } => {}
                SNode::Element { el, children } => {
                    let tag: &'i str = &el.tag;
                    // The HTML parser compares attribute names case-insensitively and drops the
                    // second of two that differ only in case (XML keeps both).
                    for (i, (name, _)) in el.attrs.iter().enumerate() {
                        if el.attrs[..i]
                            .iter()
                            .any(|(n, _)| n.eq_ignore_ascii_case(name))
                        {
                            self.structure(
                                Some(el.pos),
                                format!("<{tag}> has two attributes named '{name}' when case is ignored: the HTML parser drops the second"),
                            );
                        }
                    }
                    if el.svg {
                        // The `<svg>` root itself sits in HTML: the content rules of its parent
                        // (a table, a select, an option, the top of a block) apply to it.
                        if !parent_svg {
                            self.check_html_element(el, tag, children, ancestors);
                        }
                        self.check_svg_element(el, tag);
                    } else {
                        if matches!(tag, "g" | "path") {
                            self.error(
                                DiagnosticKind::CodegenNotSupportedUntil,
                                "element",
                                el.pos,
                                format!("<{tag}> outside <svg> (OWL creates it in the SVG namespace) code generation is not supported until {UI2_DONE}"),
                            );
                        }
                        self.check_html_element(el, tag, children, ancestors);
                    }
                    ancestors.push(tag);
                    self.check_structure(children, ancestors, el.svg);
                    ancestors.pop();
                }
            }
        }
    }

    /// The top level of a block, parsed as `<template>.innerHTML`: the first start tag picks the
    /// parser's mode for the rest of the level. After a table part (`caption`, `colgroup`, `col`,
    /// `thead`/`tbody`/`tfoot`, `tr`, `td`/`th`) only parts of the same group follow, and text is
    /// moved out; after any other element a table part is dropped. So a level holding a table part
    /// holds only elements of that part's group, markers and whitespace.
    fn check_block_top(&mut self, nodes: &[SNode<'i>]) {
        fn group(tag: &str) -> Option<&'static str> {
            match tag {
                "caption" | "colgroup" | "thead" | "tbody" | "tfoot" => {
                    Some("caption, colgroup, thead, tbody, tfoot")
                }
                "col" => Some("col"),
                "tr" => Some("tr"),
                "td" | "th" => Some("td, th"),
                _ => None,
            }
        }
        let elements: Vec<&Element> = nodes
            .iter()
            .filter_map(|n| match n {
                SNode::Element { el, .. } => Some(*el),
                _ => None,
            })
            .collect();
        let Some(first) = elements.first() else {
            return;
        };
        if !elements.iter().any(|el| group(&el.tag).is_some()) {
            return;
        }
        let first_group = group(&first.tag);
        for el in &elements[1..] {
            let tag = &*el.tag;
            match first_group {
                Some(g) if group(tag) == Some(g) => {}
                Some(g) => self.structure(
                    Some(el.pos),
                    format!(
                        "<{tag}> next to <{}> at the top of a block: after a table part the HTML parser keeps only parts of its group ({g})",
                        first.tag
                    ),
                ),
                None if group(tag).is_some() => self.structure(
                    Some(el.pos),
                    format!(
                        "<{tag}> after <{}> at the top of a block is dropped by the HTML parser outside a table",
                        first.tag
                    ),
                ),
                None => {}
            }
        }
        if first_group.is_some()
            && nodes
                .iter()
                .any(|n| matches!(n, SNode::Text(t) if !t.chars().all(crate::xml::is_xml_space)))
        {
            self.structure(
                Some(first.pos),
                format!(
                    "text next to <{}> at the top of a block is moved out by the HTML parser",
                    first.tag
                ),
            );
        }
    }

    /// An element inside `<svg>`: one of [`SVG_ELEMENTS`], so no HTML integration point, raw-text
    /// element or breakout start tag is ever reached inside foreign content.
    fn check_svg_element(&mut self, el: &Element, tag: &str) {
        if SVG_ELEMENTS.contains(&tag) {
            return;
        }
        let message = match SVG_REFUSED.iter().find(|(name, _)| *name == tag) {
            Some((_, why)) => format!("<{tag}> inside <svg> is not allowed: {why}"),
            None => format!(
                "<{tag}> inside <svg> is not supported (supported SVG elements: {}); an HTML element inside SVG is moved out of it by the HTML parser",
                SVG_ELEMENTS.join(", ")
            ),
        };
        self.structure(Some(el.pos), message);
    }

    fn structure(&mut self, pos: Option<Pos>, message: String) {
        let pos = pos.unwrap_or(Pos(0));
        self.error(DiagnosticKind::HtmlStructure, "element", pos, message);
    }

    fn check_html_element(
        &mut self,
        el: &Element,
        tag: &str,
        children: &[SNode<'_>],
        ancestors: &[&str],
    ) {
        let pos = Some(el.pos);
        let parent = ancestors.last().copied();
        // The HTML parser lowercases tag names, and every rule below compares lowercase names:
        // a mixed-case name (`noScript`, `sTyle`) would pass the rules and then be parsed as the
        // raw-text element it spells (security review S1).
        if tag.bytes().any(|b| b.is_ascii_uppercase()) {
            self.structure(
                pos,
                format!(
                    "<{tag}>: HTML element names are lowercase in templates (the HTML parser reads it as <{}>)",
                    tag.to_ascii_lowercase()
                ),
            );
            return;
        }
        if tag == "math" {
            self.structure(
                pos,
                "<math> (MathML) is not supported: the HTML parser switches to the MathML namespace inside it".to_owned(),
            );
            return;
        }
        if HTML_ELEMENTS.binary_search(&tag).is_err() && !is_custom_element(tag) {
            // `g` and `path` outside `<svg>` already have their own error (OWL makes them SVG).
            if !matches!(tag, "g" | "path") {
                let why = match tag {
                    "body" | "html" | "head" | "frame" | "frameset" => {
                        "the HTML parser drops it inside a template and moves its children up"
                    }
                    "image" => "the HTML parser reads it as <img>",
                    "param" | "keygen" | "basefont" | "bgsound" => {
                        "the HTML parser treats it as a void element"
                    }
                    "nobr" | "rb" | "rt" | "rp" | "rtc" => {
                        "the HTML parser closes open elements at it"
                    }
                    "object" | "applet" | "marquee" => {
                        "the HTML parser treats it as a scope boundary"
                    }
                    "base" => "it changes every relative URL of the page",
                    _ => {
                        "it is not an HTML element the compiler models (custom elements need a '-')"
                    }
                };
                self.structure(pos, format!("<{tag}> is not supported in templates: {why}"));
            }
            return;
        }
        if VOID.contains(&tag) && !children.is_empty() {
            self.structure(
                pos,
                format!("<{tag}> is a void element and cannot have content"),
            );
        }
        match tag {
            "script" => self.structure(pos, "<script> is not allowed in templates".to_owned()),
            "template" | "plaintext" => {
                self.structure(pos, format!("<{tag}> is not allowed in templates"))
            }
            "iframe" | "noscript" | "xmp" | "noembed" | "noframes" | "textarea" | "title"
            | "style" => {
                let only_text = children.iter().all(|c| matches!(c, SNode::Text(_)));
                let raw = matches!(
                    tag,
                    "iframe" | "noscript" | "xmp" | "noembed" | "noframes" | "style"
                );
                if !only_text {
                    self.structure(pos, format!("<{tag}> can only hold static text (the HTML parser reads its content as text)"));
                } else if raw && tag != "style" && !children.is_empty() {
                    self.structure(pos, format!("<{tag}> cannot have content in a template"));
                } else if tag == "style"
                    && children.iter().any(|c| matches!(c, SNode::Text(t) if t.to_ascii_lowercase().contains("</style") || t.contains('\r')))
                {
                    self.structure(
                        pos,
                        "<style> text cannot contain </style or a carriage return (raw text is not escaped)".to_owned(),
                    );
                }
            }
            _ => {}
        }
        // The parser's "has an element in scope": the default scope stops at these elements
        // (SVG and MathML integration points never hold HTML here, see `check_svg_element`);
        // the button scope stops at `button` too.
        let in_scope = |name: &str, button_scope: bool| {
            ancestors
                .iter()
                .rev()
                .take_while(|a| {
                    !matches!(
                        **a,
                        "table"
                            | "td"
                            | "th"
                            | "caption"
                            | "object"
                            | "marquee"
                            | "applet"
                            | "template"
                    ) && !(button_scope && **a == "button")
                })
                .any(|a| *a == name)
        };
        if CLOSES_P.contains(&tag) && in_scope("p", true) {
            self.structure(
                pos,
                format!("<{tag}> inside <p>: the HTML parser closes the <p> first"),
            );
        }
        if tag == "a" && ancestors.contains(&"a") {
            self.structure(
                pos,
                "<a> inside <a>: the HTML parser closes the outer one".to_owned(),
            );
        }
        if tag == "form" && ancestors.contains(&"form") {
            self.structure(
                pos,
                "<form> inside <form> is dropped by the HTML parser".to_owned(),
            );
        }
        if tag == "button" && in_scope("button", false) {
            self.structure(
                pos,
                "<button> inside <button>: the HTML parser closes the outer one".to_owned(),
            );
        }
        if matches!(tag, "h1" | "h2" | "h3" | "h4" | "h5" | "h6")
            && matches!(parent, Some("h1" | "h2" | "h3" | "h4" | "h5" | "h6"))
        {
            self.structure(
                pos,
                format!("<{tag}> directly inside a heading: the HTML parser closes the heading"),
            );
        }
        if tag == "li"
            && ancestors
                .iter()
                .rev()
                .take_while(|a| !matches!(**a, "ul" | "ol" | "menu"))
                .any(|a| *a == "li")
        {
            self.structure(pos, "<li> inside <li> without a list between them: the HTML parser closes the outer one".to_owned());
        }
        if matches!(tag, "dd" | "dt")
            && ancestors
                .iter()
                .rev()
                .take_while(|a| **a != "dl")
                .any(|a| matches!(*a, "dd" | "dt"))
        {
            self.structure(
                pos,
                format!("<{tag}> inside <dd>/<dt>: the HTML parser closes the outer one"),
            );
        }
        // An `<option>` or `<optgroup>` start tag closes an open `<option>`, and inside a
        // `<select>` the parsers differ on other elements in an option (the older rules drop
        // them, the customizable-select rules keep them): an option holds text only.
        if parent == Some("option") {
            self.structure(
                pos,
                format!("<{tag}> inside <option>: an option holds text only (the HTML parser closes, drops or moves elements in it)"),
            );
        }
        let allowed: Option<&[&str]> = match parent {
            Some("table") => Some(&["caption", "colgroup", "thead", "tbody", "tfoot"]),
            Some("thead" | "tbody" | "tfoot") => Some(&["tr"]),
            Some("tr") => Some(&["td", "th"]),
            Some("colgroup") => Some(&["col"]),
            Some("select") => Some(&["option", "optgroup", "hr"]),
            Some("optgroup") => Some(&["option"]),
            _ => None,
        };
        if let (Some(allowed), Some(p)) = (allowed, parent)
            && !allowed.contains(&tag)
        {
            self.structure(pos, format!("<{tag}> directly inside <{p}>: the HTML parser moves or wraps it (allowed: {})", allowed.join(", ")));
        }
        if matches!(
            tag,
            "tr" | "td" | "th" | "thead" | "tbody" | "tfoot" | "caption" | "colgroup" | "col"
        ) && let Some(p) = parent
            && !matches!(p, "table" | "thead" | "tbody" | "tfoot" | "tr" | "colgroup")
        {
            self.structure(
                pos,
                format!("<{tag}> inside <{p}> is dropped by the HTML parser outside a table"),
            );
        }
    }

    fn serialize(nodes: &[SNode<'_>], html: &mut String, parent: Option<&Element>) {
        for n in nodes {
            match n {
                SNode::Text(t) => {
                    // HTML <style> is raw text (checked to hold no </style and no CR); SVG
                    // <style> is refused by `check_svg_element`.
                    if parent.is_some_and(|p| !p.svg && &*p.tag == "style") {
                        html.push_str(t);
                    } else {
                        escape_text(t, html);
                    }
                }
                SNode::Marker { trailing, .. } => {
                    if !trailing {
                        html.push_str("<!>");
                    }
                }
                SNode::Element { el, children } => {
                    let tag = &*el.tag;
                    html.push('<');
                    html.push_str(tag);
                    for (name, value) in &el.attrs {
                        html.push(' ');
                        html.push_str(name);
                        html.push_str("=\"");
                        escape_attr(value, html);
                        html.push('"');
                    }
                    html.push('>');
                    if !el.svg && VOID.contains(&tag) {
                        continue;
                    }
                    // The parser drops one leading newline in these elements.
                    if !el.svg
                        && matches!(tag, "pre" | "textarea" | "listing")
                        && matches!(children.first(), Some(SNode::Text(t)) if t.starts_with('\n'))
                    {
                        html.push('\n');
                    }
                    Self::serialize(children, html, Some(el));
                    html.push_str("</");
                    html.push_str(tag);
                    html.push('>');
                }
            }
        }
    }

    // --- blocks --------------------------------------------------------------------------------

    /// Emits the code building `nodes` into `code` (indented by `depth`) and returns the variable
    /// holding the built node, or `None` for an empty block.
    fn block(&mut self, nodes: &'i [Node], code: &mut String, depth: usize) -> Option<String> {
        for n in nodes {
            if let Node::Set {
                local,
                value,
                body,
                pos,
                ..
            } = n
            {
                self.emit_set(*local, value.as_ref(), body.as_deref(), *pos, code, depth);
            }
        }
        let mut tree = Vec::new();
        self.static_tree(nodes, &mut tree);
        if tree.is_empty() {
            return None;
        }
        self.check_structure(&tree, &mut Vec::new(), false);
        let mut html = String::new();
        Self::serialize(&tree, &mut html, None);
        let multi = !(tree.len() == 1 && matches!(tree[0], SNode::Element { .. }));
        let t = self.templates;
        self.templates += 1;
        let before = self.statics.len();
        let _ = write!(self.statics, "const $t{t} = $.template(");
        push_js_string(&mut self.statics, &html);
        let _ = writeln!(self.statics, ", {});", u8::from(multi));
        self.charge(self.statics.len() - before);
        if self.over {
            return None;
        }
        let root = self.var();
        self.line(code, depth, format_args!("const {root} = $t{t}();"));
        if multi {
            self.walk_children(&tree, &root, code, depth);
        } else if let SNode::Element { el, children } = &tree[0] {
            self.bind_element(el, &root, code, depth);
            self.walk_children(children, &root, code, depth);
        }
        Some(root)
    }

    /// Whether a static subtree holds anything that needs a reference.
    fn needs_ref(n: &SNode<'_>) -> bool {
        match n {
            SNode::Text(_) => false,
            SNode::Marker { .. } => true,
            SNode::Element { el, children } => {
                !el.bindings.is_empty() || children.iter().any(Self::needs_ref)
            }
        }
    }

    fn walk_children(
        &mut self,
        children: &[SNode<'i>],
        parent: &str,
        code: &mut String,
        depth: usize,
    ) {
        // The last materialised sibling and how many siblings after it we have passed.
        let mut anchor: Option<(String, usize)> = None;
        for (i, child) in children.iter().enumerate() {
            if self.over {
                return;
            }
            if !Self::needs_ref(child) {
                if let Some((_, gap)) = &mut anchor {
                    *gap += 1;
                }
                continue;
            }
            if let SNode::Marker {
                node,
                trailing: true,
            } = child
            {
                // The last child of an element: appended, nothing to walk to.
                self.dynamic(node, parent, "null", code, depth);
                continue;
            }
            // The walk from the anchor (or the parent's first child), in hops of at most
            // MAX_CHAIN siblings, each hop a named node.
            let (mut base, mut steps) = match &anchor {
                None => (format!("{parent}.firstChild"), i),
                Some((prev, gap)) => (prev.clone(), *gap + 1),
            };
            while steps > MAX_CHAIN {
                if self.over {
                    return;
                }
                let hop = self.var();
                let chain = ".nextSibling".repeat(MAX_CHAIN);
                self.line(code, depth, format_args!("const {hop} = {base}{chain};"));
                base = hop;
                steps -= MAX_CHAIN;
            }
            let var = self.var();
            let chain = ".nextSibling".repeat(steps);
            self.line(code, depth, format_args!("const {var} = {base}{chain};"));
            match child {
                SNode::Marker { node, .. } => self.dynamic(node, parent, &var, code, depth),
                SNode::Element { el, children } => {
                    self.bind_element(el, &var, code, depth);
                    self.walk_children(children, &var, code, depth);
                }
                SNode::Text(_) => {}
            }
            anchor = Some((var, 0));
        }
    }

    fn attr_value(&self, v: &AttrValue) -> String {
        match v {
            AttrValue::Expr(e) => self.expr(e),
            AttrValue::Interp(parts) => {
                let mut s = String::from("$.interp(");
                for (i, p) in parts.iter().enumerate() {
                    if i > 0 {
                        s.push_str(", ");
                    }
                    match p {
                        InterpPart::Lit(l) => push_js_string(&mut s, l),
                        InterpPart::Expr(e) => print(e, self, &mut s),
                    }
                }
                s.push(')');
                s
            }
        }
    }

    fn bind_element(&mut self, el: &Element, var: &str, code: &mut String, depth: usize) {
        for b in &el.bindings {
            match b {
                Binding::Attr { name, value } => {
                    self.check_effect_limit(el.pos);
                    let mut n = String::new();
                    push_js_string(&mut n, name);
                    let value = self.attr_value(value);
                    self.line(
                        code,
                        depth,
                        format_args!("$.effect(() => $.attr({var}, {n}, {value}));"),
                    );
                }
                Binding::Class(value) => {
                    self.check_effect_limit(el.pos);
                    let value = self.attr_value(value);
                    self.line(
                        code,
                        depth,
                        format_args!("$.effect(() => $.cls({var}, {value}));"),
                    );
                }
                Binding::Style(value) => {
                    self.check_effect_limit(el.pos);
                    let value = self.attr_value(value);
                    self.line(
                        code,
                        depth,
                        format_args!("$.effect(() => $.style({var}, {value}));"),
                    );
                }
                Binding::Prop { name, value } => {
                    self.check_effect_limit(el.pos);
                    let mut n = String::new();
                    push_js_string(&mut n, name);
                    let value = self.attr_value(value);
                    self.line(
                        code,
                        depth,
                        format_args!("$.effect(() => $.prop({var}, {n}, {value}));"),
                    );
                }
                Binding::Attrs(e) => {
                    self.check_effect_limit(el.pos);
                    let value = self.expr(e);
                    self.line(
                        code,
                        depth,
                        format_args!("$.effect(() => $.attrs({var}, {value}));"),
                    );
                }
                Binding::On {
                    event,
                    flags,
                    handler,
                } => {
                    let mut n = String::new();
                    push_js_string(&mut n, event);
                    let handler = match handler {
                        None => "() => {}".to_owned(),
                        Some(e @ Expr::Arrow(..)) => self.expr(e),
                        Some(e) => format!("($e) => $.callHandler({}, $self, $e)", self.expr(e)),
                    };
                    self.line(
                        code,
                        depth,
                        format_args!("$.on({var}, {n}, {handler}, {flags});"),
                    );
                }
                Binding::Ref(expr) => {
                    let target = self.expr(expr);
                    self.check_effect_limit(el.pos);
                    self.line(
                        code,
                        depth,
                        format_args!(
                            "$.effect(() => {{ const $r = {target}; if ($r && typeof $r.set === \"function\") $r.set({var}); else if ($r && typeof $r === \"object\") $r.el = {var}; }});"
                        ),
                    );
                }
                Binding::Model {
                    target,
                    trim,
                    number,
                    lazy,
                    proxy: _,
                    tag,
                    input_type,
                } => {
                    let target_code = self.expr(target);
                    let (prop_name, read_val, event_name) =
                        if input_type.as_deref() == Some("checkbox") {
                            ("\"checked\"", "$e.target.checked".to_owned(), "\"click\"")
                        } else {
                            let read = if *number {
                                "parseFloat($e.target.value) || 0"
                            } else if *trim {
                                "$e.target.value.trim()"
                            } else {
                                "$e.target.value"
                            };
                            let ev = if *lazy || tag.as_ref() == "select" {
                                "\"change\""
                            } else {
                                "\"input\""
                            };
                            ("\"value\"", read.to_owned(), ev)
                        };
                    self.check_effect_limit(el.pos);
                    self.line(
                        code,
                        depth,
                        format_args!(
                            "$.effect(() => $.prop({var}, {prop_name}, typeof {target_code} === \"function\" ? {target_code}() : {target_code}));"
                        ),
                    );
                    self.line(
                        code,
                        depth,
                        format_args!(
                            "$.on({var}, {event_name}, ($e) => {{ const $v = {read_val}; if (typeof {target_code} === \"function\" && \"set\" in {target_code}) {target_code}.set($v); else if (typeof {target_code} === \"function\") {target_code}($v); else {target_code} = $v; }}, 0);"
                        ),
                    );
                }
            }
        }
    }

    /// A function expression `() => { …; return $nX; }` building `nodes`, or `() => null`.
    fn block_fn(&mut self, params: &str, nodes: &'i [Node], depth: usize) -> String {
        let mut inner = String::new();
        // The lines of `inner` are charged by `block`; the line embedding this function charges
        // the rest of it.
        let outer_pending = std::mem::take(&mut self.precharged);
        let built = self.block(nodes, &mut inner, depth + 1);
        self.precharged = outer_pending + inner.len();
        match built {
            None => format!("({params}) => null"),
            Some(root) => {
                let mut s = format!("({params}) => {{\n");
                s.push_str(&inner);
                indent(&mut s, depth + 1);
                let _ = writeln!(s, "return {root};");
                indent(&mut s, depth);
                s.push('}');
                s
            }
        }
    }

    fn dynamic(
        &mut self,
        node: &'i Node,
        parent: &str,
        marker: &str,
        code: &mut String,
        depth: usize,
    ) {
        match node {
            Node::Out { expr, body, .. } => {
                let value = self.expr(expr);
                match body {
                    None => {
                        self.line(
                            code,
                            depth,
                            format_args!("$.insert({parent}, () => {value}, {marker});"),
                        );
                    }
                    Some(body) => {
                        let fallback = self.block_fn("", body, depth);
                        self.line(
                            code,
                            depth,
                            format_args!("$.out({parent}, () => {value}, {marker}, {fallback});"),
                        );
                    }
                }
            }
            Node::If { branches, .. } => {
                // A flat list of `if` statements, not nested conditionals: a chain of any length
                // stays one level deep in the module (security review S4).
                let mut select = String::from("{");
                let mut has_else = false;
                for (i, (cond, _)) in branches.iter().enumerate() {
                    match cond {
                        Some(c) => {
                            select.push_str(" if (");
                            print(c, self, &mut select);
                            let _ = write!(select, ") return {i};");
                        }
                        None => {
                            let _ = write!(select, " return {i};");
                            has_else = true;
                        }
                    }
                }
                if !has_else {
                    select.push_str(" return -1;");
                }
                select.push_str(" }");
                let mut fns = Vec::with_capacity(branches.len());
                for (_, body) in branches {
                    fns.push(self.block_fn("", body, depth));
                }
                let fns = fns.join(", ");
                self.line(
                    code,
                    depth,
                    format_args!("$.cond({parent}, {marker}, () => {select}, [{fns}]);"),
                );
            }
            Node::For {
                row,
                collection,
                key,
                body,
                ..
            } => {
                let items = self.expr(collection);
                let saved = self.key_of.replace(*row);
                let key_code = self.expr(key);
                self.key_of = saved;
                let row_fn = self.block_fn(&format!("$r{row}"), body, depth);
                self.line(
                    code,
                    depth,
                    format_args!(
                        "$.list({parent}, {marker}, () => {items}, ($ki{row}, $kv{row}, $kx{row}, $kc{row}) => {key_code}, {row_fn});"
                    ),
                );
            }
            Node::Keyed { key, body, .. } => {
                let key_code = self.expr(key);
                let build = self.block_fn("", body, depth);
                self.line(
                    code,
                    depth,
                    format_args!("$.keyed({parent}, {marker}, () => {key_code}, {build});"),
                );
            }
            Node::Call {
                target,
                context,
                params,
                body,
                ..
            } => {
                let target_code = match target {
                    crate::ir::CallTarget::Static(name) => {
                        let mut s = String::new();
                        push_js_string(&mut s, name.as_str());
                        s
                    }
                    crate::ir::CallTarget::Dynamic(e) => self.expr(e),
                };
                let mut params_obj = String::from("{");
                for (i, p) in params.iter().enumerate() {
                    if i > 0 {
                        params_obj.push_str(", ");
                    }
                    push_js_string(&mut params_obj, &p.name);
                    let _ = write!(params_obj, ": {}", self.expr(&p.value));
                }
                params_obj.push('}');
                let (next_self, next_ctx) = match context {
                    Some(c) => {
                        let c_code = self.expr(c);
                        (
                            c_code.clone(),
                            format!(
                                "Object.assign({{}}, {c_code}, {{this: {c_code}}}, {params_obj})"
                            ),
                        )
                    }
                    None => (
                        "$self".to_owned(),
                        format!("Object.assign(Object.create($c || null), {params_obj})"),
                    ),
                };
                let caller_body_fn = match body {
                    Some(b) => self.block_fn("$self, $c", b, depth),
                    None => "null".to_owned(),
                };
                self.line(
                    code,
                    depth,
                    format_args!(
                        "$.keyed({parent}, {marker}, () => {target_code}, () => {{ const $target = {target_code}; const $fn = ($c && $c.$call) ? $c.$call : ($self && $self.template ? ($target, $s, $c, $b, $d) => $self.template($target)($s, $c, $b, $d) : null); if (!$fn) return null; return $fn($target, {next_self}, {next_ctx}, {caller_body_fn}, $d + 1); }});"
                    ),
                );
            }
            Node::CallerBody { .. } => {
                self.line(
                    code,
                    depth,
                    format_args!(
                        "$.out({parent}, () => ($b ? $b($self, $c) : null), {marker}, () => null);"
                    ),
                );
            }
            Node::Portal {
                target, children, ..
            } => {
                let target_code = self.expr(target);
                let build_fn = self.block_fn("", children, depth);
                self.line(
                    code,
                    depth,
                    format_args!(
                        "$.keyed({parent}, {marker}, () => {target_code}, () => {{ const $target = {target_code}; const $p = typeof $target === \"string\" ? document.querySelector($target) : $target; if ($p) {{ const $el = ({build_fn})(); if ($el) $p.appendChild($el); }} return null; }});"
                    ),
                );
            }
            Node::Tag { tag, children, .. } => {
                let tag_code = self.expr(tag);
                let build_fn = self.block_fn("", children, depth);
                self.line(
                    code,
                    depth,
                    format_args!(
                        "$.keyed({parent}, {marker}, () => {tag_code}, () => {{ const $el = document.createElement({tag_code}); const $c = ({build_fn})(); if ($c) $el.appendChild($c); return $el; }});"
                    ),
                );
            }
            Node::Component(c) => self.component(c, parent, marker, code, depth),
            Node::Slot {
                name,
                scope,
                fallback,
                ..
            } => {
                let n = match name {
                    crate::ir::SlotTarget::Static(s) => {
                        let mut str_val = String::new();
                        push_js_string(&mut str_val, s);
                        str_val
                    }
                    crate::ir::SlotTarget::Dynamic(e) => self.expr(e),
                };
                let scope = scope
                    .as_ref()
                    .map_or_else(|| "null".to_owned(), |s| format!("() => {}", self.expr(s)));
                let fallback = if fallback.is_empty() {
                    "null".to_owned()
                } else {
                    self.block_fn("", fallback, depth)
                };
                self.line(
                    code,
                    depth,
                    format_args!("$.slot({parent}, {marker}, $self, {n}, {scope}, {fallback});"),
                );
            }
            Node::Deferred { what, task, pos } => {
                self.error(
                    DiagnosticKind::CodegenNotSupportedUntil,
                    what,
                    *pos,
                    format!("{what} code generation is not supported until {task}"),
                );
            }
            Node::Element(_) | Node::Text(_) | Node::Set { .. } => {}
        }
    }

    fn component(
        &mut self,
        c: &'i Component,
        parent: &str,
        marker: &str,
        code: &mut String,
        depth: usize,
    ) {
        let name = match &c.name {
            crate::ir::ComponentTarget::Static(s) => {
                let mut n = String::new();
                push_js_string(&mut n, s);
                n
            }
            crate::ir::ComponentTarget::Dynamic(e) => self.expr(e),
        };
        let mut props = String::from("{");
        let mut first = true;
        for p in &c.props {
            if !first {
                props.push_str(", ");
            }
            first = false;
            push_js_string(&mut props, &p.name);
            let value = self.expr(&p.value);
            if p.bind {
                let _ = write!(props, ": () => ({value}).bind($self)");
            } else {
                let _ = write!(props, ": () => {value}");
            }
        }
        for ev in &c.events {
            if let Binding::On {
                event,
                flags: _,
                handler,
            } = ev
            {
                if !first {
                    props.push_str(", ");
                }
                first = false;
                let handler_prop = format!("on{}", capitalize(event));
                push_js_string(&mut props, &handler_prop);
                let h_code = match handler {
                    None => "() => {}".to_owned(),
                    Some(e @ Expr::Arrow(..)) => self.expr(e),
                    Some(e) => format!("($e) => $.callHandler({}, $self, $e)", self.expr(e)),
                };
                let _ = write!(props, ": () => {h_code}");
            }
        }
        props.push('}');
        let spread = c
            .spread
            .as_ref()
            .map_or_else(|| "null".to_owned(), |s| format!("() => {}", self.expr(s)));
        let slots = if c.slots.is_empty() {
            "null".to_owned()
        } else {
            let mut s = String::from("{");
            for (i, slot) in c.slots.iter().enumerate() {
                if i > 0 {
                    s.push_str(", ");
                }
                s.push_str(&self.slot(slot, depth));
            }
            s.push('}');
            s
        };
        self.line(
            code,
            depth,
            format_args!(
                "$.component({parent}, {marker}, $self, {name}, {props}, {spread}, {slots});"
            ),
        );
    }

    fn slot(&mut self, slot: &'i SlotIr, depth: usize) -> String {
        let mut s = String::new();
        push_js_string(&mut s, &slot.name);
        let params = slot.scope.map(|n| format!("$s{n}")).unwrap_or_default();
        let render = self.block_fn(&params, &slot.body, depth);
        let _ = write!(s, ": {{render: {render}}}");
        s
    }
}

fn indent(code: &mut String, depth: usize) {
    for _ in 0..depth {
        code.push_str("  ");
    }
}

/// Escapes text content. A carriage return (from `&#13;` in the source: the XML parser turns a
/// literal one into a newline) is written as a character reference, because the HTML parser
/// turns a literal CR into LF (rust review L5).
fn escape_text(t: &str, out: &mut String) {
    for c in t.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '\r' => out.push_str("&#13;"),
            c => out.push(c),
        }
    }
}

/// Escapes a double-quoted attribute value. `<` and `>` are escaped too: inside a raw-text
/// element the parser does not see attribute quoting, so `</noscript>` in a value would end the
/// element (security review S1).
fn escape_attr(t: &str, out: &mut String) {
    for c in t.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '\r' => out.push_str("&#13;"),
            c => out.push(c),
        }
    }
}

/// Whether `name` is a name the generator declares: `$self`, `$e`, `$t<n>`, `$n<n>`, `$r<n>`,
/// `$s<n>`, `$ki<n>`, `$kv<n>`, `$kx<n>`, `$kc<n>` (decimal counters without leading zeros) or
/// `l_<identifier>` (an arrow-function parameter).
fn generated(name: &str) -> bool {
    if matches!(
        name,
        "$self"
            | "$e"
            | "$c"
            | "$b"
            | "$d"
            | "$call"
            | "$fn"
            | "$target"
            | "$p"
            | "$el"
            | "$r"
            | "$v"
            | "$s"
    ) {
        return true;
    }
    if let Some(rest) = name.strip_prefix("l_") {
        return is_plain_identifier(rest);
    }
    let Some(rest) = name.strip_prefix('$') else {
        return false;
    };
    ["ki", "kv", "kx", "kc", "t", "n", "r", "s", "v"]
        .iter()
        .filter_map(|p| rest.strip_prefix(p))
        .any(|digits| {
            !digits.is_empty()
                && digits.bytes().all(|b| b.is_ascii_digit())
                && (digits == "0" || !digits.starts_with('0'))
        })
}

/// The module re-parse check (design note §8.3, TO-TPL.2). It is a consistency check of our own
/// output, not a sandbox: it proves the module has the shape the generator writes, so a
/// generator bug cannot slip other code through. The module must parse as one ES module whose
/// top level is exactly the ABI namespace import, `const $t<n> = …` templates and the two
/// exports `name` and `render`; every node is of a kind the generator emits; every identifier is
/// a generated name ([`generated`]), an allowlisted global, `console` in dev builds, or the ABI
/// namespace `$` used as `$.<member>` with a member of [`ABI`].
pub(crate) fn verify_module(
    arena: &mut oxc_allocator::Allocator,
    module: &str,
    mode: BuildMode,
) -> Result<(), String> {
    use oxc_ast::AstKind;
    use oxc_ast::ast::{
        BindingIdentifier, BindingPattern, Declaration, Expression, IdentifierReference,
        ImportDeclarationSpecifier, ImportNamespaceSpecifier, ImportOrExportKind, Statement,
        StaticMemberExpression, VariableDeclarationKind,
    };
    use oxc_ast_visit::Visit;

    struct Check {
        mode: BuildMode,
        problems: Vec<String>,
        /// Function and arrow-function nesting.
        functions: usize,
        /// `function` declarations seen (only `render`).
        declared_functions: usize,
    }
    impl Check {
        fn problem(&mut self, p: String) {
            self.problems.push(p);
        }
    }
    impl<'a> Visit<'a> for Check {
        fn enter_node(&mut self, kind: AstKind<'a>) {
            let allowed = match kind {
                AstKind::Function(f) => {
                    self.declared_functions += 1;
                    let render = f.id.as_ref().is_some_and(|id| id.name.as_str() == "render");
                    if !render || self.declared_functions > 1 || f.generator || f.r#async {
                        self.problem("a function other than the exported render".to_owned());
                    }
                    self.functions += 1;
                    true
                }
                AstKind::ArrowFunctionExpression(f) => {
                    if f.r#async {
                        self.problem("an async arrow function".to_owned());
                    }
                    self.functions += 1;
                    true
                }
                AstKind::VariableDeclaration(d) => {
                    if d.kind != VariableDeclarationKind::Const {
                        self.problem("a declaration other than const".to_owned());
                    }
                    true
                }
                AstKind::Program(_)
                | AstKind::IdentifierName(_)
                | AstKind::IdentifierReference(_)
                | AstKind::BindingIdentifier(_)
                | AstKind::ArrayExpression(_)
                | AstKind::Elision(_)
                | AstKind::ObjectExpression(_)
                | AstKind::ObjectProperty(_)
                | AstKind::TemplateLiteral(_)
                | AstKind::TemplateElement(_)
                | AstKind::ComputedMemberExpression(_)
                | AstKind::StaticMemberExpression(_)
                | AstKind::CallExpression(_)
                | AstKind::SpreadElement(_)
                | AstKind::UpdateExpression(_)
                | AstKind::UnaryExpression(_)
                | AstKind::BinaryExpression(_)
                | AstKind::LogicalExpression(_)
                | AstKind::ConditionalExpression(_)
                | AstKind::AssignmentExpression(_)
                | AstKind::ChainExpression(_)
                | AstKind::ParenthesizedExpression(_)
                | AstKind::BlockStatement(_)
                | AstKind::VariableDeclarator(_)
                | AstKind::ExpressionStatement(_)
                | AstKind::IfStatement(_)
                | AstKind::ReturnStatement(_)
                | AstKind::FormalParameters(_)
                | AstKind::FormalParameter(_)
                | AstKind::AssignmentPattern(_)
                | AstKind::ThrowStatement(_)
                | AstKind::FunctionBody(_)
                | AstKind::ImportDeclaration(_)
                | AstKind::ImportNamespaceSpecifier(_)
                | AstKind::ExportDeclaration(_)
                | AstKind::BooleanLiteral(_)
                | AstKind::NullLiteral(_)
                | AstKind::NumericLiteral(_)
                | AstKind::StringLiteral(_)
                | AstKind::BigIntLiteral(_) => true,
                _ => false,
            };
            if !allowed {
                self.problem(format!(
                    "a {:?} node, which the generator never emits",
                    kind.ty()
                ));
            }
        }
        fn leave_node(&mut self, kind: AstKind<'a>) {
            if matches!(
                kind,
                AstKind::Function(_) | AstKind::ArrowFunctionExpression(_)
            ) {
                self.functions = self.functions.saturating_sub(1);
            }
        }
        fn visit_identifier_reference(&mut self, it: &IdentifierReference<'a>) {
            let name = it.name.as_str();
            let ok = generated(name)
                || GLOBALS.contains(&name)
                || matches!(name, "Error" | "document" | "parseFloat")
                || (name == "console" && self.mode == BuildMode::Dev);
            if !ok {
                self.problem(format!(
                    "identifier '{name}' is not a generated name, an allowed global or $.<ABI member>"
                ));
            }
        }
        fn visit_binding_identifier(&mut self, it: &BindingIdentifier<'a>) {
            let name = it.name.as_str();
            let ok = generated(name)
                || (name == "name" && self.functions == 0)
                || (name == "render" && self.functions == 1);
            if !ok {
                self.problem(format!("declared name '{name}' is not a generated name"));
            }
        }
        // The one place `$` is declared: checked with the import by the top-level check, so its
        // binding is not visited.
        fn visit_import_namespace_specifier(&mut self, _it: &ImportNamespaceSpecifier<'a>) {}
        fn visit_static_member_expression(&mut self, it: &StaticMemberExpression<'a>) {
            if let Expression::Identifier(object) = &it.object
                && object.name.as_str() == "$"
            {
                let member = it.property.name.as_str();
                if !ABI.contains(&member) {
                    self.problem(format!("'$.{member}' is not a member of the ABI"));
                }
                return;
            }
            oxc_ast_visit::walk::walk_static_member_expression(self, it);
        }
    }

    // One arena for every module of a set, reset per module.
    arena.reset();
    let parsed = oxc_parser::Parser::new(arena, module, oxc_span::SourceType::mjs()).parse();
    if let Some(d) = parsed.diagnostics.first() {
        return Err(format!("generated module does not parse: {d}"));
    }
    let program = &parsed.program;
    let mut problems: Vec<String> = Vec::new();
    if !program.directives.is_empty() || program.hashbang.is_some() {
        problems.push("a directive or hashbang".to_owned());
    }
    // The top level: the import, the templates, then `name` and `render`, in that order.
    let mut exports: Vec<&str> = Vec::new();
    for (i, statement) in program.body.iter().enumerate() {
        match statement {
            Statement::ImportDeclaration(d) if i == 0 => {
                let namespace_only = matches!(
                    d.specifiers.as_ref().map(|v| &v[..]),
                    Some([ImportDeclarationSpecifier::ImportNamespaceSpecifier(s)])
                        if s.local.name.as_str() == "$"
                );
                if !namespace_only
                    || d.source.value.as_str() != ABI_MODULE
                    || d.with_clause.is_some()
                    || d.phase.is_some()
                    || d.import_kind != ImportOrExportKind::Value
                {
                    problems.push(format!(
                        "the import is not `import * as $ from \"{ABI_MODULE}\"`"
                    ));
                }
            }
            Statement::VariableDeclaration(d) if exports.is_empty() => {
                let templates = d.declarations.iter().all(|v| {
                    matches!(&v.id, BindingPattern::BindingIdentifier(id)
                        if id.name.as_str().strip_prefix("$t").is_some_and(|n| generated(id.name.as_str()) && !n.is_empty()))
                });
                if !templates {
                    problems.push("a top-level const that is not a $t<n> template".to_owned());
                }
            }
            Statement::ExportDeclaration(e) => match &e.declaration {
                Declaration::VariableDeclaration(d)
                    if exports.is_empty()
                        && matches!(
                            &d.declarations[..],
                            [v] if matches!(&v.id, BindingPattern::BindingIdentifier(id) if id.name.as_str() == "name")
                                && matches!(v.init, Some(Expression::StringLiteral(_)))
                        ) =>
                {
                    exports.push("name");
                }
                Declaration::FunctionDeclaration(f)
                    if exports == ["name"]
                        && f.id.as_ref().is_some_and(|id| id.name.as_str() == "render") =>
                {
                    exports.push("render");
                }
                _ => problems.push("an export other than `name` then `render`".to_owned()),
            },
            _ => problems.push("a top-level statement the generator never writes".to_owned()),
        }
    }
    if exports != ["name", "render"] {
        problems.push("the exports are not exactly `name` and `render`".to_owned());
    }
    let mut check = Check {
        mode,
        problems,
        functions: 0,
        declared_functions: 0,
    };
    check.visit_program(program);
    match check.problems.into_iter().next() {
        Some(p) => Err(format!("generated module failed the re-parse check: {p}")),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analyze::{TemplateAnalysis, analyze_template};
    use crate::expr::ExprParser;
    use crate::limits::CompilerLimits;
    use crate::names::{AddonName, TemplateName, TemplatePath};
    use crate::source::{CompileOptions, Origin, Trust};
    use crate::xml::LineIndex;
    use html5ever::tendril::TendrilSink;
    use html5ever::{LocalName, Namespace, ParseOpts, QualName, parse_fragment};
    use markup5ever_rcdom::{Handle, NodeData, RcDom};
    use proptest::prelude::*;
    use std::collections::BTreeSet;

    const HTML_NS: &str = "http://www.w3.org/1999/xhtml";
    const SVG_NS: &str = "http://www.w3.org/2000/svg";

    fn origin() -> Origin {
        Origin::AddonFile {
            addon: AddonName::new("web").unwrap(),
            path: TemplatePath::new("t.xml").unwrap(),
        }
    }

    /// The analysis of the one template in `xml`, or `None` when the front end rejects it.
    fn analysed(xml: &str) -> Option<TemplateAnalysis> {
        let limits = CompilerLimits::DEFAULT;
        let tops = crate::xml::parse_templates_file(xml, &limits, 0).ok()?;
        let origin = origin();
        let lines = LineIndex::new(xml);
        let loc = LocCtx {
            origin: &origin,
            text: xml,
            lines: Some(&lines),
        };
        let options = CompileOptions::new(BuildMode::Prod);
        let mut parser = ExprParser::new(&limits);
        let name = TemplateName::new("t").unwrap();
        let templates: BTreeSet<TemplateName> = [name.clone()].into_iter().collect();
        let a = analyze_template(
            &mut parser,
            &options,
            Trust::Trusted,
            &templates,
            &name,
            &tops.first()?.element,
            &loc,
        );
        let rejected = a
            .diagnostics
            .iter()
            .any(|d| d.severity() == crate::diag::Severity::Error);
        (!rejected).then_some(a)
    }

    fn gen_for<'i, 'l>(
        ir: &'i TemplateIr,
        loc: &'l LocCtx<'l>,
        limits: &'l crate::limits::CompilerLimits,
    ) -> Gen<'i, 'l> {
        Gen {
            ir,
            loc,
            limits,
            effects: 0,
            statics: String::new(),
            templates: 0,
            nodes: 0,
            errors: Vec::new(),
            key_of: None,
            internal: Cell::new(None),
            emitted: 0,
            precharged: 0,
            max_module: usize::MAX,
            over: false,
        }
    }

    /// Parses `html` as `<template>.innerHTML` does (fragment parsing in a `template` context,
    /// scripting on, as in a browser) and returns the document with the fragment's nodes. The
    /// document must outlive the nodes' use: dropping an rcdom node empties the child lists of
    /// all its descendants, also of those still referenced.
    fn parse_like_a_browser(html: &str) -> (RcDom, Vec<Handle>) {
        let context = QualName::new(None, Namespace::from(HTML_NS), LocalName::from("template"));
        let dom = parse_fragment(
            RcDom::default(),
            ParseOpts::default(),
            context,
            vec![],
            true,
        )
        .one(html);
        let root = dom.document.children.borrow()[0].clone();
        let nodes = root.children.borrow().clone();
        (dom, nodes)
    }

    /// Compares our static tree with the parsed DOM; `Err` names the first difference.
    fn same(ours: &[SNode<'_>], dom: &[Handle], path: &str) -> Result<(), String> {
        let ours: Vec<&SNode<'_>> = ours
            .iter()
            .filter(|n| !matches!(n, SNode::Marker { trailing: true, .. }))
            .collect();
        if ours.len() != dom.len() {
            return Err(format!(
                "{path}: {} nodes in the IR, {} in the parsed DOM",
                ours.len(),
                dom.len()
            ));
        }
        for (i, (o, d)) in ours.iter().zip(dom).enumerate() {
            let here = format!("{path}/{i}");
            match (o, &d.data) {
                (SNode::Text(t), NodeData::Text { contents }) => {
                    if **t != *contents.borrow().to_string() {
                        return Err(format!(
                            "{here}: text {t:?} parsed as {:?}",
                            contents.borrow()
                        ));
                    }
                }
                (SNode::Marker { .. }, NodeData::Comment { contents }) if contents.is_empty() => {}
                (SNode::Element { el, children }, NodeData::Element { name, attrs, .. }) => {
                    let ns = if el.svg { SVG_NS } else { HTML_NS };
                    if *name.ns != *ns || *name.local != *el.tag {
                        return Err(format!(
                            "{here}: <{}> (svg {}) parsed as {{{}}}{}",
                            el.tag, el.svg, &*name.ns, &*name.local
                        ));
                    }
                    let mut want: Vec<(String, String)> = el
                        .attrs
                        .iter()
                        .map(|(n, v)| (n.to_ascii_lowercase(), v.to_string()))
                        .collect();
                    let mut got: Vec<(String, String)> = attrs
                        .borrow()
                        .iter()
                        .map(|a| {
                            (
                                a.name.local.as_ref().to_ascii_lowercase(),
                                a.value.to_string(),
                            )
                        })
                        .collect();
                    want.sort();
                    got.sort();
                    if want != got {
                        return Err(format!("{here}: attributes {want:?} parsed as {got:?}"));
                    }
                    let dom_children = d.children.borrow().clone();
                    same(children, &dom_children, &here)?;
                }
                (o, _) => {
                    return Err(format!(
                        "{here}: {} parsed as another kind of node",
                        match o {
                            SNode::Text(_) => "text",
                            SNode::Marker { .. } => "a marker",
                            SNode::Element { .. } => "an element",
                        }
                    ));
                }
            }
        }
        Ok(())
    }

    /// What codegen does with the root block of the one template in `xml`: `None` when the front
    /// end rejects it, `Some(Err)` when the structure check refuses it, `Some(Ok(html))` when it
    /// is accepted, after asserting that a real HTML parser builds exactly the IR's tree from it.
    fn round_trip(xml: &str) -> Option<Result<String, Vec<Diagnostic>>> {
        let a = analysed(xml)?;
        let origin = origin();
        let loc = LocCtx {
            origin: &origin,
            text: xml,
            lines: None,
        };
        let limits = crate::limits::CompilerLimits::default();
        let mut g = gen_for(&a.ir, &loc, &limits);
        let mut tree = Vec::new();
        g.static_tree(&a.ir.body, &mut tree);
        g.check_structure(&tree, &mut Vec::new(), false);
        if !g.errors.is_empty() {
            return Some(Err(g.errors));
        }
        let mut html = String::new();
        Gen::serialize(&tree, &mut html, None);
        let (_document, dom) = parse_like_a_browser(&html);
        if let Err(difference) = same(&tree, &dom, "") {
            panic!("accepted markup re-parses differently: {difference}\nxml: {xml}\nhtml: {html}");
        }
        Some(Ok(html))
    }

    fn wrap(body: &str) -> String {
        format!("<templates><t t-name=\"t\">{body}</t></templates>")
    }

    fn refused(body: &str) -> Vec<String> {
        match round_trip(&wrap(body)) {
            Some(Err(errors)) => errors.iter().map(|d| d.message().to_owned()).collect(),
            other => panic!("{body}: expected a structure error, got {other:?}"),
        }
    }

    fn accepted(body: &str) -> String {
        match round_trip(&wrap(body)) {
            Some(Ok(html)) => html,
            other => panic!("{body}: expected it to be accepted, got {other:?}"),
        }
    }

    /// Security review S1: an attribute value cannot end a raw-text element, and mixed-case
    /// names of raw-text elements are refused instead of slipping past the rules.
    #[test]
    fn attribute_values_cannot_break_out_of_raw_text() {
        let html = accepted(
            "<div title=\"&lt;/noscript&gt;&lt;img src=x onerror=alert(1)&gt;\" data-q=\"a&amp;&quot;b\"/>",
        );
        assert_eq!(
            html,
            "<div title=\"&lt;/noscript&gt;&lt;img src=x onerror=alert(1)&gt;\" data-q=\"a&amp;&quot;b\"></div>"
        );
        for tag in [
            "noScript",
            "sTyle",
            "xMp",
            "iFrame",
            "tExtarea",
            "tItle",
            "sCript",
            "pLaintext",
            "tEmplate",
        ] {
            let errors = refused(&format!(
                "<{tag}><b title=\"&lt;/{lower}&gt;&lt;img src=x onerror=1&gt;\"/></{tag}>",
                lower = tag.to_ascii_lowercase()
            ));
            assert!(
                errors[0].contains("HTML element names are lowercase"),
                "{tag}: {errors:?}"
            );
        }
        let errors = refused("<noscript><b title=\"x\"/></noscript>");
        assert!(
            errors[0].contains("<noscript> can only hold static text"),
            "{errors:?}"
        );
    }

    /// Security review H1/S1/S6: inside SVG only the closed list of SVG elements is accepted;
    /// `style`, `script`, integration points, animations and `math` are refused.
    #[test]
    fn svg_takes_only_its_closed_list() {
        accepted(
            "<svg viewBox=\"0 0 1 1\"><g><path d=\"M0 0\"/><clipPath id=\"c\"><rect width=\"1\"/></clipPath><text>a &lt; b</text></g></svg>",
        );
        accepted("<svg xmlns=\"http://www.w3.org/2000/svg\"><circle r=\"1\"/></svg>");
        for (inner, why) in [
            (
                "<style>&lt;/style&gt;&lt;img src=x onerror=1&gt;</style>",
                "raw text",
            ),
            ("<script/>", "scripts are never"),
            (
                "<foreignObject><div/></foreignObject>",
                "HTML integration point",
            ),
            ("<desc><style>x</style></desc>", "HTML integration point"),
            ("<title>x</title>", "HTML integration point"),
            ("<a href=\"#\"/>", "navigation target"),
            ("<animate attributeName=\"href\"/>", "animation can change"),
            ("<set attributeName=\"href\"/>", "can change any attribute"),
            ("<div/>", "not supported"),
            ("<p/>", "not supported"),
            ("<math/>", "not supported"),
        ] {
            let errors = refused(&format!("<svg>{inner}</svg>"));
            assert!(
                errors.iter().any(|e| e.contains(why)),
                "{inner}: {errors:?}"
            );
        }
        let errors = refused("<math><mi>x</mi></math>");
        assert!(errors[0].contains("MathML"), "{errors:?}");
    }

    /// Rust review L5: a carriage return (`&#13;`) in an attribute survives as a character
    /// reference; in text it is whitespace, which collapses to a space as in OWL, except in
    /// `<pre>`, where it is kept as a reference; raw `<style>` text with one is refused.
    #[test]
    fn carriage_returns_survive() {
        let html = accepted("<p title=\"a&#13;b\">x&#13;y</p>");
        assert_eq!(html, "<p title=\"a&#13;b\">x y</p>");
        let html = accepted("<pre>x&#13;y</pre>");
        assert_eq!(html, "<pre>x&#13;y</pre>");
        // Outside `<pre>` the CR collapses to a space before the check sees it.
        assert_eq!(accepted("<style>a&#13;b</style>"), "<style>a b</style>");
        let errors = refused("<pre><style>a&#13;b</style></pre>");
        assert!(errors[0].contains("carriage return"), "{errors:?}");
    }

    /// Round-two review M1: only modelled HTML elements and custom elements are accepted.
    #[test]
    fn only_modelled_html_elements_are_accepted() {
        for (body, why) in [
            ("<div><body><b/></body></div>", "moves its children up"),
            ("<image/>", "reads it as <img>"),
            ("<param/>", "void element"),
            ("<nobr><nobr/></nobr>", "closes open elements"),
            ("<ruby>a<rt>b</rt></ruby>", "closes open elements"),
            ("<object/>", "scope boundary"),
            ("<base/>", "relative URL"),
            ("<blink/>", "not an HTML element the compiler models"),
            ("<font-face/>", "not an HTML element the compiler models"),
        ] {
            let errors = refused(body);
            assert!(errors.iter().any(|e| e.contains(why)), "{body}: {errors:?}");
        }
        accepted("<my-widget><span>x</span></my-widget>");
        accepted("<ruby>a</ruby>");
        accepted("<label><input/></label>");
    }

    /// The element allowlist is searched with `binary_search`, so it must stay sorted.
    #[test]
    fn element_allowlist_is_sorted() {
        assert!(HTML_ELEMENTS.windows(2).all(|w| w[0] < w[1]));
    }

    /// The `<svg>` root obeys its HTML parent's content rules: inside a `<table>` it would be
    /// moved out (foster parenting).
    #[test]
    fn svg_roots_follow_their_parents_rules() {
        let errors = refused("<table><svg/></table>");
        assert!(
            errors.iter().any(|e| e.contains("directly inside <table>")),
            "{errors:?}"
        );
        accepted("<div><svg><g/></svg></div>");
    }

    /// A `<button>` start tag closes an open `<button>` in the default scope, also through other
    /// elements; a `<p>` is closed only up to the nearest `<button>`.
    #[test]
    fn nested_buttons_are_refused_through_other_elements() {
        for body in [
            "<button><button/></button>",
            "<button><div><button/></div></button>",
        ] {
            let errors = refused(body);
            assert!(
                errors.iter().any(|e| e.contains("closes the outer one")),
                "{body}: {errors:?}"
            );
        }
        accepted("<p><button><div>x</div></button></p>");
        accepted("<button><table><tbody><tr><td><button/></td></tr></tbody></table></button>");
    }

    /// An option holds text only: `<optgroup>` and `<option>` close it, other elements are parsed
    /// differently by the older and the customizable-select rules.
    #[test]
    fn options_hold_text_only() {
        for inner in ["<optgroup/>", "<option/>", "<b>x</b>"] {
            let errors = refused(&format!("<select><option>{inner}</option></select>"));
            assert!(
                errors
                    .iter()
                    .any(|e| e.contains("an option holds text only")),
                "{inner}: {errors:?}"
            );
        }
        accepted("<select><option>a</option><optgroup><option>b</option></optgroup></select>");
    }

    /// `<li>`, `<dd>`, `<dt>` and the other "close a p element" start tags close an open `<p>`.
    #[test]
    fn list_items_inside_p_are_refused() {
        for tag in ["li", "dd", "dt", "summary", "listing"] {
            let errors = refused(&format!("<p><{tag}/></p>"));
            assert!(
                errors.iter().any(|e| e.contains("closes the <p> first")),
                "{tag}: {errors:?}"
            );
        }
        accepted("<p><span><b/></span></p>");
    }

    /// The top level of a block is parsed in the `<template>` context, where the first start tag
    /// fixes the mode: table parts stay in their group, nothing else mixes with them.
    #[test]
    fn table_parts_at_the_block_top_keep_their_group() {
        assert_eq!(accepted("<td>1</td><th>2</th>"), "<td>1</td><th>2</th>");
        assert_eq!(
            accepted("<tr><td/></tr><tr/>"),
            "<tr><td></td></tr><tr></tr>"
        );
        accepted("<caption>c</caption><tbody><tr/></tbody>");
        accepted("<col/><col/>");
        for (body, why) in [
            (
                "<dd/><caption/>",
                "dropped by the HTML parser outside a table",
            ),
            ("<col/><caption/>", "keeps only parts of its group (col)"),
            ("<tr/><td/>", "keeps only parts of its group (tr)"),
            ("<td/><b/>", "keeps only parts of its group (td, th)"),
            ("<td/>text", "text next to <td>"),
        ] {
            let errors = refused(body);
            assert!(errors.iter().any(|e| e.contains(why)), "{body}: {errors:?}");
        }
    }

    #[test]
    fn attributes_differing_in_case_are_refused() {
        let errors = refused("<div a=\"1\" A=\"2\"/>");
        assert!(errors[0].contains("when case is ignored"), "{errors:?}");
    }

    /// Element names, attributes, texts and markers of the generated trees: dangerous values
    /// included (end tags inside values, raw-text and RCDATA elements, SVG breakouts, mixed case).
    /// `TAGS` are elements the check accepts in some context ...
    const TAGS: [&str; 46] = [
        "div", "p", "span", "b", "a", "table", "tbody", "tr", "td", "caption", "colgroup", "col",
        "select", "option", "optgroup", "ul", "li", "dl", "dt", "dd", "pre", "textarea", "title",
        "style", "noscript", "iframe", "h1", "h2", "button", "form", "img", "br", "svg", "path",
        "g", "circle", "clipPath", "text", "thead", "tfoot", "th", "ruby", "input", "label",
        "listing", "my-el",
    ];
    /// ... and, one draw in ten, names it always refuses (mixed case, HTML integration points
    /// and parser specials), so refusals are exercised without starving the parser of input.
    const REFUSED_TAGS: [&str; 16] = [
        "noScript",
        "sTyle",
        "xMp",
        "foreignObject",
        "desc",
        "math",
        "rb",
        "rt",
        "rp",
        "rtc",
        "nobr",
        "body",
        "image",
        "param",
        "object",
        "font-face",
    ];
    const ATTR_NAMES: [&str; 6] = ["class", "title", "id", "data-x", "A", "a"];
    const VALUES: [&str; 8] = [
        "",
        "x",
        "&lt;/noscript&gt;&lt;img src=x onerror=alert(1)&gt;",
        "&lt;/style&gt;",
        "&quot;q&quot;",
        "a&amp;b",
        "&lt;b&gt;",
        "x&#13;y",
    ];
    const TEXTS: [&str; 9] = [
        "hi",
        " ",
        "\n",
        "&lt;/style&gt;",
        "&lt;b&gt;",
        "a&amp;b",
        "&#13;",
        "x&lt;/textarea&gt;y",
        "&lt;/title&gt;&lt;img&gt;",
    ];

    fn tree() -> impl Strategy<Value = String> {
        let leaf = prop_oneof![
            (0..TEXTS.len()).prop_map(|i| TEXTS[i].to_owned()),
            Just("<t t-out=\"this.a\"/>".to_owned()),
        ];
        leaf.prop_recursive(5, 48, 4, |inner| {
            (
                prop_oneof![
                    9 => (0..TAGS.len()).prop_map(|i| TAGS[i]),
                    1 => (0..REFUSED_TAGS.len()).prop_map(|i| REFUSED_TAGS[i]),
                ],
                proptest::collection::vec((0..ATTR_NAMES.len(), 0..VALUES.len()), 0..3),
                proptest::collection::vec(inner, 0..4),
            )
                .prop_map(|(tag, attrs, children)| {
                    let mut seen: Vec<&str> = Vec::new();
                    let mut s = format!("<{tag}");
                    for (n, v) in attrs {
                        let name = ATTR_NAMES[n];
                        if seen.contains(&name) {
                            continue;
                        }
                        seen.push(name);
                        s.push_str(&format!(" {name}=\"{}\"", VALUES[v]));
                    }
                    s.push('>');
                    for c in children {
                        s.push_str(&c);
                    }
                    s.push_str(&format!("</{tag}>"));
                    s
                })
        })
    }

    /// Cases per run of the round trip: 1 024, or `ERP_UI_ROUNDTRIP_CASES` for a deeper run (the
    /// review rounds ran 2 000 000 and 20 000 000 before merge to shake out parser rules).
    fn round_trip_cases() -> u32 {
        match std::env::var("ERP_UI_ROUNDTRIP_CASES") {
            Ok(v) => v.parse().expect("ERP_UI_ROUNDTRIP_CASES is a whole number"),
            Err(_) => 1024,
        }
    }

    static ROUND_TRIP_RUN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    static ROUND_TRIP_ACCEPTED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    /// Counts the round trip's cases and the accepted ones (only those reach the HTML parser)
    /// and prints both after the last case, so a deep run records how much it really parsed.
    fn count_round_trip(accepted: bool) {
        use std::sync::atomic::Ordering::Relaxed;
        if accepted {
            ROUND_TRIP_ACCEPTED.fetch_add(1, Relaxed);
        }
        let run = ROUND_TRIP_RUN.fetch_add(1, Relaxed) + 1;
        if run == u64::from(round_trip_cases()) {
            eprintln!(
                "round trip: {run} cases, {} accepted and re-parsed",
                ROUND_TRIP_ACCEPTED.load(Relaxed)
            );
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: round_trip_cases(), ..ProptestConfig::default() })]

        /// Security review S1: every tree the structure check accepts re-parses, in a real HTML5
        /// parser, to exactly the IR's tree (elements, namespaces, attributes, text, markers).
        #[test]
        fn accepted_static_html_round_trips(body in proptest::collection::vec(tree(), 1..3)) {
            let accepted = matches!(round_trip(&wrap(&body.concat())), Some(Ok(_)));
            count_round_trip(accepted);
        }
    }

    /// Security review S3: names the generator never writes are refused by the module check.
    #[test]
    fn the_module_check_is_exact() {
        let ok = concat!(
            "import * as $ from \"",
            abi_module!(),
            "\";\n",
            "const $t0 = $.template(\"x\", 0);\n",
            "export const name = \"web.T\";\n",
            "export function render($self) {\n  const $n0 = $t0();\n  return $n0;\n}\n",
        );
        let mut arena = oxc_allocator::Allocator::new();
        assert_eq!(verify_module(&mut arena, ok, BuildMode::Prod), Ok(()));
        for (bad, why) in [
            (ok.replace("$n0", "$unbound"), "is not a generated name"),
            (ok.replace("$n0", "$n01"), "is not a generated name"),
            (
                ok.replace("$.template", "$.fetch"),
                "is not a member of the ABI",
            ),
            (
                ok.replace("$t0();", "$[\"template\"]();"),
                "allowed global or $.",
            ),
            (ok.replace("$t0();", "this;"), "ThisExpression"),
            (ok.replace("$t0();", "new Date();"), "NewExpression"),
            (ok.replace("$t0();", "import(\"x\");"), "ImportExpression"),
            (
                ok.replace("$t0();", "$self`x`;"),
                "TaggedTemplateExpression",
            ),
            (ok.replace("const $n0", "let $n0"), "other than const"),
            (
                format!("{ok}export const other = 1;\n"),
                "an export other than",
            ),
            (
                ok.replace("export const name = \"web.T\";\n", ""),
                "an export other than",
            ),
            (format!("{ok}function f() {{}}\n"), "top-level statement"),
            (ok.replace("import * as $", "import $"), "the import is not"),
        ] {
            let err = verify_module(&mut arena, &bad, BuildMode::Prod).unwrap_err();
            assert!(err.contains(why), "{why}: {err}\n{bad}");
        }
    }

    #[test]
    fn generated_names_are_exact() {
        for good in [
            "$self", "$e", "$t0", "$n12", "$r3", "$s0", "$ki1", "$kv1", "$kx1", "$kc10", "l_row",
        ] {
            assert!(generated(good), "{good}");
        }
        for bad in [
            "$", "$x", "$n", "$n01", "$unbound", "$selfx", "l_", "l_class", "n0", "render",
        ] {
            assert!(!generated(bad), "{bad}");
        }
    }

    /// The helper list of the compiler and the runtime's frozen contract are one list (type review
    /// M4): the ordered `export declare function` names of `web/src/runtime/abi.d.ts` are `ABI`.
    #[test]
    fn abi_matches_the_runtime_contract() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../web/src/runtime/abi.d.ts"
        );
        let src = std::fs::read_to_string(path).unwrap();
        let declared: Vec<&str> = src
            .lines()
            .filter_map(|l| l.strip_prefix("export declare function "))
            .map(|rest| rest.split('(').next().unwrap())
            .collect();
        assert_eq!(declared, ABI, "abi.d.ts and codegen::ABI differ");
    }
}
