//! Fuzz smoke tests of the XML and expression front ends (design note §9): arbitrary input never
//! panics or overflows the stack, every problem is a diagnostic, and the result is deterministic.
//! A short property run per CI build; long fuzzing campaigns are not part of this slice.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use erp_ui_compiler::{
    AddonName, BuildMode, CompileOptions, DiagnosticKind, Severity, TemplatePath, TemplateSet,
    TemplateSource, analyze, compile,
};
use proptest::prelude::*;

fn set_of(xml: &str) -> TemplateSet {
    let mut set = TemplateSet::new();
    set.push(TemplateSource::addon_file(
        AddonName::new("web").unwrap(),
        TemplatePath::new("static/src/t.xml").unwrap(),
        xml,
    ));
    set
}

fn canonical(xml: &str) -> String {
    let options = CompileOptions::new(BuildMode::Dev);
    let mut out = String::new();
    analyze(&set_of(xml), &options).write_canonical(&mut out);
    match compile(&set_of(xml), &options) {
        Ok(c) => c.write_canonical(&mut out),
        Err(e) => e.write_canonical(&mut out),
    }
    out
}

fn xml_attr_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('"', "&quot;")
}

/// Characters that make XML and expression syntax interesting.
fn soup() -> impl Strategy<Value = String> {
    proptest::collection::vec(
        prop_oneof![
            Just("<"),
            Just(">"),
            Just("/"),
            Just("\""),
            Just("'"),
            Just("`"),
            Just("&"),
            Just(";"),
            Just("="),
            Just("t-if"),
            Just("t-foreach"),
            Just("t-out"),
            Just("t-on-click"),
            Just("<t "),
            Just("<div "),
            Just("</t>"),
            Just("</div>"),
            Just("<Foo "),
            Just("t-name=\"x\""),
            Just("${"),
            Just("}"),
            Just("{"),
            Just("("),
            Just(")"),
            Just("["),
            Just("]"),
            Just("this"),
            Just("."),
            Just("?."),
            Just("=>"),
            Just("and"),
            Just(" "),
            Just("\n"),
            Just("\u{a0}"),
            Just("&#160;"),
            Just("&#xD800;"),
            Just("<!--"),
            Just("-->"),
            Just("<![CDATA["),
            Just("]]>"),
            Just("a"),
            Just("1"),
            Just("__proto__"),
            Just("\u{2028}"),
        ],
        0..64,
    )
    .prop_map(|parts| parts.concat())
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 256, failure_persistence: None, ..ProptestConfig::default() })]

    /// Arbitrary bytes where a template file goes.
    #[test]
    fn arbitrary_files_are_diagnosed(body in soup()) {
        let xml = format!("<templates>{body}</templates>");
        let a = canonical(&xml);
        prop_assert_eq!(&a, &canonical(&xml));
    }

    /// Arbitrary text inside a template.
    #[test]
    fn arbitrary_template_content_is_diagnosed(body in soup()) {
        let xml = format!("<templates><t t-name=\"web.T\">{body}</t></templates>");
        let a = canonical(&xml);
        prop_assert_eq!(&a, &canonical(&xml));
    }

    /// Arbitrary expressions in every expression position.
    #[test]
    fn arbitrary_expressions_are_diagnosed(expr in soup(), any in "\\PC{0,48}") {
        for e in [expr.as_str(), any.as_str()] {
            let e = xml_attr_escape(e);
            let xml = format!(
                "<templates><t t-name=\"web.T\"><div t-att-x=\"{e}\" t-on-click=\"{e}\">\
                 <b t-if=\"{e}\" t-out=\"{e}\"/><Foo p=\"{e}\"/>\
                 <i t-foreach=\"{e}\" t-as=\"r\" t-key=\"{e}\"/></div></t></templates>"
            );
            let a = canonical(&xml);
            prop_assert_eq!(&a, &canonical(&xml));
        }
    }
}

/// Deep nesting is a limit error, never a stack overflow, at sizes far beyond the limits.
#[test]
fn deep_nesting_is_a_limit_not_a_crash() {
    for n in [100usize, 10_000, 200_000] {
        let expr = format!("{}1{}", "(".repeat(n), ")".repeat(n));
        let a = analyze(
            &set_of(&format!(
                "<templates><t t-name=\"web.T\"><b t-out=\"{expr}\"/></t></templates>"
            )),
            &CompileOptions::new(BuildMode::Prod),
        );
        let kinds: Vec<_> = a.templates()[0]
            .diagnostics()
            .iter()
            .filter(|d| d.severity() == Severity::Error)
            .map(|d| d.kind())
            .collect();
        assert!(
            !kinds.is_empty() && kinds.iter().all(|k| *k == DiagnosticKind::Limit),
            "{n}: {kinds:?}"
        );

        for prefix in ["!", "-", "typeof ", "a ? b : "] {
            let expr = format!("{}1", prefix.repeat(n.min(20_000)));
            let a = analyze(
                &set_of(&format!(
                    "<templates><t t-name=\"web.T\"><b t-out=\"{expr}\"/></t></templates>"
                )),
                &CompileOptions::new(BuildMode::Prod),
            );
            assert!(
                a.templates()[0]
                    .diagnostics()
                    .iter()
                    .any(|d| d.severity() == Severity::Error),
                "{prefix} x {n}"
            );
        }

        // The element depth limit is 64.
        let depth = n.max(300);
        let xml = format!(
            "<templates><t t-name=\"web.T\">{}{}</t></templates>",
            "<div>".repeat(depth),
            "</div>".repeat(depth)
        );
        let a = analyze(&set_of(&xml), &CompileOptions::new(BuildMode::Prod));
        let all: Vec<_> = a
            .set_diagnostics()
            .iter()
            .chain(a.templates().iter().flat_map(|t| t.diagnostics()))
            .map(|d| d.kind())
            .collect();
        assert!(all.contains(&DiagnosticKind::Limit), "{n}: {all:?}");
    }
}

/// The deepest template the limits accept compiles without overflowing a 2 MiB test-thread
/// stack in an unoptimised build: element nesting just under the depth limit (nested branches,
/// the most deeply nested generated code, which the re-parse check reads back), with an
/// expression just under the nesting limit at the bottom.
#[test]
fn deepest_accepted_nesting_compiles() {
    // `<templates>` and the template element are two of the 64 levels.
    let depth = 62;
    let mut body = String::new();
    for i in 0..depth {
        body.push_str(&format!("<div t-if=\"this.a{i}\">"));
    }
    let expr = format!("{}this.x{}", "(".repeat(60), ")".repeat(60));
    body.push_str(&format!("<t t-out=\"{expr}\"/>"));
    for _ in 0..depth {
        body.push_str("</div>");
    }
    let xml = format!("<templates><t t-name=\"web.T\">{body}</t></templates>");
    let outcome = compile(&set_of(&xml), &CompileOptions::new(BuildMode::Prod));
    assert!(
        outcome.is_ok(),
        "{:?}",
        outcome.err().map(|e| e.errors().to_vec())
    );
    // One level more is a limit error.
    let xml = format!("<templates><t t-name=\"web.T\"><b>{body}</b></t></templates>");
    let a = analyze(&set_of(&xml), &CompileOptions::new(BuildMode::Prod));
    assert!(
        a.set_diagnostics()
            .iter()
            .any(|d| d.kind() == DiagnosticKind::Limit),
        "{:?}",
        a.set_diagnostics()
    );
}

/// Runs `f` on a thread with a 2 MiB stack (the unoptimised test-thread size), so a test fails
/// if the deepest accepted input needs more.
fn on_small_stack(f: impl FnOnce() + Send + 'static) {
    std::thread::Builder::new()
        .stack_size(2 << 20)
        .spawn(f)
        .unwrap()
        .join()
        .unwrap();
}

/// Rust review M6: the other deep shapes the limits accept compile on a 2 MiB stack: a member
/// chain at the walk limit, a left-deep operator chain, and component/slot/loop nesting (the
/// largest recursive frames of code generation) down to the depth limit.
#[test]
fn other_deep_shapes_compile_on_a_small_stack() {
    on_small_stack(|| {
        let ok = |body: &str| {
            let xml = format!("<templates><t t-name=\"web.T\">{body}</t></templates>");
            let outcome = compile(&set_of(&xml), &CompileOptions::new(BuildMode::Prod));
            assert!(
                outcome.is_ok(),
                "{:?}",
                outcome.err().map(|e| e.errors().to_vec())
            );
        };
        // `this` and 255 members: 256 syntax-tree levels, the walk's maximum.
        ok(&format!("<b t-out=\"this{}\"/>", ".a".repeat(255)));
        // A left-deep sum of 120 terms.
        ok(&format!(
            "<b t-out=\"{}\"/>",
            vec!["this.a"; 120].join(" + ")
        ));
        // Round-two review L2: an arrow chain and an assignment chain at the `expression_nesting`
        // maximum (64 levels; in the handler the arrow and its braces take two of them).
        ok(&format!("<b t-out=\"{}this.a\"/>", "a => ".repeat(64)));
        ok(&format!(
            "<button t-on-click=\"() => {{ {}1 }}\"/>",
            "this.a = ".repeat(62)
        ));
        // Component > slot > loop, 20 times: 60 element levels under the template.
        let mut body = String::new();
        for i in 0..20 {
            body.push_str(&format!(
                "<Card><t t-set-slot=\"s\"><li t-foreach=\"this.l\" t-as=\"x{i}\" t-key=\"x{i}\">"
            ));
        }
        body.push_str("<b t-out=\"x19\"/>");
        for _ in 0..20 {
            body.push_str("</li></t></Card>");
        }
        ok(&body);
    });
}

/// Silent-failure review L8: random inheritance sets (templates, primary children and extensions,
/// valid and broken, in any order) never panic, and analysis is deterministic.
mod inheritance_smoke {
    use super::*;

    fn entry() -> impl Strategy<Value = String> {
        let name = prop_oneof![Just("web.A"), Just("web.B"), Just("web.C")];
        let xpath = prop_oneof![
            Just("<xpath expr=\"//div\" position=\"inside\"><i>x</i></xpath>"),
            Just("<xpath expr=\"//span\" position=\"replace\"/>"),
            Just(
                "<xpath expr=\"//div\" position=\"attributes\"><attribute name=\"class\">c</attribute></xpath>"
            ),
        ];
        prop_oneof![
            name.clone().prop_map(|n| format!("<t t-name=\"{n}\"><div><span/></div></t>")),
            (name.clone(), name.clone(), xpath.clone())
                .prop_map(|(n, p, x)| format!("<t t-name=\"{n}\" t-inherit=\"{p}\">{x}</t>")),
            (name.clone(), xpath.clone()).prop_map(|(p, x)| format!(
                "<t t-inherit=\"{p}\" t-inherit-mode=\"extension\">{x}</t>"
            )),
            (name.clone(), name, xpath).prop_map(|(n, p, x)| format!(
                "<t t-name=\"{n}\" t-inherit=\"{p}\" t-inherit-mode=\"extension\" class=\"y\">{x}</t>"
            )),
        ]
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

        #[test]
        fn inheritance_sets_never_panic(entries in proptest::collection::vec(entry(), 1..8)) {
            let xml = format!("<templates>{}</templates>", entries.concat());
            let first = canonical(&xml);
            prop_assert_eq!(first, canonical(&xml));
        }
    }
}
