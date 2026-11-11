//! Document → AST: a hand-written decoder over the strict JSON tree with a closed key set per
//! object. Unknown keys are refused before any value is read; every error
//! carries the path of the element.

use std::collections::HashSet;

use erp_fields::json::{self, Json};

use crate::{
    Domain, DomainError, ErrorKind, ErrorReason, Granularity, NowKind, PrincipalParam, Value,
};

use super::error::{Bound, JsonPath, QueryAstError, QueryAstReason, shown};
use super::limits::{QueryLimits, Widen};
use super::names::{FieldName, FieldPath, ModelName};
use super::parts::{
    Aggregate, AggregateFunc, FieldOrder, GroupBy, GroupOrder, HavingCondition, HavingOp, NullsPos,
    OrderDir, QueryContext,
};
use super::{Body, QUERY_AST_VERSION, QueryAst, encode};

type Res<T> = Result<T, QueryAstError>;
type Members = [(Box<str>, Json)];

/// The accepted values of each enumeration, for `UnknownValue` messages.
const KINDS: &str = "search_read, read_group";
const DIRS: &str = "asc, desc";
const NULLS: &str = "first, last";
const HAVING_OPS: &str = "=, !=, <, >, <=, >=, in, not in";
const FUNCS: &str = "count, count_distinct, sum, avg, min, max, bool_and, bool_or";
const PARAMS: &str = "uid, company_id, company_ids, today, now";
pub(crate) const GRANULARITIES: &str = "hour, day, week, month, quarter, year, year_number, \
     quarter_number, month_number, iso_week_number, day_of_year, day_of_month, day_of_week, \
     hour_number, minute_number, second_number";

const COMMON_KEYS: [&str; 8] = [
    "version", "kind", "model", "domain", "context", "order", "limit", "offset",
];

fn err(at: &JsonPath, reason: QueryAstReason) -> QueryAstError {
    QueryAstError::new(at, reason)
}

fn wrong_type(at: &JsonPath, expected: &'static str, found: &Json) -> QueryAstError {
    err(
        at,
        QueryAstReason::WrongType {
            expected,
            found: found.type_name(),
        },
    )
}

fn domain_error(at: &JsonPath, reason: ErrorReason) -> QueryAstError {
    err(
        at,
        QueryAstReason::Domain(DomainError::new(ErrorKind::Validation, reason)),
    )
}

fn too_large(at: &JsonPath, bound: Bound, max: impl Widen, found: usize) -> QueryAstError {
    err(
        at,
        QueryAstReason::TooLarge {
            bound,
            max: max.widened(),
            found: found.to_string(),
        },
    )
}

// ----- objects ---------------------------------------------------------------------------------

fn object<'a>(json: &'a Json, at: &JsonPath) -> Res<&'a Members> {
    match json {
        Json::Object(members) => Ok(members),
        other => Err(wrong_type(at, "object", other)),
    }
}

fn array<'a>(json: &'a Json, at: &JsonPath) -> Res<&'a [Json]> {
    match json {
        Json::Array(items) => Ok(items),
        other => Err(wrong_type(at, "array", other)),
    }
}

fn string<'a>(json: &'a Json, at: &JsonPath) -> Res<&'a str> {
    match json {
        Json::Str(text) => Ok(text),
        other => Err(wrong_type(at, "string", other)),
    }
}

fn get<'a>(members: &'a Members, key: &str) -> Option<&'a Json> {
    members.iter().find_map(|(k, v)| (&**k == key).then_some(v))
}

fn require<'a>(members: &'a Members, key: &'static str, at: &JsonPath) -> Res<&'a Json> {
    get(members, key).ok_or_else(|| err(at, QueryAstReason::MissingKey(key)))
}

/// Refuse the first key of `members` that is not in `allowed`.
fn only(members: &Members, allowed: &[&str], at: &JsonPath) -> Res<()> {
    match members.iter().find(|(k, _)| !allowed.contains(&&**k)) {
        Some((key, _)) => Err(err(at, QueryAstReason::UnknownKey(shown(key)))),
        None => Ok(()),
    }
}

fn unknown_value(
    at: &JsonPath,
    what: &'static str,
    value: &str,
    expected: &'static str,
) -> QueryAstError {
    err(
        at,
        QueryAstReason::UnknownValue {
            what,
            value: shown(value),
            expected,
        },
    )
}

// ----- the document ----------------------------------------------------------------------------

/// Deepest nesting of `json`, counted iteratively (a scalar is depth 0, `[]` depth 1).
fn nesting(json: &Json) -> usize {
    let mut deepest = 0;
    let mut work = vec![(json, 0usize)];
    while let Some((node, depth)) = work.pop() {
        match node {
            Json::Array(items) => {
                let inner = depth.saturating_add(1);
                deepest = deepest.max(inner);
                work.extend(items.iter().map(|item| (item, inner)));
            }
            Json::Object(members) => {
                let inner = depth.saturating_add(1);
                deepest = deepest.max(inner);
                work.extend(members.iter().map(|(_, item)| (item, inner)));
            }
            Json::Null
            | Json::Bool(_)
            | Json::Int(_)
            | Json::BigInt(_)
            | Json::Float(_)
            | Json::Str(_) => {}
        }
    }
    deepest
}

/// Refuse an object that repeats a key, at any level, as the parser does for text (a tree built
/// in Rust can hold one; reading the first copy would hide the second). Iterative: the tree is
/// not deeper than the depth bound when this runs.
fn no_repeated_keys(doc: &Json) -> Res<()> {
    let mut work = vec![(doc, JsonPath::root())];
    while let Some((node, at)) = work.pop() {
        match node {
            Json::Array(items) => {
                work.extend(
                    items
                        .iter()
                        .enumerate()
                        .map(|(i, item)| (item, at.index(i))),
                );
            }
            Json::Object(members) => {
                let mut seen = HashSet::with_capacity(members.len());
                for (key, value) in members {
                    if !seen.insert(&**key) {
                        return Err(err(&at, QueryAstReason::DuplicateKey(shown(key))));
                    }
                    work.push((value, at.key(key)));
                }
            }
            Json::Null
            | Json::Bool(_)
            | Json::Int(_)
            | Json::BigInt(_)
            | Json::Float(_)
            | Json::Str(_) => {}
        }
    }
    Ok(())
}

/// Decode and validate a parsed document.
pub(crate) fn from_json(doc: &Json, limits: &QueryLimits) -> Res<QueryAst> {
    let root = JsonPath::root();
    let depth = nesting(doc);
    if depth > limits.json_depth {
        return Err(too_large(&root, Bound::JsonDepth, limits.json_depth, depth));
    }
    no_repeated_keys(doc)?;
    let members = object(doc, &root)?;
    check_version(members, &root)?;
    let kind_at = root.key("kind");
    let kind = string(require(members, "kind", &root)?, &kind_at)?;
    let search = match kind {
        "search_read" => true,
        "read_group" => false,
        other => return Err(unknown_value(&kind_at, "kind", other, KINDS)),
    };
    let extra: &[&str] = if search {
        &["fields"]
    } else {
        &["groupby", "aggregates", "having"]
    };
    let allowed: Vec<&str> = COMMON_KEYS.iter().chain(extra).copied().collect();
    only(members, &allowed, &root)?;

    let model_at = root.key("model");
    let model = ModelName::at(
        &model_at,
        string(require(members, "model", &root)?, &model_at)?,
    )?;
    let domain = match get(members, "domain") {
        Some(json) => domain(json, &root.key("domain"), limits)?,
        None => Domain::TRUE,
    };
    let context = match get(members, "context") {
        Some(json) => context(json, &root.key("context"))?,
        None => QueryContext::default(),
    };
    let rows_or_groups = if search { Bound::Rows } else { Bound::Groups };
    let max_limit = if search { limits.rows } else { limits.groups };
    let limit = bounded_u32(
        require(members, "limit", &root)?,
        &root.key("limit"),
        rows_or_groups,
        1,
        max_limit,
    )?;
    let offset = match get(members, "offset") {
        Some(json) => bounded_u32(json, &root.key("offset"), Bound::Offset, 0, limits.offset)?,
        None => 0,
    };
    let order_json = get(members, "order");
    let body = if search {
        search_read_body(members, order_json, &root, limits)?
    } else {
        read_group_body(members, order_json, &root, limits)?
    };
    let canonical = encode::document(&model, &domain, context, &body, limit, offset, limits)?;
    let bytes = json::canonical(&canonical).len();
    if bytes > limits.bytes {
        return Err(too_large(&root, Bound::Bytes, limits.bytes, bytes));
    }
    Ok(QueryAst {
        model,
        domain,
        context,
        body,
        limit,
        offset,
        doc: canonical,
    })
}

fn check_version(members: &Members, root: &JsonPath) -> Res<()> {
    let at = root.key("version");
    match require(members, "version", root)? {
        Json::Int(n) if *n == i64::from(QUERY_AST_VERSION) => Ok(()),
        Json::Int(n) => Err(err(
            &at,
            QueryAstReason::UnsupportedVersion {
                found: n.to_string(),
            },
        )),
        Json::BigInt(text) => Err(err(
            &at,
            QueryAstReason::UnsupportedVersion { found: shown(text) },
        )),
        other => Err(wrong_type(&at, "integer", other)),
    }
}

/// An integer in `min..=max` (a negative or oversized literal is an error naming the bound).
fn bounded_u32(json: &Json, at: &JsonPath, bound: Bound, min: u32, max: u32) -> Res<u32> {
    let too_small = |found: String| {
        err(
            at,
            QueryAstReason::TooSmall {
                bound,
                min: u64::from(min),
                found,
            },
        )
    };
    let too_big = |found: String| {
        err(
            at,
            QueryAstReason::TooLarge {
                bound,
                max: u64::from(max),
                found,
            },
        )
    };
    match json {
        Json::Int(n) => {
            if *n < i64::from(min) {
                return Err(too_small(n.to_string()));
            }
            match u32::try_from(*n) {
                Ok(value) if value <= max => Ok(value),
                _ => Err(too_big(n.to_string())),
            }
        }
        Json::BigInt(text) if text.starts_with('-') => Err(too_small(shown(text))),
        Json::BigInt(text) => Err(too_big(shown(text))),
        other => Err(wrong_type(at, "integer", other)),
    }
}

// ----- context ---------------------------------------------------------------------------------

fn context(json: &Json, at: &JsonPath) -> Res<QueryContext> {
    let members = object(json, at)?;
    let mut context = QueryContext::default();
    for (key, value) in members {
        if &**key == "active_test" {
            match value {
                Json::Bool(flag) => context.active_test = Some(*flag),
                other => return Err(wrong_type(&at.key("active_test"), "boolean", other)),
            }
            continue;
        }
        let why = match &**key {
            "lang" | "tz" => "it comes from the running environment",
            "allowed_company_ids" => "the company scope is the running principal's",
            _ => "not in the allowlist (active_test)",
        };
        return Err(err(
            &at.key(&shown(key)),
            QueryAstReason::RefusedContextKey {
                key: shown(key),
                why,
            },
        ));
    }
    Ok(context)
}

// ----- domain ----------------------------------------------------------------------------------

fn is_any_family(op: &Json) -> bool {
    matches!(op, Json::Str(text) if &**text == "any" || &**text == "not any")
}

/// Conditions of `items` and of the sub-domains of `any` conditions below it; a sub-domain deeper
/// than the bound is refused on the way down. Operators are not counted: a valid prefix domain
/// has about one per condition, the canonical form writes the implicit `&` explicitly, and the
/// parser drops constant leaves and pushes `!` down, so the number of conditions can only fall
/// between a document and its canonical form (a canonical form always parses again under the
/// limits that accepted the document). The work of the other items is bounded by the input size.
fn count_items(items: &[Json], at: &JsonPath, depth: usize, limits: &QueryLimits) -> Res<usize> {
    let mut total = 0usize;
    for (index, item) in items.iter().enumerate() {
        let Json::Array(triple) = item else { continue };
        if triple.len() != 3 {
            continue;
        }
        // the count is bounded by the nodes of a tree already in memory, so it cannot wrap
        total = total.saturating_add(1);
        if let [_, op, Json::Array(sub)] = triple.as_slice()
            && is_any_family(op)
        {
            let inner = depth.saturating_add(1);
            if inner > limits.domain_depth {
                return Err(too_large(
                    at,
                    Bound::DomainDepth,
                    limits.domain_depth,
                    inner,
                ));
            }
            total =
                total.saturating_add(count_items(sub, &at.index(index).index(2), inner, limits)?);
        }
    }
    Ok(total)
}

fn domain(json: &Json, at: &JsonPath, limits: &QueryLimits) -> Res<Domain> {
    let items = array(json, at)?;
    let total = count_items(items, at, 0, limits)?;
    if total > limits.domain_items {
        return Err(too_large(
            at,
            Bound::DomainItems,
            limits.domain_items,
            total,
        ));
    }
    let values = domain_items(items, at, 0, limits)?;
    let parsed = Domain::from_list(&values).map_err(|e| err(at, QueryAstReason::Domain(e)))?;
    let depth = parsed.depth();
    if depth > limits.domain_depth {
        return Err(too_large(
            at,
            Bound::DomainDepth,
            limits.domain_depth,
            depth,
        ));
    }
    Ok(parsed)
}

/// The list-syntax items of a domain as dynamic values, with `{"param"}` objects turned into
/// principal parameters in the value position of a condition (the only place they may stand).
fn domain_items(
    items: &[Json],
    at: &JsonPath,
    depth: usize,
    limits: &QueryLimits,
) -> Res<Vec<Value>> {
    let mut out = Vec::with_capacity(items.len());
    for (index, item) in items.iter().enumerate() {
        let item_at = at.index(index);
        match item {
            Json::Array(parts) if parts.len() == 3 => {
                let field = plain_value(&parts[0], &item_at.index(0), 0, limits)?;
                let op = plain_value(&parts[1], &item_at.index(1), 0, limits)?;
                let value_at = item_at.index(2);
                let value = match &parts[2] {
                    Json::Array(sub) if is_any_family(&parts[1]) => {
                        let inner = depth.saturating_add(1);
                        let values = domain_items(sub, &value_at, inner, limits)?;
                        // the sub-domain is read again at run time; refuse its syntax errors now
                        Domain::from_list(&values)
                            .map_err(|e| err(&value_at, QueryAstReason::Domain(e)))?;
                        Value::List(values)
                    }
                    other => value(other, &value_at, 0, limits)?,
                };
                out.push(Value::List(vec![field, op, value]));
            }
            other => out.push(plain_value(other, &item_at, 0, limits)?),
        }
    }
    Ok(out)
}

fn nested_too_deep(at: &JsonPath, limits: &QueryLimits, depth: usize) -> QueryAstError {
    too_large(at, Bound::JsonDepth, limits.json_depth, depth)
}

/// A value where no parameter may stand (field, operator, loose items).
fn plain_value(json: &Json, at: &JsonPath, depth: usize, limits: &QueryLimits) -> Res<Value> {
    convert(json, at, depth, limits, false)
}

/// A value of a condition: a parameter may stand anywhere in it.
fn value(json: &Json, at: &JsonPath, depth: usize, limits: &QueryLimits) -> Res<Value> {
    convert(json, at, depth, limits, true)
}

fn convert(
    json: &Json,
    at: &JsonPath,
    depth: usize,
    limits: &QueryLimits,
    params: bool,
) -> Res<Value> {
    match json {
        Json::Array(items) => {
            let inner = depth.saturating_add(1);
            if inner > limits.json_depth {
                return Err(nested_too_deep(at, limits, inner));
            }
            let mut out = Vec::with_capacity(items.len());
            for (index, item) in items.iter().enumerate() {
                out.push(convert(item, &at.index(index), inner, limits, params)?);
            }
            Ok(Value::List(out))
        }
        Json::Object(members) if params => principal_param(members, at),
        Json::Object(members) => {
            if get(members, "param").is_some() {
                return Err(err(
                    at,
                    QueryAstReason::RefusedFeature {
                        what: "a principal parameter here".to_owned(),
                        why: "parameters stand only in the value of a condition",
                    },
                ));
            }
            Err(unsupported_object(json, at))
        }
        scalar => scalar_value(scalar, at),
    }
}

fn unsupported_object(json: &Json, at: &JsonPath) -> QueryAstError {
    let text = String::from_utf8_lossy(&json::canonical(json)).into_owned();
    domain_error(at, ErrorReason::UnsupportedValue(shown(&text)))
}

/// `{"param": name}` (and nothing else) as a principal parameter.
fn principal_param(members: &Members, at: &JsonPath) -> Res<Value> {
    let Some(name) = get(members, "param") else {
        return Err(unsupported_object(&Json::Object(members.to_vec()), at));
    };
    only(members, &["param"], at)?;
    let name_at = at.key("param");
    let param = match string(name, &name_at)? {
        "uid" => PrincipalParam::Uid,
        "company_id" => PrincipalParam::CompanyId,
        "company_ids" => PrincipalParam::CompanyIds,
        "today" => PrincipalParam::Now(NowKind::Date),
        "now" => PrincipalParam::Now(NowKind::Timestamp),
        other => return Err(unknown_value(&name_at, "param", other, PARAMS)),
    };
    Ok(Value::Param(param))
}

/// A JSON scalar as a domain value.
fn scalar_value(json: &Json, at: &JsonPath) -> Res<Value> {
    let unsupported = |text: String| domain_error(at, ErrorReason::UnsupportedValue(shown(&text)));
    match json {
        Json::Null => Ok(Value::FALSE),
        Json::Bool(flag) => Ok(Value::Bool(*flag)),
        Json::Int(n) => Ok(Value::Int(*n)),
        Json::BigInt(text) => Err(unsupported(text.to_string())),
        Json::Float(f) => {
            if !f.is_finite() {
                return Err(unsupported(f.to_string()));
            }
            if f.fract() != 0.0 {
                return Ok(Value::Float(*f));
            }
            // below 2^53 an integral double is the integer its literal says (2^53 and 2^53+1 parse to the same
            // double, so 2^53 itself is ambiguous); from 2^53 to 1e21 its canonical
            // text is an integer literal that no longer reads back as the same number (or at all
            // as an i64), so it is refused; from 1e21 the text has an exponent and stays a float
            if f.abs() < 9_007_199_254_740_992.0 {
                Ok(Value::Int(*f as i64))
            } else if f.abs() < 1e21 {
                Err(unsupported(f.to_string()))
            } else {
                Ok(Value::Float(*f))
            }
        }
        Json::Str(text) => Ok(Value::Str(text.to_string())),
        Json::Array(_) | Json::Object(_) => Err(wrong_type(at, "scalar value", json)),
    }
}

// ----- search_read -----------------------------------------------------------------------------

fn search_read_body(
    members: &Members,
    order: Option<&Json>,
    root: &JsonPath,
    limits: &QueryLimits,
) -> Res<Body> {
    let fields_at = root.key("fields");
    let items = array(require(members, "fields", root)?, &fields_at)?;
    if items.is_empty() {
        return Err(err(
            &fields_at,
            QueryAstReason::TooSmall {
                bound: Bound::Fields,
                min: 1,
                found: "0".to_owned(),
            },
        ));
    }
    if items.len() > limits.fields {
        return Err(too_large(
            &fields_at,
            Bound::Fields,
            limits.fields,
            items.len(),
        ));
    }
    let mut fields = Vec::with_capacity(items.len());
    let mut seen = HashSet::new();
    for (index, item) in items.iter().enumerate() {
        let at = fields_at.index(index);
        let name = FieldName::at(&at, string(item, &at)?)?;
        if !seen.insert(name.as_str().to_owned()) {
            return Err(err(
                &at,
                QueryAstReason::Duplicate {
                    what: "field",
                    value: name.as_str().to_owned(),
                },
            ));
        }
        fields.push(name);
    }
    let terms = match order {
        Some(json) => order_terms(json, &root.key("order"), "field", limits)?,
        None => Vec::new(),
    };
    let mut seen = HashSet::new();
    let mut order = Vec::with_capacity(terms.len());
    for (index, term) in terms.into_iter().enumerate() {
        let at = root.key("order").index(index).key("field");
        let field = FieldName::at(&at, &term.key)?;
        if !seen.insert(term.key) {
            return Err(err(
                &at,
                QueryAstReason::Duplicate {
                    what: "order term",
                    value: field.as_str().to_owned(),
                },
            ));
        }
        order.push(FieldOrder {
            field,
            dir: term.dir,
            nulls: term.nulls,
        });
    }
    Ok(Body::SearchRead { fields, order })
}

/// One order term before its key is resolved.
struct RawTerm {
    key: String,
    dir: OrderDir,
    nulls: Option<NullsPos>,
}

fn order_terms(
    json: &Json,
    at: &JsonPath,
    key_name: &'static str,
    limits: &QueryLimits,
) -> Res<Vec<RawTerm>> {
    let items = array(json, at)?;
    if items.len() > limits.order_terms {
        return Err(too_large(
            at,
            Bound::OrderTerms,
            limits.order_terms,
            items.len(),
        ));
    }
    let mut out = Vec::with_capacity(items.len());
    for (index, item) in items.iter().enumerate() {
        let term_at = at.index(index);
        let members = object(item, &term_at)?;
        only(members, &[key_name, "dir", "nulls"], &term_at)?;
        let key_at = term_at.key(key_name);
        let key = string(require(members, key_name, &term_at)?, &key_at)?.to_owned();
        let dir = match get(members, "dir") {
            None => OrderDir::Asc,
            Some(json) => {
                let dir_at = term_at.key("dir");
                let text = string(json, &dir_at)?;
                OrderDir::parse(text)
                    .ok_or_else(|| unknown_value(&dir_at, "order direction", text, DIRS))?
            }
        };
        let nulls = match get(members, "nulls") {
            None | Some(Json::Null) => None,
            Some(json) => {
                let nulls_at = term_at.key("nulls");
                let text = string(json, &nulls_at)?;
                Some(
                    NullsPos::parse(text)
                        .ok_or_else(|| unknown_value(&nulls_at, "nulls position", text, NULLS))?,
                )
            }
        };
        out.push(RawTerm { key, dir, nulls });
    }
    Ok(out)
}

// ----- read_group ------------------------------------------------------------------------------

fn read_group_body(
    members: &Members,
    order: Option<&Json>,
    root: &JsonPath,
    limits: &QueryLimits,
) -> Res<Body> {
    let groupby = match get(members, "groupby") {
        Some(json) => groupby(json, &root.key("groupby"), limits)?,
        None => Vec::new(),
    };
    let aggregates = match get(members, "aggregates") {
        Some(json) => aggregates(json, &root.key("aggregates"), limits)?,
        None => Vec::new(),
    };
    if groupby.is_empty() && aggregates.is_empty() {
        return Err(err(root, QueryAstReason::NothingToGroup));
    }
    let group_names: Vec<String> = groupby.iter().map(GroupBy::spec_name).collect();
    let aggregate_names: Vec<String> = aggregates.iter().map(Aggregate::spec_name).collect();
    let having = match get(members, "having") {
        Some(json) => having(json, &root.key("having"), &aggregate_names, limits)?,
        None => Vec::new(),
    };
    let order = match order {
        Some(json) => {
            let order_at = root.key("order");
            let terms = order_terms(json, &order_at, "key", limits)?;
            let mut seen = HashSet::new();
            let mut out = Vec::with_capacity(terms.len());
            for (index, term) in terms.into_iter().enumerate() {
                let at = order_at.index(index).key("key");
                if !group_names.contains(&term.key) && !aggregate_names.contains(&term.key) {
                    return Err(err(
                        &at,
                        QueryAstReason::DanglingReference(shown(&term.key)),
                    ));
                }
                if !seen.insert(term.key.clone()) {
                    return Err(err(
                        &at,
                        QueryAstReason::Duplicate {
                            what: "order term",
                            value: term.key,
                        },
                    ));
                }
                out.push(GroupOrder {
                    key: term.key.into_boxed_str(),
                    dir: term.dir,
                    nulls: term.nulls,
                });
            }
            out
        }
        None => Vec::new(),
    };
    Ok(Body::ReadGroup {
        groupby,
        aggregates,
        having,
        order,
    })
}

fn groupby(json: &Json, at: &JsonPath, limits: &QueryLimits) -> Res<Vec<GroupBy>> {
    let items = array(json, at)?;
    if items.len() > limits.groupby {
        return Err(too_large(at, Bound::GroupBy, limits.groupby, items.len()));
    }
    let mut out: Vec<GroupBy> = Vec::with_capacity(items.len());
    for (index, item) in items.iter().enumerate() {
        let item_at = at.index(index);
        let members = object(item, &item_at)?;
        only(members, &["path", "granularity"], &item_at)?;
        let path_at = item_at.key("path");
        let path = FieldPath::at(
            &path_at,
            string(require(members, "path", &item_at)?, &path_at)?,
            limits.path_segments,
        )?;
        let granularity = match get(members, "granularity") {
            None | Some(Json::Null) => None,
            Some(json) => {
                let g_at = item_at.key("granularity");
                let text = string(json, &g_at)?;
                Some(
                    Granularity::parse(text)
                        .ok_or_else(|| unknown_value(&g_at, "granularity", text, GRANULARITIES))?,
                )
            }
        };
        let group = GroupBy { path, granularity };
        let name = group.spec_name();
        if out.iter().any(|g| g.spec_name() == name) {
            return Err(err(
                &item_at,
                QueryAstReason::Duplicate {
                    what: "group-by",
                    value: name,
                },
            ));
        }
        out.push(group);
    }
    Ok(out)
}

/// Refused aggregate functions and the reason, by name.
fn refused_func(name: &str) -> Option<&'static str> {
    match name {
        "array_agg" | "array_agg_distinct" | "recordset" => Some("unbounded output per group"),
        "sum_currency" => Some("needs res.currency rates (a later release)"),
        _ => None,
    }
}

fn aggregates(json: &Json, at: &JsonPath, limits: &QueryLimits) -> Res<Vec<Aggregate>> {
    let items = array(json, at)?;
    if items.len() > limits.aggregates {
        return Err(too_large(
            at,
            Bound::Aggregates,
            limits.aggregates,
            items.len(),
        ));
    }
    let mut out: Vec<Aggregate> = Vec::with_capacity(items.len());
    for (index, item) in items.iter().enumerate() {
        let item_at = at.index(index);
        let members = object(item, &item_at)?;
        only(members, &["func", "field"], &item_at)?;
        let func_at = item_at.key("func");
        let name = string(require(members, "func", &item_at)?, &func_at)?;
        if let Some(why) = refused_func(name) {
            return Err(err(
                &func_at,
                QueryAstReason::RefusedFeature {
                    what: name.to_owned(),
                    why,
                },
            ));
        }
        let func = AggregateFunc::parse(name)
            .ok_or_else(|| unknown_value(&func_at, "aggregate function", name, FUNCS))?;
        let field_at = item_at.key("field");
        let field = match get(members, "field") {
            Some(Json::Null) if func == AggregateFunc::Count => None,
            None if func == AggregateFunc::Count => None,
            None => return Err(err(&item_at, QueryAstReason::MissingKey("field"))),
            Some(json) => Some(FieldName::at(&field_at, string(json, &field_at)?)?),
        };
        let aggregate = match field {
            None => Aggregate::Count,
            Some(field) => Aggregate::Field { field, func },
        };
        let name = aggregate.spec_name();
        if out.iter().any(|a| a.spec_name() == name) {
            return Err(err(
                &item_at,
                QueryAstReason::Duplicate {
                    what: "aggregate",
                    value: name,
                },
            ));
        }
        out.push(aggregate);
    }
    Ok(out)
}

fn having(
    json: &Json,
    at: &JsonPath,
    aggregate_names: &[String],
    limits: &QueryLimits,
) -> Res<Vec<HavingCondition>> {
    let items = array(json, at)?;
    if items.len() > limits.having {
        return Err(too_large(at, Bound::Having, limits.having, items.len()));
    }
    let mut out = Vec::with_capacity(items.len());
    for (index, item) in items.iter().enumerate() {
        let item_at = at.index(index);
        let members = object(item, &item_at)?;
        only(members, &["aggregate", "op", "value"], &item_at)?;
        let agg_at = item_at.key("aggregate");
        let name = string(require(members, "aggregate", &item_at)?, &agg_at)?;
        if !aggregate_names.iter().any(|n| n == name) {
            return Err(err(&agg_at, QueryAstReason::DanglingReference(shown(name))));
        }
        let op_at = item_at.key("op");
        let op_text = string(require(members, "op", &item_at)?, &op_at)?;
        let op = HavingOp::parse(op_text)
            .ok_or_else(|| unknown_value(&op_at, "having operator", op_text, HAVING_OPS))?;
        let value_at = item_at.key("value");
        let value_json = require(members, "value", &item_at)?;
        let value = if op.takes_list() {
            let list = array(value_json, &value_at)?;
            let mut values = Vec::with_capacity(list.len());
            for (i, element) in list.iter().enumerate() {
                values.push(scalar_value(element, &value_at.index(i))?);
            }
            Value::List(values)
        } else {
            scalar_value(value_json, &value_at)?
        };
        out.push(HavingCondition {
            aggregate: name.into(),
            op,
            value,
        });
    }
    Ok(out)
}
