//! Pratt parser for the Python-expression subset.
//!
//! Two bounds protect the stack (review round 2 blocking 3):
//! - the **recursion depth** of the parser itself (nested parentheses, prefix operators, nested
//!   f-strings, which continue the enclosing parser's count), and
//! - the **height of the produced tree**, counted for every node including left-associative chains
//!   (`1+1+…`, `a.b.c…`, `f()()…`) that the parser builds in a loop, and one level per comprehension
//!   `for` clause, so that evaluating, cloning or dropping any accepted tree stays within the limit.

use crate::ast::*;
use crate::error::{ExprError, ExprResult};
use crate::lexer::{Lexer, Span, Token, TokenKind};
use crate::limits::EvalLimits;

#[path = "source_preflight.rs"]
mod source_admission;
pub use source_admission::{SourceParsePreflight, source_preflight};

const PREC_LOWEST: u8 = 0;
const PREC_WALRUS: u8 = 2;
const PREC_TERNARY: u8 = 3;
const PREC_OR: u8 = 4;
const PREC_AND: u8 = 5;
const PREC_NOT: u8 = 6;
const PREC_CMP: u8 = 7;
const PREC_BIT_OR: u8 = 8;
const PREC_BIT_XOR: u8 = 9;
const PREC_BIT_AND: u8 = 10;
const PREC_SHIFT: u8 = 11;
const PREC_TERM: u8 = 12;
const PREC_FACTOR: u8 = 13;
const PREC_UNARY: u8 = 14;
const PREC_POW: u8 = 15;
const PREC_POSTFIX: u8 = 16;

/// An expression with the height of its tree.
type P = (Expr, usize);

/// Result of extending an expression by one postfix or infix construct.
enum Step {
    More(P),
    Done(P),
}

fn binary_op(kind: &TokenKind) -> Option<BinaryOp> {
    Some(match kind {
        TokenKind::Plus => BinaryOp::Add,
        TokenKind::Minus => BinaryOp::Sub,
        TokenKind::Star => BinaryOp::Mult,
        TokenKind::Slash => BinaryOp::Div,
        TokenKind::DoubleSlash => BinaryOp::FloorDiv,
        TokenKind::Percent => BinaryOp::Mod,
        TokenKind::DoubleStar => BinaryOp::Pow,
        TokenKind::Pipe => BinaryOp::BitOr,
        TokenKind::Ampersand => BinaryOp::BitAnd,
        TokenKind::Caret => BinaryOp::BitXor,
        TokenKind::LShift => BinaryOp::LShift,
        TokenKind::RShift => BinaryOp::RShift,
        _ => return None,
    })
}

type CallArgs = (Vec<Expr>, Vec<(Option<String>, Expr)>, usize);

struct Parser<'a> {
    lexer: Lexer<'a>,
    current: Token,
    peeked: Option<Token>,
    limits: EvalLimits,
    depth: usize,
    paren_depth: usize,
}

fn h_max<'e>(items: impl IntoIterator<Item = &'e usize>) -> usize {
    items.into_iter().copied().max().unwrap_or(0)
}

impl<'a> Parser<'a> {
    fn new(input: &'a str, limits: EvalLimits, depth: usize) -> ExprResult<Self> {
        if input.len() > limits.max_source_bytes() {
            return Err(ExprError::limit(format!(
                "expression source of {} bytes exceeds the limit of {} bytes",
                input.len(),
                limits.max_source_bytes()
            )));
        }
        let mut lexer = Lexer::new(input);
        let current = lexer.next_token()?;
        Ok(Self {
            lexer,
            current,
            peeked: None,
            limits,
            depth,
            paren_depth: 0,
        })
    }

    fn depth_error(&self) -> ExprError {
        ExprError::limit(format!(
            "expression nesting depth limit of {} exceeded",
            self.limits.max_depth()
        ))
    }

    /// A node of height `h`: rejected past the limit.
    fn node(&self, expr: Expr, h: usize) -> ExprResult<P> {
        if h > self.limits.max_depth() {
            return Err(self.depth_error());
        }
        Ok((expr, h))
    }

    fn err_at(&self, message: impl Into<String>, span: Span) -> ExprError {
        ExprError::parse(message, span.start, span.line, span.col)
    }

    fn err_here(&self, message: impl Into<String>) -> ExprError {
        self.err_at(message, self.current.span)
    }

    fn peek(&mut self) -> ExprResult<&TokenKind> {
        if self.peeked.is_none() {
            self.peeked = Some(self.lexer.next_token()?);
        }
        match &self.peeked {
            Some(tok) => Ok(&tok.kind),
            None => Err(self.err_here("unexpected end of input")),
        }
    }

    fn advance(&mut self) -> ExprResult<Token> {
        let next = match self.peeked.take() {
            Some(tok) => tok,
            None => self.lexer.next_token()?,
        };
        Ok(std::mem::replace(&mut self.current, next))
    }

    fn expect(&mut self, expected: &TokenKind) -> ExprResult<Token> {
        if &self.current.kind == expected {
            self.advance()
        } else {
            Err(self.err_here(format!(
                "expected {}, found {}",
                expected.describe(),
                self.current.kind.describe()
            )))
        }
    }

    fn at(&self, kind: &TokenKind) -> bool {
        &self.current.kind == kind
    }

    fn parse_expression(&mut self) -> ExprResult<P> {
        let first = self.expr_bp(PREC_LOWEST)?;
        let result = if self.at(&TokenKind::Comma) {
            let mut hs = vec![first.1];
            let mut items = vec![first.0];
            while self.at(&TokenKind::Comma) {
                self.advance()?;
                if self.at(&TokenKind::Eof) {
                    break;
                }
                let (e, h) = self.expr_bp(PREC_LOWEST)?;
                items.push(e);
                hs.push(h);
            }
            self.node(Expr::Tuple(items), 1 + h_max(&hs))?
        } else {
            first
        };
        if !self.at(&TokenKind::Eof) {
            return Err(self.err_here(format!(
                "unexpected {} after expression",
                self.current.kind.describe()
            )));
        }
        Ok(result)
    }

    fn expr_bp(&mut self, min_bp: u8) -> ExprResult<P> {
        if self.depth >= self.limits.max_depth() {
            return Err(self.depth_error());
        }
        self.depth += 1;
        let r = self.expr_bp_inner(min_bp);
        self.depth -= 1;
        r
    }

    // The parser recurses through `expr_bp` → `expr_bp_inner` → one small helper per construct.
    // Keeping each construct in its own non-inlined function keeps the stack used per nesting level
    // small, also in unoptimized builds.

    fn expr_bp_inner(&mut self, min_bp: u8) -> ExprResult<P> {
        let mut acc = self.prefix()?;
        loop {
            match self.step(acc, min_bp)? {
                Step::More(next) => acc = next,
                Step::Done(done) => return Ok(done),
            }
        }
    }

    /// One postfix or infix extension of `left`, or `Done` when the next token does not bind.
    #[inline(never)]
    fn step(&mut self, left: P, min_bp: u8) -> ExprResult<Step> {
        let more = |r: ExprResult<P>| r.map(Step::More);
        match self.current.kind {
            TokenKind::Dot if PREC_POSTFIX >= min_bp => return more(self.attribute(left)),
            TokenKind::LParen if PREC_POSTFIX >= min_bp => return more(self.call(left)),
            TokenKind::LBracket if PREC_POSTFIX >= min_bp => return more(self.index(left)),
            _ => {}
        }
        let (lbp, rbp) = infix_binding_power(&self.current.kind);
        if lbp == 0 || lbp < min_bp {
            return Ok(Step::Done(left));
        }
        match self.current.kind {
            TokenKind::If => more(self.ternary(left)),
            TokenKind::Walrus => more(self.walrus(left, rbp)),
            TokenKind::Or | TokenKind::And => more(self.bool_op(left, rbp)),
            _ => match self.compare_op()? {
                Some(first) => more(self.comparison(left, first, rbp)),
                None => match binary_op(&self.current.kind) {
                    Some(op) => more(self.binary(left, op, rbp)),
                    None => Ok(Step::Done(left)),
                },
            },
        }
    }

    #[inline(never)]
    fn attribute(&mut self, (left, lh): P) -> ExprResult<P> {
        self.advance()?;
        let tok = self.advance()?;
        let attr = match tok.kind {
            TokenKind::Ident(s) => s,
            other => {
                return Err(self.err_at(
                    format!(
                        "expected an attribute name after '.', found {}",
                        other.describe()
                    ),
                    tok.span,
                ));
            }
        };
        self.node(
            Expr::Attribute {
                value: Box::new(left),
                attr,
            },
            lh + 1,
        )
    }

    #[inline(never)]
    fn call(&mut self, (left, lh): P) -> ExprResult<P> {
        self.advance()?;
        self.paren_depth += 1;
        let parsed = self.call_args();
        self.paren_depth -= 1;
        let (args, keywords, ah) = parsed?;
        self.expect(&TokenKind::RParen)?;
        self.node(
            Expr::Call {
                func: Box::new(left),
                args,
                keywords,
            },
            1 + lh.max(ah),
        )
    }

    #[inline(never)]
    fn index(&mut self, (left, lh): P) -> ExprResult<P> {
        self.advance()?;
        self.paren_depth += 1;
        let parsed = self.subscript();
        self.paren_depth -= 1;
        let (slice, sh) = parsed?;
        self.expect(&TokenKind::RBracket)?;
        self.node(
            Expr::Subscript {
                value: Box::new(left),
                slice,
            },
            1 + lh.max(sh),
        )
    }

    #[inline(never)]
    fn ternary(&mut self, (body, bh): P) -> ExprResult<P> {
        self.advance()?;
        let (test, th) = self.expr_bp(PREC_TERNARY + 1)?;
        self.expect(&TokenKind::Else)?;
        let (orelse, oh) = self.expr_bp(PREC_TERNARY)?;
        self.node(
            Expr::IfExp {
                test: Box::new(test),
                body: Box::new(body),
                orelse: Box::new(orelse),
            },
            1 + h_max(&[bh, th, oh]),
        )
    }

    #[inline(never)]
    fn walrus(&mut self, (left, _): P, rbp: u8) -> ExprResult<P> {
        if self.paren_depth == 0 {
            return Err(self.err_here(
                "assignment expression cannot be used at top level without parentheses",
            ));
        }
        let Expr::Name(name) = left else {
            return Err(self.err_here("cannot assign to expression with ':='"));
        };
        self.advance()?;
        let (value, vh) = self.expr_bp(rbp)?;
        self.node(
            Expr::Walrus {
                name,
                value: Box::new(value),
            },
            1 + vh,
        )
    }

    #[inline(never)]
    fn bool_op(&mut self, (left, lh): P, rbp: u8) -> ExprResult<P> {
        let op = if self.at(&TokenKind::Or) {
            BoolOp::Or
        } else {
            BoolOp::And
        };
        self.advance()?;
        let (right, rh) = self.expr_bp(rbp)?;
        let (mut values, base_h) = match left {
            Expr::BoolOp { op: o, values } if o == op => (values, lh.saturating_sub(1)),
            other => (vec![other], lh),
        };
        values.push(right);
        self.node(Expr::BoolOp { op, values }, 1 + base_h.max(rh))
    }

    #[inline(never)]
    fn comparison(&mut self, (left, lh): P, first: CompareOp, rbp: u8) -> ExprResult<P> {
        let mut ops = vec![first];
        let (c, ch) = self.expr_bp(rbp)?;
        let mut comparators = vec![c];
        let mut h = lh.max(ch);
        while let Some(op) = self.compare_op()? {
            ops.push(op);
            let (c, ch) = self.expr_bp(rbp)?;
            comparators.push(c);
            h = h.max(ch);
        }
        self.node(
            Expr::Compare {
                left: Box::new(left),
                ops,
                comparators,
            },
            1 + h,
        )
    }

    #[inline(never)]
    fn binary(&mut self, (left, lh): P, op: BinaryOp, rbp: u8) -> ExprResult<P> {
        self.advance()?;
        let (right, rh) = self.expr_bp(rbp)?;
        self.node(
            Expr::Binary {
                op,
                left: Box::new(left),
                right: Box::new(right),
            },
            1 + lh.max(rh),
        )
    }

    fn compare_op(&mut self) -> ExprResult<Option<CompareOp>> {
        let op = match &self.current.kind {
            TokenKind::Eq => CompareOp::Eq,
            TokenKind::NotEq => CompareOp::NotEq,
            TokenKind::Lt => CompareOp::Lt,
            TokenKind::LtE => CompareOp::LtE,
            TokenKind::Gt => CompareOp::Gt,
            TokenKind::GtE => CompareOp::GtE,
            TokenKind::In => CompareOp::In,
            TokenKind::Is => {
                if self.peek()? == &TokenKind::Not {
                    self.advance()?;
                    self.advance()?;
                    return Ok(Some(CompareOp::IsNot));
                }
                CompareOp::Is
            }
            TokenKind::Not => {
                if self.peek()? == &TokenKind::In {
                    self.advance()?;
                    self.advance()?;
                    return Ok(Some(CompareOp::NotIn));
                }
                return Ok(None);
            }
            _ => return Ok(None),
        };
        self.advance()?;
        Ok(Some(op))
    }

    #[inline(never)]
    fn prefix(&mut self) -> ExprResult<P> {
        let tok = self.advance()?;
        match tok.kind {
            TokenKind::None => Ok((Expr::Literal(Literal::None), 1)),
            TokenKind::True => Ok((Expr::Literal(Literal::Bool(true)), 1)),
            TokenKind::False => Ok((Expr::Literal(Literal::Bool(false)), 1)),
            TokenKind::Int(i) => Ok((Expr::Literal(Literal::Int(i)), 1)),
            TokenKind::Float(f) => Ok((Expr::Literal(Literal::Float(f)), 1)),
            TokenKind::Str(s) => self.string_run(StrPart::Plain(s)),
            TokenKind::FString(s) => self.string_run(StrPart::Format(s, tok.span)),
            TokenKind::Bytes(b) => self.bytes_run(b),
            TokenKind::Ident(name) => self.name(name, tok.span),
            TokenKind::Plus => self.unary(UnaryOp::UAdd, PREC_UNARY),
            TokenKind::Minus => self.unary(UnaryOp::USub, PREC_UNARY),
            TokenKind::Tilde => self.unary(UnaryOp::Invert, PREC_UNARY),
            TokenKind::Not => self.unary(UnaryOp::Not, PREC_NOT),
            TokenKind::LParen => self.enclosed(Self::paren),
            TokenKind::LBracket => self.enclosed(Self::bracket),
            TokenKind::LBrace => self.enclosed(Self::brace),
            other => Err(self.err_at(
                format!("unexpected {} in expression", other.describe()),
                tok.span,
            )),
        }
    }

    fn enclosed(&mut self, f: fn(&mut Self) -> ExprResult<P>) -> ExprResult<P> {
        self.paren_depth += 1;
        let r = f(self);
        self.paren_depth -= 1;
        r
    }

    #[inline(never)]
    fn unary(&mut self, op: UnaryOp, bp: u8) -> ExprResult<P> {
        let (operand, h) = self.expr_bp(bp)?;
        self.node(
            Expr::Unary {
                op,
                operand: Box::new(operand),
            },
            h + 1,
        )
    }

    fn name(&self, name: String, span: Span) -> ExprResult<P> {
        match name.as_str() {
            "lambda" | "yield" | "await" | "async" | "def" | "class" | "import" | "from"
            | "return" | "del" | "global" | "nonlocal" | "assert" | "pass" | "raise" | "try"
            | "while" | "with" => Err(self.err_at(
                format!("'{name}' is not supported in the expression subset"),
                span,
            )),
            _ => Ok((Expr::Name(name), 1)),
        }
    }

    #[inline(never)]
    fn bytes_run(&mut self, mut b: Vec<u8>) -> ExprResult<P> {
        loop {
            match &self.current.kind {
                TokenKind::Bytes(_) => {
                    if let TokenKind::Bytes(next) = self.advance()?.kind {
                        b.extend_from_slice(&next);
                    }
                }
                TokenKind::Str(_) | TokenKind::FString(_) => {
                    return Err(self.err_here("cannot mix bytes and nonbytes literals"));
                }
                _ => break,
            }
        }
        Ok((Expr::Literal(Literal::Bytes(b)), 1))
    }

    /// Adjacent string literals concatenate (`'a' 'b'`, `'a' f'{x}'`); bytes cannot mix in.
    #[inline(never)]
    fn string_run(&mut self, first: StrPart) -> ExprResult<P> {
        let mut parts = vec![first];
        loop {
            match &self.current.kind {
                TokenKind::Str(_) | TokenKind::FString(_) => {
                    let tok = self.advance()?;
                    match tok.kind {
                        TokenKind::Str(s) => parts.push(StrPart::Plain(s)),
                        TokenKind::FString(s) => parts.push(StrPart::Format(s, tok.span)),
                        _ => {}
                    }
                }
                TokenKind::Bytes(_) => {
                    return Err(self.err_here("cannot mix bytes and nonbytes literals"));
                }
                _ => break,
            }
        }
        if parts.iter().all(|p| matches!(p, StrPart::Plain(_))) {
            let mut s = String::new();
            for p in parts {
                if let StrPart::Plain(text) = p {
                    s.push_str(&text);
                }
            }
            return Ok((Expr::Literal(Literal::Str(s)), 1));
        }
        let mut pieces: Vec<Expr> = Vec::new();
        let mut h = 1;
        for p in parts {
            match p {
                StrPart::Plain(s) => push_literal(&mut pieces, s),
                StrPart::Format(text, fspan) => {
                    for (e, eh) in self.fstring(&text, fspan)? {
                        h = h.max(eh);
                        match e {
                            Expr::Literal(Literal::Str(s)) => push_literal(&mut pieces, s),
                            other => pieces.push(other),
                        }
                    }
                }
            }
        }
        self.node(Expr::JoinedStr(pieces), h + 1)
    }

    #[inline(never)]
    fn paren(&mut self) -> ExprResult<P> {
        if self.at(&TokenKind::RParen) {
            self.advance()?;
            return Ok((Expr::Tuple(vec![]), 1));
        }
        let (first, fh) = self.expr_bp(PREC_LOWEST)?;
        if self.at(&TokenKind::For) {
            let (generators, gh, count) = self.comprehensions()?;
            self.expect(&TokenKind::RParen)?;
            return self.node(
                Expr::GeneratorExp {
                    elt: Box::new(first),
                    generators,
                },
                1 + count + fh.max(gh),
            );
        }
        if self.at(&TokenKind::Comma) {
            let mut items = vec![first];
            let mut hs = vec![fh];
            while self.at(&TokenKind::Comma) {
                self.advance()?;
                if self.at(&TokenKind::RParen) {
                    break;
                }
                let (e, h) = self.expr_bp(PREC_LOWEST)?;
                items.push(e);
                hs.push(h);
            }
            self.expect(&TokenKind::RParen)?;
            return self.node(Expr::Tuple(items), 1 + h_max(&hs));
        }
        self.expect(&TokenKind::RParen)?;
        Ok((first, fh))
    }

    #[inline(never)]
    fn bracket(&mut self) -> ExprResult<P> {
        if self.at(&TokenKind::RBracket) {
            self.advance()?;
            return Ok((Expr::List(vec![]), 1));
        }
        let (first, fh) = self.expr_bp(PREC_LOWEST)?;
        if self.at(&TokenKind::For) {
            let (generators, gh, count) = self.comprehensions()?;
            self.expect(&TokenKind::RBracket)?;
            return self.node(
                Expr::ListComp {
                    elt: Box::new(first),
                    generators,
                },
                1 + count + fh.max(gh),
            );
        }
        let mut items = vec![first];
        let mut hs = vec![fh];
        while self.at(&TokenKind::Comma) {
            self.advance()?;
            if self.at(&TokenKind::RBracket) {
                break;
            }
            let (e, h) = self.expr_bp(PREC_LOWEST)?;
            items.push(e);
            hs.push(h);
        }
        self.expect(&TokenKind::RBracket)?;
        self.node(Expr::List(items), 1 + h_max(&hs))
    }

    #[inline(never)]
    fn brace(&mut self) -> ExprResult<P> {
        let mut entries = Vec::new();
        let mut hs = vec![0];
        while !self.at(&TokenKind::RBrace) && !self.at(&TokenKind::Eof) {
            if self.at(&TokenKind::DoubleStar) {
                return Err(self.err_here("'**' unpacking in dict displays is not supported"));
            }
            let (key, kh) = self.expr_bp(PREC_LOWEST)?;
            if self.at(&TokenKind::For) {
                return Err(self.err_here("set comprehensions are not supported"));
            }
            if self.at(&TokenKind::Comma) || self.at(&TokenKind::RBrace) {
                return Err(self.err_here("set literals are not supported; use a list or a tuple"));
            }
            self.expect(&TokenKind::Colon)?;
            let (value, vh) = self.expr_bp(PREC_LOWEST)?;
            if self.at(&TokenKind::For) {
                return Err(self.err_here("dict comprehensions are not supported"));
            }
            entries.push((key, value));
            hs.push(kh);
            hs.push(vh);
            if self.at(&TokenKind::Comma) {
                self.advance()?;
            } else {
                break;
            }
        }
        self.expect(&TokenKind::RBrace)?;
        self.node(Expr::Dict(entries), 1 + h_max(&hs))
    }

    #[inline(never)]
    fn call_args(&mut self) -> ExprResult<CallArgs> {
        let mut args = Vec::new();
        let mut keywords = Vec::new();
        let mut h = 0;
        while !self.at(&TokenKind::RParen) && !self.at(&TokenKind::Eof) {
            if self.at(&TokenKind::DoubleStar) {
                self.advance()?;
                let (v, vh) = self.expr_bp(PREC_LOWEST)?;
                h = h.max(vh);
                keywords.push((None, v));
            } else if self.at(&TokenKind::Star) {
                self.advance()?;
                let (v, vh) = self.expr_bp(PREC_LOWEST)?;
                h = h.max(vh + 1);
                args.push(Expr::Starred(Box::new(v)));
            } else if matches!(self.current.kind, TokenKind::Ident(_))
                && self.peek()? == &TokenKind::Equal
            {
                let name = match self.advance()?.kind {
                    TokenKind::Ident(s) => s,
                    other => {
                        return Err(self.err_here(format!(
                            "expected a keyword name, found {}",
                            other.describe()
                        )));
                    }
                };
                self.advance()?;
                let (v, vh) = self.expr_bp(PREC_LOWEST)?;
                h = h.max(vh);
                keywords.push((Some(name), v));
            } else {
                if !keywords.is_empty() {
                    return Err(self.err_here("positional argument follows keyword argument"));
                }
                let (e, eh) = self.expr_bp(PREC_LOWEST)?;
                if self.at(&TokenKind::For) {
                    let (generators, gh, count) = self.comprehensions()?;
                    let gen_h = 1 + count + eh.max(gh);
                    let (g, gen_h) = self.node(
                        Expr::GeneratorExp {
                            elt: Box::new(e),
                            generators,
                        },
                        gen_h,
                    )?;
                    h = h.max(gen_h);
                    args.push(g);
                } else {
                    h = h.max(eh);
                    args.push(e);
                }
            }
            if self.at(&TokenKind::Comma) {
                self.advance()?;
            } else {
                break;
            }
        }
        Ok((args, keywords, h))
    }

    /// `for target in iter (if cond)*` clauses: `(clauses, max height inside, clause count)`.
    #[inline(never)]
    fn comprehensions(&mut self) -> ExprResult<(Vec<Comprehension>, usize, usize)> {
        let mut generators = Vec::new();
        let mut h = 0;
        while self.at(&TokenKind::For) {
            self.advance()?;
            let (target, th) = self.comprehension_target()?;
            self.expect(&TokenKind::In)?;
            let (iter, ih) = self.expr_bp(PREC_TERNARY + 1)?;
            let mut ifs = Vec::new();
            h = h.max(th).max(ih);
            while self.at(&TokenKind::If) {
                self.advance()?;
                let (cond, ch) = self.expr_bp(PREC_TERNARY + 1)?;
                h = h.max(ch);
                ifs.push(cond);
            }
            generators.push(Comprehension {
                target: Box::new(target),
                iter: Box::new(iter),
                ifs,
            });
            // Each clause is one level of evaluation recursion.
            if generators.len() + h > self.limits.max_depth() {
                return Err(self.depth_error());
            }
        }
        let count = generators.len();
        Ok((generators, h, count))
    }

    fn comprehension_target(&mut self) -> ExprResult<P> {
        let (first, fh) = self.expr_bp(PREC_CMP + 1)?;
        if !self.at(&TokenKind::Comma) {
            return Ok((first, fh));
        }
        let mut items = vec![first];
        let mut hs = vec![fh];
        while self.at(&TokenKind::Comma) {
            self.advance()?;
            if self.at(&TokenKind::In) {
                break;
            }
            let (e, h) = self.expr_bp(PREC_CMP + 1)?;
            items.push(e);
            hs.push(h);
        }
        self.node(Expr::Tuple(items), 1 + h_max(&hs))
    }

    fn optional_expr(&mut self, stops: &[TokenKind]) -> ExprResult<(Option<Box<Expr>>, usize)> {
        if stops.contains(&self.current.kind) {
            return Ok((None, 0));
        }
        let (e, h) = self.expr_bp(PREC_LOWEST)?;
        Ok((Some(Box::new(e)), h))
    }

    #[inline(never)]
    fn subscript(&mut self) -> ExprResult<(SubscriptKind, usize)> {
        let (lower, lh) = self.optional_expr(&[TokenKind::Colon])?;
        if !self.at(&TokenKind::Colon) {
            return match lower {
                Some(index) => {
                    if self.at(&TokenKind::Comma) {
                        return Err(self.err_here("tuple subscripts are not supported"));
                    }
                    Ok((SubscriptKind::Index(index), lh))
                }
                None => Err(self.err_here("expected a subscript")),
            };
        }
        self.advance()?;
        let (upper, uh) = self.optional_expr(&[TokenKind::Colon, TokenKind::RBracket])?;
        let (step, sh) = if self.at(&TokenKind::Colon) {
            self.advance()?;
            self.optional_expr(&[TokenKind::RBracket])?
        } else {
            (None, 0)
        };
        Ok((
            SubscriptKind::Slice { lower, upper, step },
            h_max(&[lh, uh, sh]),
        ))
    }

    /// Splits an f-string body into literal and formatted parts; embedded expressions are parsed
    /// by a sub-parser that continues this parser's recursion depth.
    #[inline(never)]
    fn fstring(&self, text: &str, span: Span) -> ExprResult<Vec<P>> {
        let err = |m: &str| ExprError::parse(m, span.start, span.line, span.col);
        let mut parts: Vec<P> = Vec::new();
        let mut literal = String::new();
        let mut chars = text.char_indices().peekable();
        while let Some((i, c)) = chars.next() {
            match c {
                '{' if chars.peek().map(|p| p.1) == Some('{') => {
                    chars.next();
                    literal.push('{');
                }
                '}' if chars.peek().map(|p| p.1) == Some('}') => {
                    chars.next();
                    literal.push('}');
                }
                '}' => return Err(err("f-string: single '}' is not allowed")),
                '{' => {
                    if !literal.is_empty() {
                        parts.push((Expr::Literal(Literal::Str(std::mem::take(&mut literal))), 1));
                    }
                    // Scan to the matching '}', tracking brackets and quotes so that `{x[1:2]}`,
                    // `{d['a']}` and `{a != b}` are read correctly.
                    let start = i + 1;
                    let mut depth = 0usize;
                    let mut quote: Option<char> = None;
                    let mut conv_at: Option<usize> = None;
                    let mut spec_at: Option<usize> = None;
                    let mut end = None;
                    let mut prev = '\0';
                    while let Some((j, d)) = chars.next() {
                        if let Some(q) = quote {
                            if d == q && prev != '\\' {
                                quote = None;
                            }
                        } else if spec_at.is_some() {
                            match d {
                                '{' => {
                                    return Err(err(
                                        "f-string: nested replacement fields in format specs are not supported",
                                    ));
                                }
                                '}' => {
                                    end = Some(j);
                                    break;
                                }
                                _ => {}
                            }
                        } else {
                            match d {
                                '\'' | '"' => quote = Some(d),
                                '(' | '[' | '{' => depth += 1,
                                ')' | ']' => depth = depth.saturating_sub(1),
                                '}' if depth > 0 => depth -= 1,
                                '}' => {
                                    end = Some(j);
                                    break;
                                }
                                '!' if depth == 0 && chars.peek().map(|p| p.1) != Some('=') => {
                                    conv_at = Some(j)
                                }
                                ':' if depth == 0 => spec_at = Some(j),
                                _ => {}
                            }
                        }
                        prev = d;
                    }
                    let end = end.ok_or_else(|| err("f-string: expecting '}'"))?;
                    let expr_end = conv_at.or(spec_at).unwrap_or(end);
                    let src = text
                        .get(start..expr_end)
                        .ok_or_else(|| err("f-string: invalid expression"))?;
                    let conversion = match conv_at {
                        Some(c) => {
                            let conv_end = spec_at.unwrap_or(end);
                            let conv = text.get(c + 1..conv_end).unwrap_or("");
                            match conv {
                                "r" | "s" | "a" => conv.chars().next(),
                                _ => {
                                    return Err(err(
                                        "f-string: invalid conversion character: expected 's', 'r', or 'a'",
                                    ));
                                }
                            }
                        }
                        None => None,
                    };
                    let format_spec = match spec_at {
                        Some(s) => Some(
                            text.get(s + 1..end)
                                .ok_or_else(|| err("f-string: invalid format spec"))?
                                .to_string(),
                        ),
                        None => None,
                    };
                    if src.trim().is_empty() {
                        return Err(err("f-string: empty expression not allowed"));
                    }
                    // A replacement field is two syntactic levels (the string and the field) and
                    // the most stack-hungry construct to parse, so it counts as two levels here.
                    let mut sub = Box::new(Parser::new(src, self.limits, self.depth + 2)?);
                    let (value, vh) = sub.parse_expression()?;
                    parts.push(self.node(
                        Expr::FormattedValue {
                            value: Box::new(value),
                            conversion,
                            format_spec,
                        },
                        vh + 1,
                    )?);
                }
                other => literal.push(other),
            }
        }
        if !literal.is_empty() {
            parts.push((Expr::Literal(Literal::Str(literal)), 1));
        }
        Ok(parts)
    }
}

enum StrPart {
    Plain(String),
    Format(String, Span),
}

fn push_literal(pieces: &mut Vec<Expr>, s: String) {
    if let Some(Expr::Literal(Literal::Str(prev))) = pieces.last_mut() {
        prev.push_str(&s);
    } else if !s.is_empty() {
        pieces.push(Expr::Literal(Literal::Str(s)));
    }
}

fn infix_binding_power(kind: &TokenKind) -> (u8, u8) {
    match kind {
        TokenKind::Walrus => (PREC_WALRUS, PREC_WALRUS),
        TokenKind::If => (PREC_TERNARY, PREC_TERNARY),
        TokenKind::Or => (PREC_OR, PREC_OR + 1),
        TokenKind::And => (PREC_AND, PREC_AND + 1),
        TokenKind::Eq
        | TokenKind::NotEq
        | TokenKind::Lt
        | TokenKind::LtE
        | TokenKind::Gt
        | TokenKind::GtE
        | TokenKind::In
        | TokenKind::Is
        | TokenKind::Not => (PREC_CMP, PREC_CMP + 1),
        TokenKind::Pipe => (PREC_BIT_OR, PREC_BIT_OR + 1),
        TokenKind::Caret => (PREC_BIT_XOR, PREC_BIT_XOR + 1),
        TokenKind::Ampersand => (PREC_BIT_AND, PREC_BIT_AND + 1),
        TokenKind::LShift | TokenKind::RShift => (PREC_SHIFT, PREC_SHIFT + 1),
        TokenKind::Plus | TokenKind::Minus => (PREC_TERM, PREC_TERM + 1),
        TokenKind::Star | TokenKind::Slash | TokenKind::DoubleSlash | TokenKind::Percent => {
            (PREC_FACTOR, PREC_FACTOR + 1)
        }
        // Right-associative, and binds tighter than a unary minus on its left (`-2**2 == -4`)
        // but looser than one on its right (`2**-1`).
        TokenKind::DoubleStar => (PREC_POW, PREC_UNARY),
        _ => (0, 0),
    }
}

/// Parses an expression with the default (ceiling) limits.
pub fn parse(input: &str) -> ExprResult<Expr> {
    parse_with_limits(input, EvalLimits::default())
}

/// Parses an expression with the given limits.
pub fn parse_with_limits(input: &str, limits: EvalLimits) -> ExprResult<Expr> {
    let mut parser = Parser::new(input, limits, 0)?;
    Ok(parser.parse_expression()?.0)
}
