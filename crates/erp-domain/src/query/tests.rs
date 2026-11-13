//! Tests of the query AST without a database. Error
//! tests assert the error kind (through `erp_fields::Error`) and the full message.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use std::path::PathBuf;

use erp_fields::ErrorKind;
use erp_fields::json::{self, Json};
use proptest::prelude::*;

use super::*;
use crate::{Domain, NowKind, PrincipalParam, Value};

// ----- helpers ---------------------------------------------------------------------------------

fn limits() -> QueryLimits {
    QueryLimits::default()
}

fn parse_json(text: &str) -> Json {
    json::parse(text.as_bytes(), 64).unwrap()
}

fn s(text: &str) -> Json {
    Json::Str(text.into())
}

fn parse_text(text: &str) -> Result<QueryAst, QueryAstError> {
    QueryAst::parse(text.as_bytes(), &limits())
}

fn ok(text: &str) -> QueryAst {
    match parse_text(text) {
        Ok(ast) => ast,
        Err(e) => panic!("{text} should parse, got {e}"),
    }
}

fn run(doc: &Json) -> Result<QueryAst, QueryAstError> {
    QueryAst::from_json(doc, &limits())
}

/// The message of the error for `doc`, after checking it is a `Validation` error of the shared
/// taxonomy with the AST error as its source.
fn fails(doc: &Json) -> String {
    let error = match run(doc) {
        Ok(_) => panic!(
            "{} should be refused",
            String::from_utf8_lossy(&json::canonical(doc))
        ),
        Err(e) => e,
    };
    message_of(error)
}

fn message_of(error: QueryAstError) -> String {
    let text = error.to_string();
    let shared = erp_fields::Error::from(error);
    assert_eq!(shared.kind(), ErrorKind::Validation, "{text}");
    assert_eq!(shared.message(), text);
    text
}

fn fails_text(text: &str) -> String {
    match parse_text(text) {
        Ok(_) => panic!("{text} should be refused"),
        Err(e) => message_of(e),
    }
}

/// One step into a document.
#[derive(Clone, Copy)]
enum P<'a> {
    K(&'a str),
    I(usize),
}

fn node<'a>(doc: &'a mut Json, path: &[P<'_>]) -> &'a mut Json {
    let mut current = doc;
    for step in path {
        current = match (step, current) {
            (P::K(key), Json::Object(members)) => {
                &mut members.iter_mut().find(|(k, _)| &**k == *key).unwrap().1
            }
            (P::I(index), Json::Array(items)) => &mut items[*index],
            _ => panic!("bad test path"),
        };
    }
    current
}

/// `doc` with member `key` of the object at `path` set to `value` (added when absent).
fn set(doc: &Json, path: &[P<'_>], key: &str, value: Json) -> Json {
    let mut doc = doc.clone();
    let Json::Object(members) = node(&mut doc, path) else {
        panic!("not an object");
    };
    match members.iter_mut().find(|(k, _)| &**k == key) {
        Some(slot) => slot.1 = value,
        None => members.push((key.into(), value)),
    }
    doc
}

fn del(doc: &Json, path: &[P<'_>], key: &str) -> Json {
    let mut doc = doc.clone();
    let Json::Object(members) = node(&mut doc, path) else {
        panic!("not an object");
    };
    members.retain(|(k, _)| &**k != key);
    doc
}

const SEARCH: &str = r#"{"version":1,"kind":"search_read","model":"account.move","domain":[["state","=","posted"]],"fields":["name","amount_total"],"limit":80}"#;
const GROUP: &str = r#"{"version":1,"kind":"read_group","model":"account.move","groupby":[{"path":"invoice_date","granularity":"month"}],"aggregates":[{"func":"count"},{"func":"sum","field":"amount_total"}],"limit":500}"#;

fn search() -> Json {
    parse_json(SEARCH)
}

fn group() -> Json {
    parse_json(GROUP)
}

/// Documents of both kinds, written in different styles; also the corpus of the fuzz smoke and the
/// canonical snapshots.
const CORPUS: &[(&str, &str)] = &[
    ("search_minimal", SEARCH),
    (
        "search_full",
        r#"{ "limit": 20, "offset": 40, "model": "res.partner", "kind": "search_read", "version": 1,
             "fields": ["name", "email", "country_id"], "context": {"active_test": false},
             "order": [{"field": "name", "dir": "desc", "nulls": "last"}, {"field": "id"}],
             "domain": ["|", ["name", "ilike", "acme"], "!", ["email", "=", false]] }"#,
    ),
    (
        "search_params",
        r#"{"version":1,"kind":"search_read","model":"sale.order","limit":100,
            "domain":[["user_id","=",{"param":"uid"}],["company_id","in",{"param":"company_ids"}],
                      ["date_order","<=",{"param":"now"}],["validity_date",">=",{"param":"today"}],
                      ["company_id","!=",{"param":"company_id"}]],
            "fields":["name"]}"#,
    ),
    (
        "search_any",
        r#"{"version":1,"kind":"search_read","model":"sale.order","limit":10,"fields":["name"],
            "domain":[["order_line","any",[["product_id.name","like","x"],["qty",">",1.5]]]]}"#,
    ),
    ("group_minimal", GROUP),
    (
        "group_full",
        r#"{"version":1,"kind":"read_group","model":"account.move","limit":10000,"offset":5,
            "domain":[["state","=","posted"]],"context":{"active_test":true},
            "groupby":[{"path":"invoice_date","granularity":"month"},{"path":"partner_id.country_id"}],
            "aggregates":[{"func":"count","field":null},{"func":"sum","field":"amount_total"},
                          {"func":"count_distinct","field":"partner_id"}],
            "having":[{"aggregate":"amount_total:sum","op":">","value":1000},
                      {"aggregate":"partner_id:count_distinct","op":"in","value":[1,2,3]}],
            "order":[{"key":"amount_total:sum","dir":"desc"},{"key":"invoice_date:month","nulls":"first"}]}"#,
    ),
    (
        "group_aggregates_only",
        r#"{"version":1,"kind":"read_group","model":"res.partner","limit":1,
            "aggregates":[{"func":"count"}]}"#,
    ),
    (
        "group_numbers",
        r#"{"version":1,"kind":"read_group","model":"res.partner","limit":9,
            "domain":[["credit_limit",">",1.0],["score","<",-0.0],["ratio","=",0.25],["n","=",null]],
            "groupby":[{"path":"create_date","granularity":"day_of_week"}]}"#,
    ),
];

fn snapshot_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/snapshots/qast")
        .join(format!("{name}.json"))
}

/// The committed file holds exactly what our code wrote; `ERP_BLESS=1` rewrites it (the same
/// convention as `erp_testkit::assert_snapshot`, which this pure crate does not depend on).
fn assert_snapshot(path: &std::path::Path, actual: &str) {
    if std::env::var("ERP_BLESS").is_ok_and(|v| v == "1") {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, actual).unwrap();
        return;
    }
    let expected = std::fs::read_to_string(path).unwrap_or_else(|e| {
        panic!(
            "snapshot {} unreadable ({e}); run with ERP_BLESS=1",
            path.display()
        )
    });
    assert_eq!(expected, actual, "snapshot {}", path.display());
}

fn canonical_text(ast: &QueryAst) -> String {
    String::from_utf8(ast.canonical()).unwrap()
}

fn spec_qast_001_canonical_round_trip() {
    for (name, text) in CORPUS {
        let ast = ok(text);
        let canonical = ast.canonical();
        // the canonical form parses back to an equal AST and writes the same bytes again
        let again = QueryAst::parse(&canonical, &limits()).unwrap();
        assert_eq!(again, ast, "{name}");
        assert_eq!(again.canonical(), canonical, "{name}");
        // keys sorted at every level, no whitespace outside strings
        assert!(
            !canonical.contains(&b' ') && !canonical.contains(&b'\n'),
            "{name}"
        );
        assert_sorted(&json::parse(&canonical, 64).unwrap(), name);
        // the canonical bytes are what the snapshot file holds
        assert_snapshot(&snapshot_path(name), &canonical_text(&ast));
    }
}

fn assert_sorted(json: &Json, name: &str) {
    match json {
        Json::Object(members) => {
            let keys: Vec<&str> = members.iter().map(|(k, _)| &**k).collect();
            let mut sorted = keys.clone();
            sorted.sort_unstable();
            assert_eq!(keys, sorted, "{name}");
            members.iter().for_each(|(_, v)| assert_sorted(v, name));
        }
        Json::Array(items) => items.iter().for_each(|v| assert_sorted(v, name)),
        _ => {}
    }
}

fn spec_qast_001_absent_keys_are_written_as_their_defaults() {
    // a document with every optional key absent and one that writes them all, in another key order
    let minimal = ok(SEARCH);
    let spelled = ok(
        r#"{"offset":0,"order":[],"context":{},"limit":80,"fields":["name","amount_total"],
            "domain":[["state","=","posted"]],"model":"account.move","kind":"search_read","version":1}"#,
    );
    assert_eq!(minimal.canonical(), spelled.canonical());
    let doc = json::parse(&minimal.canonical(), 64).unwrap();
    let Json::Object(members) = &doc else {
        panic!()
    };
    let get = |k: &str| &members.iter().find(|(key, _)| &**key == k).unwrap().1;
    assert_eq!(get("offset"), &Json::Int(0));
    assert_eq!(get("order"), &Json::Array(vec![]));
    assert_eq!(get("context"), &Json::Object(vec![]));
    // nulls and granularity absent forms
    let full = ok(r#"{"version":1,"kind":"read_group","model":"m","limit":1,
            "groupby":[{"path":"a"}],"aggregates":[{"func":"count"}],
            "order":[{"key":"a"}]}"#);
    let text = canonical_text(&full);
    assert!(text.contains(r#""granularity":null"#), "{text}");
    assert!(text.contains(r#""nulls":null"#), "{text}");
    assert!(text.contains(r#""dir":"asc""#), "{text}");
    assert!(text.contains(r#""having":[]"#), "{text}");
    assert!(text.contains(r#""field":null"#), "{text}");
    // two documents that differ only in key order or absent-versus-default keys are the same query
    let reordered = ok(
        r#"{"limit":1,"order":[{"nulls":null,"dir":"asc","key":"a"}],"aggregates":[{"field":null,"func":"count"}],
            "groupby":[{"granularity":null,"path":"a"}],"model":"m","kind":"read_group","version":1,"having":[]}"#,
    );
    assert_eq!(reordered, full);
}

fn spec_qast_001_domain_is_in_prefix_form_with_the_and_explicit() {
    let ast = ok(SEARCH);
    let two = ok(
        r#"{"version":1,"kind":"search_read","model":"m","fields":["a"],"limit":1,
            "domain":[["a","=",1],["b","=",2]]}"#,
    );
    let doc = json::parse(&two.canonical(), 64).unwrap();
    let Json::Object(members) = &doc else {
        panic!()
    };
    let Json::Array(items) = &members.iter().find(|(k, _)| &**k == "domain").unwrap().1 else {
        panic!()
    };
    assert_eq!(items.first(), Some(&s("&")), "implicit AND made explicit");
    assert_eq!(items.len(), 3);
    // the same domain written with the explicit operator is the same query
    let explicit = ok(
        r#"{"version":1,"kind":"search_read","model":"m","fields":["a"],"limit":1,
            "domain":["&",["a","=",1],["b","=",2]]}"#,
    );
    assert_eq!(two, explicit);
    // and the single-condition domain stays a single condition, `TRUE` is the empty list
    assert_eq!(ast.domain().to_list().len(), 1);
    let none = ok(r#"{"version":1,"kind":"search_read","model":"m","fields":["a"],"limit":1}"#);
    assert!(none.domain().is_true());
    assert!(canonical_text(&none).contains(r#""domain":[]"#));
}

fn spec_qast_001_numbers_read_back_identically() {
    let doc = |value: &str| {
        format!(
            r#"{{"version":1,"kind":"search_read","model":"m","fields":["a"],"limit":1,"domain":[["a","=",{value}]]}}"#
        )
    };
    // 1.0, -0.0 and 2^53-1 as a float are integers: JSON does not tell them apart from 1, 0, 2^53-1
    for (written, read) in [
        ("1.0", 1),
        ("-0.0", 0),
        ("9007199254740991.0", (1 << 53) - 1),
    ] {
        let ast = ok(&doc(written));
        let condition = ast.domain().as_condition().unwrap().value().clone();
        assert_eq!(condition, Value::Int(read), "{written}");
        assert_eq!(
            QueryAst::parse(&ast.canonical(), &limits()).unwrap(),
            ast,
            "{written}"
        );
    }
    let ast = ok(&doc("null"));
    assert!(canonical_text(&ast).contains("false"));
    assert_eq!(QueryAst::parse(&ast.canonical(), &limits()).unwrap(), ast);
    // fractions and exponent forms stay floats and survive the round trip
    for written in ["0.1", "-2.5e-8", "1e300", "1e21", "123456789012345.5"] {
        let ast = ok(&doc(written));
        let value = ast.domain().as_condition().unwrap().value().clone();
        assert!(matches!(value, Value::Float(_)), "{written}");
        assert_eq!(
            QueryAst::parse(&ast.canonical(), &limits()).unwrap(),
            ast,
            "{written}"
        );
    }
    // from 2^53 up an integral double may not be the integer its literal says: refused up to 1e21.
    // 2^53 and 2^53+1 parse to the same double, so both show 9007199254740992 in the message.
    for written in [
        "9007199254740992.0",
        "9007199254740993.0",
        "9007199254740994.0",
        "-9223372036854775808.0",
        "4611686018427387904.0",
    ] {
        // the text of the number is the double's own, as every domain error shows it
        let shown = written.parse::<f64>().unwrap().to_string();
        assert_eq!(
            fails_text(&doc(written)),
            format!("query AST $.domain[0][2]: unsupported value in domain: {shown}"),
            "{written}"
        );
    }
    // a fraction at that size is not integral and stays a float
    assert!(matches!(
        ok(&doc("2251799813685248.5"))
            .domain()
            .as_condition()
            .unwrap()
            .value(),
        Value::Float(_)
    ));
    assert_eq!(
        fails_text(&doc("1e20")),
        "query AST $.domain[0][2]: unsupported value in domain: 100000000000000000000"
    );
    assert_eq!(
        fails_text(&doc("9223372036854775808")),
        "query AST $.domain[0][2]: unsupported value in domain: 9223372036854775808"
    );
}

/// Found by the fuzz smoke: the canonical form writes the implicit AND of a long domain as
/// explicit `&` operators, so it has more list items than the document had. It must still parse
/// again under the limits that accepted the document, up to the bound on conditions.
fn spec_qast_001_a_canonical_form_with_many_conditions_parses_again() {
    let default = QueryLimits::default();
    for count in [2, 300, default.domain_items] {
        let doc = conditions(count);
        let ast = run(&doc).unwrap();
        let canonical = ast.canonical();
        let again = QueryAst::parse(&canonical, &limits()).unwrap();
        assert_eq!(again, ast, "{count} conditions");
        // the operators are in the canonical text, the conditions are what is bounded
        let text = String::from_utf8(canonical).unwrap();
        assert_eq!(text.matches(r#""&""#).count(), count - 1, "{count}");
    }
    // sub-domains of `any` count as well, and the same holds for them
    let any = |n: usize| {
        let Json::Array(items) = node(&mut conditions(n), &[P::K("domain")]).clone() else {
            panic!()
        };
        Json::Array(vec![Json::Array(vec![
            s("l"),
            s("any"),
            Json::Array(items),
        ])])
    };
    let ast = run(&set(&search(), &[], "domain", any(300))).unwrap();
    assert_eq!(QueryAst::parse(&ast.canonical(), &limits()).unwrap(), ast);
}

/// Part of case 003: a tree built in Rust (`from_json`, a builder) with a repeated key is refused
/// like the text is.
fn spec_qast_003_repeated_keys_in_a_built_tree() {
    let mut doc = search();
    let Json::Object(members) = &mut doc else {
        panic!()
    };
    members.push(("model".into(), s("other.model")));
    assert_eq!(fails(&doc), "query AST $: duplicate key 'model'");
    let nested = set(
        &search(),
        &[],
        "context",
        Json::Object(vec![
            ("active_test".into(), Json::Bool(true)),
            ("active_test".into(), Json::Bool(false)),
        ]),
    );
    assert_eq!(
        fails(&nested),
        "query AST $.context: duplicate key 'active_test'"
    );
    // a builder gets the same refusal for its domain tree
    let param =
        |name: &str| Json::Object(vec![("param".into(), s(name)), ("param".into(), s("uid"))]);
    let domain = Json::Array(vec![Json::Array(vec![s("a"), s("="), param("uid")])]);
    assert_eq!(
        message_of(
            QueryAst::search_read("m")
                .domain(domain)
                .field("a")
                .limit(1)
                .build(&limits())
                .unwrap_err()
        ),
        "query AST $.domain[0][2]: duplicate key 'param'"
    );
}

/// Part of case 001: the typed parts and the canonical tree are one query. Reading the canonical
/// form back agrees on every accessor, for the whole corpus.
fn spec_qast_001_typed_parts_agree_with_the_canonical_form() {
    for (name, text) in CORPUS {
        let ast = ok(text);
        let again = QueryAst::parse(&ast.canonical(), &limits()).unwrap();
        assert_eq!(again.model(), ast.model(), "{name}");
        assert_eq!(again.domain(), ast.domain(), "{name}");
        assert_eq!(again.context(), ast.context(), "{name}");
        assert_eq!(again.limit(), ast.limit(), "{name}");
        assert_eq!(again.offset(), ast.offset(), "{name}");
        assert_eq!(
            format!("{:?}", again.kind()),
            format!("{:?}", ast.kind()),
            "{name}"
        );
    }
}

#[test]
fn spec_qast_002_version() {
    assert_eq!(
        fails(&del(&search(), &[], "version")),
        "query AST $: missing key 'version'"
    );
    for found in [2i64, 0, -1, 1_000_000] {
        assert_eq!(
            fails(&set(&search(), &[], "version", Json::Int(found))),
            format!("query AST $.version: unsupported version {found}; supported: 1")
        );
    }
    assert_eq!(
        fails_text(&SEARCH.replace(r#""version":1"#, r#""version":99999999999999999999"#)),
        "query AST $.version: unsupported version 99999999999999999999; supported: 1"
    );
    for (value, found) in [
        (s("1"), "string"),
        (Json::Float(1.0), "number"),
        (Json::Null, "null"),
        (Json::Bool(true), "boolean"),
        (Json::Array(vec![]), "array"),
    ] {
        assert_eq!(
            fails(&set(&search(), &[], "version", value)),
            format!("query AST $.version: expected integer, found {found}")
        );
    }
}

fn spec_qast_003_unknown_and_duplicate_keys() {
    let x = Json::Int(1);
    let unknown = |doc: Json, at: &str| {
        assert_eq!(fails(&doc), format!("query AST {at}: unknown key 'extra'"));
    };
    unknown(set(&search(), &[], "extra", x.clone()), "$");
    unknown(set(&group(), &[], "extra", x.clone()), "$");
    let with_order = set(
        &search(),
        &[],
        "order",
        Json::Array(vec![Json::Object(vec![("field".into(), s("name"))])]),
    );
    unknown(
        set(&with_order, &[P::K("order"), P::I(0)], "extra", x.clone()),
        "$.order[0]",
    );
    unknown(
        set(&group(), &[P::K("groupby"), P::I(0)], "extra", x.clone()),
        "$.groupby[0]",
    );
    unknown(
        set(&group(), &[P::K("aggregates"), P::I(1)], "extra", x.clone()),
        "$.aggregates[1]",
    );
    let with_having = set(
        &group(),
        &[],
        "having",
        parse_json(r#"[{"aggregate":"amount_total:sum","op":">","value":1}]"#),
    );
    unknown(
        set(&with_having, &[P::K("having"), P::I(0)], "extra", x.clone()),
        "$.having[0]",
    );
    let with_param = set(
        &search(),
        &[],
        "domain",
        parse_json(r#"[["user_id","=",{"param":"uid"}]]"#),
    );
    unknown(
        set(
            &with_param,
            &[P::K("domain"), P::I(0), P::I(2)],
            "extra",
            x.clone(),
        ),
        "$.domain[0][2]",
    );
    // fields of the other kind are unknown too
    assert_eq!(
        fails(&set(&search(), &[], "groupby", Json::Array(vec![]))),
        "query AST $: unknown key 'groupby'"
    );
    assert_eq!(
        fails(&set(&group(), &[], "fields", Json::Array(vec![]))),
        "query AST $: unknown key 'fields'"
    );
    // a document cannot name who runs it
    for key in ["uid", "user_id", "principal", "sudo"] {
        let doc = set(&search(), &[], key, x.clone());
        assert_eq!(fails(&doc), format!("query AST $: unknown key '{key}'"));
    }
}

fn spec_qast_003_duplicate_keys_are_refused_by_the_parser() {
    let text =
        r#"{"version":1,"kind":"search_read","model":"m","model":"n","fields":["a"],"limit":1}"#;
    let detail = json::parse(text.as_bytes(), 32)
        .unwrap_err()
        .detail()
        .to_owned();
    assert!(detail.starts_with("duplicate key 'model'"), "{detail}");
    assert_eq!(fails_text(text), format!("query AST $: not JSON: {detail}"));
    // nested objects too
    let text = r#"{"version":1,"kind":"read_group","model":"m","limit":1,
        "aggregates":[{"func":"count","func":"sum"}]}"#;
    let detail = json::parse(text.as_bytes(), 32)
        .unwrap_err()
        .detail()
        .to_owned();
    assert!(detail.starts_with("duplicate key 'func'"), "{detail}");
    assert_eq!(fails_text(text), format!("query AST $: not JSON: {detail}"));
}

fn spec_qast_004_missing_keys_and_wrong_types() {
    for key in ["model", "kind", "limit"] {
        assert_eq!(
            fails(&del(&search(), &[], key)),
            format!("query AST $: missing key '{key}'")
        );
    }
    assert_eq!(
        fails(&del(&search(), &[], "fields")),
        "query AST $: missing key 'fields'"
    );
    assert_eq!(
        fails(&del(&group(), &[], "limit")),
        "query AST $: missing key 'limit'"
    );
    let cases: &[(&str, Json, &str)] = &[
        (
            "model",
            Json::Int(3),
            "query AST $.model: expected string, found integer",
        ),
        (
            "kind",
            Json::Null,
            "query AST $.kind: expected string, found null",
        ),
        (
            "limit",
            s("5"),
            "query AST $.limit: expected integer, found string",
        ),
        (
            "limit",
            Json::Float(5.0),
            "query AST $.limit: expected integer, found number",
        ),
        (
            "domain",
            Json::Null,
            "query AST $.domain: expected array, found null",
        ),
        (
            "domain",
            Json::Bool(true),
            "query AST $.domain: expected array, found boolean",
        ),
        (
            "context",
            Json::Array(vec![]),
            "query AST $.context: expected object, found array",
        ),
        (
            "fields",
            s("name"),
            "query AST $.fields: expected array, found string",
        ),
        (
            "order",
            Json::Object(vec![]),
            "query AST $.order: expected array, found object",
        ),
        (
            "offset",
            Json::Null,
            "query AST $.offset: expected integer, found null",
        ),
    ];
    for (key, value, message) in cases {
        assert_eq!(
            fails(&set(&search(), &[], key, value.clone())),
            *message,
            "{key}"
        );
    }
    assert_eq!(
        fails(&set(&search(), &[], "fields", parse_json(r#"["name", 5]"#))),
        "query AST $.fields[1]: expected string, found integer"
    );
    assert_eq!(
        fails(&set(&search(), &[], "kind", s("write"))),
        "query AST $.kind: unknown kind 'write'; expected one of search_read, read_group"
    );
    assert_eq!(
        fails(&parse_json("[]")),
        "query AST $: expected object, found array"
    );
}

fn spec_qast_004_empty_and_duplicate_entries() {
    assert_eq!(
        fails(&set(&search(), &[], "fields", Json::Array(vec![]))),
        "query AST $.fields: fields must be at least 1"
    );
    assert_eq!(
        fails(&set(
            &search(),
            &[],
            "fields",
            parse_json(r#"["name","id","name"]"#)
        )),
        "query AST $.fields[2]: duplicate field 'name'"
    );
    assert_eq!(
        fails(&set(
            &group(),
            &[],
            "groupby",
            parse_json(r#"[{"path":"a","granularity":"day"},{"path":"a","granularity":"day"}]"#)
        )),
        "query AST $.groupby[1]: duplicate group-by 'a:day'"
    );
    assert_eq!(
        fails(&set(
            &group(),
            &[],
            "aggregates",
            parse_json(r#"[{"func":"count"},{"func":"count","field":null}]"#)
        )),
        "query AST $.aggregates[1]: duplicate aggregate '__count'"
    );
    assert_eq!(
        fails(&set(
            &search(),
            &[],
            "order",
            parse_json(r#"[{"field":"name"},{"field":"name","dir":"desc"}]"#)
        )),
        "query AST $.order[1].field: duplicate order term 'name'"
    );
}

fn spec_qast_004_identifiers() {
    let model = |name: &str| fails(&set(&search(), &[], "model", s(name)));
    let long_model = "a".repeat(129);
    let cases = [
        (
            "Account.Move",
            "query AST $.model: invalid model name 'Account.Move': only a-z, 0-9, '_' and '.' are allowed",
        ),
        (
            "res..partner",
            "query AST $.model: invalid model name 'res..partner': it has an empty part",
        ),
        (
            "res partner",
            "query AST $.model: invalid model name 'res partner': only a-z, 0-9, '_' and '.' are allowed",
        ),
        (
            ".res",
            "query AST $.model: invalid model name '.res': it has an empty part",
        ),
        (
            "res.",
            "query AST $.model: invalid model name 'res.': it has an empty part",
        ),
        (
            "rés.partner",
            "query AST $.model: invalid model name 'rés.partner': only a-z, 0-9, '_' and '.' are allowed",
        ),
        (
            "res.partner\r\n",
            "query AST $.model: invalid model name 'res.partner\r\n': only a-z, 0-9, '_' and '.' are allowed",
        ),
        ("", "query AST $.model: invalid model name '': it is empty"),
    ];
    for (name, message) in cases {
        assert_eq!(model(name), message);
    }
    assert!(model(&long_model).ends_with("it is longer than 128 bytes"));
    // 128 bytes is accepted
    assert!(run(&set(&search(), &[], "model", s(&"a".repeat(128)))).is_ok());

    let field = |name: &str| fails(&set(&search(), &[], "fields", Json::Array(vec![s(name)])));
    let cases = [
        (
            "Name",
            "query AST $.fields[0]: invalid field name 'Name': it must match [a-z_][a-z0-9_]*",
        ),
        (
            "a.b",
            "query AST $.fields[0]: invalid field name 'a.b': it must match [a-z_][a-z0-9_]*",
        ),
        (
            "a b",
            "query AST $.fields[0]: invalid field name 'a b': it must match [a-z_][a-z0-9_]*",
        ),
        (
            "naïve",
            "query AST $.fields[0]: invalid field name 'naïve': it must match [a-z_][a-z0-9_]*",
        ),
        (
            "name\r\n",
            "query AST $.fields[0]: invalid field name 'name\r\n': it must match [a-z_][a-z0-9_]*",
        ),
        (
            "1st",
            "query AST $.fields[0]: invalid field name '1st': it must match [a-z_][a-z0-9_]*",
        ),
        (
            "__count",
            "query AST $.fields[0]: invalid field name '__count': names starting with '__' are reserved",
        ),
        (
            "",
            "query AST $.fields[0]: invalid field name '': it is empty",
        ),
    ];
    for (name, message) in cases {
        assert_eq!(field(name), message);
    }
    assert!(field(&"a".repeat(64)).ends_with("it is longer than 63 bytes"));
    assert!(
        run(&set(
            &search(),
            &[],
            "fields",
            Json::Array(vec![s(&"a".repeat(63))])
        ))
        .is_ok()
    );
    // a long name is cut in the message, not echoed whole
    let long = field(&"a".repeat(5000));
    assert!(long.len() < 300, "{}", long.len());
    assert!(long.contains('…'));
}

fn with_domain(domain: &str) -> Json {
    set(&search(), &[], "domain", parse_json(domain))
}

fn domain_parser_text(items: &str) -> String {
    let values: Vec<Value> = match Value::from_json(&serde_json::from_str(items).unwrap()).unwrap()
    {
        Value::List(items) => items,
        other => panic!("{other:?}"),
    };
    Domain::from_list(&values).unwrap_err().to_string()
}

#[test]
fn spec_qast_005_domain_errors() {
    // the domain parser's own text, behind the path of the domain
    for (items, at) in [
        (r#"["&", ["a","=",1]]"#, "$.domain"),
        (r#"[["create_date",">>","x"]]"#, "$.domain"),
        (r#"[["a","any!",[["b","=",1]]]]"#, "$.domain"),
        (r#"[5]"#, "$.domain"),
        (r#"[["a","=",1],"|"]"#, "$.domain"),
        (r#"[["","=",1]]"#, "$.domain"),
    ] {
        assert_eq!(
            fails(&with_domain(items)),
            format!("query AST {at}: {}", domain_parser_text(items)),
            "{items}"
        );
    }
    // a malformed sub-domain of `any` is refused where it stands
    assert_eq!(
        fails(&with_domain(r#"[["order_line","any",[["b",">>",1]]]]"#)),
        format!(
            "query AST $.domain[0][2]: {}",
            domain_parser_text(r#"[["b",">>",1]]"#)
        )
    );
    // an object that is not a parameter, in a value, a field position and a loose item
    for (items, at) in [
        (r#"[["a","=",{"x":1}]]"#, "$.domain[0][2]"),
        (r#"[["a","in",[1,{"x":1}]]]"#, "$.domain[0][2][1]"),
        (r#"[{"x":1}]"#, "$.domain[0]"),
        (r#"[[{"x":1},"=",1]]"#, "$.domain[0][0]"),
    ] {
        assert_eq!(
            fails(&with_domain(items)),
            format!("query AST {at}: unsupported value in domain: {{\"x\":1}}"),
            "{items}"
        );
    }
    // the kind is Validation and the source is the domain error
    let err = run(&with_domain(r#"[["a",">>",1]]"#)).unwrap_err();
    assert!(matches!(err.reason(), QueryAstReason::Domain(_)));
    assert!(std::error::Error::source(&err).is_some());
}

// ----- (partial) -----------------------------------------------------------------

/// `crates/erp-security/tests/qast_replay.rs`)
#[test]
fn spec_qast_006_principal_params() {
    let doc = r#"[["user_id","=",{"param":"uid"}],["company_id","in",{"param":"company_ids"}],
                  ["date","<=",{"param":"today"}],["stamp","<=",{"param":"now"}],
                  ["company_id","=",{"param":"company_id"}]]"#;
    let ast = run(&with_domain(doc)).unwrap();
    let values: Vec<Value> = ast
        .domain()
        .iter_conditions()
        .filter_map(Domain::as_condition)
        .map(|c| c.value().clone())
        .collect();
    assert_eq!(values.len(), 5);
    for expected in [
        Value::Param(PrincipalParam::Uid),
        Value::Param(PrincipalParam::CompanyIds),
        Value::Param(PrincipalParam::Now(NowKind::Date)),
        Value::Param(PrincipalParam::Now(NowKind::Timestamp)),
        Value::Param(PrincipalParam::CompanyId),
    ] {
        assert!(values.contains(&expected), "{expected:?}");
    }
    // the canonical form keeps the objects, and reads back as the same AST
    let canonical = canonical_text(&ast);
    for name in ["uid", "company_ids", "today", "now", "company_id"] {
        assert!(
            canonical.contains(&format!(r#"{{"param":"{name}"}}"#)),
            "{canonical}"
        );
    }
    assert_eq!(
        QueryAst::parse(ast.canonical().as_slice(), &limits()).unwrap(),
        ast
    );
    // a parameter inside a list value
    let listed = run(&with_domain(r#"[["user_id","in",[{"param":"uid"}, 0]]]"#)).unwrap();
    assert!(canonical_text(&listed).contains(r#"[{"param":"uid"},0]"#));

    let refused = |items: &str| fails(&with_domain(items));
    assert_eq!(
        refused(r#"[["user_id","=",{"param":"user_path"}]]"#),
        "query AST $.domain[0][2].param: unknown param 'user_path'; expected one of uid, company_id, company_ids, today, now"
    );
    assert_eq!(
        refused(r#"[["user_id","=",{"param":"UID"}]]"#),
        "query AST $.domain[0][2].param: unknown param 'UID'; expected one of uid, company_id, company_ids, today, now"
    );
    assert_eq!(
        refused(r#"[["user_id","=",{"param":"uid","as":"admin"}]]"#),
        "query AST $.domain[0][2]: unknown key 'as'"
    );
    assert_eq!(
        refused(r#"[["user_id","=",{"param":5}]]"#),
        "query AST $.domain[0][2].param: expected string, found integer"
    );
    assert_eq!(
        refused(r#"[["user_id",{"param":"uid"},1]]"#),
        "query AST $.domain[0][1]: a principal parameter here is not supported in a query AST: parameters stand only in the value of a condition"
    );
    assert_eq!(
        refused(r#"[[{"param":"uid"},"=",1]]"#),
        "query AST $.domain[0][0]: a principal parameter here is not supported in a query AST: parameters stand only in the value of a condition"
    );
    assert_eq!(
        refused(r#"[{"param":"uid"}]"#),
        "query AST $.domain[0]: a principal parameter here is not supported in a query AST: parameters stand only in the value of a condition"
    );
    // a parameter in the field position of a sub-domain is refused as well
    assert_eq!(
        refused(r#"[["line_ids","any",[[{"param":"uid"},"=",1]]]]"#),
        "query AST $.domain[0][2][0][0]: a principal parameter here is not supported in a query AST: parameters stand only in the value of a condition"
    );
    // but a parameter in the value of a sub-domain condition is fine
    assert!(
        run(&with_domain(
            r#"[["line_ids","any",[["user_id","=",{"param":"uid"}]]]]"#
        ))
        .is_ok()
    );
}

#[test]
fn spec_qast_007_context_allowlist() {
    let with_context = |context: &str| set(&search(), &[], "context", parse_json(context));
    for (written, expected) in [
        (r#"{"active_test":true}"#, Some(true)),
        (r#"{"active_test":false}"#, Some(false)),
        ("{}", None),
    ] {
        let ast = run(&with_context(written)).unwrap();
        assert_eq!(ast.context().active_test(), expected, "{written}");
        assert_eq!(QueryAst::parse(&ast.canonical(), &limits()).unwrap(), ast);
    }
    assert_eq!(run(&search()).unwrap().context().active_test(), None);
    assert_eq!(
        fails(&with_context(r#"{"active_test":"1"}"#)),
        "query AST $.context.active_test: expected boolean, found string"
    );
    assert_eq!(
        fails(&with_context(r#"{"active_test":null}"#)),
        "query AST $.context.active_test: expected boolean, found null"
    );
    let refused = |key: &str, why: &str| {
        (
            fails(&with_context(&format!(r#"{{"{key}":"x"}}"#))),
            format!(
                "query AST $.context.{key}: context key '{key}' is not stored in a query AST: {why}"
            ),
        )
    };
    for (key, why) in [
        ("lang", "it comes from the running environment"),
        ("tz", "it comes from the running environment"),
        (
            "allowed_company_ids",
            "the company scope is the running principal's",
        ),
        ("default_name", "not in the allowlist (active_test)"),
        ("group_by", "not in the allowlist (active_test)"),
        ("uid", "not in the allowlist (active_test)"),
        ("", "not in the allowlist (active_test)"),
    ] {
        let (got, want) = refused(key, why);
        assert_eq!(got, want);
    }
    // a valid key beside a refused one does not save the document
    assert_eq!(
        fails(&with_context(r#"{"active_test":true,"lang":"fr_FR"}"#)),
        "query AST $.context.lang: context key 'lang' is not stored in a query AST: it comes from the running environment"
    );
}

fn with_groupby(groupby: &str) -> Json {
    set(&group(), &[], "groupby", parse_json(groupby))
}

fn with_aggregates(aggregates: &str) -> Json {
    set(&group(), &[], "aggregates", parse_json(aggregates))
}

#[test]
fn spec_qast_008_group_by_and_aggregate_shapes() {
    // every granularity is accepted and named in the spec name
    for granularity in Granularity::ALL {
        let doc = with_groupby(&format!(
            r#"[{{"path":"create_date","granularity":"{}"}}]"#,
            granularity.name()
        ));
        let ast = run(&doc).unwrap();
        let QueryKind::ReadGroup { groupby, .. } = ast.kind() else {
            panic!()
        };
        assert_eq!(groupby[0].granularity(), Some(granularity));
        assert_eq!(
            groupby[0].spec_name(),
            format!("create_date:{}", granularity.name())
        );
        assert_eq!(QueryAst::parse(&ast.canonical(), &limits()).unwrap(), ast);
    }
    // the list in the message is the list of the enumeration
    let names: Vec<&str> = Granularity::ALL.iter().map(|g| g.name()).collect();
    assert_eq!(decode::GRANULARITIES, names.join(", "));
    assert_eq!(
        fails(&with_groupby(r#"[{"path":"d","granularity":"fortnight"}]"#)),
        format!(
            "query AST $.groupby[0].granularity: unknown granularity 'fortnight'; expected one of {}",
            names.join(", ")
        )
    );
    assert_eq!(
        fails(&with_groupby(r#"[{"path":"a.b.c.d.e"}]"#)),
        "query AST $.groupby[0].path: path segments is 5; at most 4 allowed"
    );
    assert!(run(&with_groupby(r#"[{"path":"a.b.c.d"}]"#)).is_ok());
    assert_eq!(
        fails(&with_groupby(r#"[{"path":"a..b"}]"#)),
        "query AST $.groupby[0].path: invalid field name '': it is empty"
    );
    assert_eq!(
        fails(&with_groupby(r#"[{"granularity":"day"}]"#)),
        "query AST $.groupby[0]: missing key 'path'"
    );
    for func in ["array_agg", "array_agg_distinct", "recordset"] {
        assert_eq!(
            fails(&with_aggregates(&format!(
                r#"[{{"func":"{func}","field":"a"}}]"#
            ))),
            format!(
                "query AST $.aggregates[0].func: {func} is not supported in a query AST: unbounded output per group"
            )
        );
    }
    assert_eq!(
        fails(&with_aggregates(r#"[{"func":"sum_currency","field":"a"}]"#)),
        "query AST $.aggregates[0].func: sum_currency is not supported in a query AST: needs res.currency rates (a later release)"
    );
    assert_eq!(
        fails(&with_aggregates(r#"[{"func":"median","field":"a"}]"#)),
        "query AST $.aggregates[0].func: unknown aggregate function 'median'; expected one of count, count_distinct, sum, avg, min, max, bool_and, bool_or"
    );
    // count with and without a field
    let ast = run(&with_aggregates(
        r#"[{"func":"count"},{"func":"count","field":"partner_id"}]"#,
    ))
    .unwrap();
    let QueryKind::ReadGroup { aggregates, .. } = ast.kind() else {
        panic!()
    };
    let names: Vec<String> = aggregates.iter().map(Aggregate::spec_name).collect();
    assert_eq!(names, ["__count", "partner_id:count"]);
    assert_eq!(
        fails(&with_aggregates(r#"[{"func":"sum"}]"#)),
        "query AST $.aggregates[0]: missing key 'field'"
    );
    assert_eq!(
        fails(&with_aggregates(r#"[{"func":"sum","field":null}]"#)),
        "query AST $.aggregates[0].field: expected string, found null"
    );
    assert_eq!(
        fails(&with_aggregates(r#"[{"field":"a"}]"#)),
        "query AST $.aggregates[0]: missing key 'func'"
    );
    // nothing to group
    let neither = del(&del(&group(), &[], "groupby"), &[], "aggregates");
    assert_eq!(
        fails(&neither),
        "query AST $: a read_group query needs groupby or aggregates"
    );
    let empty = set(
        &set(&group(), &[], "groupby", Json::Array(vec![])),
        &[],
        "aggregates",
        Json::Array(vec![]),
    );
    assert_eq!(
        fails(&empty),
        "query AST $: a read_group query needs groupby or aggregates"
    );
    // one of the two is enough
    assert!(run(&del(&group(), &[], "groupby")).is_ok());
    assert!(run(&del(&group(), &[], "aggregates")).is_ok());
}

#[test]
fn spec_qast_009_order_and_having_refer_to_declared_specs() {
    let base = set(
        &set(
            &group(),
            &[],
            "groupby",
            parse_json(r#"[{"path":"date","granularity":"month"}]"#),
        ),
        &[],
        "aggregates",
        parse_json(r#"[{"func":"count"},{"func":"sum","field":"amount"}]"#),
    );
    let with_order = |key: &str| {
        set(
            &base,
            &[],
            "order",
            parse_json(&format!(r#"[{{"key":"{key}"}}]"#)),
        )
    };
    let with_having = |name: &str, op: &str, value: &str| {
        set(
            &base,
            &[],
            "having",
            parse_json(&format!(
                r#"[{{"aggregate":"{name}","op":"{op}","value":{value}}}]"#
            )),
        )
    };
    // declared names are accepted, by spec name
    for key in ["date:month", "__count", "amount:sum"] {
        assert!(run(&with_order(key)).is_ok(), "{key}");
    }
    assert!(run(&with_having("amount:sum", ">", "8")).is_ok());
    assert!(run(&with_having("__count", "in", "[1,2]")).is_ok());
    // dangling references
    assert_eq!(
        fails(&with_order("amount:avg")),
        "query AST $.order[0].key: 'amount:avg' names no group-by or aggregate of this query"
    );
    assert_eq!(
        fails(&with_order("date")),
        "query AST $.order[0].key: 'date' names no group-by or aggregate of this query"
    );
    assert_eq!(
        fails(&with_having("amount:max", ">", "1")),
        "query AST $.having[0].aggregate: 'amount:max' names no group-by or aggregate of this query"
    );
    // having filters aggregates only, not group-bys
    assert_eq!(
        fails(&with_having("date:month", ">", "1")),
        "query AST $.having[0].aggregate: 'date:month' names no group-by or aggregate of this query"
    );
    assert_eq!(
        fails(&with_having("amount:sum", "like", "1")),
        "query AST $.having[0].op: unknown having operator 'like'; expected one of =, !=, <, >, <=, >=, in, not in"
    );
    assert_eq!(
        fails(&with_having("amount:sum", "in", "5")),
        "query AST $.having[0].value: expected array, found integer"
    );
    assert_eq!(
        fails(&with_having("amount:sum", "=", "[5]")),
        "query AST $.having[0].value: expected scalar value, found array"
    );
    assert_eq!(
        fails(&with_having("amount:sum", "in", "[[5]]")),
        "query AST $.having[0].value[0]: expected scalar value, found array"
    );
    assert_eq!(
        fails(&with_having("amount:sum", "=", r#"{"param":"uid"}"#)),
        "query AST $.having[0].value: expected scalar value, found object"
    );
    // directions and null placement
    assert_eq!(
        fails(&set(
            &base,
            &[],
            "order",
            parse_json(r#"[{"key":"__count","dir":"up"}]"#)
        )),
        "query AST $.order[0].dir: unknown order direction 'up'; expected one of asc, desc"
    );
    assert_eq!(
        fails(&set(
            &base,
            &[],
            "order",
            parse_json(r#"[{"key":"__count","nulls":"middle"}]"#)
        )),
        "query AST $.order[0].nulls: unknown nulls position 'middle'; expected one of first, last"
    );
    // the root carries no `lazy`
    assert_eq!(
        fails(&set(&base, &[], "lazy", Json::Bool(true))),
        "query AST $: unknown key 'lazy'"
    );
    // a duplicated order key is meaningless and refused
    assert_eq!(
        fails(&set(
            &base,
            &[],
            "order",
            parse_json(r#"[{"key":"__count"},{"key":"__count","dir":"desc"}]"#)
        )),
        "query AST $.order[1].key: duplicate order term '__count'"
    );
}

fn search_with_fields(count: usize) -> Json {
    let names: Vec<Json> = (0..count).map(|i| s(&format!("f{i}"))).collect();
    set(&search(), &[], "fields", Json::Array(names))
}

fn conditions(count: usize) -> Json {
    let items: Vec<Json> = (0..count)
        .map(|i| Json::Array(vec![s("a"), s("="), Json::Int(i64::try_from(i).unwrap())]))
        .collect();
    set(&search(), &[], "domain", Json::Array(items))
}

/// Nested `&`/`|` expressions in flat prefix form: a chain `k` levels deep has depth `k + 1`.
fn nested_domain(levels: usize) -> Json {
    let condition =
        |i: usize| Json::Array(vec![s("a"), s("="), Json::Int(i64::try_from(i).unwrap())]);
    let mut items = Vec::new();
    for level in 0..levels {
        items.push(s(if level % 2 == 0 { "&" } else { "|" }));
        items.push(condition(level));
    }
    items.push(condition(levels));
    set(&search(), &[], "domain", Json::Array(items))
}

/// `value` wrapped in `levels` arrays.
fn deep(levels: usize) -> Json {
    (0..levels).fold(Json::Int(1), |inner, _| Json::Array(vec![inner]))
}

fn spec_qast_010_bounds_at_and_past_the_limit() {
    let default = QueryLimits::default();
    // fields
    assert!(run(&search_with_fields(default.fields)).is_ok());
    assert_eq!(
        fails(&search_with_fields(default.fields + 1)),
        "query AST $.fields: fields is 129; at most 128 allowed"
    );
    // domain items (implicit AND adds no item)
    assert!(run(&conditions(default.domain_items)).is_ok());
    assert_eq!(
        fails(&conditions(default.domain_items + 1)),
        "query AST $.domain: domain items is 513; at most 512 allowed"
    );
    // sub-domains of `any` count toward the same bound
    let inner = |n: usize| {
        let Json::Array(items) = node(&mut conditions(n), &[P::K("domain")]).clone() else {
            panic!()
        };
        Json::Array(items)
    };
    let nested = set(
        &search(),
        &[],
        "domain",
        Json::Array(vec![
            Json::Array(vec![s("line_ids"), s("any"), inner(300)]),
            Json::Array(vec![s("a"), s("="), Json::Int(1)]),
        ]),
    );
    let total = 2 + 300;
    assert!(total <= default.domain_items);
    assert!(run(&nested).is_ok());
    let nested = set(
        &search(),
        &[],
        "domain",
        Json::Array(vec![
            Json::Array(vec![s("line_ids"), s("any"), inner(300)]),
            Json::Array(vec![s("x_ids"), s("any"), inner(300)]),
        ]),
    );
    assert_eq!(
        fails(&nested),
        "query AST $.domain: domain items is 602; at most 512 allowed"
    );
    // domain depth through the structure of the parsed domain
    let at_bound = nested_domain(default.domain_depth - 1);
    assert!(run(&at_bound).is_ok());
    assert_eq!(
        fails(&nested_domain(default.domain_depth)),
        "query AST $.domain: domain depth is 17; at most 16 allowed"
    );
    // order terms
    let terms = |n: usize| {
        let items: Vec<Json> = (0..n)
            .map(|i| Json::Object(vec![("field".into(), s(&format!("f{i}")))]))
            .collect();
        set(&search(), &[], "order", Json::Array(items))
    };
    assert!(run(&terms(default.order_terms)).is_ok());
    assert_eq!(
        fails(&terms(default.order_terms + 1)),
        "query AST $.order: order terms is 9; at most 8 allowed"
    );
    // group-bys, aggregates, having
    let groupbys = |n: usize| {
        let items: Vec<Json> = (0..n)
            .map(|i| Json::Object(vec![("path".into(), s(&format!("g{i}")))]))
            .collect();
        set(&group(), &[], "groupby", Json::Array(items))
    };
    assert!(run(&groupbys(default.groupby)).is_ok());
    assert_eq!(
        fails(&groupbys(default.groupby + 1)),
        "query AST $.groupby: group-bys is 7; at most 6 allowed"
    );
    let aggregates = |n: usize| {
        let items: Vec<Json> = (0..n)
            .map(|i| {
                Json::Object(vec![
                    ("func".into(), s("sum")),
                    ("field".into(), s(&format!("a{i}"))),
                ])
            })
            .collect();
        set(&group(), &[], "aggregates", Json::Array(items))
    };
    assert!(run(&aggregates(default.aggregates)).is_ok());
    assert_eq!(
        fails(&aggregates(default.aggregates + 1)),
        "query AST $.aggregates: aggregates is 33; at most 32 allowed"
    );
    let havings = |n: usize| {
        let items: Vec<Json> = (0..n)
            .map(|i| {
                parse_json(&format!(
                    r#"{{"aggregate":"amount_total:sum","op":">","value":{i}}}"#
                ))
            })
            .collect();
        set(&group(), &[], "having", Json::Array(items))
    };
    assert!(run(&havings(default.having)).is_ok());
    assert_eq!(
        fails(&havings(default.having + 1)),
        "query AST $.having: having conditions is 33; at most 32 allowed"
    );
}

fn spec_qast_010_page_bounds() {
    let limit = |n: i64| set(&search(), &[], "limit", Json::Int(n));
    assert!(run(&limit(1)).is_ok());
    assert!(run(&limit(2000)).is_ok());
    assert_eq!(
        fails(&limit(2001)),
        "query AST $.limit: limit is 2001; at most 2000 allowed"
    );
    assert_eq!(
        fails(&limit(0)),
        "query AST $.limit: limit must be at least 1"
    );
    assert_eq!(
        fails(&limit(-5)),
        "query AST $.limit: limit must be at least 1"
    );
    assert_eq!(
        fails(&limit(i64::MAX)),
        "query AST $.limit: limit is 9223372036854775807; at most 2000 allowed"
    );
    assert_eq!(
        fails_text(&SEARCH.replace(r#""limit":80"#, r#""limit":99999999999999999999"#)),
        "query AST $.limit: limit is 99999999999999999999; at most 2000 allowed"
    );
    assert_eq!(
        fails_text(&SEARCH.replace(r#""limit":80"#, r#""limit":-99999999999999999999"#)),
        "query AST $.limit: limit must be at least 1"
    );
    // groups have their own ceiling
    let groups = |n: i64| set(&group(), &[], "limit", Json::Int(n));
    assert!(run(&groups(10_000)).is_ok());
    assert_eq!(
        fails(&groups(10_001)),
        "query AST $.limit: limit is 10001; at most 10000 allowed"
    );
    // offset
    let offset = |n: i64| set(&search(), &[], "offset", Json::Int(n));
    assert!(run(&offset(0)).is_ok());
    assert!(run(&offset(1_000_000)).is_ok());
    assert_eq!(
        fails(&offset(1_000_001)),
        "query AST $.offset: offset is 1000001; at most 1000000 allowed"
    );
    assert_eq!(
        fails(&offset(-1)),
        "query AST $.offset: offset must be at least 0"
    );
    assert_eq!(
        fails(&offset(i64::MIN)),
        "query AST $.offset: offset must be at least 0"
    );
    assert_eq!(
        fails(&offset(i64::MAX)),
        "query AST $.offset: offset is 9223372036854775807; at most 1000000 allowed"
    );
}

fn spec_qast_010_input_size_and_depth() {
    let default = QueryLimits::default();
    // the input: exactly at the bound is accepted (trailing white space), one byte more is not
    let padded = |len: usize| {
        let mut text = SEARCH.to_owned();
        text.push_str(&" ".repeat(len - text.len()));
        text
    };
    assert!(parse_text(&padded(default.bytes)).is_ok());
    assert_eq!(
        fails_text(&padded(default.bytes + 1)),
        "query AST $: input bytes is 65537; at most 65536 allowed"
    );
    // a document whose canonical form (with the absent keys written) is past the bound
    let filler = "x".repeat(default.bytes - SEARCH.len() - 10);
    let near = SEARCH.replace(
        r#""domain":[["state","=","posted"]]"#,
        &format!(r#""domain":[["state","=","{filler}"]]"#),
    );
    assert!(near.len() <= default.bytes, "{}", near.len());
    // the canonical form adds `"context":{}`, `"offset":0` and `"order":[]` with their commas
    let added = r#","context":{},"offset":0,"order":[]"#.len();
    assert_eq!(
        fails_text(&near),
        format!(
            "query AST $: input bytes is {}; at most 65536 allowed",
            near.len() + added
        )
    );
    // a built query is checked the same way
    let big = Json::Array(vec![Json::Array(vec![
        s("a"),
        s("="),
        s(&"y".repeat(default.bytes)),
    ])]);
    let message = message_of(
        QueryAst::search_read("m")
            .domain(big)
            .field("a")
            .limit(1)
            .build(&default)
            .unwrap_err(),
    );
    assert!(
        message.starts_with("query AST $: input bytes is ")
            && message.ends_with("; at most 65536 allowed"),
        "{message}"
    );
    // nesting: depth 32 is accepted (root, domain, condition and 29 levels of list value)
    let at_depth = set(
        &search(),
        &[],
        "domain",
        Json::Array(vec![Json::Array(vec![
            s("a"),
            s("in"),
            deep(default.json_depth - 3),
        ])]),
    );
    let text = String::from_utf8(json::canonical(&at_depth)).unwrap();
    assert!(parse_text(&text).is_ok());
    let too_deep = set(
        &search(),
        &[],
        "domain",
        Json::Array(vec![Json::Array(vec![
            s("a"),
            s("in"),
            deep(default.json_depth - 2),
        ])]),
    );
    let text = String::from_utf8(json::canonical(&too_deep)).unwrap();
    let detail = json::parse(text.as_bytes(), default.json_depth)
        .unwrap_err()
        .detail()
        .to_owned();
    assert!(
        detail.starts_with("nesting deeper than 32 levels"),
        "{detail}"
    );
    assert_eq!(
        fails_text(&text),
        format!("query AST $: not JSON: {detail}")
    );
    // an already parsed tree is measured too
    assert_eq!(
        fails(&too_deep),
        "query AST $: JSON depth is 33; at most 32 allowed"
    );
    // 10,000 levels fail in the parser, which keeps its containers on a heap stack
    let bomb = format!("{}1{}", "[".repeat(10_000), "]".repeat(10_000));
    let message = fails_text(&bomb);
    assert!(
        message.starts_with("query AST $: not JSON: nesting deeper than 32 levels"),
        "{message}"
    );
    let bomb = format!("{}1{}", r#"{"a":"#.repeat(10_000), "}".repeat(10_000));
    assert!(fails_text(&bomb).starts_with("query AST $: not JSON: nesting deeper than 32 levels"));
}

fn spec_qast_010_page_of_a_built_or_paged_query() {
    let l = limits();
    let ast = QueryAst::search_read("res.partner")
        .field("name")
        .limit(10)
        .build(&l)
        .unwrap();
    assert_eq!(
        message_of(ast.with_page(2001, 0, &l).unwrap_err()),
        "query AST $.limit: limit is 2001; at most 2000 allowed"
    );
    assert_eq!(
        message_of(ast.with_page(0, 0, &l).unwrap_err()),
        "query AST $.limit: limit must be at least 1"
    );
    assert_eq!(
        message_of(ast.with_page(10, 1_000_001, &l).unwrap_err()),
        "query AST $.offset: offset is 1000001; at most 1000000 allowed"
    );
    let paged = ast.with_page(2000, 1_000_000, &l).unwrap();
    assert_eq!((paged.limit(), paged.offset()), (2000, 1_000_000));
    assert_eq!(paged.model(), ast.model());
    assert_eq!(paged.domain(), ast.domain());
    // a lowered bound applies to a page too
    let small = QueryLimits::builder().max_rows(50).build().unwrap();
    assert_eq!(
        message_of(ast.with_page(51, 0, &small).unwrap_err()),
        "query AST $.limit: limit is 51; at most 50 allowed"
    );
    // read_group pages count groups
    let grouped = QueryAst::read_group("res.partner")
        .count()
        .limit(10)
        .build(&l)
        .unwrap();
    assert!(grouped.with_page(10_000, 0, &l).is_ok());
    assert_eq!(
        message_of(grouped.with_page(10_001, 0, &l).unwrap_err()),
        "query AST $.limit: limit is 10001; at most 10000 allowed"
    );
    // a required limit
    assert_eq!(
        message_of(QueryAst::search_read("m").field("a").build(&l).unwrap_err()),
        "query AST $: missing key 'limit'"
    );
}

fn spec_qast_011_limits_can_only_be_lowered() {
    type Setter = fn(QueryLimitsBuilder, u64) -> QueryLimitsBuilder;
    let d = QueryLimits::default();
    let all: [(Bound, u64, Setter); 13] = [
        (Bound::Bytes, d.bytes as u64, |b, n| b.max_bytes(n as usize)),
        (Bound::JsonDepth, d.json_depth as u64, |b, n| {
            b.max_json_depth(n as usize)
        }),
        (Bound::DomainItems, d.domain_items as u64, |b, n| {
            b.max_domain_items(n as usize)
        }),
        (Bound::DomainDepth, d.domain_depth as u64, |b, n| {
            b.max_domain_depth(n as usize)
        }),
        (Bound::Fields, d.fields as u64, |b, n| {
            b.max_fields(n as usize)
        }),
        (Bound::GroupBy, d.groupby as u64, |b, n| {
            b.max_groupby(n as usize)
        }),
        (Bound::Aggregates, d.aggregates as u64, |b, n| {
            b.max_aggregates(n as usize)
        }),
        (Bound::OrderTerms, d.order_terms as u64, |b, n| {
            b.max_order_terms(n as usize)
        }),
        (Bound::Having, d.having as u64, |b, n| {
            b.max_having(n as usize)
        }),
        (Bound::PathSegments, d.path_segments as u64, |b, n| {
            b.max_path_segments(n as usize)
        }),
        (Bound::Rows, u64::from(d.rows), |b, n| b.max_rows(n as u32)),
        (Bound::Groups, u64::from(d.groups), |b, n| {
            b.max_groups(n as u32)
        }),
        (Bound::Offset, u64::from(d.offset), |b, n| {
            b.max_offset(n as u32)
        }),
    ];
    for (bound, default, setter) in all {
        // the default itself, and a lower value, are fine
        assert!(
            setter(QueryLimits::builder(), default).build().is_ok(),
            "{bound}"
        );
        assert!(setter(QueryLimits::builder(), 1).build().is_ok(), "{bound}");
        for found in [default + 1, 0] {
            let err = setter(QueryLimits::builder(), found).build().unwrap_err();
            // the calling code asked for it: a mistake of the host, not of a document
            assert_eq!(
                erp_fields::Error::from(err.clone()).kind(),
                ErrorKind::Programming
            );
            assert_eq!(
                err.to_string(),
                format!(
                    "query AST $: {bound} cannot be raised above {default} or set to 0 (got {found})"
                )
            );
        }
    }
    assert_eq!(
        QueryLimits::builder().build().unwrap(),
        QueryLimits::default()
    );
}

fn spec_qast_011_a_lowered_limit_is_enforced() {
    let lowered = QueryLimits::builder()
        .max_rows(100)
        .max_fields(2)
        .max_bytes(400)
        .build()
        .unwrap();
    let doc = |limit: u32, fields: &str| {
        format!(
            r#"{{"version":1,"kind":"search_read","model":"m","fields":{fields},"limit":{limit}}}"#
        )
    };
    assert!(QueryAst::parse(doc(100, r#"["a"]"#).as_bytes(), &lowered).is_ok());
    assert_eq!(
        message_of(QueryAst::parse(doc(101, r#"["a"]"#).as_bytes(), &lowered).unwrap_err()),
        "query AST $.limit: limit is 101; at most 100 allowed"
    );
    assert_eq!(
        message_of(QueryAst::parse(doc(1, r#"["a","b","c"]"#).as_bytes(), &lowered).unwrap_err()),
        "query AST $.fields: fields is 3; at most 2 allowed"
    );
    let long = doc(1, &format!(r#"["{}"]"#, "a".repeat(60)));
    let padded = format!("{long}{}", " ".repeat(400));
    assert_eq!(
        message_of(QueryAst::parse(padded.as_bytes(), &lowered).unwrap_err()),
        format!(
            "query AST $: input bytes is {}; at most 400 allowed",
            padded.len()
        )
    );
    // the lower domain depth applies to nested `any` sub-domains
    let shallow = QueryLimits::builder().max_domain_depth(1).build().unwrap();
    let any = |depth: usize| {
        let mut sub = Json::Array(vec![Json::Array(vec![s("a"), s("="), Json::Int(1)])]);
        for _ in 0..depth {
            sub = Json::Array(vec![Json::Array(vec![s("l"), s("any"), sub])]);
        }
        set(&search(), &[], "domain", sub)
    };
    assert!(QueryAst::from_json(&any(1), &shallow).is_ok());
    assert_eq!(
        message_of(QueryAst::from_json(&any(2), &shallow).unwrap_err()),
        "query AST $.domain[0][2]: domain depth is 2; at most 1 allowed"
    );
}

// ----- cases made of several checks ------------------------------------------------------------

#[test]
fn spec_qast_001_canonical_form() {
    spec_qast_001_canonical_round_trip();
    spec_qast_001_absent_keys_are_written_as_their_defaults();
    spec_qast_001_domain_is_in_prefix_form_with_the_and_explicit();
    spec_qast_001_numbers_read_back_identically();
    spec_qast_001_a_canonical_form_with_many_conditions_parses_again();
    spec_qast_001_typed_parts_agree_with_the_canonical_form();
}

#[test]
fn spec_qast_003_unknown_and_duplicate_keys_all() {
    spec_qast_003_unknown_and_duplicate_keys();
    spec_qast_003_duplicate_keys_are_refused_by_the_parser();
    spec_qast_003_repeated_keys_in_a_built_tree();
}

#[test]
fn spec_qast_004_shape_errors() {
    spec_qast_004_missing_keys_and_wrong_types();
    spec_qast_004_empty_and_duplicate_entries();
    spec_qast_004_identifiers();
}

#[test]
fn spec_qast_010_bounds() {
    spec_qast_010_bounds_at_and_past_the_limit();
    spec_qast_010_page_bounds();
    spec_qast_010_input_size_and_depth();
    spec_qast_010_page_of_a_built_or_paged_query();
}

#[test]
fn spec_qast_011_limits_lower_only() {
    spec_qast_011_limits_can_only_be_lowered();
    spec_qast_011_a_lowered_limit_is_enforced();
}

// ----- misc ------------------------------------------------------------------------------------

/// Not a spec case: the builders and the parser accept the same documents.
#[test]
fn builders_produce_what_the_parser_reads() {
    let l = limits();
    let built = QueryAst::search_read("account.move")
        .domain(parse_json(r#"[["state","=","posted"]]"#))
        .fields(["name", "amount_total"])
        .limit(80)
        .build(&l)
        .unwrap();
    assert_eq!(built, ok(SEARCH));
    let built = QueryAst::read_group("account.move")
        .group_by("invoice_date", Some(Granularity::Month))
        .count()
        .aggregate("amount_total", AggregateFunc::Sum)
        .having("amount_total:sum", HavingOp::Gt, Json::Int(1000))
        .order("amount_total:sum", OrderDir::Desc, Some(NullsPos::Last))
        .active_test(false)
        .offset(5)
        .limit(500)
        .build(&l)
        .unwrap();
    assert_eq!(QueryAst::parse(&built.canonical(), &l).unwrap(), built);
    assert_eq!(built.context().active_test(), Some(false));
    // a builder reports what the equivalent document reports
    assert_eq!(
        message_of(
            QueryAst::search_read("Bad Model")
                .field("a")
                .limit(1)
                .build(&l)
                .unwrap_err()
        ),
        "query AST $.model: invalid model name 'Bad Model': only a-z, 0-9, '_' and '.' are allowed"
    );
    assert_eq!(
        message_of(QueryAst::read_group("m").limit(1).build(&l).unwrap_err()),
        "query AST $: a read_group query needs groupby or aggregates"
    );
}

/// Not a spec case: the error converts to the shared taxonomy and keeps its source.
#[test]
fn errors_convert_to_validation_and_keep_the_source() {
    let err = parse_text("{").unwrap_err();
    assert!(matches!(err.reason(), QueryAstReason::Json(_)));
    let source = std::error::Error::source(&err).unwrap();
    assert!(err.to_string().ends_with(&source.to_string()));
    let shared = erp_fields::Error::from(err.clone());
    assert_eq!(shared.kind(), ErrorKind::Validation);
    assert_eq!(shared.message(), err.to_string());
    // invalid UTF-8 is a parser error as well
    let message = message_of(QueryAst::parse(&[0xff, 0xfe], &limits()).unwrap_err());
    assert_eq!(message, "query AST $: not JSON: invalid UTF-8 at byte 0");
}

/// What every input must satisfy: refused with a typed error, or accepted and then stable.
/// Whether the input was accepted.
fn check_input(bytes: &[u8]) -> bool {
    match QueryAst::parse(bytes, &limits()) {
        Err(error) => {
            // a typed error with a path and a message, never a panic
            assert!(error.at().as_str().starts_with('$'));
            assert!(!error.to_string().is_empty());
            false
        }
        Ok(ast) => {
            let canonical = ast.canonical();
            let again = QueryAst::parse(&canonical, &limits()).unwrap_or_else(|e| {
                panic!(
                    "canonical form refused: {e}\n{}",
                    String::from_utf8_lossy(&canonical)
                )
            });
            assert_eq!(again, ast);
            assert_eq!(again.canonical(), canonical);
            true
        }
    }
}

#[derive(Debug, Clone)]
enum Mutation {
    DropKey(u16),
    RenameKey(u16),
    DuplicateKey(u16),
    Replace(Json),
    Deepen(u8),
    Grow(u8),
}

fn nodes(json: &Json) -> usize {
    match json {
        Json::Array(items) => 1 + items.iter().map(nodes).sum::<usize>(),
        Json::Object(members) => 1 + members.iter().map(|(_, v)| nodes(v)).sum::<usize>(),
        _ => 1,
    }
}

/// Apply `mutation` to the `target`-th node (pre-order) of `json`.
fn mutate(json: &mut Json, target: &mut usize, mutation: &Mutation) -> bool {
    if *target == 0 {
        match (mutation, &mut *json) {
            (Mutation::Replace(value), slot) => *slot = value.clone(),
            (Mutation::Deepen(levels), slot) => {
                for _ in 0..*levels {
                    let inner = std::mem::replace(slot, Json::Null);
                    *slot = Json::Array(vec![inner]);
                }
            }
            (Mutation::Grow(extra), Json::Array(items)) => {
                if let Some(last) = items.last().cloned() {
                    items.extend(std::iter::repeat_n(last, usize::from(*extra)));
                }
            }
            (Mutation::DropKey(i), Json::Object(members)) if !members.is_empty() => {
                members.remove(usize::from(*i) % members.len());
            }
            (Mutation::RenameKey(i), Json::Object(members)) if !members.is_empty() => {
                let at = usize::from(*i) % members.len();
                members[at].0 = "zz".into();
            }
            (Mutation::DuplicateKey(i), Json::Object(members)) if !members.is_empty() => {
                let at = usize::from(*i) % members.len();
                members.push(members[at].clone());
            }
            _ => {}
        }
        return true;
    }
    *target -= 1;
    match json {
        Json::Array(items) => items.iter_mut().any(|item| mutate(item, target, mutation)),
        Json::Object(members) => members.iter_mut().any(|(_, v)| mutate(v, target, mutation)),
        _ => false,
    }
}

fn mutation() -> impl Strategy<Value = Mutation> {
    let replacements = vec![
        Json::Null,
        Json::Bool(true),
        Json::Int(-1),
        Json::Int(i64::MAX),
        Json::BigInt("99999999999999999999".into()),
        Json::Float(1.0),
        Json::Float(-0.0),
        Json::Float(1e20),
        Json::Float(1.5e300),
        Json::Str("".into()),
        Json::Str("a.b".into()),
        Json::Str("naïve\r\n".into()),
        Json::Str("in".into()),
        Json::Str("any".into()),
        Json::Array(vec![]),
        Json::Object(vec![]),
        Json::Object(vec![("param".into(), Json::Str("uid".into()))]),
    ];
    prop_oneof![
        any::<u16>().prop_map(Mutation::DropKey),
        any::<u16>().prop_map(Mutation::RenameKey),
        any::<u16>().prop_map(Mutation::DuplicateKey),
        (1u8..40).prop_map(Mutation::Deepen),
        (1u8..255).prop_map(Mutation::Grow),
        proptest::sample::select(replacements).prop_map(Mutation::Replace),
    ]
}

#[test]
fn spec_qast_023_fuzz_smoke() {
    use proptest::test_runner::{Config, RngAlgorithm, TestRng, TestRunner};

    // 4,096 cases of each of the three generators per run, from fixed seeds: a failure is a
    // failure of the code, reproducible on any machine (the cargo-fuzz target
    // explores with fresh randomness)
    let runner = |seed: u8| {
        let config = Config {
            cases: 4096,
            failure_persistence: None,
            ..Config::default()
        };
        TestRunner::new_with_rng(
            config,
            TestRng::from_seed(RngAlgorithm::ChaCha, &[seed; 32]),
        )
    };
    // random bytes
    runner(1)
        .run(&proptest::collection::vec(any::<u8>(), 0..512), |bytes| {
            check_input(&bytes);
            Ok(())
        })
        .unwrap();
    // random text, which reaches the JSON decoder more often than bytes do
    runner(2)
        .run(&"\\PC{0,400}", |text| {
            check_input(text.as_bytes());
            Ok(())
        })
        .unwrap();
    // structure-aware mutations of the corpus, and a flipped byte after them; the generator must
    // reach both outcomes, or the property proves nothing
    let accepted = std::cell::Cell::new(0u32);
    let refused = std::cell::Cell::new(0u32);
    let mutated = (
        0..CORPUS.len(),
        proptest::collection::vec((any::<u16>(), mutation()), 1..4),
        proptest::option::of((any::<u16>(), any::<u8>())),
    );
    runner(3)
        .run(&mutated, |(which, steps, flip)| {
            let mut doc = parse_json(CORPUS[which].1);
            for (seed, mutation) in &steps {
                let mut target = usize::from(*seed) % nodes(&doc);
                mutate(&mut doc, &mut target, mutation);
            }
            let mut bytes = json::canonical(&doc);
            if let Some((at, byte)) = flip
                && !bytes.is_empty()
            {
                let at = usize::from(at) % bytes.len();
                bytes[at] = byte;
            }
            let counter = if check_input(&bytes) {
                &accepted
            } else {
                &refused
            };
            counter.set(counter.get() + 1);
            Ok(())
        })
        .unwrap();
    assert!(
        accepted.get() > 200,
        "only {} mutations were accepted",
        accepted.get()
    );
    assert!(
        refused.get() > 200,
        "only {} mutations were refused",
        refused.get()
    );
    // and the corpus itself
    for (name, text) in CORPUS {
        check_input(text.as_bytes());
        assert!(parse_text(text).is_ok(), "{name}");
    }
}
