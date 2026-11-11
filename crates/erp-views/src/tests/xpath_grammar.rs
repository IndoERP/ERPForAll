//! The supported XPath grammar and its XPath 1.0 semantics; every other construct is an error.

use super::{arch, on_small_stack, select_all, xpath_error};
use crate::error::{LimitKind, ViewError, XpathErrorKind};

const FORM: &str = r#"<form><group><field name="f1"/><field name="f2"/></group><group><field name="f3"/></group></form>"#;

fn paths(xml: &str, expr: &str) -> Vec<Vec<usize>> {
    select_all(&arch(xml), expr).expect("expression is valid")
}

fn invalid(expr: &str, kind: XpathErrorKind) -> ViewError {
    ViewError::InvalidXpath {
        expr: expr.to_owned(),
        kind,
    }
}

#[test]
fn absolute_relative_and_descendant_steps() {
    assert_eq!(paths(FORM, "/form"), vec![Vec::<usize>::new()]);
    assert_eq!(paths(FORM, "/group"), Vec::<Vec<usize>>::new());
    assert_eq!(paths(FORM, "/form/group"), vec![vec![0], vec![1]]);
    assert_eq!(paths(FORM, "./group"), vec![vec![0], vec![1]]);
    assert_eq!(paths(FORM, "group"), vec![vec![0], vec![1]]);
    assert_eq!(paths(FORM, "./form"), Vec::<Vec<usize>>::new());
    assert_eq!(paths(FORM, "."), vec![Vec::<usize>::new()]);
    assert_eq!(paths(FORM, "//form"), vec![Vec::<usize>::new()]);
    assert_eq!(
        paths(FORM, ".//field"),
        vec![vec![0, 0], vec![0, 1], vec![1, 0]]
    );
    assert_eq!(
        paths(FORM, "/form//field"),
        vec![vec![0, 0], vec![0, 1], vec![1, 0]]
    );
    assert_eq!(paths(FORM, "/form/*"), vec![vec![0], vec![1]]);
    assert_eq!(
        paths(FORM, "//*"),
        vec![vec![], vec![0], vec![0, 0], vec![0, 1], vec![1], vec![1, 0]]
    );
    assert_eq!(paths(FORM, "group/field[@name='f3']"), vec![vec![1, 0]]);
}

#[test]
fn position_index_counts_per_parent() {
    assert_eq!(paths(FORM, "//field[1]"), vec![vec![0, 0], vec![1, 0]]);
    assert_eq!(paths(FORM, "//field[2]"), vec![vec![0, 1]]);
    assert_eq!(paths(FORM, "//group[2]/field[1]"), vec![vec![1, 0]]);
    assert_eq!(paths(FORM, "/form/group[3]"), Vec::<Vec<usize>>::new());
    // The root is the first `form` child of the document.
    assert_eq!(paths(FORM, "//form[1]"), vec![Vec::<usize>::new()]);
}

#[test]
fn predicates_filter_in_sequence() {
    let xml = r#"<r><d class="x"/><d/><d class="x"/></r>"#;
    assert_eq!(paths(xml, "//d[hasclass('x')][2]"), vec![vec![2]]);
    assert_eq!(
        paths(xml, "//d[2][hasclass('x')]"),
        Vec::<Vec<usize>>::new()
    );
    assert_eq!(paths(xml, "//d[2]"), vec![vec![1]]);
    assert_eq!(paths(xml, "//d[1][1]"), vec![vec![0]]);
    assert_eq!(paths(xml, "//d[1][2]"), Vec::<Vec<usize>>::new());
}

#[test]
fn descendant_after_a_step_excludes_the_step_node() {
    // `//a//b` must not select the outer `b`.
    assert_eq!(
        paths(r#"<b><a><b id="1"/></a></b>"#, "//a//b"),
        vec![vec![0, 0]]
    );
}

#[test]
fn nested_context_nodes_give_each_match_once_in_document_order() {
    // Both `a` elements are context nodes and the inner one is inside the outer one.
    let xml = "<r><a><a><b/></a><b/></a></r>";
    assert_eq!(paths(xml, "//a//b[1]"), vec![vec![0, 0, 0], vec![0, 1]]);
    assert_eq!(paths(xml, "//a//b"), vec![vec![0, 0, 0], vec![0, 1]]);
    assert_eq!(paths(xml, "//a/b"), vec![vec![0, 0, 0], vec![0, 1]]);
}

#[test]
fn boolean_operators_and_grouping() {
    let xml = r#"<r><d class="x" id="1"/><d class="x"/><d id="2"/><d/></r>"#;
    assert_eq!(paths(xml, "//d[@class and @id]"), vec![vec![0]]);
    assert_eq!(
        paths(xml, "//d[@class or @id]"),
        vec![vec![0], vec![1], vec![2]]
    );
    assert_eq!(paths(xml, "//d[not(@class) and not(@id)]"), vec![vec![3]]);
    assert_eq!(paths(xml, "//d[@class and (@id or @x)]"), vec![vec![0]]);
    assert_eq!(paths(xml, "//d[@class and(@id)]"), vec![vec![0]]);
    assert_eq!(
        paths(xml, "//d[(@id='2')or(@id='1')]"),
        vec![vec![0], vec![2]]
    );
    // `and` binds tighter than `or`
    assert_eq!(
        paths(xml, "//d[@id='2' or @class and @id]"),
        vec![vec![0], vec![2]]
    );
    assert_eq!(paths(xml, r#"//d[@id="2"]"#), vec![vec![2]]);
}

#[test]
fn text_test_is_true_when_any_text_node_child_equals() {
    // XPath 1.0 node-set comparison: the tail of <b/> is a text node child of the second <p>.
    let xml = "<r><p>hi</p><p><b/>hi</p><p> hi </p><p>hi<b/>there</p></r>";
    assert_eq!(
        paths(xml, "//p[text()='hi']"),
        vec![vec![0], vec![1], vec![3]]
    );
    assert_eq!(paths(xml, "//p[text()=' hi ']"), vec![vec![2]]);
    assert_eq!(paths(xml, "//p[text()='there']"), vec![vec![3]]);
    assert_eq!(paths(xml, "//p[text()='']"), Vec::<Vec<usize>>::new());
}

#[test]
fn non_ascii_names_and_values() {
    let xml = r#"<formulário><campo nome="ção"/></formulário>"#;
    assert_eq!(paths(xml, "//campo[@nome='ção']"), vec![vec![0]]);
}

#[test]
fn unsupported_constructs_are_named() {
    let cases: Vec<(&str, XpathErrorKind)> = vec![
        (
            "//field[@name!='a']",
            XpathErrorKind::Unsupported("the '!=' comparison"),
        ),
        (
            "//field[@sequence<2]",
            XpathErrorKind::Unsupported("comparison operators '<', '<=', '>', '>='"),
        ),
        (
            "//a | //b",
            XpathErrorKind::Unsupported("the '|' union operator"),
        ),
        (
            "//field/..",
            XpathErrorKind::Unsupported("the parent step '..'"),
        ),
        (
            "//field/parent::group",
            XpathErrorKind::Unsupported("axes ('::')"),
        ),
        (
            "//t:field",
            XpathErrorKind::Unsupported("namespace prefixes"),
        ),
        ("//field[$x]", XpathErrorKind::Unsupported("variables")),
        (
            "//field[1.5]",
            XpathErrorKind::Unsupported("decimal numbers"),
        ),
        (
            "//field[.5]",
            XpathErrorKind::Unsupported("decimal numbers"),
        ),
        (
            "//field[@a+1]",
            XpathErrorKind::Unsupported("arithmetic operators"),
        ),
        (
            "//field[contains(@name, 'f')]",
            XpathErrorKind::UnsupportedFunction("contains".to_owned()),
        ),
        (
            "//field[position()=1]",
            XpathErrorKind::UnsupportedFunction("position".to_owned()),
        ),
        (
            "//node()",
            XpathErrorKind::UnsupportedFunction("node".to_owned()),
        ),
        ("//field[@*]", XpathErrorKind::Unsupported("'@*'")),
        (
            "//field/@name",
            XpathErrorKind::Unsupported("selecting attributes as a step"),
        ),
        (
            "//field/.",
            XpathErrorKind::Unsupported("'.' after the first step"),
        ),
        (".[1]", XpathErrorKind::Unsupported("predicates on '.'")),
        (
            "//group[field]",
            XpathErrorKind::Unsupported("path expressions inside predicates"),
        ),
        (
            "//field['x'=@name]",
            XpathErrorKind::Unsupported("a string literal outside a comparison"),
        ),
        ("//field[0]", XpathErrorKind::InvalidIndex("0".to_owned())),
        (
            "//field[99999999999999999999999]",
            XpathErrorKind::InvalidIndex("99999999999999999999999".to_owned()),
        ),
        (
            "//field[1 and @a]",
            XpathErrorKind::PositionInsideExpression,
        ),
        (
            "//field[@a and 1]",
            XpathErrorKind::PositionInsideExpression,
        ),
        ("//field[not(1)]", XpathErrorKind::PositionInsideExpression),
        ("//field[@a or 2]", XpathErrorKind::PositionInsideExpression),
        (
            "//div[hasclass()]",
            XpathErrorKind::HasClassWithoutArguments,
        ),
        ("//field[@name='a]", XpathErrorKind::UnclosedString),
        ("//field[#]", XpathErrorKind::UnexpectedChar('#')),
        ("", XpathErrorKind::Empty),
        ("   ", XpathErrorKind::Empty),
        (
            "/",
            XpathErrorKind::UnexpectedEnd {
                expected: "a tag name or '*'",
            },
        ),
        (
            "//a/",
            XpathErrorKind::UnexpectedEnd {
                expected: "a tag name or '*'",
            },
        ),
        (
            "//a[@b=@c]",
            XpathErrorKind::UnexpectedToken {
                expected: "a quoted string",
                found: "@".to_owned(),
            },
        ),
        ("//a[@b", XpathErrorKind::UnexpectedEnd { expected: "']'" }),
        (
            "//a b",
            XpathErrorKind::UnexpectedToken {
                expected: "'/', '//' or '['",
                found: "b".to_owned(),
            },
        ),
        (
            "//a[not(@b]",
            XpathErrorKind::UnexpectedToken {
                expected: "')' closing not(",
                found: "]".to_owned(),
            },
        ),
    ];
    for (expr, kind) in cases {
        assert_eq!(
            xpath_error(expr),
            invalid(expr, kind),
            "expression {expr:?}"
        );
    }
    assert_eq!(
        xpath_error("//field[@name!='a']").to_string(),
        "Invalid Expression while parsing xpath “//field[@name!='a']”: the '!=' comparison is \
         not supported in view inheritance XPath"
    );
    assert_eq!(
        xpath_error("//field[1 and @a]").to_string(),
        "Invalid Expression while parsing xpath “//field[1 and @a]”: a position index is only \
         supported as a whole predicate such as [2], not inside and/or/not()"
    );
}

#[test]
fn nesting_limit_is_an_error() {
    let at_limit = format!("//a[{}@x{}]", "not(".repeat(32), ")".repeat(32));
    assert!(select_all(&arch("<a x='1'/>"), &at_limit).is_ok());
    let over = format!("//a[{}@x{}]", "not(".repeat(33), ")".repeat(33));
    assert_eq!(
        xpath_error(&over),
        ViewError::LimitExceeded {
            limit: LimitKind::XpathNesting,
            max: 32,
            actual: Some(33)
        }
    );
}

/// Review round 2, blocking 2: nested `(` / `not(` used to recurse once per level in the parser.
#[test]
fn deeply_nested_expressions_fail_cleanly_on_a_small_stack() {
    let parens = format!("//a[{}@x{}]", "(".repeat(2000), ")".repeat(2000));
    let nots = format!("//a[{}@x{}]", "not(".repeat(800), ")".repeat(800));
    let huge = format!("//a[{}@x{}]", "(".repeat(500_000), ")".repeat(500_000));
    let (e1, e2, e3) =
        on_small_stack(move || (xpath_error(&parens), xpath_error(&nots), xpath_error(&huge)));
    let nesting = ViewError::LimitExceeded {
        limit: LimitKind::XpathNesting,
        max: 32,
        actual: Some(33),
    };
    assert_eq!(e1, nesting);
    assert_eq!(e2, nesting);
    assert_eq!(
        e3,
        ViewError::LimitExceeded {
            limit: LimitKind::XpathLength,
            max: 4096,
            actual: Some(1_000_007)
        }
    );
    assert_eq!(
        e3.to_string(),
        "XPath expression length limit exceeded: maximum 4096, reached 1000007"
    );
}

#[test]
fn step_limit_is_an_error() {
    let ok = vec!["a"; 64].join("/");
    assert!(select_all(&arch("<a/>"), &ok).is_ok());
    let over = vec!["a"; 65].join("/");
    assert_eq!(
        xpath_error(&over),
        ViewError::LimitExceeded {
            limit: LimitKind::XpathSteps,
            max: 64,
            actual: Some(65)
        }
    );
}
