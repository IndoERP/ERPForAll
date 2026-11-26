// SPDX-License-Identifier: MIT OR Apache-2.0
use crate::{Diagnostic, ProfileKind as K, ReportError, ReportLimits, profile, style};
use erp_views::{Arch, LimitKind, ViewCatalogue, ViewDescriptor};
use serde::Serialize;

/// A dynamic value that MUST be validated after evaluation by the report compiler/evaluator.
/// This is not proof that a dynamic value belongs to the print vocabulary.
#[derive(Debug, Clone, Serialize)]
#[non_exhaustive]
pub struct DynamicValue {
    template: String,
    line: u32,
    attribute: String,
    expression: String,
}
impl DynamicValue {
    /// Template whose dynamic value needs evaluation.
    pub fn template(&self) -> &str {
        &self.template
    }
    /// Element-origin line; subsequent attribute edits may have a different source.
    pub fn line(&self) -> u32 {
        self.line
    }
    /// Attribute to validate, `*` for a mapping, or `tag`/`template` for dynamic identities.
    pub fn attribute(&self) -> &str {
        &self.attribute
    }
    /// Unevaluated expression or format string.
    pub fn expression(&self) -> &str {
        &self.expression
    }
}
/// Static findings plus obligations that the later evaluator must discharge.
#[derive(Debug, Default, Serialize)]
#[non_exhaustive]
pub struct Validation {
    diagnostics: Vec<Diagnostic>,
    obligations: Vec<DynamicValue>,
}
impl Validation {
    /// All static vocabulary violations, bounded by ReportLimits.
    pub fn diagnostics(&self) -> &[Diagnostic] {
        &self.diagnostics
    }
    /// Values still requiring validation after evaluation.
    pub fn obligations(&self) -> &[DynamicValue] {
        &self.obligations
    }
    fn strict(mut self) -> Result<Self, ReportError> {
        if !self.diagnostics.is_empty() {
            return Err(ReportError::Profile(self.diagnostics.remove(0)));
        }
        Ok(self)
    }
}
pub(crate) fn parse_arch(
    template: &str,
    xml: &str,
    limits: ReportLimits,
) -> Result<Arch, ReportError> {
    if template.len() > limits.views.get(LimitKind::XpathLength) {
        return Err(ReportError::Limit {
            name: "template_identity_bytes",
            maximum: limits.views.get(LimitKind::XpathLength),
        });
    }
    let result = ViewCatalogue::with_limits(
        vec![ViewDescriptor::new(1, template, "", xml)],
        limits.views,
    )
    .and_then(|c| c.combine(1));
    result
        .map(|v| v.into_arch())
        .map_err(|source| ReportError::View {
            template: template.into(),
            source: Box::new(source),
        })
}
/// Inspect arch text without evaluating expressions, reading records or fetching assets.
/// Syntax/limit errors abort rather than return incomplete diagnostics.
pub fn inspect(template: &str, xml: &str, limits: ReportLimits) -> Result<Validation, ReportError> {
    inspect_arch(template, &parse_arch(template, xml, limits)?, limits)
}
/// Refuse the first static violation. A successful result may still contain dynamic obligations.
///
/// ```
/// use erp_report::{validate, ReportLimits};
/// let result = validate("demo.report", "<p class='fw-bold'>Hello</p>", ReportLimits::default())?;
/// assert!(result.obligations().is_empty());
/// # Ok::<(), erp_report::ReportError>(())
/// ```
pub fn validate(
    template: &str,
    xml: &str,
    limits: ReportLimits,
) -> Result<Validation, ReportError> {
    inspect(template, xml, limits)?.strict()
}
/// Validate one evaluated attribute using the static profile's target and value grammar.
/// The evaluator supplies the final element and must separately validate dynamic element names.
pub fn validate_attribute(
    element: &str,
    name: &str,
    value: &str,
    limits: ReportLimits,
) -> Result<Validation, ReportError> {
    let mut out = Validation::default();
    let mut work = limits.work();
    work.charge(element.len())?;
    work.charge(name.len())?;
    work.charge(value.len())?;
    if !profile::element(element) {
        diagnostic(
            &mut out,
            "evaluated attribute",
            1,
            element,
            K::Element,
            limits,
        )?;
    } else if !profile::attribute(element, name) {
        diagnostic(
            &mut out,
            "evaluated attribute",
            1,
            name,
            K::Attribute,
            limits,
        )?;
    } else {
        literal(&mut out, "evaluated attribute", 1, name, value, limits)?;
    }
    out.strict()
}
/// Validate an existing combined arch. Lines describe element origins, not the origin of subsequent
/// attribute edits. The caller retains the catalogue/source map for those edits.
pub fn validate_arch(
    template: &str,
    arch: &Arch,
    limits: ReportLimits,
) -> Result<Validation, ReportError> {
    inspect_arch(template, arch, limits)?.strict()
}
/// Inspect an existing combined arch, preserving its element-origin line numbers.
pub fn inspect_arch(
    template: &str,
    arch: &Arch,
    limits: ReportLimits,
) -> Result<Validation, ReportError> {
    if template.len() > limits.views.get(LimitKind::XpathLength) {
        return Err(ReportError::Limit {
            name: "template_identity_bytes",
            maximum: limits.views.get(LimitKind::XpathLength),
        });
    }
    let mut out = Validation::default();
    let mut work = limits.work();
    let maximum = limits.views.get(LimitKind::CombinedNodes);
    if arch.node_count() > maximum {
        return Err(ReportError::Limit {
            name: "arch_nodes",
            maximum,
        });
    }
    let maximum = limits.views.get(LimitKind::CombinedBytes);
    let mut bytes = maximum;
    let mut nodes = vec![(arch.root(), 1)];
    while let Some((node, depth)) = nodes.pop() {
        work.charge(1)?;
        if depth > limits.views.get(LimitKind::Depth) {
            return Err(ReportError::Limit {
                name: "arch_depth",
                maximum: limits.views.get(LimitKind::Depth),
            });
        }
        if node.attributes().count() > limits.views.get(LimitKind::ElementAttributes) {
            return Err(ReportError::Limit {
                name: "element_attributes",
                maximum: limits.views.get(LimitKind::ElementAttributes),
            });
        }
        for value in [node.tag(), node.text(), node.tail()]
            .into_iter()
            .chain(node.attributes().flat_map(|(k, v)| [k, v]))
        {
            bytes = bytes.checked_sub(value.len()).ok_or(ReportError::Limit {
                name: "arch_bytes",
                maximum,
            })?;
            work.charge(value.len())?;
        }
        let line = node.origin().line();
        if !profile::element(node.tag()) {
            diagnostic(&mut out, template, line, node.tag(), K::Element, limits)?;
        }
        for (name, value) in node.attributes() {
            work.charge(name.len().saturating_add(value.len()))?;
            let dynamic = name
                .strip_prefix("t-attf-")
                .map(|n| (n, true))
                .or_else(|| name.strip_prefix("t-att-").map(|n| (n, false)));
            if let Some((target, formatted)) = dynamic {
                if !profile::attribute(node.tag(), target) {
                    diagnostic(&mut out, template, line, target, K::Attribute, limits)?;
                    continue;
                }
                if !formatted {
                    expression(template, line, value)?;
                } else if !format_expressions(template, line, value)? {
                    invalid_value(&mut out, template, line, name, value, limits)?;
                    continue;
                }
                obligation(&mut out, template, line, target, value, limits)?;
            } else if name == "groups" {
                if value.trim().is_empty() {
                    invalid_value(&mut out, template, line, name, value, limits)?;
                }
            } else if name.starts_with("t-") {
                if !profile::listed(profile::DIRECTIVES, name) {
                    diagnostic(&mut out, template, line, name, K::Directive, limits)?;
                    continue;
                }
                match name {
                    "t-att" => {
                        expression(template, line, value)?;
                        obligation(&mut out, template, line, "*", value, limits)?;
                    }
                    "t-tag" => {
                        expression(template, line, value)?;
                        obligation(&mut out, template, line, "tag", value, limits)?;
                    }
                    "t-call" if !value.contains(['{', '#']) => {} // A static template key.
                    "t-call" if format_expressions(template, line, value)? => {
                        obligation(&mut out, template, line, "template", value, limits)?;
                    }
                    "t-call" => invalid_value(&mut out, template, line, name, value, limits)?,
                    "t-if" | "t-elif" | "t-foreach" | "t-value" | "t-esc" | "t-out" | "t-field"
                    | "t-options" | "t-lang" => {
                        expression(template, line, value)?;
                    }
                    "t-translation" if value != "off" => {
                        invalid_value(&mut out, template, line, name, value, limits)?
                    }
                    "t-as" | "t-set" if !identifier(value) => {
                        invalid_value(&mut out, template, line, name, value, limits)?
                    }
                    _ => {} // Remaining declared directives carry metadata or body semantics, not expressions.
                }
            } else if let Some(formatted) = call_argument(node, name) {
                // A value handed to the called template, as the reference passes them.
                if !formatted {
                    expression(template, line, value)?;
                } else if !format_expressions(template, line, value)? {
                    invalid_value(&mut out, template, line, name, value, limits)?;
                }
            } else if !profile::attribute(node.tag(), name) {
                diagnostic(&mut out, template, line, name, K::Attribute, limits)?;
            } else {
                literal(&mut out, template, line, name, value, limits)?;
            }
        }
        let children: Vec<_> = node.children().collect();
        nodes.extend(children.into_iter().rev().map(|n| (n, depth + 1)));
    }
    Ok(out)
}
/// An attribute of a `<t t-call>` that names a value for the called template: an identifier holding
/// an expression, or `name.f` / `name.translate` holding a format string (`Some(true)`).
pub(crate) fn call_argument(node: erp_views::NodeRef<'_>, name: &str) -> Option<bool> {
    if node.tag() != "t" || node.attr("t-call").is_none() {
        return None;
    }
    let (base, formatted) = match name
        .strip_suffix(".f")
        .or_else(|| name.strip_suffix(".translate"))
    {
        Some(base) => (base, true),
        None => (name, false),
    };
    identifier(base).then_some(formatted)
}
pub(crate) fn identifier(value: &str) -> bool {
    let mut chars = value.chars();
    chars.next().is_some_and(|c| c == '_' || c.is_alphabetic())
        && chars.all(|c| c == '_' || c.is_alphanumeric())
}
pub(crate) fn expression(
    template: &str,
    line: u32,
    value: &str,
) -> Result<erp_expr::Expr, ReportError> {
    erp_expr::parse_with_limits(value, erp_expr::EvalLimits::default()).map_err(|source| {
        ReportError::Expression {
            template: template.into(),
            line,
            expression: value.into(),
            source,
        }
    })
}
/// Parse every `{{ … }}` and `#{ … }` part of a format string; `false` means a part is not closed.
/// One forward pass: each byte is looked at once whatever the number of parts.
fn format_expressions(template: &str, line: u32, value: &str) -> Result<bool, ReportError> {
    let mut rest = value;
    loop {
        let opener = |pair: &[u8]| pair == b"{{" || pair == b"#{";
        let Some(at) = rest.as_bytes().windows(2).position(opener) else {
            return Ok(true);
        };
        let close = if rest.as_bytes()[at] == b'{' {
            "}}"
        } else {
            "}"
        };
        let body = &rest[at + 2..];
        let Some(end) = body.find(close) else {
            return Ok(false);
        };
        expression(template, line, body[..end].trim())?;
        rest = &body[end + close.len()..];
    }
}
/// A declared attribute or directive whose literal value is outside its grammar.
fn invalid_value(
    out: &mut Validation,
    template: &str,
    line: u32,
    name: &str,
    value: &str,
    limits: ReportLimits,
) -> Result<(), ReportError> {
    let token = format!("{name}:{value}");
    diagnostic(out, template, line, &token, K::AttributeValue, limits)
}
fn diagnostic(
    out: &mut Validation,
    template: &str,
    line: u32,
    token: &str,
    kind: K,
    limits: ReportLimits,
) -> Result<(), ReportError> {
    if out.diagnostics.len() + out.obligations.len() >= limits.diagnostics {
        return Err(ReportError::Limit {
            name: "diagnostics",
            maximum: limits.diagnostics,
        });
    }
    out.diagnostics.push(Diagnostic {
        template: template.into(),
        line,
        token: token.into(),
        kind,
    });
    Ok(())
}
fn obligation(
    out: &mut Validation,
    template: &str,
    line: u32,
    attribute: &str,
    expression: &str,
    limits: ReportLimits,
) -> Result<(), ReportError> {
    if out.diagnostics.len() + out.obligations.len() >= limits.diagnostics {
        return Err(ReportError::Limit {
            name: "diagnostics",
            maximum: limits.diagnostics,
        });
    }
    out.obligations.push(DynamicValue {
        template: template.into(),
        line,
        attribute: attribute.into(),
        expression: expression.into(),
    });
    Ok(())
}
fn literal(
    out: &mut Validation,
    template: &str,
    line: u32,
    name: &str,
    value: &str,
    limits: ReportLimits,
) -> Result<(), ReportError> {
    let mut invalid = |kind, token: &str| diagnostic(out, template, line, token, kind, limits);
    let valid = match name {
        "class" => {
            for token in value.split_whitespace() {
                if profile::class_meaning(token).is_none() {
                    invalid(K::Class, token)?;
                }
            }
            true
        }
        "style" => {
            for item in value.split(';').map(str::trim).filter(|s| !s.is_empty()) {
                match item.split_once(':') {
                    Some((p, _)) if !style::known(p.trim()) => invalid(K::Style, p.trim())?,
                    Some((p, v)) if !style::valid(p.trim(), v.trim()) => {
                        invalid(K::StyleValue, &format!("{}:{}", p.trim(), v.trim()))?
                    }
                    Some(_) => {}
                    None => invalid(K::StyleValue, item)?,
                }
            }
            true
        }
        "data-icon" if !profile::listed(profile::ICONS, value) => {
            invalid(K::Icon, value)?;
            true
        }
        "href" => safe_link(value),
        "src" => safe_source(value),
        "dir" => profile::listed("ltr rtl auto", value),
        "colspan" | "rowspan" => value.parse::<u16>().is_ok_and(|n| n > 0),
        "width" | "height" => value.parse::<u32>().is_ok() || style::length(value, false),
        _ => true, // The remaining declared attributes are inert text or metadata.
    };
    if !valid {
        invalid_value(out, template, line, name, value, limits)?;
    }
    Ok(())
}
/// One leading slash: a path on this site, never `//host` or its `/\host` spelling.
fn site_path(value: &str) -> bool {
    value.starts_with('/') && !value[1..].starts_with(['/', '\\'])
}
fn safe_link(value: &str) -> bool {
    let trimmed = value.trim();
    !value.chars().any(char::is_control)
        && (trimmed.starts_with('#')
            || site_path(trimmed)
            || trimmed.split_once(':').is_some_and(|(scheme, _)| {
                ["http", "https", "mailto"]
                    .iter()
                    .any(|s| scheme.eq_ignore_ascii_case(s))
            }))
}
/// An image carried in the value itself; nothing is read to print it.
pub(crate) fn embedded_image(value: &str) -> bool {
    let scheme = value.get(..11);
    scheme.is_some_and(|p| p.eq_ignore_ascii_case("data:image/"))
        && !value.chars().any(char::is_control)
}
/// Printing fetches nothing (design §6.6): a literal image is a site path or an embedded image.
fn safe_source(value: &str) -> bool {
    embedded_image(value) || (site_path(value) && !value.chars().any(char::is_control))
}
