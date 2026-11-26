// SPDX-License-Identifier: MIT OR Apache-2.0
//! The compile step (design §4.1): validated template archs become one closed [`ReportProgram`].
//! Every `t-call` target is resolved here, so evaluation looks nothing up. Nothing is evaluated,
//! read or fetched by this module.
use crate::{
    AttachmentPolicy, FieldInfo, ReportError, ReportLimits, RestrictedField, TemplateSource,
    check_attachment,
    error::invalid,
    look::Look,
    validate::{call_argument, expression, identifier, parse_arch, validate_arch},
};
use erp_expr::Expr;
use erp_views::NodeRef;
use std::{collections::BTreeMap, sync::Arc};

/// The task that adds what this slice refuses by name.
pub(crate) const FORMATS_TASK: &str = "a later release";

/// The language a program is compiled for, with the translated terms of its templates.
///
/// Terms name literal text or a phrase with numbered placeholders for its inline elements.
#[derive(Debug, Clone)]
pub struct Language {
    code: String,
    terms: BTreeMap<String, String>,
    template_terms: BTreeMap<String, BTreeMap<String, String>>,
}
impl Language {
    pub fn new(code: &str) -> Result<Self, ReportError> {
        let (base, variant) = code.split_once('@').unwrap_or((code, "a"));
        let (lang, region) = base.split_once('_').unwrap_or((base, "AA"));
        let ok = (2..=3).contains(&lang.len())
            && lang.bytes().all(|c| c.is_ascii_lowercase())
            && (2..=4).contains(&region.len())
            && region.bytes().all(|c| c.is_ascii_alphanumeric())
            && (1..=16).contains(&variant.len())
            && variant.bytes().all(|c| c.is_ascii_lowercase());
        if !ok {
            return Err(invalid(
                "language code must look like en, en_US, ar_001 or sr@latin",
            ));
        }
        Ok(Self {
            code: code.into(),
            terms: BTreeMap::new(),
            template_terms: BTreeMap::new(),
        })
    }
    /// Add the translation of one source term.
    pub fn with_term(mut self, source: &str, translation: &str) -> Self {
        self.terms
            .insert(source.trim().into(), translation.trim().into());
        self
    }
    /// Bind a term to one report template, ahead of the language-wide fallback.
    pub fn with_template_term(mut self, template: &str, source: &str, translated: &str) -> Self {
        self.template_terms
            .entry(template.to_owned())
            .or_default()
            .insert(source.trim().to_owned(), translated.trim().to_owned());
        self
    }

    fn term(&self, template: &str, source: &str) -> Option<&String> {
        self.template_terms
            .get(template)
            .and_then(|terms| terms.get(source))
            .or_else(|| self.terms.get(source))
    }

    /// The full code, as given.
    pub fn code(&self) -> &str {
        &self.code
    }
    /// The ISO 639 part the layout engine takes.
    pub(crate) fn iso(&self) -> &str {
        self.code.split(['_', '@']).next().unwrap_or(&self.code)
    }
}

/// Where an operation came from, for errors.
#[derive(Debug, Clone)]
pub(crate) struct At {
    pub(crate) template: Arc<str>,
    pub(crate) line: u32,
}
impl At {
    pub(crate) fn compile(&self, reason: impl Into<String>) -> ReportError {
        ReportError::Compile {
            template: self.template.as_ref().into(),
            line: self.line,
            reason: reason.into(),
        }
    }
    pub(crate) fn evaluate(&self, reason: impl Into<String>) -> ReportError {
        ReportError::Evaluate {
            template: self.template.as_ref().into(),
            line: self.line,
            reason: reason.into(),
        }
    }
}
/// A parsed expression with its text, which errors quote.
#[derive(Debug, Clone)]
pub(crate) struct Code {
    pub(crate) expr: Expr,
    pub(crate) source: String,
}
/// One piece of a format string (`text {{ expr }} text`).
#[derive(Debug, Clone)]
pub(crate) enum Part {
    Literal(String),
    Value(Code),
}
/// An attribute of an element, in the order the reference applies them: static ones first, then
/// the dynamic ones as written. A later one of the same name replaces an earlier one.
#[derive(Debug, Clone)]
pub(crate) enum Attr {
    Static { name: String, value: String },
    Value { name: String, code: Code },
    Format { name: String, parts: Vec<Part> },
    Mapping(Code),
}
/// A value handed to a called template.
#[derive(Debug, Clone)]
pub(crate) enum Argument {
    Value(Code),
    Format(Vec<Part>),
}
/// What an element prints.
#[derive(Debug, Clone)]
pub(crate) enum Content {
    Children(Vec<Op>),
    /// `t-out`/`t-esc`: `None` is the caller's body (`t-out="0"`).
    Out {
        value: Option<Code>,
        options: Option<Code>,
        default: Vec<Op>,
    },
    /// `t-field="record.field"`.
    Field {
        record: Code,
        field: String,
        options: Option<Code>,
        default: Vec<Op>,
    },
}
#[derive(Debug, Clone)]
pub(crate) struct Element {
    pub(crate) tag: String,
    pub(crate) attrs: Vec<Attr>,
    /// The resolved appearance when no attribute is dynamic.
    pub(crate) look: Option<Look>,
    pub(crate) content: Content,
    pub(crate) at: At,
}
#[derive(Debug, Clone)]
pub(crate) struct Branch {
    /// `None` is `t-else`.
    pub(crate) test: Option<Code>,
    pub(crate) body: Vec<Op>,
}
#[derive(Debug, Clone)]
pub(crate) enum Op {
    Text(String),
    Element(Box<Element>),
    If {
        branches: Vec<Branch>,
        at: At,
    },
    Foreach {
        items: Code,
        name: String,
        body: Vec<Op>,
        at: At,
    },
    Set {
        name: String,
        value: Option<Code>,
        body: Vec<Op>,
        at: At,
    },
    Call {
        template: usize,
        arguments: Vec<(String, Argument)>,
        body: Vec<Op>,
        lang: Option<Code>,
        at: At,
    },
}

/// A compiled report: a closed tree of operations for one language. Build one per language and
/// keep it for as long as the templates do not change (design §4.1, §5.3).
#[derive(Debug)]
pub struct ReportProgram {
    /// The root template's key.
    pub(crate) name: Arc<str>,
    pub(crate) templates: Vec<Vec<Op>>,
    pub(crate) root: usize,
    pub(crate) language: Language,
    pub(crate) limits: ReportLimits,
    nodes: usize,
}
impl ReportProgram {
    /// Compile `root` and every template it calls from the supplied combined archs.
    ///
    /// Each arch is validated against the print profile first; a `t-call` of a key that is not in
    /// `sources`, a call cycle and a directive on an element that cannot carry it are
    /// [`ReportError::Compile`] errors.
    ///
    /// ```
    /// use erp_report::{Language, ReportLimits, ReportProgram, TemplateSource};
    /// let sources = [TemplateSource::new("demo.report", "<p>Hello <b t-out='name'/></p>")];
    /// let program =
    ///     ReportProgram::compile("demo.report", &sources, &Language::new("en_US")?, ReportLimits::default())?;
    /// assert_eq!(program.language().code(), "en_US");
    /// # Ok::<(), erp_report::ReportError>(())
    /// ```
    pub fn compile(
        root: &str,
        sources: &[TemplateSource<'_>],
        language: &Language,
        limits: ReportLimits,
    ) -> Result<Self, ReportError> {
        Self::compile_for_groups(root, sources, language, limits, &BTreeMap::new())
    }

    /// Static group expressions referenced by the templates. Callers evaluate these under
    /// the printing user's ORM environment before planning any data reads.
    pub fn group_conditions(
        sources: &[TemplateSource<'_>],
        limits: ReportLimits,
    ) -> Result<std::collections::BTreeSet<String>, ReportError> {
        let mut conditions = std::collections::BTreeSet::new();
        let mut work = limits.work();
        for source in sources {
            work.charge(source.arch().len())?;
            if !source.arch().contains("groups") {
                continue;
            }
            let arch = parse_arch(source.key(), source.arch(), limits)?;
            let mut pending = vec![arch.root()];
            while let Some(node) = pending.pop() {
                work.charge(1)?;
                if let Some(group) = node.attr("groups") {
                    conditions.insert(group.to_owned());
                }
                pending.extend(node.children());
            }
        }
        Ok(conditions)
    }

    /// Compile for one audience. Missing membership decisions are errors, including for
    /// standalone callers that have no group resolver.
    pub fn compile_for_groups(
        root: &str,
        sources: &[TemplateSource<'_>],
        language: &Language,
        limits: ReportLimits,
        groups: &BTreeMap<String, bool>,
    ) -> Result<Self, ReportError> {
        let mut compiler = Compiler {
            sources: sources.iter().map(|s| (s.key(), s.arch())).collect(),
            language,
            limits,
            groups,
            templates: Vec::new(),
            depths: Vec::new(),
            index: BTreeMap::new(),
            stack: Vec::new(),
            nodes: 0,
        };
        let at = At {
            template: root.into(),
            line: 1,
        };
        let name = at.template.clone();
        let root = compiler.template(root, &at, 0)?;
        Ok(Self {
            name,
            templates: compiler.templates,
            root,
            language: language.clone(),
            limits,
            nodes: compiler.nodes,
        })
    }
    /// Compile a report that is archived as an attachment: the group-restricted field analysis runs
    /// first (design §6.6) and its error stops the compile. The known restricted
    /// fields of an acknowledged report are returned with the program.
    pub fn compile_for_attachment<F: Fn(&str, &str) -> Option<FieldInfo>>(
        policy: &AttachmentPolicy<'_>,
        root: &str,
        sources: &[TemplateSource<'_>],
        fields: &F,
        language: &Language,
        limits: ReportLimits,
    ) -> Result<(Self, Vec<RestrictedField>), ReportError> {
        let restricted = check_attachment(policy, sources, root, fields, limits)?;
        Ok((Self::compile(root, sources, language, limits)?, restricted))
    }
    /// The language the program was compiled for.
    pub fn language(&self) -> &Language {
        &self.language
    }
    /// Operations in the program, called templates counted once each.
    pub fn node_count(&self) -> usize {
        self.nodes
    }
}

struct Compiler<'a> {
    groups: &'a BTreeMap<String, bool>,
    sources: BTreeMap<&'a str, &'a str>,
    language: &'a Language,
    limits: ReportLimits,
    templates: Vec<Vec<Op>>,
    /// Element nesting of each compiled template, its calls included.
    depths: Vec<usize>,
    index: BTreeMap<String, usize>,
    stack: Vec<String>,
    nodes: usize,
}
/// What a subtree inherits while it is compiled.
#[derive(Clone)]
struct Cx {
    template: Arc<str>,
    translate: bool,
    depth: usize,
}
impl Compiler<'_> {
    fn template(&mut self, key: &str, at: &At, depth: usize) -> Result<usize, ReportError> {
        if self.stack.iter().any(|k| k == key) {
            let mut chain = self.stack.join(" -> ");
            chain.push_str(" -> ");
            chain.push_str(key);
            return Err(at.compile(format!("t-call cycle: {chain}")));
        }
        let index = match self.index.get(key) {
            Some(index) => *index,
            None => {
                if self.stack.len() >= self.limits.calls {
                    return Err(ReportError::Limit {
                        name: "call_depth",
                        maximum: self.limits.calls,
                    });
                }
                let Some(xml) = self.sources.get(key).copied() else {
                    return Err(at.compile(format!(
                        "template `{key}` is not among the supplied templates"
                    )));
                };
                let arch = parse_arch(key, xml, self.limits)?;
                validate_arch(key, &arch, self.limits)?;
                self.stack.push(key.into());
                let cx = Cx {
                    template: key.into(),
                    translate: true,
                    depth: 0,
                };
                let mut deepest = 0;
                let mut ops = Vec::new();
                self.element(arch.root(), &cx, false, &mut ops, &mut deepest)?;
                self.stack.pop();
                let index = self.templates.len();
                self.templates.push(ops);
                self.depths.push(deepest);
                self.index.insert(key.into(), index);
                index
            }
        };
        self.depth(depth + self.depths[index])?;
        Ok(index)
    }
    fn depth(&self, depth: usize) -> Result<(), ReportError> {
        if depth > self.limits.program_depth {
            return Err(ReportError::Limit {
                name: "program_depth",
                maximum: self.limits.program_depth,
            });
        }
        Ok(())
    }
    fn count(&mut self) -> Result<(), ReportError> {
        self.nodes += 1;
        if self.nodes > self.limits.program_nodes {
            return Err(ReportError::Limit {
                name: "program_nodes",
                maximum: self.limits.program_nodes,
            });
        }
        Ok(())
    }
    fn code(&self, at: &At, source: &str) -> Result<Code, ReportError> {
        Ok(Code {
            expr: expression(&at.template, at.line, source)?,
            source: source.into(),
        })
    }
    /// Split a format string into literal text and `{{ … }}` / `#{ … }` expressions.
    fn format(&self, at: &At, value: &str) -> Result<Vec<Part>, ReportError> {
        let mut parts = Vec::new();
        let mut rest = value;
        loop {
            let opener = |pair: &[u8]| pair == b"{{" || pair == b"#{";
            let Some(start) = rest.as_bytes().windows(2).position(opener) else {
                break;
            };
            let close = if rest.as_bytes()[start] == b'{' {
                "}}"
            } else {
                "}"
            };
            let body = &rest[start + 2..];
            let Some(end) = body.find(close) else {
                return Err(at.compile(format!("format string `{value}` is not closed")));
            };
            if start > 0 {
                parts.push(Part::Literal(rest[..start].into()));
            }
            parts.push(Part::Value(self.code(at, body[..end].trim())?));
            rest = &body[end + close.len()..];
        }
        if !rest.is_empty() {
            parts.push(Part::Literal(rest.into()));
        }
        Ok(parts)
    }
    fn text(&mut self, text: &str, cx: &Cx, out: &mut Vec<Op>) -> Result<(), ReportError> {
        if text.is_empty() {
            return Ok(());
        }
        let trimmed = text.trim();
        let translated = match self.language.term(&cx.template, trimmed) {
            Some(term) if cx.translate && !trimmed.is_empty() => {
                let start = text.len() - text.trim_start().len();
                let end = text.trim_end().len();
                format!("{}{}{}", &text[..start], term, &text[end..])
            }
            _ => text.into(),
        };
        self.count()?;
        match out.last_mut() {
            Some(Op::Text(last)) => last.push_str(&translated),
            _ => out.push(Op::Text(translated)),
        }
        Ok(())
    }
    /// The text and child elements of `node`, with `t-if` chains joined.
    /// Keep the original markup as numbered slots. A translator can reorder those slots,
    /// but cannot create markup, expressions, attributes, or duplicate an evaluated child.
    fn phrase(
        &mut self,
        node: NodeRef<'_>,
        cx: &Cx,
        deepest: &mut usize,
    ) -> Result<Option<Vec<Op>>, ReportError> {
        if !cx.translate
            || self.language.terms.is_empty() && self.language.template_terms.is_empty()
        {
            return Ok(None);
        }
        let kids: Vec<_> = node.children().collect();
        if kids.is_empty()
            || kids.iter().any(|kid| {
                !matches!(
                    kid.tag(),
                    "b" | "strong" | "i" | "em" | "u" | "span" | "small" | "sub" | "sup"
                ) || kid.attributes().any(|(name, _)| {
                    matches!(name, "t-if" | "t-elif" | "t-else" | "t-set" | "t-foreach")
                })
            })
        {
            return Ok(None);
        }
        let mut source = node.text().to_owned();
        for (index, kid) in kids.iter().enumerate() {
            use std::fmt::Write;
            let _ = write!(source, "{{{index}}}{}", kid.tail());
        }
        let Some(term) = self.language.term(&cx.template, source.trim()).cloned() else {
            return Ok(None);
        };
        let at = At {
            template: cx.template.clone(),
            line: node.origin().line(),
        };
        let mut out = Vec::new();
        let mut used = vec![false; kids.len()];
        let leading = source.len() - source.trim_start().len();
        let rest = format!(
            "{}{}{}",
            &source[..leading],
            term,
            &source[source.trim_end().len()..]
        );
        let mut input = rest.as_str();
        while let Some(start) = input.find('{') {
            let (text, after) = input.split_at(start);
            let Some((index, next)) = after[1..].split_once('}') else {
                return Err(at.compile("unclosed inline translation placeholder"));
            };
            let number = index
                .parse::<usize>()
                .ok()
                .filter(|idx| *idx < kids.len() && idx.to_string() == index)
                .ok_or_else(|| at.compile("invalid inline translation placeholder"))?;
            if std::mem::replace(&mut used[number], true) {
                return Err(at.compile("repeated inline translation placeholder"));
            }
            if !text.is_empty() {
                self.count()?;
                out.push(Op::Text(text.to_owned()));
            }
            self.element(kids[number], cx, false, &mut out, deepest)?;
            input = next;
        }
        if used.contains(&false) {
            return Err(at.compile("missing inline translation placeholder"));
        }
        if !input.is_empty() {
            self.count()?;
            out.push(Op::Text(input.to_owned()));
        }
        Ok(Some(out))
    }
    fn children(
        &mut self,
        node: NodeRef<'_>,
        cx: &Cx,
        deepest: &mut usize,
    ) -> Result<Vec<Op>, ReportError> {
        if let Some(phrase) = self.phrase(node, cx, deepest)? {
            return Ok(phrase);
        }
        let mut out = Vec::new();
        self.text(node.text(), cx, &mut out)?;
        let kids: Vec<NodeRef<'_>> = node.children().collect();
        let mut i = 0;
        while i < kids.len() {
            let kid = kids[i];
            let at = At {
                template: cx.template.clone(),
                line: kid.origin().line(),
            };
            if kid.attr("t-elif").is_some() || kid.attr("t-else").is_some() {
                return Err(at.compile("t-elif and t-else need a t-if on the element before"));
            }
            let chained = kid.attr("t-if").filter(|_| kid.attr("t-foreach").is_none());
            let Some(test) = chained else {
                self.element(kid, cx, false, &mut out, deepest)?;
                self.text(kid.tail(), cx, &mut out)?;
                i += 1;
                continue;
            };
            self.count()?;
            let mut branches = vec![Branch {
                test: Some(self.code(&at, test)?),
                body: self.branch(kid, cx, deepest)?,
            }];
            // White space between the branches belongs to none of them.
            while let Some(next) = kids.get(i + 1).copied() {
                let more = next.attr("t-elif").is_some() || next.attr("t-else").is_some();
                if !more || !kids[i].tail().trim().is_empty() {
                    break;
                }
                let at = At {
                    template: cx.template.clone(),
                    line: next.origin().line(),
                };
                if branches.last().is_some_and(|b| b.test.is_none()) {
                    return Err(at.compile("a branch follows t-else"));
                }
                let test = match next.attr("t-elif") {
                    Some(test) => Some(self.code(&at, test)?),
                    None => None,
                };
                branches.push(Branch {
                    test,
                    body: self.branch(next, cx, deepest)?,
                });
                i += 1;
            }
            out.push(Op::If { branches, at });
            self.text(kids[i].tail(), cx, &mut out)?;
            i += 1;
        }
        Ok(out)
    }
    fn branch(
        &mut self,
        node: NodeRef<'_>,
        cx: &Cx,
        deepest: &mut usize,
    ) -> Result<Vec<Op>, ReportError> {
        let mut body = Vec::new();
        self.element(node, cx, true, &mut body, deepest)?;
        Ok(body)
    }
    /// One element with its directives, in the reference's order: loop, condition, then the call,
    /// assignment or element itself. `tested` says the caller already compiled the condition.
    fn element(
        &mut self,
        node: NodeRef<'_>,
        cx: &Cx,
        tested: bool,
        out: &mut Vec<Op>,
        deepest: &mut usize,
    ) -> Result<(), ReportError> {
        if let Some(condition) = node.attr("groups") {
            match self.groups.get(condition) {
                Some(false) => return Ok(()),
                Some(true) => {}
                None => {
                    return Err(At {
                        template: cx.template.clone(),
                        line: node.origin().line(),
                    }
                    .compile(format!("missing report group decision for {condition}")));
                }
            }
        }

        let at = At {
            template: cx.template.clone(),
            line: node.origin().line(),
        };
        let cx = &Cx {
            template: cx.template.clone(),
            translate: cx.translate && node.attr("t-translation").is_none(),
            depth: cx.depth + 1,
        };
        self.depth(cx.depth)?;
        *deepest = (*deepest).max(cx.depth);
        self.count()?;
        if let Some(items) = node.attr("t-foreach") {
            let name = node
                .attr("t-as")
                .filter(|name| identifier(name))
                .ok_or_else(|| at.compile("t-foreach needs a t-as name"))?;
            let mut body = Vec::new();
            self.conditional(node, cx, &at, false, &mut body, deepest)?;
            out.push(Op::Foreach {
                items: self.code(&at, items)?,
                name: name.into(),
                body,
                at,
            });
            return Ok(());
        }
        if node.attr("t-as").is_some() {
            return Err(at.compile("t-as needs a t-foreach on the same element"));
        }
        self.conditional(node, cx, &at, tested, out, deepest)
    }
    fn conditional(
        &mut self,
        node: NodeRef<'_>,
        cx: &Cx,
        at: &At,
        tested: bool,
        out: &mut Vec<Op>,
        deepest: &mut usize,
    ) -> Result<(), ReportError> {
        match node.attr("t-if").filter(|_| !tested) {
            Some(test) => {
                let mut body = Vec::new();
                self.core(node, cx, at, &mut body, deepest)?;
                out.push(Op::If {
                    branches: vec![Branch {
                        test: Some(self.code(at, test)?),
                        body,
                    }],
                    at: at.clone(),
                });
                Ok(())
            }
            None => self.core(node, cx, at, out, deepest),
        }
    }
    fn core(
        &mut self,
        node: NodeRef<'_>,
        cx: &Cx,
        at: &At,
        out: &mut Vec<Op>,
        deepest: &mut usize,
    ) -> Result<(), ReportError> {
        let tag = node.tag();
        let only_on_t = |directive: &str| -> Result<(), ReportError> {
            if tag == "t" {
                Ok(())
            } else {
                Err(at.compile(format!(
                    "{directive} must be on a <t> element, not on <{tag}>"
                )))
            }
        };
        if let Some(target) = node.attr("t-call") {
            only_on_t("t-call")?;
            if target.contains(['{', '#']) {
                return Err(ReportError::Unsupported {
                    feature: "a t-call target computed at render time",
                    task: FORMATS_TASK,
                });
            }
            let template = self.template(target, at, cx.depth)?;
            *deepest = (*deepest).max(cx.depth + self.depths[template]);
            let mut arguments = Vec::new();
            for (name, value) in node.attributes() {
                let argument = match call_argument(node, name) {
                    // A `name.translate` text is a term of the language, as a text node is. A
                    // translation is data: it prints as written, never as an expression.
                    Some(true) if cx.translate && name.ends_with(".translate") => {
                        match self.language.term(&cx.template, value.trim()) {
                            Some(term) => Argument::Format(vec![Part::Literal(term.clone())]),
                            None => Argument::Format(self.format(at, value)?),
                        }
                    }
                    Some(true) => Argument::Format(self.format(at, value)?),
                    Some(false) => Argument::Value(self.code(at, value)?),
                    None => continue,
                };
                let name = name.split('.').next().unwrap_or(name);
                arguments.push((name.to_owned(), argument));
            }
            let lang = match node.attr("t-lang") {
                Some(lang) => Some(self.code(at, lang)?),
                None => None,
            };
            out.push(Op::Call {
                template,
                arguments,
                body: self.children(node, cx, deepest)?,
                lang,
                at: at.clone(),
            });
            return Ok(());
        }
        if node.attr("t-lang").is_some() {
            return Err(at.compile("t-lang is only read on the element that has t-call"));
        }
        if let Some(name) = node.attr("t-set") {
            only_on_t("t-set")?;
            if name.contains("__") {
                return Err(at.compile(format!("t-set name `{name}` contains `__`")));
            }
            let (value, body) = match node.attr("t-value") {
                Some(value) => (Some(self.code(at, value)?), Vec::new()),
                None => (None, self.children(node, cx, deepest)?),
            };
            out.push(Op::Set {
                name: name.into(),
                value,
                body,
                at: at.clone(),
            });
            return Ok(());
        }
        if node.attr("t-value").is_some() {
            return Err(at.compile("t-value needs a t-set on the same element"));
        }
        if node.attr("t-tag").is_some() {
            return Err(ReportError::Unsupported {
                feature: "t-tag",
                task: FORMATS_TASK,
            });
        }
        let options = match node.attr("t-options") {
            Some(options) => Some(self.code(at, options)?),
            None => None,
        };
        let out_value = node.attr("t-out").or_else(|| node.attr("t-esc"));
        let content = if let Some(field) = node.attr("t-field") {
            if tag == "t" {
                return Err(at.compile(
                    "t-field cannot be on a <t> element; put it on the element that shows the value",
                ));
            }
            if out_value.is_some() {
                return Err(at.compile("t-field and t-out on one element"));
            }
            let (record, name) = field
                .rsplit_once('.')
                .filter(|(_, name)| identifier(name))
                .ok_or_else(|| at.compile(format!("t-field `{field}` must be record.field")))?;
            Content::Field {
                record: self.code(at, record)?,
                field: name.into(),
                options,
                default: self.children(node, cx, deepest)?,
            }
        } else if let Some(value) = out_value {
            Content::Out {
                value: match value.trim() {
                    "0" => None,
                    value => Some(self.code(at, value)?),
                },
                options,
                default: self.children(node, cx, deepest)?,
            }
        } else {
            if options.is_some() {
                return Err(at.compile("t-options configures a t-out or t-field of its own element; this one has neither"));
            }
            Content::Children(self.children(node, cx, deepest)?)
        };
        let mut attrs = Vec::new();
        let mut dynamic = Vec::new();
        for (name, value) in node.attributes() {
            if let Some(target) = name.strip_prefix("t-attf-") {
                dynamic.push(Attr::Format {
                    name: target.into(),
                    parts: self.format(at, value)?,
                });
            } else if let Some(target) = name.strip_prefix("t-att-") {
                dynamic.push(Attr::Value {
                    name: target.into(),
                    code: self.code(at, value)?,
                });
            } else if name == "t-att" {
                dynamic.push(Attr::Mapping(self.code(at, value)?));
            } else if name != "groups" && !name.starts_with("t-") {
                attrs.push(Attr::Static {
                    name: name.into(),
                    value: value.into(),
                });
            }
        }
        let look = dynamic.is_empty().then(|| {
            Look::of(
                tag,
                attrs.iter().filter_map(|a| match a {
                    Attr::Static { name, value } => Some((name.as_str(), value.as_str())),
                    _ => None,
                }),
            )
        });
        match content {
            // A bare <t> is only a place for directives: its content stands where it stood.
            Content::Children(children) if tag == "t" && dynamic.is_empty() => {
                for op in children {
                    match (out.last_mut(), op) {
                        (Some(Op::Text(last)), Op::Text(text)) => last.push_str(&text),
                        (_, op) => out.push(op),
                    }
                }
            }
            content => {
                attrs.extend(dynamic);
                out.push(Op::Element(Box::new(Element {
                    tag: tag.into(),
                    attrs,
                    look,
                    content,
                    at: at.clone(),
                })));
            }
        }
        Ok(())
    }
}
