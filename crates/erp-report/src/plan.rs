// SPDX-License-Identifier: MIT OR Apache-2.0
//! The prefetch plan (design §4.1): which fields of which models a compiled program reads, found
//! by typing its expressions. A name holds records of a model when the report context declares it,
//! when a loop draws it from such records, or when it is assigned from them; a field read on such a
//! name is a path (`docs.partner_id.name`). Nothing is evaluated or read here.
//!
//! The same pass is the helper allowlist: a method called on records must be a registered report
//! helper with declared field dependencies, and any other call is a compile error naming it.
use crate::{
    FieldInfo, ReportError, ReportProgram,
    limits::Work,
    program::{Argument, At, Attr, Code, Content, Op, Part},
    validate::identifier,
};
use erp_expr::{Comprehension, Expr, Literal, SubscriptKind};
use std::collections::{BTreeMap, BTreeSet};

/// What the monetary widget reads of a currency record.
const CURRENCY_FIELDS: [&str; 3] = ["symbol", "position", "decimal_places"];

/// A model method an addon registered as a report helper (data-callable level), with
/// every field it reads, written as paths from the record it is called on (`partner_id.name`).
/// The registry is authoritative: a field the helper reads and does not declare is not loaded.
#[derive(Debug, Clone)]
pub struct ReportHelper<'a> {
    model: &'a str,
    method: &'a str,
    dependencies: &'a [&'a str],
    parameters: Option<Vec<String>>,
}
impl<'a> ReportHelper<'a> {
    /// Parameter names in the callable binder's positional order; omitted arguments stay omitted.
    pub fn with_parameters(mut self, parameters: &[&str]) -> Self {
        self.parameters = Some(parameters.iter().map(|name| (*name).to_owned()).collect());
        self
    }
    pub fn model(&self) -> &str {
        self.model
    }
    pub fn method(&self) -> &str {
        self.method
    }
    pub fn dependencies(&self) -> &[&str] {
        self.dependencies
    }
    pub fn new(model: &'a str, method: &'a str, dependencies: &'a [&'a str]) -> Self {
        Self {
            model,
            method,
            dependencies,
            parameters: None,
        }
    }
}
/// What the report context holds: `docs` of the report's model, the other names that hold records
/// and the registered helpers. A name that is neither declared nor assigned in the templates is a
/// plain value to the plan, and no field is loaded through it.
#[derive(Debug, Clone, Copy)]
pub struct PlanContext<'a> {
    model: &'a str,
    bindings: &'a [(&'a str, Option<&'a str>)],
    helpers: &'a [ReportHelper<'a>],
}
impl<'a> PlanContext<'a> {
    /// A context whose `docs` are records of `model`.
    pub fn new(model: &'a str) -> Self {
        Self {
            model,
            bindings: &[],
            helpers: &[],
        }
    }
    /// Names other than `docs`: the model a name holds records of (`company`, `user`), or `None`
    /// for a plain value.
    pub fn with_bindings(mut self, bindings: &'a [(&'a str, Option<&'a str>)]) -> Self {
        self.bindings = bindings;
        self
    }
    /// The report helpers templates may call.
    pub fn with_helpers(mut self, helpers: &'a [ReportHelper<'a>]) -> Self {
        self.helpers = helpers;
        self
    }
}
/// One relation path from a context name, with what is read at its end.
#[derive(Debug)]
pub(crate) struct Path {
    pub(crate) model: String,
    pub(crate) depth: usize,
    fields: BTreeSet<String>,
    display_name: bool,
    /// Relation field to the path that continues through it.
    pub(crate) children: BTreeMap<String, usize>,
}
impl Path {
    /// Whether the records at the end of this path are read at all. A relation that is only tested
    /// or counted needs nothing of its records.
    pub(crate) fn reads(&self) -> bool {
        self.display_name || !self.fields.is_empty()
    }
}
/// Everything the plan reads of one model, over all its paths: a record is read once, whole.
#[derive(Debug, Default)]
pub(crate) struct ModelFields {
    pub(crate) fields: BTreeSet<String>,
    pub(crate) display_name: bool,
}
/// One read of the data stage: the records of one model at one relation depth.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanLevel {
    depth: usize,
    model: String,
    fields: Vec<String>,
    display_name: bool,
}
impl PlanLevel {
    /// Relations between a context name and these records; the context's own records are 0.
    pub fn depth(&self) -> usize {
        self.depth
    }
    pub fn model(&self) -> &str {
        &self.model
    }
    /// The fields read of the model, in name order. They are the same at every depth of a model.
    pub fn fields(&self) -> &[String] {
        &self.fields
    }
    /// Whether the display name is read too.
    pub fn display_name(&self) -> bool {
        self.display_name
    }
}
/// The fields a program reads, as relation paths from the names of its context. The data stage
/// ([`crate::DataStage`]) turns it into one read per [`PlanLevel`].
#[derive(Debug)]
pub struct PrefetchPlan {
    pub(crate) paths: Vec<Path>,
    /// Context names that hold records, with their path.
    pub(crate) roots: Vec<(String, usize)>,
    pub(crate) models: BTreeMap<String, ModelFields>,
    helpers: BTreeSet<(String, String)>,
    pub(crate) invocations: Vec<crate::helper::Invocation>,
    precisions: BTreeSet<String>,
}
impl PrefetchPlan {
    /// Distinct literal precision names, in name order; loaded together once per batch.
    pub fn precision_names(&self) -> impl ExactSizeIterator<Item = &str> {
        self.precisions.iter().map(String::as_str)
    }
    /// The reads of the plan, by depth then model. Their number bounds the reads of a batch,
    /// whatever its size. A nonempty precision name set adds one metadata read.
    pub fn levels(&self) -> Vec<PlanLevel> {
        let keys: BTreeSet<(usize, &str)> = self
            .paths
            .iter()
            .filter(|path| path.reads())
            .map(|path| (path.depth, path.model.as_str()))
            .collect();
        keys.into_iter()
            .map(|(depth, model)| {
                let read = self.models.get(model);
                PlanLevel {
                    depth,
                    model: model.into(),
                    fields: read
                        .map(|read| read.fields.iter().cloned().collect())
                        .unwrap_or_default(),
                    display_name: read.is_some_and(|read| read.display_name),
                }
            })
            .collect()
    }
    /// The context names that hold records, with their model.
    pub fn roots(&self) -> impl Iterator<Item = (&str, &str)> {
        self.roots
            .iter()
            .map(|(name, path)| (name.as_str(), self.paths[*path].model.as_str()))
    }
    /// The report helpers the program calls, as (model, method).
    pub fn helpers(&self) -> impl Iterator<Item = (&str, &str)> {
        self.helpers
            .iter()
            .map(|(model, method)| (model.as_str(), method.as_str()))
    }
    pub(crate) fn root(&self, name: &str) -> Option<usize> {
        self.roots
            .iter()
            .find(|(root, _)| root == name)
            .map(|(_, path)| *path)
    }
    /// One more than the deepest path.
    pub(crate) fn depths(&self) -> usize {
        self.paths
            .iter()
            .map(|path| path.depth + 1)
            .max()
            .unwrap_or(0)
    }
}

/// What a value may be: records of these models, each at these paths. Empty is any other value.
#[derive(Debug, Clone, Default)]
struct Ty(BTreeMap<String, BTreeSet<usize>>);
impl Ty {
    fn one(model: &str, path: usize) -> Self {
        Self(BTreeMap::from([(model.to_owned(), BTreeSet::from([path]))]))
    }
    /// Either value: what is read of one is read of both.
    fn join(mut self, other: Self) -> Self {
        for (model, paths) in other.0 {
            self.0.entry(model).or_default().extend(paths);
        }
        self
    }
}
type Scope = BTreeMap<String, Ty>;
/// The expression an error quotes and where it stands.
struct Cx<'c> {
    at: &'c At,
    source: &'c str,
}
struct Analysis<'a, F> {
    program: &'a ReportProgram,
    registered: &'a [ReportHelper<'a>],
    fields: &'a F,
    paths: Vec<Path>,
    helpers: BTreeSet<(String, String)>,
    pub(crate) invocations: Vec<crate::helper::Invocation>,
    precisions: BTreeSet<String>,
    work: Work,
    helper_bytes: usize,
}
impl<F: Fn(&str, &str) -> Option<FieldInfo>> Analysis<'_, F> {
    fn path(&mut self, model: &str, depth: usize) -> Result<usize, ReportError> {
        let maximum = self.program.limits.plan_paths;
        if self.paths.len() >= maximum {
            return Err(ReportError::Limit {
                name: "plan_paths",
                maximum,
            });
        }
        self.paths.push(Path {
            model: model.into(),
            depth,
            fields: BTreeSet::new(),
            display_name: false,
            children: BTreeMap::new(),
        });
        Ok(self.paths.len() - 1)
    }
    fn child(&mut self, parent: usize, field: &str, target: &str) -> Result<usize, ReportError> {
        if let Some(child) = self.paths[parent].children.get(field) {
            return Ok(*child);
        }
        let child = self.path(target, self.paths[parent].depth + 1)?;
        self.paths[parent].children.insert(field.into(), child);
        Ok(child)
    }
    fn info(&self, model: &str, field: &str, cx: &Cx<'_>) -> Result<FieldInfo, ReportError> {
        let info = (self.fields)(model, field).ok_or_else(|| {
            cx.at.compile(format!(
                "`{}`: model `{model}` has no field `{field}`",
                cx.source
            ))
        })?;
        if info.relation.as_deref() == Some("") {
            return Err(cx.at.compile(format!(
                "`{}`: relation `{model}.{field}` names no model",
                cx.source
            )));
        }
        Ok(info)
    }
    /// `value.attr`: the field joins every path of the value; a relation continues them.
    fn attribute(&mut self, value: &Ty, attr: &str, cx: &Cx<'_>) -> Result<Ty, ReportError> {
        let mut out = Ty::default();
        for (model, paths) in &value.0 {
            self.work.charge(paths.len())?;
            match attr {
                // Known of the recordset itself.
                "id" | "ids" => {}
                "display_name" => {
                    for path in paths {
                        self.paths[*path].display_name = true;
                    }
                }
                _ => {
                    let info = self.info(model, attr, cx)?;
                    for path in paths {
                        self.paths[*path].fields.insert(attr.into());
                        self.paths[*path].display_name |= info.html;
                        if let Some(target) = &info.relation {
                            let child = self.child(*path, attr, target)?;
                            out.0.entry(target.clone()).or_default().insert(child);
                        }
                    }
                }
            }
        }
        Ok(out)
    }
    fn contact_fields(
        &mut self,
        relation: &Ty,
        options: Option<&Code>,
        at: &At,
    ) -> Result<(), ReportError> {
        let Some(options) = options else {
            return Ok(());
        };
        let Expr::Dict(entries) = &options.expr else {
            return Ok(());
        };
        let entry = |name: &str| {
            entries.iter().find_map(|(key, value)| {
                matches!(key, Expr::Literal(Literal::Str(k)) if k == name).then_some(value)
            })
        };
        if !matches!(entry("widget"), Some(Expr::Literal(Literal::Str(s))) if s == "contact") {
            return Ok(());
        }
        for paths in relation.0.values() {
            for path in paths {
                self.paths[*path].display_name = true;
            }
        }
        let Some(fields) = entry("fields") else {
            return Ok(());
        };
        let (Expr::List(fields) | Expr::Tuple(fields)) = fields else {
            return Err(at.compile("contact fields must be a literal list of line names"));
        };
        for field in fields {
            let Expr::Literal(Literal::Str(field)) = field else {
                return Err(at.compile("contact fields must be literal line names"));
            };
            for column in crate::widget::contact_columns(field)
                .ok_or_else(|| at.compile(format!("unknown contact line {field}")))?
            {
                self.attribute(
                    relation,
                    column,
                    &Cx {
                        at,
                        source: &options.source,
                    },
                )?;
            }
        }
        Ok(())
    }
    /// The fields the monetary widget reads of a currency.
    fn currency(&mut self, currency: &Ty, cx: &Cx<'_>) -> Result<(), ReportError> {
        for field in CURRENCY_FIELDS {
            self.attribute(currency, field, cx)?;
        }
        Ok(())
    }
    /// `t-field`: the field, the display names a relation prints and the currency of an amount.
    fn printed(
        &mut self,
        record: &Ty,
        field: &str,
        options: Option<&Code>,
        cx: &Cx<'_>,
    ) -> Result<(), ReportError> {
        let value = self.attribute(record, field, cx)?;
        self.contact_fields(&value, options, cx.at)?;
        for paths in value.0.values() {
            for path in paths {
                self.paths[*path].display_name = true;
            }
        }
        for (model, paths) in &record.0 {
            if matches!(field, "id" | "ids" | "display_name") {
                continue;
            }
            if let Some(holder) = self.info(model, field, cx)?.currency {
                let one = Ty(BTreeMap::from([(model.clone(), paths.clone())]));
                let currency = self.attribute(&one, &holder, cx)?;
                self.currency(&currency, cx)?;
            }
        }
        Ok(())
    }
    /// A method called on records: a registered helper, whose declared fields join the plan.
    fn helper(
        &mut self,
        receiver: &Ty,
        method: &str,
        args: &[Expr],
        keywords: &[(Option<String>, Expr)],
        cx: &Cx<'_>,
    ) -> Result<(), ReportError> {
        let registered = self.registered;
        for (model, paths) in &receiver.0 {
            let helper = registered
                .iter()
                .find(|helper| helper.model == model && helper.method == method)
                .ok_or_else(|| {
                    cx.at.compile(format!(
                        "`{}`: `{model}.{method}` is not a registered report helper",
                        cx.source
                    ))
                })?;
            let (mut args, mut kwargs) = crate::helper::static_args(args, keywords)
                .ok_or_else(|| cx.at.compile(format!("report helper `{model}.{method}` requires static scalar arguments and unique named keywords")))?;
            if let Some(parameters) = &helper.parameters {
                crate::helper::normalize(&mut args, &mut kwargs, parameters).ok_or_else(|| {
                    cx.at.compile(format!(
                        "report helper `{model}.{method}` has invalid or duplicate arguments"
                    ))
                })?;
            }
            let invocation = crate::helper::Invocation {
                model: model.clone(),
                method: method.into(),
                parameters: helper.parameters.clone(),
                args,
                kwargs,
                paths: paths.clone(),
                dependencies: helper.dependencies.iter().map(|s| (*s).into()).collect(),
            };
            self.work.charge(invocation.byte_len())?;
            self.helper_bytes =
                self.helper_bytes
                    .checked_add(invocation.byte_len())
                    .ok_or(ReportError::Limit {
                        name: "evaluation_bytes",
                        maximum: self.program.limits.bytes,
                    })?;
            if self.helper_bytes > self.program.limits.bytes {
                return Err(ReportError::Limit {
                    name: "evaluation_bytes",
                    maximum: self.program.limits.bytes,
                });
            }
            if let Some(existing) = self
                .invocations
                .iter_mut()
                .find(|v| v.same_call(&invocation))
            {
                existing.paths.extend(paths);
            } else {
                if self.invocations.len() >= self.program.limits.plan_paths {
                    return Err(ReportError::Limit {
                        name: "plan_paths",
                        maximum: self.program.limits.plan_paths,
                    });
                }
                self.invocations.push(invocation);
            }
            for dependency in helper.dependencies {
                self.work.charge(dependency.len())?;
                let declared = Cx {
                    at: cx.at,
                    source: dependency,
                };
                let mut value = Ty(BTreeMap::from([(model.clone(), paths.clone())]));
                for segment in dependency.split('.') {
                    if value.0.is_empty() || !identifier(segment) {
                        return Err(cx.at.compile(format!(
                            "report helper `{model}.{method}` declares `{dependency}`, which is not a path of fields"
                        )));
                    }
                    value = self.attribute(&value, segment, &declared)?;
                }
            }
            for path in paths {
                if !self.paths[*path].reads() {
                    self.paths[*path].fields.insert("id".into());
                }
            }
            self.helpers.insert((model.clone(), method.to_owned()));
        }
        Ok(())
    }
    fn bind(target: &Expr, value: Ty, scope: &mut Scope) {
        match target {
            Expr::Name(name) => {
                scope.insert(name.clone(), value);
            }
            // Unpacked names are plain values to the plan.
            Expr::Tuple(names) | Expr::List(names) => {
                for name in names {
                    Self::bind(name, Ty::default(), scope);
                }
            }
            _ => {}
        }
    }
    fn comprehension(
        &mut self,
        element: &Expr,
        generators: &[Comprehension],
        cx: &Cx<'_>,
        scope: &Scope,
    ) -> Result<Ty, ReportError> {
        self.work.charge(scope.len())?;
        let mut inner = scope.clone();
        for generator in generators {
            let items = self.infer(&generator.iter, cx, &mut inner)?;
            Self::bind(&generator.target, items, &mut inner);
            for test in &generator.ifs {
                self.infer(test, cx, &mut inner)?;
            }
        }
        self.infer(element, cx, &mut inner)
    }
    fn each(&mut self, values: &[Expr], cx: &Cx<'_>, scope: &mut Scope) -> Result<Ty, ReportError> {
        let mut joined = Ty::default();
        for value in values {
            joined = joined.join(self.infer(value, cx, scope)?);
        }
        Ok(joined)
    }
    /// What `expr` may be, with every field it reads added to the plan.
    fn infer(&mut self, expr: &Expr, cx: &Cx<'_>, scope: &mut Scope) -> Result<Ty, ReportError> {
        self.work.charge(1)?;
        Ok(match expr {
            Expr::Literal(_) => Ty::default(),
            Expr::Name(name) => scope.get(name).cloned().unwrap_or_default(),
            Expr::Attribute { value, attr } => {
                let value = self.infer(value, cx, scope)?;
                self.attribute(&value, attr, cx)?
            }
            Expr::Subscript { value, slice } => {
                // An index or a slice of records are records of the same paths.
                let value = self.infer(value, cx, scope)?;
                match slice {
                    SubscriptKind::Index(index) => {
                        self.infer(index, cx, scope)?;
                    }
                    SubscriptKind::Slice { lower, upper, step } => {
                        for part in [lower, upper, step].into_iter().flatten() {
                            self.infer(part, cx, scope)?;
                        }
                    }
                    _ => {
                        return Err(cx.at.compile(format!(
                            "`{}`: this subscript is not analysed by the prefetch plan",
                            cx.source
                        )));
                    }
                }
                value
            }
            Expr::Call {
                func,
                args,
                keywords,
            } => {
                for argument in args.iter().chain(keywords.iter().map(|(_, value)| value)) {
                    self.infer(argument, cx, scope)?;
                }
                match func.as_ref() {
                    Expr::Attribute { value, attr } => {
                        let receiver = self.infer(value, cx, scope)?;
                        self.helper(&receiver, attr, args, keywords, cx)?;
                    }
                    function => {
                        self.infer(function, cx, scope)?;
                    }
                }
                Ty::default()
            }
            Expr::Unary { operand, .. } => {
                self.infer(operand, cx, scope)?;
                Ty::default()
            }
            Expr::Binary { left, right, .. } => {
                // `a + b` and `a | b` of records are records; any other pair is a plain value.
                let left = self.infer(left, cx, scope)?;
                let right = self.infer(right, cx, scope)?;
                if left.0.is_empty() || right.0.is_empty() {
                    Ty::default()
                } else {
                    left.join(right)
                }
            }
            // `a or b` is either; so are the items a loop draws from a list.
            Expr::BoolOp { values, .. } | Expr::List(values) | Expr::Tuple(values) => {
                self.each(values, cx, scope)?
            }
            Expr::IfExp { test, body, orelse } => {
                self.infer(test, cx, scope)?;
                let body = self.infer(body, cx, scope)?;
                body.join(self.infer(orelse, cx, scope)?)
            }
            Expr::Compare {
                left, comparators, ..
            } => {
                self.infer(left, cx, scope)?;
                self.each(comparators, cx, scope)?;
                Ty::default()
            }
            Expr::JoinedStr(values) => {
                self.each(values, cx, scope)?;
                Ty::default()
            }
            Expr::Dict(entries) => {
                for (key, value) in entries {
                    self.infer(key, cx, scope)?;
                    self.infer(value, cx, scope)?;
                }
                Ty::default()
            }
            Expr::FormattedValue { value, .. } => {
                self.infer(value, cx, scope)?;
                Ty::default()
            }
            Expr::Starred(value) => self.infer(value, cx, scope)?,
            Expr::Walrus { name, value } => {
                let value = self.infer(value, cx, scope)?;
                scope.insert(name.clone(), value.clone());
                value
            }
            Expr::ListComp {
                elt, generators, ..
            }
            | Expr::GeneratorExp {
                elt, generators, ..
            } => self.comprehension(elt, generators, cx, scope)?,
            _ => {
                return Err(cx.at.compile(format!(
                    "`{}`: this expression form is not analysed by the prefetch plan",
                    cx.source
                )));
            }
        })
    }
    fn code(&mut self, code: &Code, at: &At, scope: &mut Scope) -> Result<Ty, ReportError> {
        let cx = Cx {
            at,
            source: &code.source,
        };
        self.infer(&code.expr, &cx, scope)
    }
    fn parts(&mut self, parts: &[Part], at: &At, scope: &mut Scope) -> Result<(), ReportError> {
        for part in parts {
            if let Part::Value(code) = part {
                self.code(code, at, scope)?;
            }
        }
        Ok(())
    }
    /// Widget options: an expression like any other, and the currency a monetary widget is given.
    fn options(
        &mut self,
        options: Option<&Code>,
        at: &At,
        scope: &mut Scope,
    ) -> Result<(), ReportError> {
        let Some(code) = options else {
            return Ok(());
        };
        let cx = Cx {
            at,
            source: &code.source,
        };
        if let Some(name) = crate::precision::literal_name(code)
            .map_err(|reason| at.compile(format!("`{}`: {reason}", code.source)))?
        {
            self.work.charge(name.len())?;
            if !self.precisions.contains(name) {
                if self.precisions.len() >= self.program.limits.plan_paths {
                    return Err(ReportError::Limit {
                        name: "plan_paths",
                        maximum: self.program.limits.plan_paths,
                    });
                }
                self.precisions.insert(name.to_owned());
            }
        }
        let Expr::Dict(entries) = &code.expr else {
            self.infer(&code.expr, &cx, scope)?;
            return Ok(());
        };
        for (key, value) in entries {
            self.infer(key, &cx, scope)?;
            let value = self.infer(value, &cx, scope)?;
            if matches!(key, Expr::Literal(Literal::Str(name)) if name == "display_currency") {
                self.currency(&value, &cx)?;
            }
        }
        Ok(())
    }
    fn ops(&mut self, ops: &[Op], scope: &mut Scope) -> Result<(), ReportError> {
        for op in ops {
            self.work.charge(1)?;
            match op {
                Op::Text(_) => {}
                Op::Element(element) => {
                    let at = &element.at;
                    for attr in &element.attrs {
                        match attr {
                            Attr::Static { .. } => {}
                            Attr::Value { code, .. } | Attr::Mapping(code) => {
                                self.code(code, at, scope)?;
                            }
                            Attr::Format { parts, .. } => self.parts(parts, at, scope)?,
                        }
                    }
                    match &element.content {
                        Content::Children(children) => self.ops(children, scope)?,
                        Content::Out {
                            value,
                            options,
                            default,
                        } => {
                            if let Some(value) = value {
                                let relation = self.code(value, at, scope)?;
                                self.contact_fields(&relation, options.as_ref(), at)?;
                            }
                            self.options(options.as_ref(), at, scope)?;
                            self.ops(default, scope)?;
                        }
                        Content::Field {
                            record,
                            field,
                            options,
                            default,
                        } => {
                            let value = self.code(record, at, scope)?;
                            let source = format!("{}.{field}", record.source);
                            self.printed(
                                &value,
                                field,
                                options.as_ref(),
                                &Cx {
                                    at,
                                    source: &source,
                                },
                            )?;
                            self.options(options.as_ref(), at, scope)?;
                            self.ops(default, scope)?;
                        }
                    }
                }
                Op::If { branches, at } => {
                    for branch in branches {
                        if let Some(test) = &branch.test {
                            self.code(test, at, scope)?;
                        }
                        self.ops(&branch.body, scope)?;
                    }
                }
                Op::Foreach {
                    items,
                    name,
                    body,
                    at,
                } => {
                    let items = self.code(items, at, scope)?;
                    // The loop's names end with it; what its body assigns stays.
                    let mut names = vec![name.clone(), format!("{name}_value")];
                    names.extend(
                        ["index", "first", "last", "size", "odd", "even", "parity"]
                            .iter()
                            .map(|helper| format!("{name}_{helper}")),
                    );
                    let before: Vec<Option<Ty>> =
                        names.iter().map(|name| scope.get(name).cloned()).collect();
                    for (position, name) in names.iter().enumerate() {
                        let value = if position < 2 {
                            items.clone()
                        } else {
                            Ty::default()
                        };
                        scope.insert(name.clone(), value);
                    }
                    self.ops(body, scope)?;
                    for (name, value) in names.into_iter().zip(before) {
                        match value {
                            Some(value) => scope.insert(name, value),
                            None => scope.remove(&name),
                        };
                    }
                }
                Op::Set {
                    name,
                    value,
                    body,
                    at,
                } => {
                    let value = match value {
                        Some(code) => self.code(code, at, scope)?,
                        None => {
                            self.ops(body, scope)?;
                            Ty::default()
                        }
                    };
                    scope.insert(name.clone(), value);
                }
                Op::Call {
                    template,
                    arguments,
                    body,
                    lang,
                    at,
                } => {
                    if let Some(lang) = lang {
                        self.code(lang, at, scope)?;
                    }
                    // The called template works on a copy of the caller's names plus its
                    // arguments; the body stays in the caller's.
                    self.work.charge(scope.len())?;
                    let mut callee = scope.clone();
                    for (name, argument) in arguments {
                        let value = match argument {
                            Argument::Value(code) => self.code(code, at, scope)?,
                            Argument::Format(parts) => {
                                self.parts(parts, at, scope)?;
                                Ty::default()
                            }
                        };
                        callee.insert(name.clone(), value);
                    }
                    self.ops(body, scope)?;
                    let program = self.program;
                    self.ops(&program.templates[*template], &mut callee)?;
                }
            }
        }
        Ok(())
    }
}
impl ReportProgram {
    /// The fields this program reads, for the data stage.
    ///
    /// `fields` answers from the model registry: `None` is a field the model does not have. A
    /// field read of a model that lacks it, a method called on records that is not a registered
    /// helper, and a context name that is empty or repeated are [`ReportError::Compile`] errors
    /// naming the expression; more relation paths than the limit of the program's
    /// [`crate::ReportLimits`] is a [`ReportError::Limit`]. A field reached through a value the
    /// analysis cannot type is not loaded, and evaluation refuses it by name.
    ///
    /// ```
    /// use erp_report::{FieldInfo, Language, PlanContext, ReportLimits, ReportProgram, TemplateSource};
    /// let arch = "<div><p t-foreach='docs' t-as='o'><span t-out='o.partner_id.name'/></p></div>";
    /// let sources = [TemplateSource::new("demo.report", arch)];
    /// let program =
    ///     ReportProgram::compile("demo.report", &sources, &Language::new("en_US")?, ReportLimits::default())?;
    /// let fields = |model: &str, field: &str| match (model, field) {
    ///     ("sale.order", "partner_id") => Some(FieldInfo::relation("res.partner", false)),
    ///     ("res.partner", "name") => Some(FieldInfo::scalar(false)),
    ///     _ => None,
    /// };
    /// let plan = program.prefetch_plan(&PlanContext::new("sale.order"), &fields)?;
    /// let levels: Vec<_> = plan.levels().iter().map(|l| (l.depth(), l.model().to_owned())).collect();
    /// assert_eq!(levels, [(0, "sale.order".to_owned()), (1, "res.partner".to_owned())]);
    /// # Ok::<(), erp_report::ReportError>(())
    /// ```
    pub fn prefetch_plan<F: Fn(&str, &str) -> Option<FieldInfo>>(
        &self,
        context: &PlanContext<'_>,
        fields: &F,
    ) -> Result<PrefetchPlan, ReportError> {
        let at = At {
            template: self.name.clone(),
            line: 1,
        };
        let mut analysis = Analysis {
            program: self,
            registered: context.helpers,
            fields,
            paths: Vec::new(),
            helpers: BTreeSet::new(),
            invocations: Vec::new(),
            precisions: BTreeSet::new(),
            work: self.limits.work(),
            helper_bytes: 0,
        };
        let mut scope = Scope::new();
        let mut roots = Vec::new();
        let declared = context.bindings.iter().copied();
        for (name, model) in [("docs", Some(context.model))].into_iter().chain(declared) {
            if !identifier(name) || model == Some("") || scope.contains_key(name) {
                return Err(at.compile(format!(
                    "context name `{name}` is not a name, is declared twice or names no model"
                )));
            }
            let value = match model {
                Some(model) => {
                    let path = analysis.path(model, 0)?;
                    roots.push((name.to_owned(), path));
                    Ty::one(model, path)
                }
                None => Ty::default(),
            };
            scope.insert(name.into(), value);
        }
        analysis.ops(&self.templates[self.root], &mut scope)?;
        let mut models: BTreeMap<String, ModelFields> = BTreeMap::new();
        for path in analysis.paths.iter().filter(|path| path.reads()) {
            let read = models.entry(path.model.clone()).or_default();
            read.fields.extend(path.fields.iter().cloned());
            read.display_name |= path.display_name;
        }
        Ok(PrefetchPlan {
            paths: analysis.paths,
            roots,
            models,
            helpers: analysis.helpers,
            invocations: analysis.invocations,
            precisions: analysis.precisions,
        })
    }
}
