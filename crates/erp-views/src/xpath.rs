//! The XPath subset used by view inheritance specs.
//!
//! Grammar (anything else is [`ViewError::InvalidXpath`] naming the construct):
//!
//! ```text
//! path      := ('/' | '//')? step (('/' | '//') step)*  |  '.' (('/' | '//') step)*
//! step      := (Name | '*') predicate*
//! predicate := '[' Number ']'  |  '[' or ']'
//! or        := and ('or' and)*
//! and       := unary ('and' unary)*
//! unary     := 'not' '(' or ')'  |  '(' or ')'  |  '@' Name ('=' Literal)?
//!            | 'text' '(' ')' '=' Literal  |  'hasclass' '(' Literal (',' Literal)* ')'
//! ```

use crate::arch::{Arch, NodeId};
use crate::error::{LimitKind, ViewError, ViewResult, XpathErrorKind};
use crate::limits::{Budget, ViewLimits};
use std::collections::HashMap;

/// Attributes that hold translated text; with or without
/// the `t-attf-` prefix they may not be used as selectors.
const TRANSLATED_ATTRS: &[&str] = &[
    "string",
    "add-label",
    "help",
    "sum",
    "avg",
    "confirm",
    "placeholder",
    "alt",
    "title",
    "aria-label",
    "aria-keyshortcuts",
    "aria-placeholder",
    "aria-roledescription",
    "aria-valuetext",
    "value_label",
    "data-tooltip",
    "label",
    "confirm-label",
    "cancel-label",
    "confirm-title",
];

pub(crate) fn is_translated_attr(name: &str) -> bool {
    let base = match name.strip_prefix("t-attf-") {
        Some(base) => base,
        None => name,
    };
    TRANSLATED_ATTRS.contains(&base)
}

// --- tokens --------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tok<'a> {
    Slash,
    DoubleSlash,
    Dot,
    Star,
    At,
    Eq,
    Comma,
    LBracket,
    RBracket,
    LParen,
    RParen,
    Number(&'a str),
    Literal(&'a str),
    Name(&'a str),
}

impl Tok<'_> {
    fn spelled(&self) -> String {
        match self {
            Tok::Slash => "/".to_owned(),
            Tok::DoubleSlash => "//".to_owned(),
            Tok::Dot => ".".to_owned(),
            Tok::Star => "*".to_owned(),
            Tok::At => "@".to_owned(),
            Tok::Eq => "=".to_owned(),
            Tok::Comma => ",".to_owned(),
            Tok::LBracket => "[".to_owned(),
            Tok::RBracket => "]".to_owned(),
            Tok::LParen => "(".to_owned(),
            Tok::RParen => ")".to_owned(),
            Tok::Number(s) | Tok::Name(s) => (*s).to_owned(),
            Tok::Literal(s) => format!("'{s}'"),
        }
    }
}

fn is_name_start(c: char) -> bool {
    c.is_alphabetic() || c == '_'
}

fn is_name_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '_' | '-' | '.')
}

fn tokenize(expr: &str) -> Result<Vec<Tok<'_>>, XpathErrorKind> {
    let mut toks = Vec::new();
    let mut i = 0;
    while let Some(c) = expr.get(i..).and_then(|rest| rest.chars().next()) {
        if c.is_whitespace() {
            i += c.len_utf8();
            continue;
        }
        let next = expr.as_bytes().get(i + 1).copied();
        if let Some(what) = unsupported(c, next) {
            return Err(XpathErrorKind::Unsupported(what));
        }
        let (tok, len) = match punctuation(c, next) {
            Some(punct) => punct,
            None => word(expr, i, c)?,
        };
        toks.push(tok);
        i += len;
    }
    Ok(toks)
}

/// Constructs outside the grammar that one or two characters identify.
fn unsupported(c: char, next: Option<u8>) -> Option<&'static str> {
    match (c, next) {
        ('.', Some(b'.')) => Some("the parent step '..'"),
        ('.', Some(d)) if d.is_ascii_digit() => Some("decimal numbers"),
        ('!', Some(b'=')) => Some("the '!=' comparison"),
        ('<' | '>', _) => Some("comparison operators '<', '<=', '>', '>='"),
        ('|', _) => Some("the '|' union operator"),
        (':', Some(b':')) => Some("axes ('::')"),
        (':', _) => Some("namespace prefixes"),
        ('$', _) => Some("variables"),
        ('+' | '-', _) => Some("arithmetic operators"),
        // Everything else is a token or an unexpected character, decided by the callers below.
        _ => None,
    }
}

fn punctuation(c: char, next: Option<u8>) -> Option<(Tok<'static>, usize)> {
    let tok = match c {
        '/' if next == Some(b'/') => return Some((Tok::DoubleSlash, 2)),
        '/' => Tok::Slash,
        '.' => Tok::Dot,
        '*' => Tok::Star,
        '@' => Tok::At,
        '=' => Tok::Eq,
        ',' => Tok::Comma,
        '[' => Tok::LBracket,
        ']' => Tok::RBracket,
        '(' => Tok::LParen,
        ')' => Tok::RParen,
        // Not punctuation: a literal, number or name (or an error), see `word`.
        _ => return None,
    };
    Some((tok, 1))
}

/// A string literal, number or name starting at byte `i`.
fn word(expr: &str, i: usize, c: char) -> Result<(Tok<'_>, usize), XpathErrorKind> {
    let run_end = |keep: fn(char) -> bool| {
        expr[i..]
            .find(|ch: char| !keep(ch))
            .map_or(expr.len(), |k| i + k)
    };
    match c {
        '\'' | '"' => {
            let body = &expr[i + 1..];
            let end = body.find(c).ok_or(XpathErrorKind::UnclosedString)?;
            Ok((Tok::Literal(&body[..end]), end + 2))
        }
        '0'..='9' => {
            let end = run_end(|ch| ch.is_ascii_digit());
            if expr.as_bytes().get(end) == Some(&b'.') {
                return Err(XpathErrorKind::Unsupported("decimal numbers"));
            }
            Ok((Tok::Number(&expr[i..end]), end - i))
        }
        c if is_name_start(c) => {
            let end = run_end(is_name_char);
            Ok((Tok::Name(&expr[i..end]), end - i))
        }
        other => Err(XpathErrorKind::UnexpectedChar(other)),
    }
}

// --- syntax tree ---------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Start {
    Document,
    ContextNode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Axis {
    /// `.` as the first step: the context node itself.
    SelfNode,
    /// `/name` or a relative first step: children of the context nodes.
    Child,
    /// `//name`: children of the context nodes or of any of their descendants.
    Descendant,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum NameTest {
    Any,
    Tag(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Pred {
    Position(usize),
    Test(Test),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Test {
    Attr(String),
    AttrEq(String, String),
    TextEq(String),
    HasClass(Vec<String>),
    Not(Box<Test>),
    And(Vec<Test>),
    Or(Vec<Test>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Step {
    axis: Axis,
    test: NameTest,
    preds: Vec<Pred>,
}

/// A parsed expression.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct XPath {
    start: Start,
    steps: Vec<Step>,
}

struct Parser<'a, 'l> {
    toks: Vec<Tok<'a>>,
    pos: usize,
    nesting: usize,
    limits: &'l ViewLimits,
}

/// Parse failure before it is tied to the expression text.
enum Fail {
    Syntax(XpathErrorKind),
    Limit(ViewError),
}

impl From<XpathErrorKind> for Fail {
    fn from(kind: XpathErrorKind) -> Self {
        Fail::Syntax(kind)
    }
}

type PResult<T> = Result<T, Fail>;

impl<'a> Parser<'a, '_> {
    fn peek(&self) -> Option<Tok<'a>> {
        self.toks.get(self.pos).copied()
    }

    fn peek2(&self) -> Option<Tok<'a>> {
        self.toks.get(self.pos + 1).copied()
    }

    fn bump(&mut self) -> Option<Tok<'a>> {
        let t = self.peek();
        if t.is_some() {
            self.pos += 1;
        }
        t
    }

    fn expect(&mut self, want: Tok<'static>, expected: &'static str) -> PResult<()> {
        match self.bump() {
            Some(t) if t == want => Ok(()),
            other => Err(unexpected(other, expected)),
        }
    }

    fn literal(&mut self) -> PResult<String> {
        match self.bump() {
            Some(Tok::Literal(s)) => Ok(s.to_owned()),
            other => Err(unexpected(other, "a quoted string")),
        }
    }

    /// The start of the path: where evaluation begins and the axis of the first named step.
    /// `None` when the path is `.` alone (already pushed as a self step).
    fn start(&mut self, steps: &mut Vec<Step>) -> PResult<Option<(Start, Axis)>> {
        match self.peek() {
            Some(Tok::Slash) => {
                self.bump();
                Ok(Some((Start::Document, Axis::Child)))
            }
            Some(Tok::DoubleSlash) => {
                self.bump();
                Ok(Some((Start::Document, Axis::Descendant)))
            }
            Some(Tok::Dot) => {
                self.bump();
                steps.push(Step {
                    axis: Axis::SelfNode,
                    test: NameTest::Any,
                    preds: Vec::new(),
                });
                match self.bump() {
                    None => Ok(None),
                    Some(Tok::Slash) => Ok(Some((Start::ContextNode, Axis::Child))),
                    Some(Tok::DoubleSlash) => Ok(Some((Start::ContextNode, Axis::Descendant))),
                    Some(Tok::LBracket) => {
                        Err(XpathErrorKind::Unsupported("predicates on '.'").into())
                    }
                    other => Err(unexpected(other, "'/' or '//' after '.'")),
                }
            }
            // A relative path starts with its first step; `step` validates the token.
            _ => Ok(Some((Start::ContextNode, Axis::Child))),
        }
    }

    fn path(&mut self) -> PResult<XPath> {
        let mut steps = Vec::new();
        let Some((start, mut axis)) = self.start(&mut steps)? else {
            return Ok(XPath {
                start: Start::ContextNode,
                steps,
            });
        };
        let max_steps = self.limits.get(LimitKind::XpathSteps);
        loop {
            if steps.len() >= max_steps {
                return Err(Fail::Limit(
                    self.limits
                        .exceeded(LimitKind::XpathSteps, steps.len().checked_add(1)),
                ));
            }
            steps.push(self.step(axis)?);
            axis = match self.bump() {
                None => break,
                Some(Tok::Slash) => Axis::Child,
                Some(Tok::DoubleSlash) => Axis::Descendant,
                other => return Err(unexpected(other, "'/', '//' or '['")),
            };
        }
        Ok(XPath { start, steps })
    }

    fn step(&mut self, axis: Axis) -> PResult<Step> {
        let test = match self.bump() {
            Some(Tok::Name(name)) if self.peek() == Some(Tok::LParen) => {
                return Err(XpathErrorKind::UnsupportedFunction(name.to_owned()).into());
            }
            Some(Tok::Name(name)) => NameTest::Tag(name.to_owned()),
            Some(Tok::Star) => NameTest::Any,
            Some(Tok::Dot) => {
                return Err(XpathErrorKind::Unsupported("'.' after the first step").into());
            }
            Some(Tok::At) => {
                return Err(XpathErrorKind::Unsupported("selecting attributes as a step").into());
            }
            other => return Err(unexpected(other, "a tag name or '*'")),
        };
        let mut preds = Vec::new();
        while self.peek() == Some(Tok::LBracket) {
            self.bump();
            preds.push(self.predicate()?);
            self.expect(Tok::RBracket, "']'")?;
        }
        Ok(Step { axis, test, preds })
    }
    fn predicate(&mut self) -> PResult<Pred> {
        if let (Some(Tok::Number(n)), Some(Tok::RBracket)) = (self.peek(), self.peek2()) {
            self.bump();
            return match n.parse::<usize>() {
                Ok(index) if index >= 1 => Ok(Pred::Position(index)),
                _ => Err(XpathErrorKind::InvalidIndex(n.to_owned()).into()),
            };
        }
        Ok(Pred::Test(self.or()?))
    }

    fn nested<T>(&mut self, inner: impl FnOnce(&mut Self) -> PResult<T>) -> PResult<T> {
        self.nesting += 1;
        let max = self.limits.get(LimitKind::XpathNesting);
        if self.nesting > max {
            return Err(Fail::Limit(
                self.limits
                    .exceeded(LimitKind::XpathNesting, Some(self.nesting)),
            ));
        }
        let result = inner(self);
        self.nesting -= 1;
        result
    }

    fn or(&mut self) -> PResult<Test> {
        let mut terms = vec![self.and()?];
        while self.peek() == Some(Tok::Name("or")) {
            self.bump();
            terms.push(self.and()?);
        }
        Ok(if terms.len() == 1 {
            terms.remove(0)
        } else {
            Test::Or(terms)
        })
    }

    fn and(&mut self) -> PResult<Test> {
        let mut terms = vec![self.unary()?];
        while self.peek() == Some(Tok::Name("and")) {
            self.bump();
            terms.push(self.unary()?);
        }
        Ok(if terms.len() == 1 {
            terms.remove(0)
        } else {
            Test::And(terms)
        })
    }

    fn unary(&mut self) -> PResult<Test> {
        let followed_by_paren = self.peek2() == Some(Tok::LParen);
        match self.bump() {
            Some(Tok::Name("not")) if followed_by_paren => {
                self.bump();
                let inner = self.grouped("')' closing not(")?;
                Ok(Test::Not(Box::new(inner)))
            }
            Some(Tok::LParen) => self.grouped("')'"),
            Some(Tok::At) => self.attribute_test(),
            Some(Tok::Name("text")) if followed_by_paren => {
                self.bump();
                self.expect(Tok::RParen, "')' after text(")?;
                self.expect(Tok::Eq, "'=' after text()")?;
                Ok(Test::TextEq(self.literal()?))
            }
            Some(Tok::Name("hasclass")) if followed_by_paren => {
                self.bump();
                self.hasclass_test()
            }
            Some(Tok::Name(name)) if followed_by_paren => {
                Err(XpathErrorKind::UnsupportedFunction(name.to_owned()).into())
            }
            Some(Tok::Name(_) | Tok::Dot | Tok::Slash | Tok::DoubleSlash | Tok::Star) => {
                Err(XpathErrorKind::Unsupported("path expressions inside predicates").into())
            }
            Some(Tok::Number(_)) => Err(XpathErrorKind::PositionInsideExpression.into()),
            Some(Tok::Literal(_)) => {
                Err(XpathErrorKind::Unsupported("a string literal outside a comparison").into())
            }
            other => Err(unexpected(other, "a predicate")),
        }
    }

    /// The inside of `(...)` or `not(...)`, after the opening parenthesis.
    fn grouped(&mut self, closing: &'static str) -> PResult<Test> {
        self.nested(|p| {
            let t = p.or()?;
            p.expect(Tok::RParen, closing)?;
            Ok(t)
        })
    }

    /// `@name` or `@name='value'`, after the `@`.
    fn attribute_test(&mut self) -> PResult<Test> {
        let name = match self.bump() {
            Some(Tok::Name(n)) => n.to_owned(),
            Some(Tok::Star) => return Err(XpathErrorKind::Unsupported("'@*'").into()),
            other => return Err(unexpected(other, "an attribute name after '@'")),
        };
        if self.peek() == Some(Tok::Eq) {
            self.bump();
            Ok(Test::AttrEq(name, self.literal()?))
        } else {
            Ok(Test::Attr(name))
        }
    }

    /// The arguments of `hasclass(`, after its opening parenthesis.
    fn hasclass_test(&mut self) -> PResult<Test> {
        if self.peek() == Some(Tok::RParen) {
            return Err(XpathErrorKind::HasClassWithoutArguments.into());
        }
        let mut classes = vec![self.literal()?];
        while self.peek() == Some(Tok::Comma) {
            self.bump();
            classes.push(self.literal()?);
        }
        self.expect(Tok::RParen, "')' closing hasclass(")?;
        Ok(Test::HasClass(classes))
    }
}

/// The error for a token (or the end) where the grammar expected something else.
fn unexpected(found: Option<Tok<'_>>, expected: &'static str) -> Fail {
    match found {
        Some(t) => XpathErrorKind::UnexpectedToken {
            expected,
            found: t.spelled(),
        }
        .into(),
        None => XpathErrorKind::UnexpectedEnd { expected }.into(),
    }
}

impl XPath {
    /// Parses `expr` against the grammar and the XPath limits.
    pub(crate) fn parse(expr: &str, limits: &ViewLimits) -> ViewResult<XPath> {
        if expr.len() > limits.get(LimitKind::XpathLength) {
            return Err(limits.exceeded(LimitKind::XpathLength, Some(expr.len())));
        }
        let invalid = |kind| ViewError::InvalidXpath {
            expr: expr.to_owned(),
            kind,
        };
        let toks = tokenize(expr).map_err(invalid)?;
        if toks.is_empty() {
            return Err(invalid(XpathErrorKind::Empty));
        }
        let mut parser = Parser {
            toks,
            pos: 0,
            nesting: 0,
            limits,
        };
        parser.path().map_err(|fail| match fail {
            Fail::Syntax(kind) => invalid(kind),
            Fail::Limit(err) => err,
        })
    }

    /// The first translated attribute used as a selector, in expression order.
    pub(crate) fn translated_selector(&self) -> Option<&str> {
        fn in_test(t: &Test) -> Option<&str> {
            match t {
                Test::Attr(a) | Test::AttrEq(a, _) => is_translated_attr(a).then_some(a.as_str()),
                Test::TextEq(_) | Test::HasClass(_) => None,
                Test::Not(inner) => in_test(inner),
                Test::And(ts) | Test::Or(ts) => ts.iter().find_map(in_test),
            }
        }
        self.steps
            .iter()
            .flat_map(|s| s.preds.iter())
            .find_map(|p| match p {
                Pred::Position(_) => None,
                Pred::Test(t) => in_test(t),
            })
    }

    /// The first matching node in document order.
    ///
    /// # Errors
    ///
    /// [`ViewError::LimitExceeded`] when the evaluation steps run out (see [`Budget`]).
    pub(crate) fn first(&self, arch: &Arch, budget: &mut Budget) -> ViewResult<Option<NodeId>> {
        Ok(self.select(arch, true, budget)?.into_iter().next())
    }

    /// All matching nodes in document order.
    #[cfg(test)]
    pub(crate) fn all(&self, arch: &Arch, budget: &mut Budget) -> ViewResult<Vec<NodeId>> {
        self.select(arch, false, budget)
    }

    /// Evaluates the steps one walk of the tree each. A step marks the nodes it selects; the next
    /// step reads those marks. Because each walk is in document order, the last step can stop at
    /// its first match when only the first result is wanted.
    fn select(
        &self,
        arch: &Arch,
        first_only: bool,
        budget: &mut Budget,
    ) -> ViewResult<Vec<NodeId>> {
        let root = arch.root_id();
        let mut context = Context {
            marks: budget.slot_table(arch.slots())?,
            document: self.start == Start::Document,
        };
        if self.start == Start::ContextNode {
            context.marks[root.index()] = true;
        }
        let mut results = Vec::new();
        let count = self.steps.len();
        for (i, step) in self.steps.iter().enumerate() {
            let last = i + 1 == count;
            if step.axis == Axis::SelfNode {
                // Only produced as the first step, on the root context.
                if last {
                    results.push(root);
                }
                continue;
            }
            if last {
                step.walk_matches(arch, &context, budget, |id| {
                    results.push(id);
                    !first_only
                })?;
            } else {
                let mut marks = budget.slot_table(arch.slots())?;
                step.walk_matches(arch, &context, budget, |id| {
                    marks[id.index()] = true;
                    true
                })?;
                context = Context {
                    marks,
                    document: false,
                };
            }
        }
        Ok(results)
    }
}

/// The nodes selected by the previous step (or the start of the path).
struct Context {
    marks: Vec<bool>,
    /// The document node itself is in the context (absolute paths).
    document: bool,
}

impl Step {
    /// Walks the tree once in document order and calls `visit` with each node the step selects
    /// from `context`; `visit` returns `false` to stop. Every visited node costs one step of
    /// `budget`; name tests and predicates are charged by [`Step::matches`].
    fn walk_matches(
        &self,
        arch: &Arch,
        context: &Context,
        budget: &mut Budget,
        mut visit: impl FnMut(NodeId) -> bool,
    ) -> ViewResult<()> {
        let mut positions: HashMap<(Option<NodeId>, usize), usize> = HashMap::new();
        // Depth of the shallowest context node on the current path, while inside its subtree.
        let mut inside_context: Option<usize> = None;
        for (id, depth) in arch.walk(arch.root_id()) {
            budget.charge()?;
            if inside_context.is_some_and(|d| depth <= d) {
                inside_context = None;
            }
            if arch.is_sentinel(id) {
                continue;
            }
            let parent = arch.parent(id);
            let candidate = match self.axis {
                Axis::Child => match parent {
                    Some(p) => context.marks[p.index()],
                    None => context.document,
                },
                Axis::Descendant => context.document || inside_context.is_some(),
                Axis::SelfNode => false,
            };
            if candidate && self.matches(arch, id, parent, &mut positions, budget)? && !visit(id) {
                return Ok(());
            }
            if inside_context.is_none() && context.marks[id.index()] {
                inside_context = Some(depth);
            }
        }
        Ok(())
    }

    fn matches(
        &self,
        arch: &Arch,
        id: NodeId,
        parent: Option<NodeId>,
        positions: &mut HashMap<(Option<NodeId>, usize), usize>,
        budget: &mut Budget,
    ) -> ViewResult<bool> {
        if let NameTest::Tag(tag) = &self.test
            && !budget.str_eq(arch.tag(id), tag)?
        {
            return Ok(false);
        }
        for (k, pred) in self.preds.iter().enumerate() {
            match pred {
                Pred::Position(n) => {
                    let seen = positions.entry((parent, k)).or_insert(0);
                    *seen = seen.saturating_add(1);
                    if *seen != *n {
                        return Ok(false);
                    }
                }
                Pred::Test(t) => {
                    if !t.holds(arch, id, budget)? {
                        return Ok(false);
                    }
                }
            }
        }
        Ok(true)
    }
}

impl Test {
    /// Evaluates the test on `id`. Each test node (leaf, `not`, `and`, `or`) costs one step; leaves
    /// add their work: attribute names
    /// scanned, values compared, children visited for `text()`, the class value split once per
    /// class name for `hasclass`.
    fn holds(&self, arch: &Arch, id: NodeId, budget: &mut Budget) -> ViewResult<bool> {
        budget.charge()?;
        match self {
            Test::Not(inner) => Ok(!inner.holds(arch, id, budget)?),
            Test::And(ts) => {
                for t in ts {
                    if !t.holds(arch, id, budget)? {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
            Test::Or(ts) => {
                for t in ts {
                    if t.holds(arch, id, budget)? {
                        return Ok(true);
                    }
                }
                Ok(false)
            }
            Test::Attr(a) => Ok(arch.attr_charged(id, a, budget)?.is_some()),
            Test::AttrEq(a, v) => match arch.attr_charged(id, a, budget)? {
                Some(have) => budget.str_eq(have, v),
                None => Ok(false),
            },
            Test::TextEq(v) => text_equals(arch, id, v, budget),
            Test::HasClass(classes) => {
                let Some(attr) = arch.attr_charged(id, "class", budget)? else {
                    return Ok(false);
                };
                for class in classes {
                    budget.charge_bytes(attr.len())?;
                    if !attr.split_whitespace().any(|have| have == class) {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
        }
    }
}

/// XPath 1.0's `text()='v'` node-set comparison: true when the element's text or one of its
/// children's tails equals `v`. Every child visited costs a step, every comparison its bytes.
fn text_equals(arch: &Arch, id: NodeId, v: &str, budget: &mut Budget) -> ViewResult<bool> {
    let text = arch.text(id);
    if !text.is_empty() && budget.str_eq(text, v)? {
        return Ok(true);
    }
    let mut child = arch.first_child(id);
    while let Some(c) = child {
        budget.charge()?;
        let tail = arch.tail(c);
        if !tail.is_empty() && budget.str_eq(tail, v)? {
            return Ok(true);
        }
        child = arch.next_sibling(c);
    }
    Ok(false)
}

/// The text node children of an element: its text and the tails of its children (lxml has no
/// empty text nodes).
pub(crate) fn text_nodes(arch: &Arch, id: NodeId) -> impl Iterator<Item = &str> {
    std::iter::once(arch.text(id))
        .chain(
            std::iter::successors(arch.first_child(id), move |&c| arch.next_sibling(c))
                .map(move |c| arch.tail(c)),
        )
        .filter(|t| !t.is_empty())
}
