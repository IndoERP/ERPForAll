//! Decoding the embedded wire document of an expression slot under the request's budget.
//!
//! The independent payload visitors never trust what the builder claims about an expression: they
//! decode the embedded `WireDocument` again with the closed `ValidatedAst::from_json` admission and
//! derive the references from the resulting tree. `erp-expr` allocates inside that call without an
//! external hook, so everything it can keep alive at once is reserved from counts read off the
//! input *before* it runs: the serialized buffer, the normalized copy, the node arena with its
//! growth, the strings, every edge container and the rebuilt tree. The allocation harness
//! (`tests/alloc_expression_preflight.rs`) checks the bound against measured peak live bytes.
use crate::expression_references::{
    BudgetMeter, ExpressionKind, walk_expression, walk_scoped_domain,
};
use crate::reference_index::Collector;
use crate::{DescriptionBudget, LimitKind, ViewError, ViewResult};
use erp_expr::Expr;
use erp_expr::wire::{
    MAX_ENCODED_BYTES, MAX_JSON_DEPTH, MAX_NODES, ValidatedAst, WireCompareOp, WireComprehension,
    WireDocument, WireKeyword, WireLiteral, WireNode,
};
use serde_json::Value;
use std::io::Write;
use std::mem::size_of;

fn refuse(message: &str) -> ViewError {
    ViewError::InvalidDescription(message.to_owned())
}
fn overflow() -> ViewError {
    refuse("Description expression reservation overflow")
}
fn not_a_document() -> ViewError {
    refuse("Description expression requires a versioned wire document")
}

/// What decoding one embedded wire document needs, known before anything is allocated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExpressionDecodePreflight {
    encoded_bytes: usize,
    nodes: usize,
    reserve_bytes: usize,
}
impl ExpressionDecodePreflight {
    /// Encoded JSON bytes of the document, which is what the aggregate byte budget is charged.
    pub fn encoded_bytes(&self) -> usize {
        self.encoded_bytes
    }
    /// Wire nodes in the document.
    pub fn nodes(&self) -> usize {
        self.nodes
    }
    /// Bytes reserved before the document is decoded.
    pub fn reserve_bytes(&self) -> usize {
        self.reserve_bytes
    }
}

/// Counts `serde_json` output without keeping it, stopping at the wire byte limit.
struct Counter(usize);
impl Write for Counter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 = self
            .0
            .checked_add(bytes.len())
            .filter(|total| *total <= MAX_ENCODED_BYTES)
            .ok_or_else(|| std::io::Error::other("expression byte limit"))?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
pub(crate) fn encoded_len(value: &impl serde::Serialize) -> ViewResult<usize> {
    let mut counter = Counter(0);
    serde_json::to_writer(&mut counter, value).map_err(|_| ViewError::LimitExceeded {
        limit: LimitKind::DescriptionExpressionBytes,
        max: MAX_ENCODED_BYTES,
        actual: None,
    })?;
    Ok(counter.0)
}

/// Sizes of the containers a decode builds, read off the input.
#[derive(Default)]
struct Profile {
    /// Elements in every array of the node list (edges, operators, keywords, generators, bytes).
    elements: usize,
    /// Bytes the arrays occupy while filled by `push`/`collect` (first allocation of at least
    /// four elements, doubling, previous buffer alive), at the largest element any of them holds.
    array_bytes: usize,
}
impl Profile {
    const ELEMENT: usize = max(
        max(size_of::<WireKeyword>(), size_of::<WireComprehension>()),
        max(size_of::<(u32, u32)>(), size_of::<WireCompareOp>()),
    );
    fn array(&mut self, len: usize) -> ViewResult<()> {
        if len == 0 {
            return Ok(());
        }
        self.elements = self.elements.checked_add(len).ok_or_else(overflow)?;
        let bytes = len
            .max(4)
            .checked_next_power_of_two()
            .and_then(|capacity| capacity.checked_mul(Self::ELEMENT))
            .and_then(|bytes| bytes.checked_mul(2))
            .ok_or_else(overflow)?;
        self.array_bytes = self.array_bytes.checked_add(bytes).ok_or_else(overflow)?;
        Ok(())
    }
}
const fn max(a: usize, b: usize) -> usize {
    if a > b { a } else { b }
}

/// Arrays nested anywhere inside one node (edge lists, keyword and generator objects, `ifs`).
fn profile_value(value: &Value, depth: usize, profile: &mut Profile) -> ViewResult<()> {
    if depth > MAX_JSON_DEPTH {
        return Err(refuse("Description expression JSON nesting exceeds 16"));
    }
    match value {
        Value::Array(items) => {
            profile.array(items.len())?;
            items
                .iter()
                .try_for_each(|item| profile_value(item, depth + 1, profile))
        }
        Value::Object(members) => members
            .values()
            .try_for_each(|member| profile_value(member, depth + 1, profile)),
        _ => Ok(()),
    }
}

/// Arrays the same decode builds for a `WireDocument` we already hold.
fn profile_document(document: &WireDocument) -> ViewResult<Profile> {
    let mut profile = Profile::default();
    for node in &document.nodes {
        match node {
            WireNode::Literal {
                value: WireLiteral::Bytes { value },
            } => profile.array(value.len())?,
            WireNode::Literal { .. }
            | WireNode::Name { .. }
            | WireNode::Walrus { .. }
            | WireNode::Attribute { .. }
            | WireNode::Unary { .. }
            | WireNode::Binary { .. }
            | WireNode::IfExp { .. }
            | WireNode::Subscript { .. }
            | WireNode::Starred { .. }
            | WireNode::FormattedValue { .. } => {}
            WireNode::BoolOp { values, .. } => profile.array(values.len())?,
            WireNode::List { items }
            | WireNode::Tuple { items }
            | WireNode::JoinedStr { items } => profile.array(items.len())?,
            WireNode::Compare {
                ops, comparators, ..
            } => {
                profile.array(ops.len())?;
                profile.array(comparators.len())?;
            }
            WireNode::Call { args, keywords, .. } => {
                profile.array(args.len())?;
                profile.array(keywords.len())?;
            }
            WireNode::Dict { items } => profile.array(items.len())?,
            WireNode::ListComp { generators, .. } | WireNode::GeneratorExp { generators, .. } => {
                profile.array(generators.len())?;
                for generator in generators {
                    profile.array(generator.ifs.len())?;
                }
            }
            _ => {
                return Err(refuse(
                    "Description expression has an unsupported wire node",
                ));
            }
        }
    }
    Ok(profile)
}

fn reserve_bytes(encoded: usize, nodes: usize, profile: &Profile) -> ViewResult<usize> {
    let arena = nodes
        .max(4)
        .checked_next_power_of_two()
        .and_then(|capacity| capacity.checked_mul(size_of::<WireNode>()))
        .and_then(|bytes| bytes.checked_mul(2))
        .ok_or_else(overflow)?;
    let rebuilt = nodes
        .checked_mul(size_of::<Option<Expr>>() + size_of::<Expr>())
        .ok_or_else(overflow)?;
    // Edges become `Vec<Expr>`/`Vec<(Expr, Expr)>`/`Vec<(Option<String>, Expr)>` of exact size.
    let children = profile
        .elements
        .checked_mul(2 * size_of::<Expr>() + size_of::<Option<String>>())
        .ok_or_else(overflow)?;
    [
        encoded,                                      // the serialized text
        encoded,                                      // the normalized copy, never longer
        encoded.checked_mul(2).ok_or_else(overflow)?, // string scratch plus the owned strings
        arena,
        profile.array_bytes,
        nodes.checked_mul(2).ok_or_else(overflow)?, // the validator's scratch bytes
        rebuilt,
        children,
    ]
    .into_iter()
    .try_fold(0usize, |total, bytes| total.checked_add(bytes))
    .ok_or_else(overflow)
}

/// Bound what decoding the embedded wire document in `value` keeps alive at once.
///
/// Refuses what is not a wire document or is beyond the wire limits; nothing is allocated.
pub fn expression_decode_preflight(value: &Value) -> ViewResult<ExpressionDecodePreflight> {
    let (nodes, profile) = shape(value)?;
    let encoded_bytes = encoded_len(value)?;
    Ok(ExpressionDecodePreflight {
        encoded_bytes,
        nodes,
        reserve_bytes: reserve_bytes(encoded_bytes, nodes, &profile)?,
    })
}

fn shape(value: &Value) -> ViewResult<(usize, Profile)> {
    let nodes = value
        .as_object()
        .and_then(|document| document.get("nodes"))
        .and_then(Value::as_array)
        .ok_or_else(not_a_document)?;
    if nodes.len() > MAX_NODES {
        return Err(refuse("Description expression exceeds its node limit"));
    }
    let mut profile = Profile::default();
    for node in nodes {
        profile_value(node, 3, &mut profile)?;
    }
    Ok((nodes.len(), profile))
}

/// Where a slot's references point.
#[derive(Clone, Copy)]
pub(crate) struct Slot<'a> {
    pub(crate) kind: ExpressionKind,
    pub(crate) scope: Option<&'a str>,
    pub(crate) parent_model: Option<&'a str>,
    pub(crate) scoped_model: Option<&'a str>,
    pub(crate) panel_syntax: bool,
}

/// Decode the embedded document of an untrusted payload value and collect its references.
/// Returns the expression's logical height.
pub(crate) fn collect_value<'a>(
    value: &Value,
    slot: Slot<'a>,
    budget: &DescriptionBudget,
    collector: &mut Collector<'a>,
) -> ViewResult<usize> {
    budget.check()?;
    let (nodes, profile) = shape(value).map_err(|error| wire_problem(error, slot))?;
    let encoded = encoded_len(value)?;
    decode_and_walk(
        encoded,
        nodes,
        &profile,
        |buffer| serde_json::to_writer(buffer, value),
        slot,
        budget,
        collector,
    )
}

/// The same for the document a builder retained, as the independent walk of a constructed payload.
pub(crate) fn collect_document<'a>(
    document: &WireDocument,
    slot: Slot<'a>,
    budget: &DescriptionBudget,
    collector: &mut Collector<'a>,
) -> ViewResult<usize> {
    budget.check()?;
    let profile = profile_document(document)?;
    let encoded = encoded_len(document)?;
    decode_and_walk(
        encoded,
        document.nodes.len(),
        &profile,
        |buffer| serde_json::to_writer(buffer, document),
        slot,
        budget,
        collector,
    )
}

fn decode_and_walk<'a>(
    encoded: usize,
    nodes: usize,
    profile: &Profile,
    write: impl FnOnce(&mut Vec<u8>) -> Result<(), serde_json::Error>,
    slot: Slot<'a>,
    budget: &DescriptionBudget,
    collector: &mut Collector<'a>,
) -> ViewResult<usize> {
    // Quotas first, then the reservation, then the first allocation: a refusal allocates nothing.
    budget.charge_expression_bytes(encoded)?;
    budget.charge_work(nodes)?;
    let _reservation = budget.reserve(reserve_bytes(encoded, nodes, profile)?)?;
    let mut buffer = Vec::new();
    if buffer.try_reserve_exact(encoded).is_err() {
        budget.poison();
        return Err(refuse("Description expression allocation failed"));
    }
    write(&mut buffer).map_err(|error| {
        budget.poison();
        refuse(&format!(
            "Description expression could not be encoded: {error}"
        ))
    })?;
    let admitted = ValidatedAst::from_json(&buffer).map_err(|error| {
        if slot.scoped_model.is_some() {
            refuse("Description scoped domain has an invalid wire document")
        } else {
            refuse(&format!(
                "Description expression is not a valid wire document: {error}"
            ))
        }
    })?;
    drop(buffer);
    if slot.panel_syntax {
        crate::validate_panel_domain(&admitted)?;
    }
    if let Some(model) = slot.scoped_model {
        return walk_scoped_domain(admitted.as_expr(), &mut BudgetMeter(budget), |found| {
            collector.push_scoped_found(model, found)
        });
    }
    walk_expression(
        admitted.as_expr(),
        slot.kind,
        slot.parent_model.is_some(),
        &mut BudgetMeter(budget),
        |found| collector.push_found(slot.scope, slot.parent_model, found),
    )
}

fn wire_problem(error: ViewError, slot: Slot<'_>) -> ViewError {
    match error {
        ViewError::InvalidDescription(_) if slot.scoped_model.is_some() => {
            refuse("Description scoped domain has an invalid wire document")
        }
        other => other,
    }
}

/// Independently admit a final panel domain and derive every scoped reference under one budget.
///
/// The AST is decoded again using the existing preflight reservations before the strict v1
/// grammar and reference walk run. Criterion paths retain `model`; bare RHS category names
/// retain root scope. The host must still compare the descriptive model with the actual
/// relation and validate category membership, paths and caller access. No JSON value copy is made.
///
/// # Errors
/// Invalid model or AST, unsupported panel syntax, or exhausted shared construction/work/byte
/// allowance. Every refusal poisons the request budget and no partial index is returned.
///
/// ```
/// let expression = erp_expr::parse("[('name', '=', category)]")?;
/// let document = erp_expr::wire::WireDocument::from_expr(&expression)?;
/// let budget = erp_views::DescriptionBudget::new();
/// let refs = erp_views::panel_domain_references("example.tag", &document, &budget)?;
/// assert_eq!(refs.fields().count(), 2);
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub fn panel_domain_references<'a>(
    model: &'a str,
    document: &WireDocument,
    budget: &DescriptionBudget,
) -> ViewResult<crate::SchemaReferences<'a>> {
    let result = (|| {
        budget.check()?;
        if !crate::description_schema::model_reference(model) {
            return Err(refuse("Description scoped domain has an invalid model"));
        }
        let mut collector = Collector::new(budget)?;
        collect_document(
            document,
            Slot {
                kind: ExpressionKind::Domain,
                scope: None,
                parent_model: None,
                scoped_model: Some(model),
                panel_syntax: true,
            },
            budget,
            &mut collector,
        )?;
        collector.finish()
    })();
    if result.is_err() {
        budget.poison();
    }
    result
}
