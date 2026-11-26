// SPDX-License-Identifier: MIT OR Apache-2.0
//! Data-only card plans emitted by the guest. Every expression is a checked field projection.
use super::{ArchCompileRequest, Operation, RequestError, Response};
use erp_ui_compiler::LimitKind;
use roxmltree::{Document, Node, ParsingOptions};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

fn identifier(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name.as_bytes()[0].is_ascii_alphabetic()
        && name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_')
        && !matches!(name, "constructor" | "prototype" | "toString" | "valueOf")
}

struct Compiler<'a> {
    request: &'a ArchCompileRequest,
    fields: BTreeSet<&'a str>,
    known: BTreeSet<String>,
    calls: BTreeMap<String, BTreeSet<String>>,
    nodes: usize,
}

impl Compiler<'_> {
    fn limit(&self, kind: LimitKind, ceiling: usize) -> usize {
        self.request
            .limits()
            .iter()
            .find(|(k, _)| *k == kind)
            .map_or(kind.default_value(), |(_, n)| *n)
            .min(ceiling)
    }

    fn error(&self, node: Node<'_, '_>, reason: &str) -> RequestError {
        let pos = node.document().text_pos_at(node.range().start);
        RequestError::new(format!(
            "{}:{}:{}: {reason}",
            self.request.arch_id(),
            pos.row,
            pos.col
        ))
    }

    fn projection(&self, node: Node<'_, '_>, expr: &str) -> Result<Value, RequestError> {
        if expr.len() > self.limit(LimitKind::ExpressionBytes, 4096) {
            return Err(self.error(node, "field projection exceeds the expression byte limit"));
        }
        let parts: Vec<_> = expr.trim().splitn(4, '.').collect();
        match parts.as_slice() {
            ["record", field, property]
                if self.fields.contains(field) && matches!(*property, "value" | "raw_value") =>
            {
                Ok(json!([field, property]))
            }
            _ => Err(self.error(
                node,
                "expected a declared record field and value or raw_value",
            )),
        }
    }

    fn node(
        &mut self,
        node: Node<'_, '_>,
        template: &str,
        depth: usize,
    ) -> Result<Value, RequestError> {
        self.nodes += 1;
        if depth > self.limit(LimitKind::Depth, 64)
            || self.nodes > self.limit(LimitKind::FileNodes, 16_384)
        {
            return Err(self.error(node, "restricted template node or depth limit exceeded"));
        }
        if node.is_text() {
            return Ok(json!({"text": node.text().unwrap_or_default()}));
        }
        let tag = node.tag_name().name();
        if node.tag_name().namespace().is_some()
            || !matches!(
                tag,
                "t" | "field"
                    | "div"
                    | "span"
                    | "p"
                    | "strong"
                    | "b"
                    | "em"
                    | "i"
                    | "small"
                    | "br"
                    | "ul"
                    | "ol"
                    | "li"
                    | "section"
                    | "article"
                    | "header"
                    | "footer"
                    | "h2"
                    | "h3"
                    | "h4"
                    | "time"
            )
        {
            return Err(self.error(node, "element is outside the restricted card vocabulary"));
        }
        let mut output = json!({"tag":tag});
        let mut attrs = BTreeMap::new();
        if node.attributes().len() > self.limit(LimitKind::Attributes, 32) {
            return Err(self.error(node, "too many attributes"));
        }
        for attr in node.attributes() {
            if matches!(attr.name(), "t-esc" | "t-out" | "name") && output.get("field").is_some() {
                return Err(self.error(node, "the field projection is already defined"));
            }
            let value = attr.value();
            match (attr.namespace(), attr.name()) {
                (None, "t-name") if depth == 1 && value == template => {}
                (None, "t-if") => output["when"] = self.projection(node, value)?,
                (None, "t-esc" | "t-out") => output["field"] = self.projection(node, value)?,
                (None, "name") if tag == "field" && self.fields.contains(value) => {
                    output["field"] = json!([value, "value"])
                }
                (None, "t-call") if tag == "t" && self.known.contains(value) => {
                    self.calls
                        .entry(template.to_owned())
                        .or_default()
                        .insert(value.to_owned());
                    output["call"] = json!(value);
                }
                (None, "class" | "title" | "aria-label" | "role" | "dir" | "lang")
                    if tag != "t" && tag != "field" =>
                {
                    attrs.insert(attr.name(), value);
                }
                _ => {
                    return Err(self.error(
                        node,
                        "attribute or directive is outside the restricted card vocabulary",
                    ));
                }
            }
        }
        if tag == "field" && output.get("field").is_none() {
            return Err(self.error(node, "field element requires a declared field"));
        }
        let children = node.children().filter(|n| n.is_element() || n.is_text());
        if output.get("field").is_some() || output.get("call").is_some() {
            if output.get("field").is_some() && output.get("call").is_some()
                || children
                    .into_iter()
                    .any(|n| n.is_element() || n.text().is_some_and(|s| !s.trim().is_empty()))
            {
                return Err(self.error(node, "field and call nodes cannot also supply a body"));
            }
        } else {
            output["children"] = Value::Array(
                children
                    .map(|child| self.node(child, template, depth + 1))
                    .collect::<Result<_, _>>()?,
            );
        }
        if !attrs.is_empty() {
            output["attrs"] = json!(attrs);
        }
        Ok(output)
    }
}

pub(super) fn compile(bytes: &[u8]) -> Result<Response, RequestError> {
    let request = ArchCompileRequest::decode(bytes)?;
    if request.compiler_policy() != "1" {
        return Err(RequestError::new("unknown restricted compiler policy"));
    }
    let schema: Value = serde_json::from_str(request.host_schema())
        .map_err(|_| RequestError::new("invalid card host schema JSON"))?;
    let schema = schema
        .as_object()
        .filter(|s| s.len() == 1)
        .ok_or_else(|| RequestError::new("expected a card field schema"))?;
    let declared = schema
        .get("fields")
        .and_then(Value::as_array)
        .filter(|s| s.len() <= 256)
        .ok_or_else(|| RequestError::new("expected at most 256 declared fields"))?;
    let mut compiler = Compiler {
        request: &request,
        fields: BTreeSet::new(),
        known: BTreeSet::new(),
        calls: BTreeMap::new(),
        nodes: 0,
    };
    for field in declared {
        let field = field
            .as_str()
            .filter(|s| identifier(s))
            .ok_or_else(|| RequestError::new("invalid card field name"))?;
        if !compiler.fields.insert(field) {
            return Err(RequestError::new("duplicate card field"));
        }
    }
    if request.source().len() > compiler.limit(LimitKind::FileBytes, 1_048_576)
        || request.source().len() > compiler.limit(LimitKind::InputBytes, 1_048_576)
    {
        return Err(RequestError::new("restricted source exceeds byte limit"));
    }
    let doc = Document::parse_with_options(
        request.source(),
        ParsingOptions {
            allow_dtd: false,
            nodes_limit: compiler.limit(LimitKind::FileNodes, 16_384) as u32,
            ..Default::default()
        },
    )
    .map_err(|e| {
        RequestError::new(format!(
            "{}:{}:{}: invalid restricted XML",
            request.arch_id(),
            e.pos().row,
            e.pos().col
        ))
    })?;
    let root = doc.root_element();
    if root.tag_name().namespace().is_some() || root.attributes().len() != 0 {
        return Err(compiler.error(root, "unsupported card wrapper or wrapper attributes"));
    }
    let templates = match root.tag_name().name() {
        "templates" => root,
        "kanban" => {
            let mut sets = root.children().filter(|n| n.has_tag_name("templates"));
            let first = sets
                .next()
                .ok_or_else(|| compiler.error(root, "kanban requires one templates element"))?;
            if sets.next().is_some() {
                return Err(compiler.error(root, "multiple template sets"));
            }
            for node in root
                .children()
                .filter(Node::is_element)
                .filter(|node| *node != first)
            {
                if !node.has_tag_name("field")
                    || node.attributes().len() != 1
                    || !node
                        .attribute("name")
                        .is_some_and(|name| compiler.fields.contains(name))
                    || node.children().any(|child| {
                        child.is_element() || child.text().is_some_and(|s| !s.trim().is_empty())
                    })
                {
                    return Err(compiler
                        .error(node, "kanban children must be declared fields or templates"));
                }
            }
            first
        }
        _ => return Err(compiler.error(root, "expected templates or kanban arch")),
    };
    if templates.attributes().len() != 0
        || [root, templates].into_iter().any(|parent| {
            parent
                .children()
                .any(|child| child.is_text() && child.text().is_some_and(|s| !s.trim().is_empty()))
        })
    {
        return Err(compiler.error(templates, "unexpected template wrapper content"));
    }
    for child in templates.children().filter(Node::is_element) {
        let name = child
            .attribute("t-name")
            .ok_or_else(|| compiler.error(child, "each card template requires t-name"))?;
        if !child.has_tag_name("t")
            || erp_ui_compiler::TemplateName::new(name).is_err()
            || !compiler.known.insert(name.to_owned())
        {
            return Err(compiler.error(child, "invalid or duplicate card template"));
        }
    }
    if compiler.known.is_empty() || compiler.known.len() > compiler.limit(LimitKind::Templates, 256)
    {
        return Err(compiler.error(templates, "invalid template count"));
    }
    let mut plans = BTreeMap::new();
    for child in templates.children().filter(Node::is_element) {
        let name = child
            .attribute("t-name")
            .ok_or_else(|| compiler.error(child, "missing template name"))?;
        plans.insert(name, compiler.node(child, name, 1)?);
    }
    let mut done = BTreeSet::new();
    for _ in 0..compiler.limit(LimitKind::CallDepth, 64) {
        let additions: Vec<_> = compiler
            .known
            .iter()
            .filter(|name| {
                !done.contains(*name)
                    && compiler
                        .calls
                        .get(*name)
                        .is_none_or(|calls| calls.is_subset(&done))
            })
            .cloned()
            .collect();
        if additions.is_empty() {
            break;
        }
        done.extend(additions);
    }
    if done != compiler.known {
        return Err(compiler.error(templates, "cyclic or over-depth card call"));
    }
    let artifact = json!({"abi":"erpforall.card.v1","arch":request.arch_id(),"source_sha256":request.source_digest(),
        "language":request.language(),"policy":request.compiler_policy(),"fields":compiler.fields,"templates":plans});
    let output = if request.operation() == Operation::Analyze {
        json!({"abi":"erpforall.card.v1","templates":compiler.known,"nodes":compiler.nodes})
    } else {
        artifact
    };
    let bytes = serde_json::to_vec(&output).map_err(|e| RequestError::new(e.to_string()))?;
    if bytes.len() > compiler.limit(LimitKind::OutputBytes, 1_048_576)
        || bytes.len() > compiler.limit(LimitKind::ModuleBytes, 1_048_576)
    {
        return Err(RequestError::new(
            "restricted artifact exceeds output limit",
        ));
    }
    Ok(Response { bytes, clean: true })
}
