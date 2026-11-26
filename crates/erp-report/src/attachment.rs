// SPDX-License-Identifier: MIT OR Apache-2.0
//! T-RPT.4 library boundary. Missing metadata is not evidence that a field is unrestricted.
use crate::{
    ReportError, ReportLimits, profile,
    validate::{embedded_image, expression, identifier, parse_arch},
};
use erp_expr::{Expr, Literal};
use erp_views::{LimitKind, NodeRef};
use std::{collections::BTreeMap, rc::Rc};

/// Metadata of a model field, supplied from an authoritative registry by the caller.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct FieldInfo {
    pub(crate) relation: Option<String>,
    restricted: bool,
    pub(crate) currency: Option<String>,
    pub(crate) html: bool,
}
impl FieldInfo {
    /// A scalar field with or without a field-level groups restriction.
    pub fn scalar(restricted: bool) -> Self {
        Self {
            relation: None,
            restricted,
            currency: None,
            html: false,
        }
    }
    /// Mark an HTML field so warning display names join the same authorized read.
    pub fn with_html(mut self) -> Self {
        self.html = true;
        self
    }
    /// A relational field and its target model.
    pub fn relation(model: impl Into<String>, restricted: bool) -> Self {
        Self {
            relation: Some(model.into()),
            restricted,
            currency: None,
            html: false,
        }
    }
    /// A monetary field names the field of the same record that holds its currency; the prefetch
    /// plan reads that currency with every amount a `t-field` prints.
    pub fn with_currency_field(mut self, field: impl Into<String>) -> Self {
        self.currency = Some(field.into());
        self
    }
}
/// Per-report metadata. Model/load integration belongs to the report-action slice.
#[derive(Debug)]
#[non_exhaustive]
pub struct AttachmentPolicy<'a> {
    report: &'a str,
    model: &'a str,
    attachment: bool,
    acknowledged: bool,
    bindings: &'a [(&'a str, Option<&'a str>)],
    helpers: &'a [HelperDependencies<'a>],
    report_helpers: &'a [crate::ReportHelper<'a>],
}
impl<'a> AttachmentPolicy<'a> {
    /// A report on `model` with no attachment expression and no acknowledgement.
    pub fn new(report: &'a str, model: &'a str) -> Self {
        Self {
            report,
            model,
            attachment: false,
            acknowledged: false,
            bindings: &[],
            helpers: &[],
            report_helpers: &[],
        }
    }
    /// Whether the action has an `attachment` expression; attachment reuse does not matter here.
    pub fn with_attachment(mut self, present: bool) -> Self {
        self.attachment = present;
        self
    }
    /// The action's explicit `allow_attachment_group_fields` value.
    pub fn with_acknowledgement(mut self, acknowledged: bool) -> Self {
        self.acknowledged = acknowledged;
        self
    }
    /// Context names other than `docs`, as the report declares them: the model a name holds, or
    /// `None` for a plain value. A name neither declared here nor assigned in the arch is
    /// unresolved, never guessed.
    pub fn with_bindings(mut self, bindings: &'a [(&'a str, Option<&'a str>)]) -> Self {
        self.bindings = bindings;
        self
    }
    /// Authoritative model helpers, whose dependencies are relative to their receiver.
    pub fn with_report_helpers(mut self, helpers: &'a [crate::ReportHelper<'a>]) -> Self {
        self.report_helpers = helpers;
        self
    }
    /// Authoritative dependencies of scalar helpers, written in the report context's names
    /// (`docs` and the declared bindings), never in names the arch assigns.
    /// The registry must declare every hidden field read; source guesses are not authoritative.
    pub fn with_helpers(mut self, helpers: &'a [HelperDependencies<'a>]) -> Self {
        self.helpers = helpers;
        self
    }
}
/// Trusted registry declaration for a scalar helper. Expressions are analyzed, never executed.
#[derive(Debug)]
pub struct HelperDependencies<'a> {
    name: &'a str,
    dependencies: &'a [&'a str],
}
impl<'a> HelperDependencies<'a> {
    /// Declare the helper's canonical name and all implicit dependencies.
    pub fn new(name: &'a str, dependencies: &'a [&'a str]) -> Self {
        Self { name, dependencies }
    }
}
/// A caller-supplied combined template arch, addressed by canonical key.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct TemplateSource<'a> {
    key: &'a str,
    arch: &'a str,
}
impl<'a> TemplateSource<'a> {
    /// Supply arch text without I/O or implicit catalogue lookup.
    pub fn new(key: &'a str, arch: &'a str) -> Self {
        Self { key, arch }
    }
    pub(crate) fn key(&self) -> &'a str {
        self.key
    }
    pub(crate) fn arch(&self) -> &'a str {
        self.arch
    }
}
/// A known restricted dependency, retained even when explicitly acknowledged.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct RestrictedField {
    template: String,
    line: u32,
    field: String,
}
impl RestrictedField {
    /// Fully qualified model and field.
    pub fn field(&self) -> &str {
        &self.field
    }
    /// Template that references the field.
    pub fn template(&self) -> &str {
        &self.template
    }
    /// Line of the containing element.
    pub fn line(&self) -> u32 {
        self.line
    }
}
/// Directives whose value is one expression the analysis resolves.
const ANALYSED: [&str; 9] = [
    "t-out",
    "t-esc",
    "t-field",
    "t-if",
    "t-elif",
    "t-options",
    "t-lang",
    "t-tag",
    "t-att",
];
type Bindings = BTreeMap<String, Option<String>>;
/// Longest name an arch may bind; a longer one is refused, so scope copies stay small.
const BINDING_NAME_BYTES: usize = 64;
enum Event<'a> {
    Node(&'a str, NodeRef<'a>),
    Restore(Bindings),
    Enter(&'a str, NodeRef<'a>),
    LeaveCall,
}
struct Analysis<'a, F> {
    policy: &'a AttachmentPolicy<'a>,
    fields: &'a F,
    limits: ReportLimits,
    findings: Vec<RestrictedField>,
    work: crate::limits::Work,
    helper_depth: usize,
    /// The report context (`docs` and the declared names), where helper dependencies resolve.
    context: Rc<Bindings>,
}
impl<F: Fn(&str, &str) -> Option<FieldInfo>> Analysis<'_, F> {
    fn unresolved(&self, key: &str, line: u32, value: &str) -> ReportError {
        ReportError::Unresolved {
            report: self.policy.report.into(),
            template: key.into(),
            line,
            expression: value.into(),
        }
    }
    fn reference(
        &mut self,
        key: &str,
        line: u32,
        expr: &Expr,
        bindings: &Bindings,
        original: &str,
    ) -> Result<Option<String>, ReportError> {
        self.work.charge(1)?;
        match expr {
            Expr::Call {
                func,
                args,
                keywords,
            } => {
                if let Expr::Attribute { value, attr } = func.as_ref() {
                    let model = self
                        .reference(key, line, value, bindings, original)?
                        .ok_or_else(|| self.unresolved(key, line, original))?;
                    let helpers = self.policy.report_helpers;
                    let mut matches = helpers
                        .iter()
                        .filter(|h| h.model() == model && h.method() == attr);
                    let helper = matches
                        .next()
                        .ok_or_else(|| self.unresolved(key, line, original))?;
                    if matches.next().is_some()
                        || crate::helper::static_args(args, keywords).is_none()
                    {
                        return Err(self.unresolved(key, line, original));
                    }
                    let local = BTreeMap::from([("receiver".into(), Some(model))]);
                    for dependency in helper.dependencies() {
                        self.work.charge(dependency.len())?;
                        if !dependency.split('.').all(identifier) {
                            return Err(self.unresolved(key, line, original));
                        }
                        let source = format!("receiver.{dependency}");
                        let parsed = expression(key, line, &source)?;
                        self.reference(key, line, &parsed, &local, original)?;
                    }
                    return Ok(None);
                }
                let Expr::Name(name) = func.as_ref() else {
                    return Err(self.unresolved(key, line, original));
                };
                let mut matches = self.policy.helpers.iter().filter(|h| h.name == name);
                let helper = matches
                    .next()
                    .ok_or_else(|| self.unresolved(key, line, original))?;
                if matches.next().is_some() {
                    return Err(self.unresolved(key, line, original));
                }
                if self.helper_depth >= self.limits.calls {
                    return Err(ReportError::Limit {
                        name: "helper_depth",
                        maximum: self.limits.calls,
                    });
                }
                self.helper_depth += 1;
                for arg in args.iter().chain(keywords.iter().map(|(_, value)| value)) {
                    if self
                        .reference(key, line, arg, bindings, original)?
                        .is_some()
                    {
                        return Err(self.unresolved(key, line, original));
                    }
                }
                let context = Rc::clone(&self.context);
                for dependency in helper.dependencies {
                    self.work.charge(dependency.len())?;
                    let parsed = expression(key, line, dependency)?;
                    self.reference(key, line, &parsed, &context, dependency)?;
                }
                self.helper_depth -= 1;
                Ok(None)
            }
            Expr::Name(name) => bindings
                .get(name)
                .cloned()
                .ok_or_else(|| self.unresolved(key, line, original)),
            Expr::Attribute { value, attr } => {
                let model = self
                    .reference(key, line, value, bindings, original)?
                    .ok_or_else(|| self.unresolved(key, line, original))?;
                let field = (self.fields)(&model, attr)
                    .ok_or_else(|| self.unresolved(key, line, original))?;
                if field.restricted {
                    let path = format!("{model}.{attr}");
                    if !self.policy.acknowledged {
                        return Err(ReportError::Restricted {
                            report: self.policy.report.into(),
                            template: key.into(),
                            line,
                            field: path,
                        });
                    }
                    if self.findings.len() >= self.limits.diagnostics {
                        return Err(ReportError::Limit {
                            name: "restricted_fields",
                            maximum: self.limits.diagnostics,
                        });
                    }
                    self.findings.push(RestrictedField {
                        template: key.into(),
                        line,
                        field: path,
                    });
                }
                if field.relation.as_ref().is_some_and(|s| {
                    s.is_empty() || s.len() > self.limits.views.get(LimitKind::XpathLength)
                }) {
                    return Err(self.unresolved(key, line, original));
                }
                Ok(field.relation)
            }
            Expr::Literal(
                Literal::None
                | Literal::Bool(_)
                | Literal::Int(_)
                | Literal::Float(_)
                | Literal::Str(_)
                | Literal::Bytes(_),
            ) => Ok(None),
            Expr::Unary { operand, .. } => {
                self.reference(key, line, operand, bindings, original)?;
                Ok(None)
            }
            Expr::Binary { left, right, .. } => {
                self.scalar(key, line, left, bindings, original)?;
                self.scalar(key, line, right, bindings, original)?;
                Ok(None)
            }
            Expr::BoolOp { values, .. }
            | Expr::List(values)
            | Expr::Tuple(values)
            | Expr::JoinedStr(values) => {
                for item in values {
                    if self
                        .reference(key, line, item, bindings, original)?
                        .is_some()
                    {
                        return Err(self.unresolved(key, line, original));
                    }
                }
                Ok(None)
            }
            Expr::Compare {
                left, comparators, ..
            } => {
                self.reference(key, line, left, bindings, original)?;
                for item in comparators {
                    self.reference(key, line, item, bindings, original)?;
                }
                Ok(None)
            }
            Expr::IfExp { test, body, orelse } => {
                self.reference(key, line, test, bindings, original)?;
                let a = self.reference(key, line, body, bindings, original)?;
                let b = self.reference(key, line, orelse, bindings, original)?;
                if a != b {
                    return Err(self.unresolved(key, line, original));
                }
                Ok(a)
            }
            Expr::Dict(entries) => {
                for (a, b) in entries {
                    self.scalar(key, line, a, bindings, original)?;
                    self.scalar(key, line, b, bindings, original)?;
                }
                Ok(None)
            }
            Expr::FormattedValue { value, .. } => {
                self.scalar(key, line, value, bindings, original)?;
                Ok(None)
            }
            // Dynamic subscripts and comprehensions are not analysed for attachments; the prefetch
            // plan types them, and this check stays the stricter of the two.
            _ => Err(self.unresolved(key, line, original)),
        }
    }
    fn scalar(
        &mut self,
        key: &str,
        line: u32,
        expr: &Expr,
        bindings: &Bindings,
        original: &str,
    ) -> Result<(), ReportError> {
        if self
            .reference(key, line, expr, bindings, original)?
            .is_some()
        {
            return Err(self.unresolved(key, line, original));
        }
        Ok(())
    }
    fn bind(
        &self,
        bindings: &mut Bindings,
        name: &str,
        model: Option<String>,
        key: &str,
        line: u32,
    ) -> Result<(), ReportError> {
        if !identifier(name) {
            return Err(self.unresolved(key, line, name));
        }
        if name.len() > BINDING_NAME_BYTES {
            return Err(ReportError::Limit {
                name: "binding_name_bytes",
                maximum: BINDING_NAME_BYTES,
            });
        }
        if bindings.len() >= self.limits.views.get(LimitKind::ElementAttributes)
            && !bindings.contains_key(name)
        {
            return Err(ReportError::Limit {
                name: "bindings",
                maximum: self.limits.views.get(LimitKind::ElementAttributes),
            });
        }
        // A differing assignment could be conditional. Refusing prevents one branch from hiding
        // another branch's field model; the compiler can later supply a union-aware dependency plan.
        if bindings.get(name).is_some_and(|old| old != &model) {
            return Err(self.unresolved(key, line, name));
        }
        bindings.insert(name.into(), model);
        Ok(())
    }
    /// A scope boundary. The copy is charged by its size, so many scopes cannot copy for free.
    fn scope<'e>(&mut self, bindings: &Bindings) -> Result<Event<'e>, ReportError> {
        let bytes = bindings
            .iter()
            .map(|(name, model)| name.len() + model.as_ref().map_or(0, String::len))
            .fold(1usize, usize::saturating_add);
        self.work.charge(bytes)?;
        Ok(Event::Restore(bindings.clone()))
    }
}
/// Check attachment field exposure without executing expressions or reading records.
/// Unsupported dependency analysis returns `Unresolved`, including acknowledged reports.
/// This is library-only coverage; callers must enforce it at install/upgrade.
pub fn check_attachment<F: Fn(&str, &str) -> Option<FieldInfo>>(
    policy: &AttachmentPolicy<'_>,
    sources: &[TemplateSource<'_>],
    root: &str,
    fields: &F,
    limits: ReportLimits,
) -> Result<Vec<RestrictedField>, ReportError> {
    if !policy.attachment {
        return Ok(Vec::new());
    }
    if !crate::ReportProgram::group_conditions(sources, limits)?.is_empty() {
        return Err(crate::error::invalid(
            "an archived report cannot depend on the printing user's groups",
        ));
    }
    if policy
        .helpers
        .len()
        .saturating_add(policy.report_helpers.len())
        > limits.diagnostics
    {
        return Err(ReportError::Limit {
            name: "helpers",
            maximum: limits.diagnostics,
        });
    }
    let mut analysis = Analysis {
        policy,
        fields,
        limits,
        findings: Vec::new(),
        work: limits.work(),
        helper_depth: 0,
        context: Rc::default(),
    };
    if sources.len() > limits.views.get(LimitKind::CatalogueViews) {
        return Err(ReportError::Limit {
            name: "templates",
            maximum: limits.views.get(LimitKind::CatalogueViews),
        });
    }
    let mut arches = BTreeMap::new();
    let mut bytes = 0usize;
    for source in sources {
        bytes = bytes
            .checked_add(source.arch.len())
            .and_then(|n| n.checked_add(source.key.len()))
            .ok_or(ReportError::Limit {
                name: "template_bytes",
                maximum: limits.views.get(LimitKind::CatalogueBytes),
            })?;
        if bytes > limits.views.get(LimitKind::CatalogueBytes) {
            return Err(ReportError::Limit {
                name: "template_bytes",
                maximum: limits.views.get(LimitKind::CatalogueBytes),
            });
        }
        if arches
            .insert(source.key, parse_arch(source.key, source.arch, limits)?)
            .is_some()
        {
            return Err(analysis.unresolved(source.key, 1, "duplicate template key"));
        }
    }
    let arch = arches
        .get(root)
        .ok_or_else(|| analysis.unresolved(root, 1, "missing template"))?;
    let mut events = vec![Event::Node(root, arch.root())];
    let mut calls = vec![root];
    // Only `docs` is the report's own recordset; any other name must be declared or assigned.
    let mut bindings = Bindings::new();
    let declared = policy.bindings.iter().copied();
    for (name, model) in [("docs", Some(policy.model))].into_iter().chain(declared) {
        if name.is_empty() || model == Some("") || bindings.contains_key(name) {
            return Err(analysis.unresolved(root, 1, &format!("invalid binding {name}")));
        }
        analysis.bind(&mut bindings, name, model.map(Into::into), root, 1)?;
    }
    analysis.context = Rc::new(bindings.clone());
    while let Some(event) = events.pop() {
        analysis.work.charge(1)?;
        let (key, node) = match event {
            Event::Restore(old) => {
                bindings = old;
                continue;
            }
            Event::LeaveCall => {
                calls.pop();
                continue;
            }
            Event::Enter(key, node) => {
                calls.push(key);
                (key, node)
            }
            Event::Node(key, node) => (key, node),
        };
        let line = node.origin().line();
        if ["t-if", "t-elif", "t-else"]
            .iter()
            .any(|d| node.attr(d).is_some())
        {
            // A branch may not run: what it assigns ends with it, and a later use is unresolved.
            events.push(analysis.scope(&bindings)?);
        }
        if let Some(iter) = node.attr("t-foreach") {
            let model = analysis
                .reference(key, line, &expression(key, line, iter)?, &bindings, iter)?
                .ok_or_else(|| analysis.unresolved(key, line, iter))?;
            let alias = node
                .attr("t-as")
                .ok_or_else(|| analysis.unresolved(key, line, "missing t-as"))?;
            events.push(analysis.scope(&bindings)?);
            // The alias shadows an outer name until the loop's restore boundary.
            bindings.remove(alias);
            analysis.bind(&mut bindings, alias, Some(model.clone()), key, line)?;
            // QWeb also binds `<alias>_value` to the iterated record: it carries the loop's model,
            // never an outer binding of that name.
            // Set like the scalar helpers below: the alias already passed the name and count checks.
            bindings.insert(format!("{alias}_value"), Some(model));
            for helper in ["index", "first", "last", "size", "odd", "even", "parity"] {
                // QWeb's scalar loop helpers (design §5.1) shadow any outer name for the loop.
                bindings.insert(format!("{alias}_{helper}"), None);
            }
        }
        if let Some(alias) = node.attr("t-set") {
            let model = if let Some(value) = node.attr("t-value") {
                analysis.reference(key, line, &expression(key, line, value)?, &bindings, value)?
            } else {
                None
            };
            analysis.bind(&mut bindings, alias, model, key, line)?;
        }
        for (name, value) in node.attributes() {
            analysis.work.charge(value.len())?;
            let formatted = name.starts_with("t-attf-");
            if formatted && value.contains(['{', '#']) {
                // A formatted value may print a field; until that is analysed it is refused.
                return Err(analysis.unresolved(key, line, value));
            }
            if ANALYSED.contains(&name) || name.starts_with("t-att-") {
                let parsed = expression(key, line, value)?;
                // The data stage reads an image path with the printer's rights: only a
                // declared helper, whose dependencies are known, may compute one.
                let image = name == "t-att-src" && !matches!(parsed, Expr::Call { .. });
                let model = analysis.reference(key, line, &parsed, &bindings, value)?;
                if image || (model.is_some() && !["t-if", "t-elif"].contains(&name)) {
                    return Err(analysis.unresolved(key, line, value));
                }
            } else if matches!(name, "src" | "t-attf-src") && !embedded_image(value) {
                return Err(analysis.unresolved(key, line, value));
            } else if name == "t-value" && node.attr("t-set").is_none() {
                return Err(analysis.unresolved(key, line, "t-value without t-set"));
            } else if name.starts_with("t-")
                && !formatted
                && !profile::listed(profile::DIRECTIVES, name)
            {
                // A directive this analysis does not read may print anything.
                return Err(analysis.unresolved(key, line, name));
            }
        }
        if let Some(target) = node.attr("t-call") {
            if calls.contains(&target) {
                return Err(analysis.unresolved(key, line, &format!("cyclic t-call {target}")));
            }
            if calls.len() >= limits.calls {
                return Err(ReportError::Limit {
                    name: "call_depth",
                    maximum: limits.calls,
                });
            }
            let called = arches
                .get(target)
                .ok_or_else(|| analysis.unresolved(key, line, target))?;
            // The body below is read first, in the caller's chain; the callee is entered after it.
            events.push(analysis.scope(&bindings)?);
            events.push(Event::LeaveCall);
            events.push(Event::Enter(target, called.root()));
        }
        let children: Vec<_> = node.children().collect();
        events.extend(children.into_iter().rev().map(|n| Event::Node(key, n)));
    }
    Ok(analysis.findings)
}
