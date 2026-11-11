//! Render an expression back to Python-like text for error messages (`RuleError::Unsupported`
//! names the expression it refuses). Bounded: the output is cut at [`MAX_RENDER`] characters.

use erp_expr::{BinaryOp, BoolOp, CompareOp, Expr, Literal, SubscriptKind, UnaryOp};

/// Longest rendering of an expression in a message (characters).
pub(crate) const MAX_RENDER: usize = 120;

/// The Python repr of a string (single quotes unless the text has one and no double quote).
pub(crate) fn py_str_repr(s: &str) -> String {
    let quote = if s.contains('\'') && !s.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::with_capacity(s.len() + 2);
    out.push(quote);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if c.is_control() => out.push_str(&format!("\\x{:02x}", u32::from(c))),
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

/// `expr` as text, at most [`MAX_RENDER`] characters (`…` marks a cut).
pub(crate) fn render(expr: &Expr) -> String {
    let mut out = Out {
        text: String::new(),
        full: false,
    };
    write(&mut out, expr);
    let mut text = out.text;
    if let Some((cut, _)) = text.char_indices().nth(MAX_RENDER) {
        text.truncate(cut);
        text.push('…');
    }
    text
}

struct Out {
    text: String,
    full: bool,
}

impl Out {
    fn push(&mut self, s: &str) {
        if self.full {
            return;
        }
        self.text.push_str(s);
        // keep a little more than the cut so the ellipsis is placed by `render`
        if self.text.len() > MAX_RENDER * 4 {
            self.full = true;
        }
    }
}

fn write_all(out: &mut Out, items: &[Expr], sep: &str) {
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            out.push(sep);
        }
        write(out, item);
    }
}

// The wildcard arms below cover erp-expr's `#[non_exhaustive]` syntax enums: they only change
// how a refused expression is shown in a message, never what is refused.
fn write(out: &mut Out, expr: &Expr) {
    if out.full {
        return;
    }
    match expr {
        Expr::Literal(literal) => out.push(&literal_text(literal)),
        Expr::Name(name) => out.push(name),
        Expr::Unary { op, operand } => {
            out.push(match op {
                UnaryOp::UAdd => "+",
                UnaryOp::USub => "-",
                UnaryOp::Not => "not ",
                UnaryOp::Invert => "~",
                _ => "<unary> ",
            });
            write(out, operand);
        }
        Expr::Binary { op, left, right } => {
            write(out, left);
            out.push(match op {
                BinaryOp::Add => " + ",
                BinaryOp::Sub => " - ",
                BinaryOp::Mult => " * ",
                BinaryOp::Div => " / ",
                BinaryOp::FloorDiv => " // ",
                BinaryOp::Mod => " % ",
                BinaryOp::Pow => " ** ",
                BinaryOp::BitAnd => " & ",
                BinaryOp::BitOr => " | ",
                BinaryOp::BitXor => " ^ ",
                BinaryOp::LShift => " << ",
                BinaryOp::RShift => " >> ",
                _ => " <op> ",
            });
            write(out, right);
        }
        Expr::BoolOp { op, values } => write_all(
            out,
            values,
            match op {
                BoolOp::And => " and ",
                BoolOp::Or => " or ",
                _ => " <bool op> ",
            },
        ),
        Expr::Compare {
            left,
            ops,
            comparators,
        } => {
            write(out, left);
            for (op, right) in ops.iter().zip(comparators) {
                out.push(match op {
                    CompareOp::Eq => " == ",
                    CompareOp::NotEq => " != ",
                    CompareOp::Lt => " < ",
                    CompareOp::LtE => " <= ",
                    CompareOp::Gt => " > ",
                    CompareOp::GtE => " >= ",
                    CompareOp::In => " in ",
                    CompareOp::NotIn => " not in ",
                    CompareOp::Is => " is ",
                    CompareOp::IsNot => " is not ",
                    _ => " <cmp> ",
                });
                write(out, right);
            }
        }
        Expr::IfExp { test, body, orelse } => {
            write(out, body);
            out.push(" if ");
            write(out, test);
            out.push(" else ");
            write(out, orelse);
        }
        Expr::Attribute { value, attr } => {
            write(out, value);
            out.push(".");
            out.push(attr);
        }
        Expr::Subscript { value, slice } => {
            write(out, value);
            out.push("[");
            match slice {
                SubscriptKind::Index(index) => write(out, index),
                SubscriptKind::Slice { lower, upper, step } => {
                    if let Some(lower) = lower {
                        write(out, lower);
                    }
                    out.push(":");
                    if let Some(upper) = upper {
                        write(out, upper);
                    }
                    if let Some(step) = step {
                        out.push(":");
                        write(out, step);
                    }
                }
                _ => out.push("…"),
            }
            out.push("]");
        }
        Expr::Call {
            func,
            args,
            keywords,
        } => {
            write(out, func);
            out.push("(");
            write_all(out, args, ", ");
            for (i, (name, value)) in keywords.iter().enumerate() {
                if i > 0 || !args.is_empty() {
                    out.push(", ");
                }
                match name {
                    Some(name) => {
                        out.push(name);
                        out.push("=");
                    }
                    None => out.push("**"),
                }
                write(out, value);
            }
            out.push(")");
        }
        Expr::List(items) => {
            out.push("[");
            write_all(out, items, ", ");
            out.push("]");
        }
        Expr::Tuple(items) => {
            out.push("(");
            write_all(out, items, ", ");
            if items.len() == 1 {
                out.push(",");
            }
            out.push(")");
        }
        Expr::Dict(pairs) => {
            out.push("{");
            for (i, (key, value)) in pairs.iter().enumerate() {
                if i > 0 {
                    out.push(", ");
                }
                write(out, key);
                out.push(": ");
                write(out, value);
            }
            out.push("}");
        }
        Expr::Walrus { name, value } => {
            out.push(name);
            out.push(" := ");
            write(out, value);
        }
        Expr::Starred(inner) => {
            out.push("*");
            write(out, inner);
        }
        Expr::JoinedStr(_) | Expr::FormattedValue { .. } => out.push("f'…'"),
        Expr::ListComp { .. } => out.push("[… for …]"),
        Expr::GeneratorExp { .. } => out.push("(… for …)"),
        _ => out.push("<expression>"),
    }
}

fn literal_text(literal: &Literal) -> String {
    match literal {
        Literal::None => "None".to_owned(),
        Literal::Bool(true) => "True".to_owned(),
        Literal::Bool(false) => "False".to_owned(),
        Literal::Int(i) => i.to_string(),
        Literal::Float(f) => format!("{f:?}"),
        Literal::Str(s) => py_str_repr(s),
        Literal::Bytes(b) => format!("b<{} bytes>", b.len()),
        _ => "<literal>".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(src: &str) -> String {
        render(&erp_expr::parse(src).unwrap())
    }

    #[test]
    fn renders_the_rule_forms() {
        assert_eq!(
            r("user.has_group('base.group_user')"),
            "user.has_group('base.group_user')"
        );
        assert_eq!(
            r("user.env['res.partner'].search([])"),
            "user.env['res.partner'].search([])"
        );
        assert_eq!(r("company_ids + [False]"), "company_ids + [False]");
        assert_eq!(r("(1,)"), "(1,)");
        assert_eq!(r("user.x if a else -1"), "user.x if a else -1");
        assert_eq!(r("f(a, b=2)"), "f(a, b=2)");
        assert_eq!(py_str_repr("it's"), "\"it's\"");
        assert_eq!(py_str_repr("a\nb'\""), "'a\\nb\\'\"'");
        let long = format!("[{}]", vec!["1"; 200].join(", "));
        let text = r(&long);
        assert_eq!(text.chars().count(), MAX_RENDER + 1);
        assert!(text.ends_with('…'));
    }
}
