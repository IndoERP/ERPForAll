//! The evaluator: walks a syntax tree against an immutable [`EvalContext`] under a fresh
//! [`Budget`].
//!
//! Recursion follows the tree, whose height the parser already bounds; every node also enters the
//! budget's depth counter, so hand-built trees are bounded too. Each node kind is evaluated in its
//! own small function to keep the per-level stack use low.

use std::collections::HashMap;

use crate::ast::{
    BinaryOp, BoolOp, CompareOp, Comprehension, Expr, Literal, SubscriptKind, UnaryOp,
};
use crate::context::{EvalContext, host_provided};
use crate::error::{ExprError, ExprResult};
use crate::format;
use crate::limits::Budget;
use crate::methods::{self, Iter};
use crate::ops;
use crate::parser::parse_with_limits;
use crate::value::{ExprValue, Kwargs, Map, SLOT};

/// Parses and evaluates `src` with the context's names and limits.
pub fn eval(src: &str, ctx: &EvalContext) -> ExprResult<ExprValue> {
    let ast = parse_with_limits(src, ctx.limits())?;
    eval_ast(&ast, ctx)
}

/// Evaluates an already-parsed expression. Counters start from zero on every call.
pub fn eval_ast(ast: &Expr, ctx: &EvalContext) -> ExprResult<ExprValue> {
    let mut ev = Evaluator {
        ctx,
        budget: Budget::new(ctx.limits()),
        comp: Vec::new(),
        locals: HashMap::new(),
    };
    ev.eval(ast)
}

struct Evaluator<'c> {
    ctx: &'c EvalContext,
    budget: Budget,
    /// Comprehension scopes, innermost last.
    comp: Vec<HashMap<String, ExprValue>>,
    /// `:=` bindings of this evaluation (never written to the host context).
    locals: HashMap<String, ExprValue>,
}

/// Items of a comprehension being built, with their running deep size.
struct Collected {
    items: Vec<ExprValue>,
    bytes: usize,
}

impl Evaluator<'_> {
    fn eval(&mut self, e: &Expr) -> ExprResult<ExprValue> {
        self.budget.tick()?;
        self.budget.enter()?;
        let r = self.dispatch(e);
        self.budget.leave();
        r
    }

    #[inline(never)]
    fn dispatch(&mut self, e: &Expr) -> ExprResult<ExprValue> {
        match e {
            Expr::Literal(lit) => self.literal(lit),
            Expr::Name(name) => self.name(name),
            Expr::Unary { op, operand } => self.unary(*op, operand),
            Expr::Binary { op, left, right } => self.binary(*op, left, right),
            Expr::BoolOp { op, values } => self.bool_op(*op, values),
            Expr::Compare {
                left,
                ops,
                comparators,
            } => self.compare(left, ops, comparators),
            Expr::IfExp { test, body, orelse } => {
                let cond = self.eval(test)?;
                if cond.is_truthy() {
                    self.eval(body)
                } else {
                    self.eval(orelse)
                }
            }
            Expr::Attribute { value, attr } => self.attribute(value, attr),
            Expr::Subscript { value, slice } => self.subscript(value, slice),
            Expr::Call {
                func,
                args,
                keywords,
            } => self.call(func, args, keywords),
            Expr::List(items) => self.sequence(items, true),
            Expr::Tuple(items) => self.sequence(items, false),
            Expr::Dict(entries) => self.dict(entries),
            Expr::Walrus { name, value } => self.walrus(name, value),
            Expr::JoinedStr(parts) => self.joined_str(parts),
            Expr::FormattedValue { .. } => self.joined_str(std::slice::from_ref(e)),
            Expr::ListComp { elt, generators } | Expr::GeneratorExp { elt, generators } => {
                self.comprehension(elt, generators)
            }
            Expr::Starred(_) => Err(ExprError::syntax(
                "starred expressions are only supported as call arguments",
            )),
        }
    }

    #[inline(never)]
    fn literal(&mut self, lit: &Literal) -> ExprResult<ExprValue> {
        Ok(match lit {
            Literal::None => ExprValue::None,
            Literal::Bool(b) => ExprValue::Bool(*b),
            Literal::Int(i) => ExprValue::Int(*i),
            Literal::Float(f) => ExprValue::Float(*f),
            Literal::Str(s) => {
                self.budget.alloc(SLOT.saturating_add(s.len()))?;
                ExprValue::Str(s.clone())
            }
            Literal::Bytes(b) => {
                self.budget.alloc(SLOT.saturating_add(b.len()))?;
                ExprValue::Bytes(b.clone())
            }
        })
    }

    /// Innermost binding of `name`; hashed scopes, at most one probe per nested comprehension
    /// (bounded by the depth limit) plus the walrus scope and the host context.
    fn lookup(&self, name: &str) -> Option<&ExprValue> {
        for scope in self.comp.iter().rev() {
            if let Some(v) = scope.get(name) {
                return Some(v);
            }
        }
        if let Some(v) = self.locals.get(name) {
            return Some(v);
        }
        self.ctx.get(name)
    }

    #[inline(never)]
    fn name(&mut self, name: &str) -> ExprResult<ExprValue> {
        self.budget.steps(self.comp.len())?;
        match self.lookup(name) {
            Some(v) => {
                let v = v.clone();
                self.budget.admit(&v)?;
                Ok(v)
            }
            None => match host_provided(name) {
                Some(provider) => Err(ExprError::unavailable(name, provider)),
                None => Err(ExprError::name_error(name)),
            },
        }
    }

    #[inline(never)]
    fn unary(&mut self, op: UnaryOp, operand: &Expr) -> ExprResult<ExprValue> {
        let v = self.eval(operand)?;
        ops::unary(op, &v)
    }

    #[inline(never)]
    fn binary(&mut self, op: BinaryOp, left: &Expr, right: &Expr) -> ExprResult<ExprValue> {
        let l = self.eval(left)?;
        let r = self.eval(right)?;
        ops::binary(op, &l, &r, &mut self.budget)
    }

    #[inline(never)]
    fn bool_op(&mut self, op: BoolOp, values: &[Expr]) -> ExprResult<ExprValue> {
        let mut last = ExprValue::None;
        for v in values {
            last = self.eval(v)?;
            let stop = match op {
                BoolOp::And => !last.is_truthy(),
                BoolOp::Or => last.is_truthy(),
            };
            if stop {
                break;
            }
        }
        Ok(last)
    }

    #[inline(never)]
    fn compare(
        &mut self,
        left: &Expr,
        ops_: &[CompareOp],
        comparators: &[Expr],
    ) -> ExprResult<ExprValue> {
        let mut current = self.eval(left)?;
        for (op, next_expr) in ops_.iter().zip(comparators) {
            let next = self.eval(next_expr)?;
            if !ops::compare(*op, &current, &next, &mut self.budget)? {
                return Ok(ExprValue::Bool(false));
            }
            current = next;
        }
        Ok(ExprValue::Bool(true))
    }

    /// `true` when values coming from `v` were produced by the host and must be charged here.
    fn from_host(v: &ExprValue) -> bool {
        match v {
            ExprValue::Object(o) => !o.self_charging(),
            ExprValue::Callable(c) => !c.self_charging(),
            _ => false,
        }
    }

    fn attribute_of(&mut self, v: ExprValue, attr: &str) -> ExprResult<ExprValue> {
        let host = Self::from_host(&v);
        let r = methods::get_attr(v, attr, &mut self.budget)?;
        if host {
            self.budget.admit(&r)?;
        } else {
            self.budget.check_value(&r)?;
        }
        Ok(r)
    }

    #[inline(never)]
    fn attribute(&mut self, value: &Expr, attr: &str) -> ExprResult<ExprValue> {
        let v = self.eval(value)?;
        self.attribute_of(v, attr)
    }

    fn slice_bound(&mut self, e: &Option<Box<Expr>>) -> ExprResult<Option<i64>> {
        let Some(e) = e else {
            return Ok(None);
        };
        match self.eval(e)? {
            ExprValue::None => Ok(None),
            v => v.as_int().map(Some).ok_or_else(|| {
                ExprError::type_error(
                    "slice indices must be integers or None or have an __index__ method",
                )
            }),
        }
    }

    #[inline(never)]
    fn subscript(&mut self, value: &Expr, slice: &SubscriptKind) -> ExprResult<ExprValue> {
        let target = self.eval(value)?;
        match slice {
            SubscriptKind::Index(idx) => {
                let key = self.eval(idx)?;
                if let ExprValue::Object(o) = &target {
                    let r = o.get_item(&key, &mut self.budget)?;
                    if o.self_charging() {
                        self.budget.check_value(&r)?;
                    } else {
                        self.budget.admit(&r)?;
                    }
                    return Ok(r);
                }
                methods::get_item(&target, &key, &mut self.budget)
            }
            SubscriptKind::Slice { lower, upper, step } => {
                let l = self.slice_bound(lower)?;
                let u = self.slice_bound(upper)?;
                let s = self.slice_bound(step)?;
                methods::get_slice(&target, l, u, s, &mut self.budget)
            }
        }
    }

    #[inline(never)]
    fn call(
        &mut self,
        func: &Expr,
        args: &[Expr],
        keywords: &[(Option<String>, Expr)],
    ) -> ExprResult<ExprValue> {
        if let Expr::Attribute { value, attr } = func {
            let recv = self.eval(value)?;
            if methods::has_method(&recv, attr) {
                let (a, k) = self.call_args(args, keywords)?;
                let r = methods::call_method(&recv, attr, &a, &k, &mut self.budget)?;
                self.budget.check_value(&r)?;
                return Ok(r);
            }
            let f = self.attribute_of(recv, attr)?;
            return self.invoke(f, args, keywords);
        }
        let f = self.eval(func)?;
        self.invoke(f, args, keywords)
    }

    fn invoke(
        &mut self,
        f: ExprValue,
        args: &[Expr],
        keywords: &[(Option<String>, Expr)],
    ) -> ExprResult<ExprValue> {
        if !matches!(f, ExprValue::Callable(_) | ExprValue::Object(_)) {
            return Err(ExprError::type_error(format!(
                "'{}' object is not callable",
                f.type_name()
            )));
        }
        let (a, k) = self.call_args(args, keywords)?;
        let host = Self::from_host(&f);
        let r = match &f {
            ExprValue::Callable(c) => c.call(&a, &k, &mut self.budget)?,
            ExprValue::Object(o) => o.call(&a, &k, &mut self.budget)?,
            other => {
                return Err(ExprError::type_error(format!(
                    "'{}' object is not callable",
                    other.type_name()
                )));
            }
        };
        if host {
            self.budget.admit(&r)?;
        } else {
            self.budget.check_value(&r)?;
        }
        Ok(r)
    }

    fn call_args(
        &mut self,
        args: &[Expr],
        keywords: &[(Option<String>, Expr)],
    ) -> ExprResult<(Vec<ExprValue>, Kwargs)> {
        let mut values = Vec::with_capacity(args.len());
        for a in args {
            if let Expr::Starred(inner) = a {
                let v = self.eval(inner)?;
                let mut it = Iter::new(v)
                    .map_err(|_| ExprError::type_error("argument after * must be an iterable"))?;
                while let Some(item) = it.next(&mut self.budget)? {
                    values.push(item);
                }
            } else {
                values.push(self.eval(a)?);
            }
        }
        let mut kwargs = Kwargs::with_capacity(keywords.len());
        for (name, e) in keywords {
            let v = self.eval(e)?;
            match name {
                Some(n) => {
                    if kwargs.contains_key(n) {
                        return Err(ExprError::syntax(format!("keyword argument repeated: {n}")));
                    }
                    kwargs.insert(n.clone(), v);
                }
                None => {
                    let ExprValue::Dict(m) = v else {
                        return Err(ExprError::type_error(format!(
                            "argument after ** must be a mapping, not {}",
                            v.type_name()
                        )));
                    };
                    for (k, val) in m.iter() {
                        let ExprValue::Str(k) = k else {
                            return Err(ExprError::type_error("keywords must be strings"));
                        };
                        if kwargs.contains_key(k) {
                            return Err(ExprError::type_error(format!(
                                "got multiple values for keyword argument '{k}'"
                            )));
                        }
                        kwargs.insert(k.clone(), val.clone());
                    }
                }
            }
        }
        Ok((values, kwargs))
    }

    #[inline(never)]
    fn sequence(&mut self, items: &[Expr], is_list: bool) -> ExprResult<ExprValue> {
        self.budget
            .alloc(SLOT.saturating_mul(items.len().saturating_add(1)))?;
        let mut out = Vec::with_capacity(items.len());
        for item in items {
            out.push(self.eval(item)?);
        }
        let v = if is_list {
            ExprValue::list(out)
        } else {
            ExprValue::tuple(out)
        };
        self.budget.check_value(&v)?;
        Ok(v)
    }

    #[inline(never)]
    fn dict(&mut self, entries: &[(Expr, Expr)]) -> ExprResult<ExprValue> {
        self.budget.alloc(
            SLOT.saturating_add(entries.len().saturating_mul(SLOT.saturating_mul(2) + 16)),
        )?;
        let mut pairs = Vec::with_capacity(entries.len());
        for (k, v) in entries {
            let key = self.eval(k)?;
            let value = self.eval(v)?;
            pairs.push((key, value));
        }
        let d = ExprValue::Dict(Map::from_pairs(pairs)?);
        self.budget.check_value(&d)?;
        Ok(d)
    }

    #[inline(never)]
    fn walrus(&mut self, name: &str, value: &Expr) -> ExprResult<ExprValue> {
        self.budget.steps(self.comp.len())?;
        if self.comp.iter().any(|s| s.contains_key(name)) {
            return Err(ExprError::syntax(format!(
                "assignment expression cannot rebind comprehension iteration variable '{name}'"
            )));
        }
        let v = self.eval(value)?;
        self.budget.check_value(&v)?;
        let bound = v.clone();
        self.budget.admit(&bound)?;
        self.locals.insert(name.to_string(), bound);
        Ok(v)
    }

    #[inline(never)]
    fn joined_str(&mut self, parts: &[Expr]) -> ExprResult<ExprValue> {
        let mut out = self.budget.out();
        for part in parts {
            match part {
                Expr::Literal(Literal::Str(s)) => out.push_str(s)?,
                Expr::FormattedValue {
                    value,
                    conversion,
                    format_spec,
                } => {
                    let v = self.eval(value)?;
                    let spec = format_spec.as_deref().unwrap_or("");
                    format::write_field(&mut out, &v, *conversion, spec, &mut self.budget)?;
                }
                other => {
                    let v = self.eval(other)?;
                    format::write_field(&mut out, &v, None, "", &mut self.budget)?;
                }
            }
        }
        Ok(ExprValue::Str(self.budget.finish(out)?))
    }

    #[inline(never)]
    fn comprehension(&mut self, elt: &Expr, generators: &[Comprehension]) -> ExprResult<ExprValue> {
        self.comp.push(HashMap::new());
        let mut out = Collected {
            items: Vec::new(),
            bytes: SLOT,
        };
        let r = self.comp_level(elt, generators, 0, &mut out);
        self.comp.pop();
        r?;
        self.budget.alloc(SLOT)?;
        let list = ExprValue::list(out.items);
        self.budget.check_value(&list)?;
        Ok(list)
    }

    fn comp_level(
        &mut self,
        elt: &Expr,
        gens: &[Comprehension],
        i: usize,
        out: &mut Collected,
    ) -> ExprResult<()> {
        let Some(generator) = gens.get(i) else {
            let v = self.eval(elt)?;
            self.budget.alloc(SLOT)?;
            out.bytes = out.bytes.saturating_add(v.deep_size());
            self.budget.check_size(out.bytes)?;
            out.items.push(v);
            return Ok(());
        };
        self.budget.enter()?;
        let r = self.comp_generator(elt, gens, i, generator, out);
        self.budget.leave();
        r
    }

    #[inline(never)]
    fn comp_generator(
        &mut self,
        elt: &Expr,
        gens: &[Comprehension],
        i: usize,
        generator: &Comprehension,
        out: &mut Collected,
    ) -> ExprResult<()> {
        let iterable = self.eval(&generator.iter)?;
        let mut it = Iter::new(iterable)?;
        while let Some(item) = it.next(&mut self.budget)? {
            self.budget.tick()?;
            self.bind_target(&generator.target, item)?;
            let mut pass = true;
            for cond in &generator.ifs {
                if !self.eval(cond)?.is_truthy() {
                    pass = false;
                    break;
                }
            }
            if pass {
                self.comp_level(elt, gens, i + 1, out)?;
            }
        }
        Ok(())
    }

    fn bind_target(&mut self, target: &Expr, value: ExprValue) -> ExprResult<()> {
        match target {
            Expr::Name(name) => {
                let Some(scope) = self.comp.last_mut() else {
                    return Err(ExprError::syntax(
                        "comprehension target outside a comprehension",
                    ));
                };
                scope.insert(name.clone(), value);
                Ok(())
            }
            Expr::Tuple(targets) | Expr::List(targets) => {
                let type_name = value.type_name().to_string();
                let mut it = Iter::new(value).map_err(|_| {
                    ExprError::type_error(format!("cannot unpack non-iterable {type_name} object"))
                })?;
                let mut items = Vec::with_capacity(targets.len());
                while let Some(item) = it.next(&mut self.budget)? {
                    if items.len() == targets.len() {
                        return Err(ExprError::value_error(format!(
                            "too many values to unpack (expected {})",
                            targets.len()
                        )));
                    }
                    items.push(item);
                }
                if items.len() < targets.len() {
                    return Err(ExprError::value_error(format!(
                        "not enough values to unpack (expected {}, got {})",
                        targets.len(),
                        items.len()
                    )));
                }
                for (t, item) in targets.iter().zip(items) {
                    self.bind_target(t, item)?;
                }
                Ok(())
            }
            _ => Err(ExprError::syntax(
                "cannot assign to expression in comprehension target",
            )),
        }
    }
}
