//! Test tooling for the Python-compatible portion of the pinned JavaScript card corpus.
//! This bridge neither implements nor certifies the restricted JavaScript grammar.
use erp_expr::{
    Budget, EvalContext, ExprError, ExprObject, ExprResult, ExprValue, eval_ast, parse,
    wire::{WireDocument, WireNode},
};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, error::Error, sync::Arc};

const SEED: u32 = 0x5549_5802;
const RECORDS: usize = 1000;

#[derive(Deserialize)]
struct Corpus {
    schema: u32,
    entries: Vec<Entry>,
}
#[derive(Deserialize)]
struct Entry {
    file: String,
    line: usize,
    attribute: String,
    role: String,
    expressions: Vec<String>,
}
#[derive(Debug, Default, Clone, Serialize)]
pub struct Shape {
    children: BTreeMap<String, Shape>,
}
#[derive(Debug, Serialize)]
pub struct Differential {
    seed: u32,
    records: Vec<u32>,
    occurrences: usize,
    metadata: BTreeMap<String, usize>,
    unsupported: BTreeMap<String, usize>,
    rows: Vec<Row>,
}
#[derive(Debug, Serialize)]
struct Row {
    source: String,
    origins: Vec<String>,
    ast: WireDocument,
    bindings: BTreeMap<String, Shape>,
    outcomes: Vec<Outcome>,
    cases: Vec<usize>,
}
#[derive(Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum Outcome {
    Ok {
        repr: String,
    },
    Error {
        class: &'static str,
        message: String,
    },
}

/// Mask quoted text before recognizing JavaScript-only constructs. The Rust parser still
/// owns all syntax admission; this scanner cannot turn a rejected parse into a supported row.
pub fn unsupported(source: &str) -> Result<Option<&'static str>, Box<dyn Error>> {
    let mut code = String::new();
    let mut quote = None;
    let mut escaped = false;
    for ch in source.chars() {
        if let Some(q) = quote {
            code.push(' ');
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == q {
                quote = None;
            }
        } else if ch == '\'' || ch == '"' {
            quote = Some(ch);
            code.push(' ');
        } else {
            code.push(ch);
        }
    }
    if quote.is_some() {
        return Err("unterminated quoted card expression".into());
    }
    for (token, category) in [
        ("=>", "js_arrow"),
        ("===", "js_strict_equality"),
        ("!==", "js_strict_equality"),
        ("?.", "js_optional_chain"),
        ("&&", "js_and"),
        ("||", "js_or"),
        ("??", "js_nullish"),
        ("?", "js_conditional"),
        ("`", "js_template_literal"),
    ] {
        if code.contains(token) {
            return Ok(Some(category));
        }
    }
    let chars: Vec<_> = code.chars().collect();
    if chars
        .iter()
        .enumerate()
        .any(|(i, c)| *c == '!' && chars.get(i + 1) != Some(&'='))
    {
        return Ok(Some("js_not"));
    }
    let mut chars = code.char_indices().peekable();
    while let Some((start, ch)) = chars.next() {
        if !(ch.is_alphabetic() || ch == '_') {
            continue;
        }
        let mut end = start + ch.len_utf8();
        while let Some(&(pos, ch)) = chars.peek() {
            if !(ch.is_alphanumeric() || ch == '_') {
                break;
            }
            end = pos + ch.len_utf8();
            chars.next();
        }
        let name = &code[start..end];
        let prefix = code[..start].trim_end();
        let member = prefix.ends_with('.') && !prefix.ends_with("...");
        let following = code[end..].trim_start();
        if !member && matches!(name, "true" | "false" | "null" | "undefined") {
            return Ok(Some("js_literal"));
        }
        if !member && name == "this" {
            return Ok(Some("js_this"));
        }
        if !member && matches!(name, "JSON" | "Math" | "Array" | "luxon") {
            return Ok(Some("js_namespace"));
        }
        if !member && name == "new" {
            return Ok(Some("js_new"));
        }
        if member && name == "length" && !following.starts_with('.') {
            return Ok(Some("js_length"));
        }
        if member
            && following.starts_with('(')
            && matches!(name, "includes" | "indexOf" | "map" | "keys")
        {
            return Ok(Some("js_method"));
        }
        if !member && name == "typeof" {
            return Ok(Some("js_typeof"));
        }
    }

    Ok(None)
}

fn bindings(ast: &WireDocument) -> BTreeMap<String, Shape> {
    let standard = EvalContext::standard();
    let called: std::collections::BTreeSet<_> = ast
        .nodes
        .iter()
        .filter_map(|node| match node {
            WireNode::Call { func, .. } => Some(*func as usize),
            _ => None, // Only call targets select method attributes; no input is dropped.
        })
        .collect();
    let mut result = BTreeMap::<String, Shape>::new();
    let mut paths: Vec<Option<Vec<String>>> = Vec::new();
    for (index, node) in ast.nodes.iter().enumerate() {
        let path = match node {
            WireNode::Name { name } if standard.get(name).is_none() => Some(vec![name.clone()]),
            WireNode::Attribute { value, attr } if !called.contains(&index) => {
                paths[*value as usize].as_ref().map(|p| {
                    let mut p = p.clone();
                    p.push(attr.clone());
                    p
                })
            }
            // Only name/attribute chains describe fixture fields. All other nodes still evaluate normally.
            _ => None,
        };
        if let Some(path) = &path {
            let mut shape = result.entry(path[0].clone()).or_default();
            for part in &path[1..] {
                shape = shape.children.entry(part.clone()).or_default();
            }
        }
        paths.push(path);
    }
    result
}

fn leaf(state: u32, name: &str) -> ExprValue {
    let hash = name.bytes().fold(state, |hash, byte| {
        hash.wrapping_mul(31).wrapping_add(u32::from(byte))
    });
    match hash % 14 {
        0 => ExprValue::None,
        1 => ExprValue::Bool(false),
        2 => ExprValue::Bool(true),
        3 => ExprValue::Int(0),
        4 => ExprValue::Int(-1),
        5 => ExprValue::Int(i64::MAX),
        6 => ExprValue::str(""),
        7 => ExprValue::str("é🙂"),
        8 => ExprValue::list(vec![]),
        9 => ExprValue::list(vec![ExprValue::Int(1), ExprValue::Int(-1)]),
        10 => ExprValue::Int(i64::MIN),
        11 => ExprValue::Float(-0.0),
        12 => ExprValue::Float(1.5),
        // Remainder 13 is the final family, covering nested Unicode values.
        _ => ExprValue::list(vec![ExprValue::list(vec![ExprValue::str("é🙂")])]),
    }
}
#[derive(Debug)]
struct Fixture {
    shape: Shape,
    state: u32,
    path: String,
}
impl ExprObject for Fixture {
    fn type_name(&self) -> &str {
        "card_fixture"
    }
    fn get_attr(&self, name: &str, _budget: &mut Budget) -> ExprResult<ExprValue> {
        let shape = self
            .shape
            .children
            .get(name)
            .ok_or_else(|| ExprError::attribute_error("card_fixture", name))?;
        Ok(fixture(shape, self.state, &format!("{}.{name}", self.path)))
    }
}
fn fixture(shape: &Shape, state: u32, path: &str) -> ExprValue {
    if shape.children.is_empty() {
        leaf(state, path)
    } else {
        ExprValue::Object(Arc::new(Fixture {
            shape: shape.clone(),
            state,
            path: path.to_owned(),
        }))
    }
}

/// Produce a deterministic, dictionary-compressed oracle. Compression preserves every case;
/// it does not sample expressions or records, and is not a performance acceptance test.
pub fn generate(input: &str) -> Result<Differential, Box<dyn Error>> {
    let corpus: Corpus = serde_json::from_str(input)?;
    if corpus.schema != 1 {
        return Err("unsupported card corpus schema".into());
    }
    let mut state = SEED;
    let records: Vec<u32> = (0..RECORDS)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            state
        })
        .collect();
    let mut metadata = BTreeMap::<String, usize>::new();
    let mut excluded = BTreeMap::<String, usize>::new();
    let mut sources = BTreeMap::<String, Vec<String>>::new();
    let mut occurrences = 0;
    for entry in corpus.entries {
        if !matches!(
            entry.role.as_str(),
            "expression" | "format" | "binding" | "branch"
        ) {
            return Err(format!("unknown card role {}", entry.role).into());
        }
        if matches!(entry.role.as_str(), "binding" | "branch") {
            if !entry.expressions.is_empty() {
                return Err("structural directive contains executable expressions".into());
            }
            *metadata.entry(entry.role).or_default() += 1;
        }
        for source in entry.expressions {
            occurrences += 1;
            if let Some(category) = unsupported(&source)? {
                *excluded.entry(category.to_owned()).or_default() += 1;
                continue;
            }
            let expression = parse(&source).map_err(|e| {
                format!(
                    "unclassified construct {}:{} [{}] {source:?}: {e}",
                    entry.file, entry.line, entry.attribute
                )
            })?;
            let ast = WireDocument::from_expr(&expression)?;
            // Unknown named callables require the JavaScript host API from UI2-DONE.
            // Python methods remain supported and evaluate against the seeded leaf values.
            let mut host_call = false;
            for node in &ast.nodes {
                if let WireNode::Call { func, .. } = node {
                    match &ast.nodes[*func as usize] {
                        WireNode::Name { name } if EvalContext::standard().get(name).is_none() => {
                            host_call = true
                        }

                        _ => {} // Standard Python builtin or callable expression: evaluated by erp-expr.
                    }
                }
            }
            if host_call {
                *excluded.entry("host_call".to_owned()).or_default() += 1;
                continue;
            }
            sources.entry(source).or_default().push(format!(
                "{}:{} [{}]",
                entry.file, entry.line, entry.attribute
            ));
        }
    }
    if sources.is_empty() {
        return Err("card differential has no supported expressions".into());
    }
    let mut rows = Vec::new();
    for (source, origins) in sources {
        let expression = parse(&source)?;
        let ast = WireDocument::from_expr(&expression)?;
        let bindings = bindings(&ast);
        let mut outcomes = Vec::new();
        let mut cases = Vec::with_capacity(RECORDS);
        for &state in &records {
            let mut context = EvalContext::standard();
            for (name, shape) in &bindings {
                context = context.with_var(name, fixture(shape, state, name));
            }
            let result = match eval_ast(&expression, &context) {
                Ok(value) => Outcome::Ok { repr: value.repr() },
                Err(error) => Outcome::Error {
                    class: error.class(),
                    message: error.to_string(),
                },
            };
            let index = match outcomes.iter().position(|outcome| *outcome == result) {
                Some(index) => index,
                None => {
                    outcomes.push(result);
                    outcomes.len() - 1
                }
            };
            cases.push(index);
        }
        rows.push(Row {
            source,
            origins,
            ast,
            bindings,
            outcomes,
            cases,
        });
    }
    Ok(Differential {
        seed: SEED,
        records,
        occurrences,
        metadata,
        unsupported: excluded,
        rows,
    })
}
