//! Template expressions (design note §6.2, §6.4, §7.1): pre-lexed, parsed by `oxc_parser`,
//! checked by an explicit grammar walk that converts the accepted subset into our own owned tree,
//! and re-printed from that tree. Source text never reaches a generated module: identifiers are
//! resolved to generated names, strings are re-escaped and compound operands are parenthesised.

use crate::diag::DiagnosticKind;
use crate::jsstr::{Quote, push_escaped, push_js_string};
use crate::limits::{CompilerLimits, LimitHit, LimitKind};
use crate::names::is_plain_identifier;
use crate::prelex::{PrelexError, prelex};
use crate::source::{BuildMode, Trust};
use oxc_allocator::Allocator;
use oxc_ast::ast::{
    Argument, ArrayExpressionElement, ArrowFunctionExpression, AssignmentTarget, BindingPattern,
    ChainElement, Expression, MemberExpression, ObjectPropertyKind, PropertyKey, PropertyKind,
    SimpleAssignmentTarget, Statement,
};
use oxc_parser::Parser;
use oxc_span::SourceType;
use oxc_syntax::operator::{BinaryOperator, UnaryOperator};

/// Index of a template local in the analyser's table of the template being analysed.
pub(crate) type LocalId = u32;

/// Globals an expression may read (design note §6.4).
pub(crate) const GLOBALS: [&str; 12] = [
    "Array",
    "Boolean",
    "Date",
    "Infinity",
    "JSON",
    "Math",
    "NaN",
    "Number",
    "Object",
    "RegExp",
    "String",
    "undefined",
];

/// An accepted expression.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Expr {
    Null,
    Bool(bool),
    /// A numeric literal as written (validated to `[0-9A-Za-z_.]`).
    Num(Box<str>),
    /// A BigInt literal as written, with its `n`.
    BigInt(Box<str>),
    Str(Box<str>),
    /// Untagged template literal: cooked parts (one more than the expressions) and expressions.
    Template(Vec<Box<str>>, Vec<Expr>),
    This,
    Local(LocalId),
    /// An arrow-function parameter.
    Param(Box<str>),
    Global(&'static str),
    /// A name no rule binds: a caller's local when the template is called with `t-call`
    /// (design note §6.4); the analyser decides.
    Free(Box<str>),
    Array(Vec<Elem>),
    Object(Vec<Prop>),
    Member(Box<Expr>, Box<str>, bool),
    Index(Box<Expr>, Box<Expr>, bool),
    Call(Box<Expr>, Vec<Elem>, bool),
    /// An optional chain (`a?.b.c`), printed in parentheses so it ends where the source's ended.
    Chain(Box<Expr>),
    Unary(&'static str, Box<Expr>),
    Binary(&'static str, Box<Expr>, Box<Expr>),
    Cond(Box<Expr>, Box<Expr>, Box<Expr>),
    Assign(&'static str, Box<Expr>, Box<Expr>),
    Update(&'static str, bool, Box<Expr>),
    Arrow(Vec<Box<str>>, ArrowBody),
}

/// An array element or call argument.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Elem {
    Expr(Expr),
    Spread(Expr),
    Hole,
}

/// An object literal member.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Prop {
    Init(Key, Expr),
    Spread(Expr),
}

/// An object literal key.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Key {
    Name(Box<str>),
    Num(Box<str>),
    Computed(Expr),
}

/// An arrow function body.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ArrowBody {
    Expr(Box<Expr>),
    /// A block of expression statements.
    Block(Vec<Expr>),
}

/// A constant an expression folds to (design note §7.1: literals only).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Constant<'e> {
    Null,
    Bool(bool),
    Str(&'e str),
}

impl Expr {
    /// The constant this expression is, when it is a string, boolean or `null` literal (a
    /// parenthesised one included). `undefined` is a global, not folded.
    pub(crate) fn constant(&self) -> Option<Constant<'_>> {
        match self {
            Expr::Null => Some(Constant::Null),
            Expr::Bool(b) => Some(Constant::Bool(*b)),
            Expr::Str(s) => Some(Constant::Str(s)),
            _ => None,
        }
    }

    /// Calls `f` on every free name the expression reads.
    pub(crate) fn for_each_free<'e>(&'e self, f: &mut dyn FnMut(&'e str)) {
        fn go<'e>(e: &'e Expr, f: &mut dyn FnMut(&'e str)) {
            if let Expr::Free(name) = e {
                f(name);
            }
            e.children(&mut |c| go(c, f));
        }
        go(self, f);
    }

    fn children<'e>(&'e self, f: &mut dyn FnMut(&'e Expr)) {
        let elems = |items: &'e [Elem], f: &mut dyn FnMut(&'e Expr)| {
            for item in items {
                match item {
                    Elem::Expr(e) | Elem::Spread(e) => f(e),
                    Elem::Hole => {}
                }
            }
        };
        match self {
            Expr::Null
            | Expr::Bool(_)
            | Expr::Num(_)
            | Expr::BigInt(_)
            | Expr::Str(_)
            | Expr::This
            | Expr::Local(_)
            | Expr::Param(_)
            | Expr::Global(_)
            | Expr::Free(_) => {}
            Expr::Template(_, exprs) => exprs.iter().for_each(&mut *f),
            Expr::Array(items) => elems(items, f),
            Expr::Object(props) => {
                for prop in props {
                    match prop {
                        Prop::Init(Key::Computed(k), v) => {
                            f(k);
                            f(v);
                        }
                        Prop::Init(_, v) | Prop::Spread(v) => f(v),
                    }
                }
            }
            Expr::Member(o, _, _) | Expr::Chain(o) | Expr::Unary(_, o) | Expr::Update(_, _, o) => {
                f(o);
            }
            Expr::Index(a, b, _) | Expr::Binary(_, a, b) | Expr::Assign(_, a, b) => {
                f(a);
                f(b);
            }
            Expr::Call(callee, args, _) => {
                f(callee);
                elems(args, f);
            }
            Expr::Cond(a, b, c) => {
                f(a);
                f(b);
                f(c);
            }
            Expr::Arrow(_, ArrowBody::Expr(e)) => f(e),
            Expr::Arrow(_, ArrowBody::Block(stmts)) => stmts.iter().for_each(&mut *f),
        }
    }
}

/// Where an expression is evaluated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExprContext {
    /// While rendering: no assignment or update outside arrow bodies.
    Render,
    /// A `t-on-*` handler: evaluated when the event fires.
    Handler,
}

/// What an expression may see.
pub(crate) struct ExprEnv<'e> {
    /// Resolves a template local by name.
    pub(crate) locals: &'e dyn Fn(&str) -> Option<LocalId>,
    pub(crate) mode: BuildMode,
    pub(crate) context: ExprContext,
    /// The trust mode of the template (design note §6.4): decides what `this` is.
    pub(crate) trust: Trust,
}

/// Why an expression was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ExprError {
    pub(crate) kind: DiagnosticKind,
    pub(crate) message: String,
    /// The limit hit, for a [`DiagnosticKind::Limit`] error.
    pub(crate) limit: Option<LimitHit>,
}

impl ExprError {
    fn new(kind: DiagnosticKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            limit: None,
        }
    }

    fn limit(hit: LimitHit, message: String) -> Self {
        Self {
            kind: DiagnosticKind::Limit,
            message,
            limit: Some(hit),
        }
    }

    fn forbidden(what: &str) -> Self {
        Self::new(
            DiagnosticKind::ForbiddenSyntax,
            format!("{what} is not allowed in template expressions"),
        )
    }
}

/// Parses expressions with one arena reused across a whole compile (design note §14).
pub(crate) struct ExprParser {
    arena: Allocator,
    max_bytes: usize,
    max_nesting: usize,
}

impl ExprParser {
    pub(crate) fn new(limits: &CompilerLimits) -> Self {
        Self {
            arena: Allocator::new(),
            max_bytes: limits.get(LimitKind::ExpressionBytes),
            max_nesting: limits.get(LimitKind::ExpressionNesting),
        }
    }

    /// The `expression_bytes` limit, for format strings and dynamic names checked by the caller.
    pub(crate) fn max_bytes(&self) -> usize {
        self.max_bytes
    }

    /// Parses and checks one expression.
    pub(crate) fn parse(&mut self, src: &str, env: &ExprEnv<'_>) -> Result<Expr, ExprError> {
        if src.len() > self.max_bytes {
            let hit = LimitHit::new(LimitKind::ExpressionBytes, self.max_bytes, src.len());
            return Err(ExprError::limit(hit, hit.message()));
        }
        if src.trim().is_empty() {
            return Err(ExprError::new(
                DiagnosticKind::ExpressionSyntax,
                "empty expression",
            ));
        }
        let pre = prelex(src, self.max_nesting).map_err(|e| match e {
            PrelexError::Comment => ExprError::forbidden("a comment"),
            PrelexError::Unterminated(what) => ExprError::new(
                DiagnosticKind::ExpressionSyntax,
                format!("unterminated {what}"),
            ),
            PrelexError::TooDeep { max, actual } => {
                let hit = LimitHit::new(LimitKind::ExpressionNesting, max, actual);
                ExprError::limit(hit, hit.message())
            }
        })?;
        self.arena.reset();
        let parsed = Parser::new(&self.arena, &pre.text, SourceType::mjs()).parse_expression();
        let ast = match parsed {
            Ok(ast) => ast,
            Err(diagnostics) => {
                let message = diagnostics
                    .first()
                    .map_or_else(|| "invalid expression".to_owned(), ToString::to_string);
                return Err(ExprError::new(DiagnosticKind::ExpressionSyntax, message));
            }
        };
        let mut walk = Walk {
            env,
            params: Vec::new(),
            depth: 0,
            max_depth: self.max_nesting.saturating_mul(4),
            arrows: 0,
        };
        walk.expr(&ast)
    }
}

struct Walk<'w, 'e> {
    env: &'w ExprEnv<'e>,
    /// Arrow parameters in scope, innermost last.
    params: Vec<Box<str>>,
    depth: usize,
    max_depth: usize,
    /// Arrow bodies we are inside of.
    arrows: usize,
}

impl Walk<'_, '_> {
    /// One syntax-tree level: every expression node counts once.
    fn enter(&mut self) -> Result<(), ExprError> {
        self.depth += 1;
        if self.depth > self.max_depth {
            // The walk's own bound: four syntax-tree levels per bracket level of the limit. The
            // typed values are the walk's (maximum and reached tree depth).
            let hit = LimitHit::new(LimitKind::ExpressionNesting, self.max_depth, self.depth);
            return Err(ExprError::limit(
                hit,
                format!(
                    "expression_nesting limit exceeded: the syntax tree is deeper than {} levels (four per allowed bracket level)",
                    self.max_depth
                ),
            ));
        }
        Ok(())
    }

    fn writes_allowed(&self) -> bool {
        self.arrows > 0 || self.env.context == ExprContext::Handler
    }

    fn expr(&mut self, e: &Expression<'_>) -> Result<Expr, ExprError> {
        self.enter()?;
        let out = self.expr_inner(e);
        self.depth -= 1;
        out
    }

    fn boxed(&mut self, e: &Expression<'_>) -> Result<Box<Expr>, ExprError> {
        self.expr(e).map(Box::new)
    }

    /// The node kinds deep expressions are made of (member chains, operators, calls, branches),
    /// kept apart from the others so that this frame, which every syntax-tree level of such a
    /// chain stacks, stays small: in an unoptimised build one function holding every arm took
    /// about 12 KiB per level, and 255 members overflowed a 2 MiB test-thread stack.
    fn expr_inner(&mut self, e: &Expression<'_>) -> Result<Expr, ExprError> {
        Ok(match e {
            Expression::Identifier(id) => self.identifier(id.name.as_str())?,
            Expression::ThisExpression(_) => match self.env.trust {
                // Trusted templates see their component (restricted mode, will
                // not expose it).
                Trust::Trusted => Expr::This,
            },
            Expression::BinaryExpression(b) => {
                if b.operator == BinaryOperator::Instanceof {
                    return Err(ExprError::forbidden("the 'instanceof' operator"));
                }
                Expr::Binary(
                    b.operator.as_str(),
                    self.boxed(&b.left)?,
                    self.boxed(&b.right)?,
                )
            }
            Expression::LogicalExpression(l) => Expr::Binary(
                l.operator.as_str(),
                self.boxed(&l.left)?,
                self.boxed(&l.right)?,
            ),
            Expression::UnaryExpression(u) => match u.operator {
                UnaryOperator::Void | UnaryOperator::Delete => {
                    return Err(ExprError::forbidden(&format!(
                        "the '{}' operator",
                        u.operator.as_str()
                    )));
                }
                UnaryOperator::Typeof => Expr::Unary("typeof ", self.boxed(&u.argument)?),
                op => Expr::Unary(op.as_str(), self.boxed(&u.argument)?),
            },
            Expression::ConditionalExpression(c) => Expr::Cond(
                self.boxed(&c.test)?,
                self.boxed(&c.consequent)?,
                self.boxed(&c.alternate)?,
            ),
            Expression::CallExpression(c) => self.call(c)?,
            Expression::ChainExpression(c) => {
                let inner = match &c.expression {
                    ChainElement::CallExpression(call) => self.call(call)?,
                    other => match other.as_member_expression() {
                        Some(m) => self.member(m, true)?,
                        None => return Err(ExprError::forbidden("this optional chain")),
                    },
                };
                Expr::Chain(Box::new(inner))
            }
            Expression::ParenthesizedExpression(p) => self.expr_inner(&p.expression)?,
            Expression::StaticMemberExpression(_)
            | Expression::ComputedMemberExpression(_)
            | Expression::PrivateFieldExpression(_) => match e.as_member_expression() {
                Some(m) => self.member(m, true)?,
                None => return Err(ExprError::forbidden("this member expression")),
            },
            // Arrow and assignment chains nest too (`a => b => …`, `x = y = …`); their work is
            // in small helpers of their own (round-two review L2).
            Expression::ArrowFunctionExpression(f) => self.arrow(f)?,
            Expression::AssignmentExpression(a) => self.assign(a)?,
            _ => self.expr_cold(e)?,
        })
    }

    /// Every other node kind: literals, arrays, objects, updates and the forbidden forms.
    #[inline(never)]
    fn expr_cold(&mut self, e: &Expression<'_>) -> Result<Expr, ExprError> {
        Ok(match e {
            Expression::Identifier(_)
            | Expression::ThisExpression(_)
            | Expression::BinaryExpression(_)
            | Expression::LogicalExpression(_)
            | Expression::UnaryExpression(_)
            | Expression::ConditionalExpression(_)
            | Expression::CallExpression(_)
            | Expression::ChainExpression(_)
            | Expression::ParenthesizedExpression(_)
            | Expression::StaticMemberExpression(_)
            | Expression::ComputedMemberExpression(_)
            | Expression::PrivateFieldExpression(_)
            | Expression::ArrowFunctionExpression(_)
            | Expression::AssignmentExpression(_) => {
                return Err(ExprError::new(
                    DiagnosticKind::Internal,
                    "internal compiler error: an expression kind of the hot path reached the cold path",
                ));
            }
            Expression::BooleanLiteral(b) => Expr::Bool(b.value),
            Expression::NullLiteral(_) => Expr::Null,
            Expression::NumericLiteral(n) => {
                let raw = n.raw.map(|r| r.as_str()).unwrap_or("");
                if raw.is_empty() {
                    return Err(ExprError::new(
                        DiagnosticKind::Internal,
                        "internal compiler error: numeric literal without its source text",
                    ));
                }
                if !is_numeric_literal_text(raw) {
                    return Err(ExprError::new(
                        DiagnosticKind::ExpressionSyntax,
                        format!("numeric literal '{raw}' is not in the accepted form"),
                    ));
                }
                Expr::Num(raw.into())
            }
            Expression::BigIntLiteral(n) => {
                let raw = n.raw.map(|r| r.as_str()).unwrap_or("");
                let safe = raw.ends_with('n')
                    && raw.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
                if !safe {
                    return Err(ExprError::new(
                        DiagnosticKind::ExpressionSyntax,
                        "BigInt literal without its source text",
                    ));
                }
                Expr::BigInt(raw.into())
            }
            Expression::RegExpLiteral(_) => {
                return Err(ExprError::forbidden("a regular expression literal"));
            }
            Expression::StringLiteral(s) => {
                if s.lone_surrogates {
                    return Err(ExprError::forbidden(
                        "a string with a lone surrogate escape",
                    ));
                }
                Expr::Str(s.value.as_str().into())
            }
            Expression::TemplateLiteral(t) => {
                let mut parts = Vec::with_capacity(t.quasis.len());
                for q in &t.quasis {
                    if q.lone_surrogates {
                        return Err(ExprError::forbidden(
                            "a template with a lone surrogate escape",
                        ));
                    }
                    let Some(cooked) = q.value.cooked else {
                        return Err(ExprError::new(
                            DiagnosticKind::ExpressionSyntax,
                            "invalid escape sequence in a template literal",
                        ));
                    };
                    parts.push(cooked.as_str().into());
                }
                let mut exprs = Vec::with_capacity(t.expressions.len());
                for x in &t.expressions {
                    exprs.push(self.expr(x)?);
                }
                if parts.len() != exprs.len() + 1 {
                    return Err(ExprError::new(
                        DiagnosticKind::ExpressionSyntax,
                        "malformed template literal",
                    ));
                }
                Expr::Template(parts, exprs)
            }
            Expression::ArrayExpression(a) => {
                let mut items = Vec::with_capacity(a.elements.len());
                for el in &a.elements {
                    items.push(match el {
                        ArrayExpressionElement::SpreadElement(s) => {
                            Elem::Spread(self.expr(&s.argument)?)
                        }
                        ArrayExpressionElement::Elision(_) => Elem::Hole,
                        other => match other.as_expression() {
                            Some(x) => Elem::Expr(self.expr(x)?),
                            None => return Err(ExprError::forbidden("this array element")),
                        },
                    });
                }
                Expr::Array(items)
            }
            Expression::ObjectExpression(o) => {
                let mut props = Vec::with_capacity(o.properties.len());
                for p in &o.properties {
                    props.push(match p {
                        ObjectPropertyKind::SpreadProperty(s) => {
                            Prop::Spread(self.expr(&s.argument)?)
                        }
                        ObjectPropertyKind::ObjectProperty(p) => {
                            if p.kind != PropertyKind::Init || p.method {
                                return Err(ExprError::forbidden(
                                    "a getter, setter or method in an object literal",
                                ));
                            }
                            let key = if p.computed {
                                match p.key.as_expression() {
                                    Some(k) => Key::Computed(self.expr(k)?),
                                    None => return Err(ExprError::forbidden("this computed key")),
                                }
                            } else {
                                match &p.key {
                                    PropertyKey::StaticIdentifier(id) => {
                                        proto_key(id.name.as_str())?
                                    }
                                    PropertyKey::PrivateIdentifier(_) => {
                                        return Err(ExprError::forbidden("a private name"));
                                    }
                                    PropertyKey::StringLiteral(s) => {
                                        if s.lone_surrogates {
                                            return Err(ExprError::forbidden(
                                                "a key with a lone surrogate escape",
                                            ));
                                        }
                                        proto_key(s.value.as_str())?
                                    }
                                    PropertyKey::NumericLiteral(_) => {
                                        match p.key.as_expression().map(|k| self.expr(k)) {
                                            Some(Ok(Expr::Num(raw))) => Key::Num(raw),
                                            Some(Err(e)) => return Err(e),
                                            _ => {
                                                return Err(ExprError::forbidden(
                                                    "this object key",
                                                ));
                                            }
                                        }
                                    }
                                    _ => return Err(ExprError::forbidden("this object key")),
                                }
                            };
                            Prop::Init(key, self.expr(&p.value)?)
                        }
                    });
                }
                Expr::Object(props)
            }
            Expression::UpdateExpression(u) => {
                if !self.writes_allowed() {
                    return Err(write_during_render());
                }
                let target = match &u.argument {
                    SimpleAssignmentTarget::AssignmentTargetIdentifier(id) => {
                        self.assignable_name(id.name.as_str())?
                    }
                    other => match other.as_member_expression() {
                        Some(m) => self.member(m, false)?,
                        None => return Err(ExprError::forbidden("this update target")),
                    },
                };
                Expr::Update(u.operator.as_str(), u.prefix, Box::new(target))
            }
            Expression::SequenceExpression(_) => {
                return Err(ExprError::forbidden("the comma operator"));
            }
            Expression::TaggedTemplateExpression(_) => {
                return Err(ExprError::forbidden("a tagged template"));
            }
            Expression::Super(_) => return Err(ExprError::forbidden("'super'")),
            Expression::AwaitExpression(_) => return Err(ExprError::forbidden("'await'")),
            Expression::YieldExpression(_) => return Err(ExprError::forbidden("'yield'")),
            Expression::NewExpression(_) => return Err(ExprError::forbidden("'new'")),
            Expression::FunctionExpression(_) => {
                return Err(ExprError::forbidden(
                    "a function expression (use an arrow function)",
                ));
            }
            Expression::ClassExpression(_) => {
                return Err(ExprError::forbidden("a class expression"));
            }
            Expression::ImportExpression(_) => return Err(ExprError::forbidden("'import(…)'")),
            Expression::ImportMeta(_) => return Err(ExprError::forbidden("'import.meta'")),
            Expression::NewTarget(_) => return Err(ExprError::forbidden("'new.target'")),
            Expression::PrivateInExpression(_) => {
                return Err(ExprError::forbidden("a private name"));
            }
            Expression::JSXElement(_) | Expression::JSXFragment(_) => {
                return Err(ExprError::forbidden("JSX"));
            }
            Expression::V8IntrinsicExpression(_) => {
                return Err(ExprError::forbidden("a V8 intrinsic"));
            }
            Expression::TSAsExpression(_)
            | Expression::TSSatisfiesExpression(_)
            | Expression::TSTypeAssertion(_)
            | Expression::TSNonNullExpression(_)
            | Expression::TSInstantiationExpression(_) => {
                return Err(ExprError::forbidden("TypeScript syntax"));
            }
        })
    }

    fn identifier(&self, name: &str) -> Result<Expr, ExprError> {
        if name == "eval" {
            return Err(ExprError::forbidden("'eval'"));
        }
        if self.params.iter().rev().any(|p| &**p == name) {
            return Ok(Expr::Param(name.into()));
        }
        if let Some(id) = (self.env.locals)(name) {
            return Ok(Expr::Local(id));
        }
        if let Some(g) = GLOBALS.iter().find(|g| **g == name) {
            return Ok(Expr::Global(g));
        }
        if name == "console" && self.env.mode == BuildMode::Dev {
            return Ok(Expr::Global("console"));
        }
        Ok(Expr::Free(name.into()))
    }

    /// A bare name on the left of an assignment: only arrow parameters can be assigned.
    fn assignable_name(&self, name: &str) -> Result<Expr, ExprError> {
        if self.params.iter().rev().any(|p| &**p == name) {
            return Ok(Expr::Param(name.into()));
        }
        Err(ExprError::new(
            DiagnosticKind::ForbiddenSyntax,
            format!(
                "cannot assign to '{name}': only properties (this.state.x = …) and arrow-function parameters can be assigned"
            ),
        ))
    }

    /// A member expression: one syntax-tree level, counted by the caller's `expr` (or `enter`).
    fn member(&mut self, m: &MemberExpression<'_>, _read: bool) -> Result<Expr, ExprError> {
        Ok(match m {
            MemberExpression::StaticMemberExpression(s) => Expr::Member(
                self.boxed(&s.object)?,
                s.property.name.as_str().into(),
                s.optional,
            ),
            MemberExpression::ComputedMemberExpression(c) => Expr::Index(
                self.boxed(&c.object)?,
                self.boxed(&c.expression)?,
                c.optional,
            ),
            MemberExpression::PrivateFieldExpression(_) => {
                return Err(ExprError::forbidden("a private name"));
            }
        })
    }

    fn call(&mut self, c: &oxc_ast::ast::CallExpression<'_>) -> Result<Expr, ExprError> {
        if matches!(&c.callee, Expression::Identifier(id) if id.name.as_str() == "eval") {
            return Err(ExprError::forbidden("'eval'"));
        }
        let callee = self.boxed(&c.callee)?;
        let mut args = Vec::with_capacity(c.arguments.len());
        for a in &c.arguments {
            args.push(match a {
                Argument::SpreadElement(s) => Elem::Spread(self.expr(&s.argument)?),
                other => match other.as_expression() {
                    Some(x) => Elem::Expr(self.expr(x)?),
                    None => return Err(ExprError::forbidden("this argument")),
                },
            });
        }
        Ok(Expr::Call(callee, args, c.optional))
    }

    #[inline(never)]
    fn assign(&mut self, a: &oxc_ast::ast::AssignmentExpression<'_>) -> Result<Expr, ExprError> {
        if !self.writes_allowed() {
            return Err(write_during_render());
        }
        let target = match &a.left {
            AssignmentTarget::AssignmentTargetIdentifier(id) => {
                self.assignable_name(id.name.as_str())?
            }
            other => match other
                .as_simple_assignment_target()
                .and_then(SimpleAssignmentTarget::as_member_expression)
            {
                Some(m) => self.member(m, false)?,
                None => return Err(ExprError::forbidden("destructuring assignment")),
            },
        };
        Ok(Expr::Assign(
            a.operator.as_str(),
            Box::new(target),
            self.boxed(&a.right)?,
        ))
    }

    #[inline(never)]
    fn arrow(&mut self, f: &ArrowFunctionExpression<'_>) -> Result<Expr, ExprError> {
        if f.r#async {
            return Err(ExprError::forbidden("an async arrow function"));
        }
        if f.params.rest.is_some() {
            return Err(ExprError::forbidden("a rest parameter"));
        }
        let mut names: Vec<Box<str>> = Vec::with_capacity(f.params.items.len());
        for p in &f.params.items {
            if p.initializer.is_some() {
                return Err(ExprError::forbidden("a default parameter value"));
            }
            let BindingPattern::BindingIdentifier(id) = &p.pattern else {
                return Err(ExprError::forbidden("a destructuring parameter"));
            };
            let name = id.name.as_str();
            if !is_plain_identifier(name) {
                return Err(ExprError::new(
                    DiagnosticKind::InvalidName,
                    format!("arrow-function parameter '{name}' is not a plain identifier"),
                ));
            }
            names.push(name.into());
        }
        let mark = self.params.len();
        self.params.extend(names.iter().cloned());
        self.arrows += 1;
        let body = self.arrow_body(f);
        self.arrows -= 1;
        self.params.truncate(mark);
        Ok(Expr::Arrow(names, body?))
    }

    fn arrow_body(&mut self, f: &ArrowFunctionExpression<'_>) -> Result<ArrowBody, ExprError> {
        if let Some(e) = f.body.as_expression() {
            return Ok(ArrowBody::Expr(self.boxed(e)?));
        }
        let Some(block) = f.body.as_function_body() else {
            return Err(ExprError::forbidden("this arrow-function body"));
        };
        if !block.directives.is_empty() {
            return Err(ExprError::forbidden(
                "a directive in an arrow-function body",
            ));
        }
        let mut stmts = Vec::with_capacity(block.statements.len());
        for s in &block.statements {
            match s {
                Statement::ExpressionStatement(es) => stmts.push(self.expr(&es.expression)?),
                Statement::EmptyStatement(_) => {}
                _ => {
                    return Err(ExprError::forbidden(
                        "a statement other than an expression in an arrow-function body",
                    ));
                }
            }
        }
        Ok(ArrowBody::Block(stmts))
    }
}

fn write_during_render() -> ExprError {
    ExprError::new(
        DiagnosticKind::WriteDuringRender,
        "assignments and updates are only allowed in t-on-* handlers and inside arrow functions, not in an expression evaluated while rendering (the check is syntactic: a method call that changes state is not detected)",
    )
}

/// Whether `raw` is a numeric literal's text we re-print as is: ASCII letters, digits, `_` and
/// `.`, and a sign directly after the exponent `e`/`E` of a decimal literal (`1e+5`, `2.5E-3`).
fn is_numeric_literal_text(raw: &str) -> bool {
    let bytes = raw.as_bytes();
    let radix_prefix = bytes.len() > 1
        && bytes[0] == b'0'
        && matches!(bytes[1], b'x' | b'X' | b'o' | b'O' | b'b' | b'B');
    bytes.iter().enumerate().all(|(i, &b)| {
        b.is_ascii_alphanumeric()
            || b == b'_'
            || b == b'.'
            || (matches!(b, b'+' | b'-')
                && !radix_prefix
                && i > 0
                && matches!(bytes[i - 1], b'e' | b'E'))
    })
}

/// A literal object key; `__proto__` is refused because it sets the prototype instead of a
/// property.
fn proto_key(name: &str) -> Result<Key, ExprError> {
    if name == crate::names::PROTO_KEY {
        return Err(ExprError::forbidden(
            "a '__proto__' key (it sets the prototype, not a property)",
        ));
    }
    Ok(Key::Name(name.into()))
}

/// How the printer writes generated names.
pub(crate) trait Names {
    /// Appends the code reading template local `id`.
    fn local(&self, id: LocalId, out: &mut String);
    /// Appends the code reading free name `name` (a caller's local).
    fn free(&self, name: &str, out: &mut String);
}

/// Prints `e` as an operand: atoms and postfix chains bare, everything else in parentheses.
pub(crate) fn print(e: &Expr, names: &dyn Names, out: &mut String) {
    if is_atom(e) {
        print_bare(e, names, out);
    } else {
        out.push('(');
        print_bare(e, names, out);
        out.push(')');
    }
}

fn is_atom(e: &Expr) -> bool {
    matches!(
        e,
        Expr::Null
            | Expr::Bool(_)
            | Expr::Num(_)
            | Expr::BigInt(_)
            | Expr::Str(_)
            | Expr::Template(..)
            | Expr::This
            | Expr::Local(_)
            | Expr::Param(_)
            | Expr::Global(_)
            | Expr::Free(_)
            | Expr::Array(_)
            | Expr::Member(..)
            | Expr::Index(..)
            | Expr::Call(..)
    )
}

/// The object of a member access or the callee of a call.
fn print_object(e: &Expr, names: &dyn Names, out: &mut String) {
    if matches!(e, Expr::Num(_) | Expr::BigInt(_)) {
        out.push('(');
        print_bare(e, names, out);
        out.push(')');
    } else {
        print(e, names, out);
    }
}

fn print_elems(items: &[Elem], names: &dyn Names, out: &mut String) {
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        match item {
            Elem::Expr(e) => print(e, names, out),
            Elem::Spread(e) => {
                out.push_str("...");
                print(e, names, out);
            }
            Elem::Hole => {}
        }
    }
    // A trailing hole needs its own comma to survive.
    if matches!(items.last(), Some(Elem::Hole)) {
        out.push(',');
    }
}

/// Whether `name` can be written after `.`.
fn is_dot_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes
        .next()
        .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_' || b == b'$')
        && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'$')
}

fn print_bare(e: &Expr, names: &dyn Names, out: &mut String) {
    match e {
        Expr::Null => out.push_str("null"),
        Expr::Bool(true) => out.push_str("true"),
        Expr::Bool(false) => out.push_str("false"),
        Expr::Num(raw) | Expr::BigInt(raw) => out.push_str(raw),
        Expr::Str(s) => push_js_string(out, s),
        Expr::Template(parts, exprs) => {
            out.push('`');
            for (i, part) in parts.iter().enumerate() {
                push_escaped(out, part, Quote::Backtick);
                if let Some(x) = exprs.get(i) {
                    out.push_str("${");
                    print(x, names, out);
                    out.push('}');
                }
            }
            out.push('`');
        }
        Expr::This => out.push_str("$self"),
        Expr::Local(id) => names.local(*id, out),
        Expr::Param(name) => {
            out.push_str("l_");
            out.push_str(name);
        }
        Expr::Global(name) => out.push_str(name),
        Expr::Free(name) => names.free(name, out),
        Expr::Array(items) => {
            out.push('[');
            print_elems(items, names, out);
            out.push(']');
        }
        Expr::Object(props) => {
            out.push('{');
            for (i, prop) in props.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                match prop {
                    Prop::Init(key, value) => {
                        match key {
                            Key::Name(name) => push_js_string(out, name),
                            Key::Num(raw) => out.push_str(raw),
                            Key::Computed(k) => {
                                out.push('[');
                                print(k, names, out);
                                out.push(']');
                            }
                        }
                        out.push_str(": ");
                        print(value, names, out);
                    }
                    Prop::Spread(value) => {
                        out.push_str("...");
                        print(value, names, out);
                    }
                }
            }
            out.push('}');
        }
        Expr::Member(obj, name, optional) => {
            print_object(obj, names, out);
            if is_dot_name(name) {
                out.push_str(if *optional { "?." } else { "." });
                out.push_str(name);
            } else {
                out.push_str(if *optional { "?.[" } else { "[" });
                push_js_string(out, name);
                out.push(']');
            }
        }
        Expr::Index(obj, index, optional) => {
            print_object(obj, names, out);
            out.push_str(if *optional { "?.[" } else { "[" });
            print(index, names, out);
            out.push(']');
        }
        Expr::Call(callee, args, optional) => {
            print_object(callee, names, out);
            out.push_str(if *optional { "?.(" } else { "(" });
            print_elems(args, names, out);
            out.push(')');
        }
        Expr::Chain(inner) => print_bare(inner, names, out),
        Expr::Unary(op, arg) => {
            out.push_str(op);
            print(arg, names, out);
        }
        Expr::Binary(op, l, r) | Expr::Assign(op, l, r) => {
            print(l, names, out);
            out.push(' ');
            out.push_str(op);
            out.push(' ');
            print(r, names, out);
        }
        Expr::Cond(t, c, a) => {
            print(t, names, out);
            out.push_str(" ? ");
            print(c, names, out);
            out.push_str(" : ");
            print(a, names, out);
        }
        Expr::Update(op, prefix, target) => {
            if *prefix {
                out.push_str(op);
                print(target, names, out);
            } else {
                print(target, names, out);
                out.push_str(op);
            }
        }
        Expr::Arrow(params, body) => {
            out.push('(');
            for (i, p) in params.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                out.push_str("l_");
                out.push_str(p);
            }
            out.push_str(") => ");
            match body {
                ArrowBody::Expr(b) => print(b, names, out),
                ArrowBody::Block(stmts) => {
                    out.push('{');
                    for s in stmts {
                        out.push(' ');
                        print(s, names, out);
                        out.push(';');
                    }
                    out.push_str(" }");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestNames;
    impl Names for TestNames {
        fn local(&self, id: LocalId, out: &mut String) {
            out.push_str(&format!("$r{id}.item()"));
        }
        fn free(&self, name: &str, out: &mut String) {
            out.push_str(&format!("$free[{name:?}]"));
        }
    }

    fn env_with<'e>(
        locals: &'e dyn Fn(&str) -> Option<LocalId>,
        context: ExprContext,
    ) -> ExprEnv<'e> {
        ExprEnv {
            locals,
            mode: BuildMode::Prod,
            context,
            trust: Trust::Trusted,
        }
    }

    fn parse_in(src: &str, context: ExprContext) -> Result<Expr, ExprError> {
        let locals = |n: &str| (n == "record").then_some(7);
        let env = env_with(&locals, context);
        ExprParser::new(&CompilerLimits::DEFAULT).parse(src, &env)
    }

    fn printed(src: &str) -> String {
        let e = parse_in(src, ExprContext::Render).unwrap();
        let mut out = String::new();
        print(&e, &TestNames, &mut out);
        out
    }

    fn kind(src: &str) -> DiagnosticKind {
        parse_in(src, ExprContext::Render).unwrap_err().kind
    }

    /// errors; the runtime half is UI0-RUNTIME's)
    #[test]
    fn names_resolve_to_this_locals_params_globals_or_free() {
        assert_eq!(printed("this.props.label"), "$self.props.label");
        assert_eq!(printed("record.data"), "$r7.item().data");
        assert_eq!(printed("Math.max(1, 2)"), "Math.max(1, 2)");
        assert_eq!(
            printed("(x) => x + record"),
            "((l_x) => (l_x + $r7.item()))"
        );
        assert_eq!(printed("props"), "$free[\"props\"]");
        assert_eq!(printed("console"), "$free[\"console\"]");
        let locals = |_: &str| None;
        let env = ExprEnv {
            locals: &locals,
            mode: BuildMode::Dev,
            context: ExprContext::Render,
            trust: Trust::Trusted,
        };
        let e = ExprParser::new(&CompilerLimits::DEFAULT)
            .parse("console", &env)
            .unwrap();
        assert_eq!(e, Expr::Global("console"));
    }

    #[test]
    fn printing_parenthesises_compounds_and_reescapes_strings() {
        assert_eq!(
            printed("a and b or !c"),
            "(($free[\"a\"] && $free[\"b\"]) || (!$free[\"c\"]))"
        );
        assert_eq!(
            printed("'</script>' + \"\\u2028\""),
            "(\"\\u003c\\/script\\u003e\" + \"\\u2028\")"
        );
        assert_eq!(
            printed("`a${record}\\`b${'x'}`"),
            "`a${$r7.item()}\\`b${\"x\"}`"
        );
        assert_eq!(
            printed("{a: 1, 'b-c': 2, [record]: 3, ...this.x}"),
            "({\"a\": 1, \"b-c\": 2, [$r7.item()]: 3, ...$self.x})"
        );
        assert_eq!(printed("this?.a.b"), "($self?.a.b)");
        assert_eq!(printed("(this?.a).b"), "($self?.a).b");
        assert_eq!(printed("this.f?.(1)"), "($self.f?.(1))");
        assert_eq!(printed("1..toFixed(2)"), "(1.).toFixed(2)");
        assert_eq!(printed("[1, , 2, ]"), "[1, , 2]");
        assert_eq!(printed("[1, , ]"), "[1, ,]");
        assert_eq!(printed("typeof record"), "(typeof $r7.item())");
        assert_eq!(printed("typeof record.x"), "(typeof $r7.item().x)");
        assert_eq!(printed("'x' in record"), "(\"x\" in $r7.item())");
        assert_eq!(printed("-(-1)"), "(-(-1))");
        assert_eq!(printed("this['a b']"), "$self[\"a b\"]");
        assert_eq!(printed("() => {}"), "(() => { })");
        assert_eq!(printed("() => ({})"), "(() => ({}))");
    }

    #[test]
    fn forbidden_syntax_is_refused_with_a_reason() {
        for (src, why) in [
            ("function () {}", "function expression"),
            ("new Date()", "'new'"),
            ("a, b", "comma operator"),
            ("eval('1')", "'eval'"),
            ("a instanceof B", "'instanceof'"),
            ("({__proto__: a})", "'__proto__' key"),
            ("({'__proto__': a})", "'__proto__' key"),
            ("delete a.b", "'delete'"),
            ("void 0", "'void'"),
            ("tag`x`", "tagged template"),
            ("/re/", "regular expression"),
            ("class {}", "class expression"),
            ("import('x')", "import"),
            ("({get a() { return 1 }})", "getter"),
            ("({a() {}})", "getter, setter or method"),
            ("async () => 1", "async"),
            ("({a}) => a", "destructuring"),
            ("(...a) => a", "rest parameter"),
            ("(a = 1) => a", "default parameter"),
            ("() => { if (a) {} }", "statement"),
            ("a // c", "comment"),
        ] {
            let err = parse_in(src, ExprContext::Render).unwrap_err();
            assert!(
                matches!(
                    err.kind,
                    DiagnosticKind::ForbiddenSyntax | DiagnosticKind::ExpressionSyntax
                ),
                "{src}: {err:?}"
            );
            assert!(
                err.message.contains(why) || err.kind == DiagnosticKind::ExpressionSyntax,
                "{src}: {}",
                err.message
            );
        }
    }

    #[test]
    fn writes_only_in_handlers_and_arrow_bodies() {
        assert_eq!(kind("this.state.x = 1"), DiagnosticKind::WriteDuringRender);
        assert_eq!(kind("this.n++"), DiagnosticKind::WriteDuringRender);
        assert!(parse_in("this.state.x = 1", ExprContext::Handler).is_ok());
        assert!(
            parse_in(
                "() => this.state.open = !this.state.open",
                ExprContext::Render
            )
            .is_ok()
        );
        assert!(parse_in("() => { this.a = 1; this.b++ }", ExprContext::Render).is_ok());
        assert!(parse_in("(v) => v = 2", ExprContext::Render).is_ok());
        assert_eq!(
            parse_in("record = 1", ExprContext::Handler)
                .unwrap_err()
                .kind,
            DiagnosticKind::ForbiddenSyntax
        );
        assert_eq!(
            parse_in("[a] = b", ExprContext::Handler).unwrap_err().kind,
            DiagnosticKind::ForbiddenSyntax
        );
    }

    #[test]
    fn limits_and_syntax_errors() {
        assert_eq!(kind(""), DiagnosticKind::ExpressionSyntax);
        assert_eq!(kind("a +"), DiagnosticKind::ExpressionSyntax);
        let long = "a".repeat(4097);
        assert_eq!(kind(&long), DiagnosticKind::Limit);
        let deep = format!("{}1{}", "[".repeat(65), "]".repeat(65));
        assert_eq!(kind(&deep), DiagnosticKind::Limit);
        let err = parse_in(&long, ExprContext::Render).unwrap_err();
        assert_eq!(
            err.limit,
            Some(LimitHit::new(LimitKind::ExpressionBytes, 4096, 4097))
        );
        assert_eq!(kind("'\\uD800'"), DiagnosticKind::ForbiddenSyntax);
    }

    /// Rust review L3: a member counts one syntax-tree level, so a chain of 255 members fits the
    /// walk's 256 levels (64 × 4) and one more does not.
    #[test]
    fn member_chains_count_one_level_per_member() {
        let chain = |n: usize| format!("this{}", ".a".repeat(n));
        assert!(parse_in(&chain(255), ExprContext::Render).is_ok());
        let err = parse_in(&chain(256), ExprContext::Render).unwrap_err();
        assert_eq!(err.kind, DiagnosticKind::Limit);
        assert_eq!(
            err.limit,
            Some(LimitHit::new(LimitKind::ExpressionNesting, 256, 257))
        );
        assert!(
            err.message.contains("deeper than 256 levels"),
            "{}",
            err.message
        );
    }

    /// Rust review M4: a signed exponent is a valid decimal literal; a sign is accepted nowhere
    /// else in a literal's text.
    #[test]
    fn numeric_literals_take_signed_exponents() {
        assert_eq!(printed("1e+5"), "1e+5");
        assert_eq!(printed("2.5E-3"), "2.5E-3");
        assert_eq!(printed("0x1e+5"), "(0x1e + 5)");
        assert!(is_numeric_literal_text("1_000.5e-7"));
        assert!(!is_numeric_literal_text("0x1e+5"));
        assert!(!is_numeric_literal_text("+1"));
        assert!(!is_numeric_literal_text("1-2"));
    }

    #[test]
    fn luxon_is_not_a_global() {
        assert!(!GLOBALS.contains(&"luxon"));
        assert_eq!(printed("luxon"), "$free[\"luxon\"]");
    }

    #[test]
    fn constants_fold_only_literals() {
        assert_eq!(
            parse_in("'a'", ExprContext::Render).unwrap().constant(),
            Some(Constant::Str("a"))
        );
        assert_eq!(
            parse_in("(true)", ExprContext::Render).unwrap().constant(),
            Some(Constant::Bool(true))
        );
        assert_eq!(
            parse_in("null", ExprContext::Render).unwrap().constant(),
            Some(Constant::Null)
        );
        assert_eq!(parse_in("1", ExprContext::Render).unwrap().constant(), None);
        assert_eq!(
            parse_in("undefined", ExprContext::Render)
                .unwrap()
                .constant(),
            None
        );
    }
}
