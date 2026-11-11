//! Source-derived reservation for embedding an admitted expression as its wire document.
//!
//! `erp_expr::wire::WireDocument::from_expr` allocates without an external budget hook, so the
//! description builder computes, from the admitted [`Expr`] alone and before calling it, an upper
//! bound of the bytes that call keeps alive at once (the final arena and containers, plus the
//! previous buffer a growing `Vec` still holds, plus the validator's per-node scratch). Nothing
//! here allocates, and no multiplier is guessed: every term is a count read from the tree times a
//! `size_of`, with the growth rule of the std containers `convert.rs` fills them with. The
//! allocation harness (`tests/alloc_expression_preflight.rs`) checks the bound against measured
//! peak live bytes.
use crate::{ViewError, ViewResult};
use erp_expr::wire::{
    MAX_AST_DEPTH, MAX_NODES, WireCompareOp, WireComprehension, WireDocument, WireKeyword,
    WireLiteral, WireNode,
};
use erp_expr::{Expr, Literal, SubscriptKind};
use std::mem::size_of;

fn refuse(message: &str) -> ViewError {
    ViewError::InvalidDescription(message.to_owned())
}

/// What building the wire document of one admitted expression needs, known before it is built.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExpressionPreflight {
    nodes: usize,
    payload_bytes: usize,
    reserve_bytes: usize,
}
impl ExpressionPreflight {
    /// Expression nodes, which is also the wire document's node count.
    pub fn nodes(&self) -> usize {
        self.nodes
    }
    /// Text and byte-literal payload bytes copied into the document.
    pub fn payload_bytes(&self) -> usize {
        self.payload_bytes
    }
    /// Bytes to reserve before the document is built.
    pub fn reserve_bytes(&self) -> usize {
        self.reserve_bytes
    }
}

struct Tally {
    nodes: usize,
    payload_bytes: usize,
    reserve_bytes: usize,
}
impl Tally {
    fn reserve(&mut self, bytes: usize) -> ViewResult<()> {
        self.reserve_bytes = self
            .reserve_bytes
            .checked_add(bytes)
            .ok_or_else(|| refuse("Description expression reservation overflow"))?;
        Ok(())
    }
    /// `to_owned()` of a string or byte literal: exactly its length.
    fn exact_text(&mut self, len: usize) -> ViewResult<()> {
        self.payload_bytes = self
            .payload_bytes
            .checked_add(len)
            .ok_or_else(|| refuse("Description expression reservation overflow"))?;
        self.reserve(len)
    }
    /// A `String` filled by `fmt::Write` (integers, float bits, a conversion char): capacity at
    /// most the next power of two, never below std's eight-byte minimum; old buffer included.
    fn formatted_text(&mut self, len: usize) -> ViewResult<()> {
        let capacity = len.max(8).checked_next_power_of_two();
        self.reserve(
            capacity
                .and_then(|c| c.checked_mul(2))
                .ok_or_else(|| refuse("Description expression reservation overflow"))?,
        )
    }
    /// A `Vec` filled by `push`/`collect` without a length hint: first allocation holds at least
    /// four elements, then doubles; the previous buffer is alive while the next is allocated.
    fn grown_vec(&mut self, len: usize, element: usize) -> ViewResult<()> {
        if len == 0 {
            return Ok(());
        }
        let capacity = len.max(4).checked_next_power_of_two();
        self.reserve(
            capacity
                .and_then(|c| c.checked_mul(element))
                .and_then(|c| c.checked_mul(2))
                .ok_or_else(|| refuse("Description expression reservation overflow"))?,
        )
    }
    fn many(&mut self, values: &[Expr], depth: usize) -> ViewResult<()> {
        self.grown_vec(values.len(), size_of::<u32>())?;
        values.iter().try_for_each(|value| self.node(value, depth))
    }
    fn node(&mut self, expr: &Expr, depth: usize) -> ViewResult<()> {
        if depth > MAX_AST_DEPTH {
            return Err(refuse("Description expression exceeds its logical depth"));
        }
        self.nodes += 1;
        if self.nodes > MAX_NODES {
            return Err(refuse("Description expression exceeds its node limit"));
        }
        let next = depth + 1;
        match expr {
            Expr::Literal(Literal::None | Literal::Bool(_)) => {}
            Expr::Literal(Literal::Int(value)) => {
                let mut digits = Digits(0);
                std::fmt::Write::write_fmt(&mut digits, format_args!("{value}"))
                    .map_err(|_| refuse("Description expression integer could not be sized"))?;
                self.formatted_text(digits.0)?;
            }
            // `format!("{:016x}")`: sixteen characters.
            Expr::Literal(Literal::Float(_)) => self.formatted_text(16)?,
            Expr::Literal(Literal::Str(value)) => self.exact_text(value.len())?,
            Expr::Literal(Literal::Bytes(value)) => self.exact_text(value.len())?,
            Expr::Name(name) => self.exact_text(name.len())?,
            Expr::Unary { operand, .. } => self.node(operand, next)?,
            Expr::Binary { left, right, .. } => {
                self.node(left, next)?;
                self.node(right, next)?;
            }
            Expr::BoolOp { values, .. } => self.many(values, next)?,
            Expr::Compare {
                left,
                ops,
                comparators,
            } => {
                self.grown_vec(ops.len(), size_of::<WireCompareOp>())?;
                self.node(left, next)?;
                self.many(comparators, next)?;
            }
            Expr::IfExp { test, body, orelse } => {
                self.node(test, next)?;
                self.node(body, next)?;
                self.node(orelse, next)?;
            }
            Expr::Attribute { value, attr } => {
                self.exact_text(attr.len())?;
                self.node(value, next)?;
            }
            Expr::Subscript { value, slice } => {
                self.node(value, next)?;
                match slice {
                    SubscriptKind::Index(index) => self.node(index, next)?,
                    SubscriptKind::Slice { lower, upper, step } => {
                        for part in [lower, upper, step].into_iter().flatten() {
                            self.node(part, next)?;
                        }
                    }
                    _ => return Err(refuse("Description expression has an unsupported slice")),
                }
            }
            Expr::Call {
                func,
                args,
                keywords,
            } => {
                self.node(func, next)?;
                self.many(args, next)?;
                self.grown_vec(keywords.len(), size_of::<WireKeyword>())?;
                for (name, value) in keywords {
                    if let Some(name) = name {
                        self.exact_text(name.len())?;
                    }
                    self.node(value, next)?;
                }
            }
            Expr::List(items) | Expr::Tuple(items) | Expr::JoinedStr(items) => {
                self.many(items, next)?;
            }
            Expr::Dict(items) => {
                self.grown_vec(items.len(), size_of::<(u32, u32)>())?;
                for (key, value) in items {
                    self.node(key, next)?;
                    self.node(value, next)?;
                }
            }
            Expr::Walrus { name, value } => {
                self.exact_text(name.len())?;
                self.node(value, next)?;
            }
            Expr::FormattedValue {
                value,
                conversion,
                format_spec,
            } => {
                if let Some(conversion) = conversion {
                    self.formatted_text(conversion.len_utf8())?;
                }
                if let Some(spec) = format_spec {
                    self.exact_text(spec.len())?;
                }
                self.node(value, next)?;
            }
            Expr::ListComp { elt, generators } | Expr::GeneratorExp { elt, generators } => {
                // Clause levels count toward logical depth; recursion depth is never above it.
                self.grown_vec(generators.len(), size_of::<WireComprehension>())?;
                self.node(elt, next)?;
                for generator in generators {
                    self.node(&generator.target, next)?;
                    self.node(&generator.iter, next)?;
                    self.many(&generator.ifs, next)?;
                }
            }
            Expr::Starred(value) => self.node(value, next)?,
            _ => return Err(refuse("Description expression has an unsupported node")),
        }
        Ok(())
    }
}

struct Digits(usize);
impl std::fmt::Write for Digits {
    fn write_str(&mut self, text: &str) -> std::fmt::Result {
        self.0 += text.len();
        Ok(())
    }
}

/// Bound the bytes `WireDocument::from_expr` keeps alive at once for this admitted tree.
///
/// Fixed terms: the node arena (starts at one slot, doubles; the previous buffer is alive during
/// the last growth) and the validator's two scratch bytes per node. Per-node terms are read off
/// the tree in the same order `convert.rs` fills them. Refuses a tree beyond the wire limits.
pub fn expression_preflight(expr: &Expr) -> ViewResult<ExpressionPreflight> {
    let mut tally = Tally {
        nodes: 0,
        payload_bytes: 0,
        reserve_bytes: 0,
    };
    tally.node(expr, 1)?;
    let arena = tally
        .nodes
        .checked_next_power_of_two()
        .and_then(|c| c.checked_mul(size_of::<WireNode>()))
        .and_then(|c| c.checked_mul(2))
        .ok_or_else(|| refuse("Description expression reservation overflow"))?;
    tally.reserve(arena)?;
    let scratch = tally
        .nodes
        .checked_mul(size_of::<(bool, u8)>())
        .ok_or_else(|| refuse("Description expression reservation overflow"))?;
    tally.reserve(scratch)?;
    Ok(ExpressionPreflight {
        nodes: tally.nodes,
        payload_bytes: tally.payload_bytes,
        reserve_bytes: tally.reserve_bytes,
    })
}

/// Bytes the built document keeps alive: every capacity actually held, not a length.
pub(crate) fn retained_bytes(document: &WireDocument) -> ViewResult<usize> {
    let mut total = document
        .nodes
        .capacity()
        .checked_mul(size_of::<WireNode>())
        .ok_or_else(|| refuse("Description expression storage overflow"))?;
    let mut add = |bytes: usize| -> ViewResult<()> {
        total = total
            .checked_add(bytes)
            .ok_or_else(|| refuse("Description expression storage overflow"))?;
        Ok(())
    };
    let edges = |values: &Vec<u32>| values.capacity() * size_of::<u32>();
    for node in &document.nodes {
        match node {
            WireNode::Literal { value } => match value {
                WireLiteral::None {} | WireLiteral::Bool { .. } => {}
                WireLiteral::Int { value }
                | WireLiteral::Float { value }
                | WireLiteral::Str { value } => add(value.capacity())?,
                WireLiteral::Bytes { value } => add(value.capacity())?,
                _ => return Err(refuse("Description expression has an unsupported literal")),
            },
            WireNode::Name { name } | WireNode::Walrus { name, .. } => add(name.capacity())?,
            WireNode::Attribute { attr, .. } => add(attr.capacity())?,
            WireNode::Unary { .. }
            | WireNode::Binary { .. }
            | WireNode::IfExp { .. }
            | WireNode::Subscript { .. }
            | WireNode::Starred { .. } => {}
            WireNode::BoolOp { values, .. } => add(edges(values))?,
            WireNode::List { items }
            | WireNode::Tuple { items }
            | WireNode::JoinedStr { items } => add(edges(items))?,
            WireNode::Compare {
                ops, comparators, ..
            } => {
                add(ops.capacity() * size_of::<WireCompareOp>())?;
                add(edges(comparators))?;
            }
            WireNode::Call { args, keywords, .. } => {
                add(edges(args))?;
                add(keywords.capacity() * size_of::<WireKeyword>())?;
                for keyword in keywords {
                    add(keyword.name.as_ref().map_or(0, String::capacity))?;
                }
            }
            WireNode::Dict { items } => add(items.capacity() * size_of::<(u32, u32)>())?,
            WireNode::FormattedValue {
                conversion,
                format_spec,
                ..
            } => {
                add(conversion.as_ref().map_or(0, String::capacity))?;
                add(format_spec.as_ref().map_or(0, String::capacity))?;
            }
            WireNode::ListComp { generators, .. } | WireNode::GeneratorExp { generators, .. } => {
                add(generators.capacity() * size_of::<WireComprehension>())?;
                for generator in generators {
                    add(edges(&generator.ifs))?;
                }
            }
            _ => {
                return Err(refuse(
                    "Description expression has an unsupported wire node",
                ));
            }
        }
    }
    Ok(total)
}
