use super::*;
use crate::{Comprehension, Literal, SubscriptKind};

struct Encoder {
    nodes: Vec<WireNode>,
    payload_bytes: usize,
}
pub(super) fn from_expr(expr: &Expr) -> Result<WireDocument, WireError> {
    let mut encoder = Encoder {
        nodes: Vec::with_capacity(1),
        payload_bytes: 0,
    };
    encoder.node(expr, 1)?;
    Ok(WireDocument {
        schema: 1,
        nodes: encoder.nodes,
    })
}
impl Encoder {
    fn payload(&mut self, length: usize) -> Result<(), WireError> {
        self.payload_bytes = self
            .payload_bytes
            .checked_add(length)
            .filter(|n| *n <= MAX_ENCODED_BYTES)
            .ok_or_else(byte_limit)?;
        Ok(())
    }
    fn text(&mut self, value: &str) -> Result<String, WireError> {
        self.payload(value.len())?;
        Ok(value.to_owned())
    }
    fn optional_text(&mut self, value: Option<&str>) -> Result<Option<String>, WireError> {
        value.map(|v| self.text(v)).transpose()
    }
    fn many(&mut self, values: &[Expr], depth: usize) -> Result<Vec<u32>, WireError> {
        if values.len() >= MAX_NODES {
            return Err(node_limit());
        }
        values.iter().map(|expr| self.node(expr, depth)).collect()
    }
    fn optional_node(
        &mut self,
        value: Option<&Expr>,
        depth: usize,
    ) -> Result<Option<u32>, WireError> {
        value.map(|expr| self.node(expr, depth)).transpose()
    }
    fn generators(
        &mut self,
        values: &[Comprehension],
        depth: usize,
    ) -> Result<Vec<WireComprehension>, WireError> {
        values
            .iter()
            .map(|generator| {
                Ok(WireComprehension {
                    target: self.node(&generator.target, depth)?,
                    iter: self.node(&generator.iter, depth)?,
                    ifs: self.many(&generator.ifs, depth)?,
                })
            })
            .collect()
    }
    fn node(&mut self, expr: &Expr, depth: usize) -> Result<u32, WireError> {
        if depth > MAX_AST_DEPTH {
            return Err(depth_limit());
        }
        if self.nodes.len() >= MAX_NODES {
            return Err(node_limit());
        }
        let next = depth.checked_add(1).ok_or_else(depth_limit)?;
        let node = match expr {
            Expr::Literal(value) => WireNode::Literal {
                value: match value {
                    Literal::None => WireLiteral::None {},
                    Literal::Bool(value) => WireLiteral::Bool { value: *value },
                    Literal::Int(value) => WireLiteral::Int {
                        value: value.to_string(),
                    },
                    Literal::Float(value) => WireLiteral::Float {
                        value: format!("{:016x}", value.to_bits()),
                    },
                    Literal::Str(value) => WireLiteral::Str {
                        value: self.text(value)?,
                    },
                    Literal::Bytes(value) => {
                        self.payload(value.len())?;
                        WireLiteral::Bytes {
                            value: value.clone(),
                        }
                    }
                },
            },
            Expr::Name(name) => WireNode::Name {
                name: self.text(name)?,
            },
            Expr::Unary { op, operand } => WireNode::Unary {
                op: (*op).into(),
                operand: self.node(operand, next)?,
            },
            Expr::Binary { op, left, right } => WireNode::Binary {
                op: (*op).into(),
                left: self.node(left, next)?,
                right: self.node(right, next)?,
            },
            Expr::BoolOp { op, values } => WireNode::BoolOp {
                op: (*op).into(),
                values: self.many(values, next)?,
            },
            Expr::Compare {
                left,
                ops,
                comparators,
            } => {
                if ops.len() >= MAX_NODES {
                    return Err(node_limit());
                }
                WireNode::Compare {
                    left: self.node(left, next)?,
                    ops: ops.iter().map(|op| (*op).into()).collect(),
                    comparators: self.many(comparators, next)?,
                }
            }
            Expr::IfExp { test, body, orelse } => WireNode::IfExp {
                test: self.node(test, next)?,
                body: self.node(body, next)?,
                orelse: self.node(orelse, next)?,
            },
            Expr::Attribute { value, attr } => WireNode::Attribute {
                value: self.node(value, next)?,
                attr: self.text(attr)?,
            },
            Expr::Subscript { value, slice } => WireNode::Subscript {
                value: self.node(value, next)?,
                slice: match slice {
                    SubscriptKind::Index(index) => WireSubscript::Index {
                        value: self.node(index, next)?,
                    },
                    SubscriptKind::Slice { lower, upper, step } => WireSubscript::Slice {
                        lower: self.optional_node(lower.as_deref(), next)?,
                        upper: self.optional_node(upper.as_deref(), next)?,
                        step: self.optional_node(step.as_deref(), next)?,
                    },
                },
            },
            Expr::Call {
                func,
                args,
                keywords,
            } => {
                if keywords.len() >= MAX_NODES {
                    return Err(node_limit());
                }
                WireNode::Call {
                    func: self.node(func, next)?,
                    args: self.many(args, next)?,
                    keywords: keywords
                        .iter()
                        .map(|(name, value)| {
                            Ok(WireKeyword {
                                name: self.optional_text(name.as_deref())?,
                                value: self.node(value, next)?,
                            })
                        })
                        .collect::<Result<_, WireError>>()?,
                }
            }
            Expr::List(items) => WireNode::List {
                items: self.many(items, next)?,
            },
            Expr::Tuple(items) => WireNode::Tuple {
                items: self.many(items, next)?,
            },
            Expr::Dict(items) => {
                if items.len() >= MAX_NODES / 2 {
                    return Err(node_limit());
                }
                WireNode::Dict {
                    items: items
                        .iter()
                        .map(|(key, value)| Ok((self.node(key, next)?, self.node(value, next)?)))
                        .collect::<Result<_, WireError>>()?,
                }
            }
            Expr::Walrus { name, value } => WireNode::Walrus {
                name: self.text(name)?,
                value: self.node(value, next)?,
            },
            Expr::JoinedStr(items) => WireNode::JoinedStr {
                items: self.many(items, next)?,
            },
            Expr::FormattedValue {
                value,
                conversion,
                format_spec,
            } => WireNode::FormattedValue {
                value: self.node(value, next)?,
                conversion: conversion.map(|v| v.to_string()),
                format_spec: self.optional_text(format_spec.as_deref())?,
            },
            Expr::ListComp { elt, generators } => {
                let depth = next
                    .checked_add(generators.len())
                    .filter(|n| *n <= MAX_AST_DEPTH)
                    .ok_or_else(depth_limit)?;
                WireNode::ListComp {
                    elt: self.node(elt, depth)?,
                    generators: self.generators(generators, depth)?,
                }
            }
            Expr::GeneratorExp { elt, generators } => {
                let depth = next
                    .checked_add(generators.len())
                    .filter(|n| *n <= MAX_AST_DEPTH)
                    .ok_or_else(depth_limit)?;
                WireNode::GeneratorExp {
                    elt: self.node(elt, depth)?,
                    generators: self.generators(generators, depth)?,
                }
            }
            Expr::Starred(value) => WireNode::Starred {
                value: self.node(value, next)?,
            },
        };
        if self.nodes.len() >= MAX_NODES {
            return Err(node_limit());
        }
        let index = u32::try_from(self.nodes.len()).map_err(|_| node_limit())?;
        self.nodes.push(node);
        Ok(index)
    }
}

// This consumes the validated postorder arena. Each child moves once: no recursive cloning or
// shared-subtree expansion. The resulting recursive tree has already passed the height limit.
pub(super) fn into_expr(document: WireDocument) -> Result<Expr, WireError> {
    let mut built = Vec::with_capacity(document.nodes.len());
    for node in document.nodes {
        let expr = match node {
            WireNode::Literal { value } => Expr::Literal(match value {
                WireLiteral::None {} => Literal::None,
                WireLiteral::Bool { value } => Literal::Bool(value),
                WireLiteral::Int { value } => {
                    Literal::Int(validate::parse_int(&value).map_err(|()| {
                        WireError::new("$.nodes", "validated integer became invalid")
                    })?)
                }
                WireLiteral::Float { value } => {
                    Literal::Float(validate::parse_float(&value).map_err(|()| {
                        WireError::new("$.nodes", "validated float became invalid")
                    })?)
                }
                WireLiteral::Str { value } => Literal::Str(value),
                WireLiteral::Bytes { value } => Literal::Bytes(value),
            }),
            WireNode::Name { name } => Expr::Name(name),
            WireNode::Unary { op, operand } => Expr::Unary {
                op: op.into(),
                operand: Box::new(take(&mut built, operand)?),
            },
            WireNode::Binary { op, left, right } => Expr::Binary {
                op: op.into(),
                left: Box::new(take(&mut built, left)?),
                right: Box::new(take(&mut built, right)?),
            },
            WireNode::BoolOp { op, values } => Expr::BoolOp {
                op: op.into(),
                values: take_many(&mut built, values)?,
            },
            WireNode::Compare {
                left,
                ops,
                comparators,
            } => Expr::Compare {
                left: Box::new(take(&mut built, left)?),
                ops: ops.into_iter().map(Into::into).collect(),
                comparators: take_many(&mut built, comparators)?,
            },
            WireNode::IfExp { test, body, orelse } => Expr::IfExp {
                test: Box::new(take(&mut built, test)?),
                body: Box::new(take(&mut built, body)?),
                orelse: Box::new(take(&mut built, orelse)?),
            },
            WireNode::Attribute { value, attr } => Expr::Attribute {
                value: Box::new(take(&mut built, value)?),
                attr,
            },
            WireNode::Subscript { value, slice } => Expr::Subscript {
                value: Box::new(take(&mut built, value)?),
                slice: match slice {
                    WireSubscript::Index { value } => {
                        SubscriptKind::Index(Box::new(take(&mut built, value)?))
                    }
                    WireSubscript::Slice { lower, upper, step } => SubscriptKind::Slice {
                        lower: take_optional(&mut built, lower)?,
                        upper: take_optional(&mut built, upper)?,
                        step: take_optional(&mut built, step)?,
                    },
                },
            },
            WireNode::Call {
                func,
                args,
                keywords,
            } => Expr::Call {
                func: Box::new(take(&mut built, func)?),
                args: take_many(&mut built, args)?,
                keywords: keywords
                    .into_iter()
                    .map(|keyword| Ok((keyword.name, take(&mut built, keyword.value)?)))
                    .collect::<Result<_, WireError>>()?,
            },
            WireNode::List { items } => Expr::List(take_many(&mut built, items)?),
            WireNode::Tuple { items } => Expr::Tuple(take_many(&mut built, items)?),
            WireNode::Dict { items } => Expr::Dict(
                items
                    .into_iter()
                    .map(|(key, value)| Ok((take(&mut built, key)?, take(&mut built, value)?)))
                    .collect::<Result<_, WireError>>()?,
            ),
            WireNode::Walrus { name, value } => Expr::Walrus {
                name,
                value: Box::new(take(&mut built, value)?),
            },
            WireNode::JoinedStr { items } => Expr::JoinedStr(take_many(&mut built, items)?),
            WireNode::FormattedValue {
                value,
                conversion,
                format_spec,
            } => Expr::FormattedValue {
                value: Box::new(take(&mut built, value)?),
                conversion: conversion
                    .map(|s| {
                        s.chars().next().ok_or_else(|| {
                            WireError::new("$.nodes", "validated conversion became empty")
                        })
                    })
                    .transpose()?,
                format_spec,
            },
            WireNode::ListComp { elt, generators } => Expr::ListComp {
                elt: Box::new(take(&mut built, elt)?),
                generators: take_generators(&mut built, generators)?,
            },
            WireNode::GeneratorExp { elt, generators } => Expr::GeneratorExp {
                elt: Box::new(take(&mut built, elt)?),
                generators: take_generators(&mut built, generators)?,
            },
            WireNode::Starred { value } => Expr::Starred(Box::new(take(&mut built, value)?)),
        };
        built.push(Some(expr));
    }
    built.pop().flatten().ok_or_else(node_limit)
}
fn take(built: &mut [Option<Expr>], index: u32) -> Result<Expr, WireError> {
    built
        .get_mut(index as usize)
        .and_then(Option::take)
        .ok_or_else(|| WireError::new("$.nodes", "validated child is missing"))
}
fn take_many(built: &mut [Option<Expr>], indices: Vec<u32>) -> Result<Vec<Expr>, WireError> {
    indices
        .into_iter()
        .map(|index| take(built, index))
        .collect()
}
fn take_optional(
    built: &mut [Option<Expr>],
    index: Option<u32>,
) -> Result<Option<Box<Expr>>, WireError> {
    index
        .map(|index| take(built, index).map(Box::new))
        .transpose()
}
fn take_generators(
    built: &mut [Option<Expr>],
    generators: Vec<WireComprehension>,
) -> Result<Vec<Comprehension>, WireError> {
    generators
        .into_iter()
        .map(|generator| {
            Ok(Comprehension {
                target: Box::new(take(built, generator.target)?),
                iter: Box::new(take(built, generator.iter)?),
                ifs: take_many(built, generator.ifs)?,
            })
        })
        .collect()
}
