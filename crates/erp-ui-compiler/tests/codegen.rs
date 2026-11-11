//! Code generation through the public API: one module checked line by line against the §7.2 ABI
//! (written by hand), determinism, and the base injection set of design note §8 item 3.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use erp_ui_compiler::{
    AddonName, BuildMode, CompileOptions, DiagnosticKind, LimitKind, Severity, TemplatePath,
    TemplateSet, TemplateSource, analyze, compile,
};
use std::path::Path;

fn set_of(xml: &str) -> TemplateSet {
    let mut set = TemplateSet::new();
    set.push(TemplateSource::addon_file(
        AddonName::new("web").unwrap(),
        TemplatePath::new("static/src/t.xml").unwrap(),
        xml,
    ));
    set
}

fn wrap(body: &str) -> String {
    format!("<templates><t t-name=\"web.T\">{body}</t></templates>")
}

fn module_of(xml: &str) -> String {
    match compile(&set_of(xml), &CompileOptions::new(BuildMode::Prod)) {
        Ok(c) => c.modules()[0].code().to_owned(),
        Err(e) => panic!("compile failed: {:?}", e.errors()),
    }
}

fn error_kinds(xml: &str) -> Vec<DiagnosticKind> {
    let a = analyze(&set_of(xml), &CompileOptions::new(BuildMode::Prod));
    a.set_diagnostics()
        .iter()
        .chain(a.templates().iter().flat_map(|t| t.diagnostics()))
        .filter(|d| d.severity() == Severity::Error)
        .map(|d| d.kind())
        .collect()
}

/// The whole module of a small template, written by hand from the §7.2 ABI: one static
/// template, a walk to the dynamic positions, an effect per binding, an insert per output.
#[test]
fn a_small_template_compiles_to_the_documented_module() {
    // The import specifier is one constant of the crate (naming note); the module below is
    // written by hand with it.
    assert_eq!(erp_ui_compiler::ABI_MODULE, "@erpforall/elang/abi");
    let code = module_of(&wrap(
        "<p class=\"x\" t-att-title=\"this.title\">Hi <b t-out=\"this.name\"/>!</p>",
    ));
    let expected = concat!(
        "/*! SPDX-License-Identifier: MIT OR Apache-2.0 */\n",
        "import * as $ from \"@erpforall/elang/abi\";\n",
        "const $t0 = $.template(\"\\u003cp class=\\\"x\\\"\\u003eHi \\u003cb\\u003e\\u003c\\/b\\u003e!\\u003c\\/p\\u003e\", 0);\n",
        "export const name = \"web.T\";\n",
        "export function render($self) {\n",
        "  const $n0 = $t0();\n",
        "  $.effect(() => $.attr($n0, \"title\", $self.title));\n",
        "  const $n1 = $n0.firstChild.nextSibling;\n",
        "  $.insert($n1, () => $self.name, null);\n",
        "  return $n0;\n",
        "}\n",
    );
    assert_eq!(code, expected);
}

/// The same set gives the same bytes every time, also through the canonical form.
#[test]
fn compilation_is_deterministic() {
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let mut set = TemplateSet::new();
    let web = AddonName::new("fixtures").unwrap();
    for name in ["basic.xml", "inherit.xml"] {
        let xml = std::fs::read_to_string(fixtures.join(name)).unwrap();
        set.push(TemplateSource::addon_file(
            web.clone(),
            TemplatePath::new(name).unwrap(),
            xml,
        ));
    }
    let options = CompileOptions::new(BuildMode::Prod);
    let first = compile(&set, &options).unwrap();
    let mut a = String::new();
    first.write_canonical(&mut a);
    for _ in 0..3 {
        let mut b = String::new();
        compile(&set, &options).unwrap().write_canonical(&mut b);
        assert_eq!(a, b);
    }
    assert!(a.starts_with("erpforall-ui-compiled 1\nmodules "));
}

/// Design note §8 item 3, base set: data in every string position stays data.
#[test]
fn injection_base_set() {
    // Text and attribute values: escaped into JSON strings, no raw `<`, `</script`, U+2028/9.
    let code = module_of(&wrap(
        "<div title=\"&quot;);alert(1);(&quot;\" data-x=\"&lt;/script&gt;\">\
         &lt;/script&gt;&lt;script&gt;alert(1)&lt;/script&gt; \u{2028}\u{2029} `${this}` &lt;!-- --&gt;\
         </div>",
    ));
    for bad in [
        "</script",
        "<script",
        "<!--",
        "\u{2028}",
        "\u{2029}",
        "\");alert",
    ] {
        assert!(!code.contains(bad), "{bad:?} in {code}");
    }
    // Expression breakouts do not parse, or are refused.
    for expr in [
        "this.a); alert(1); (this.b",
        "this.a // c",
        "this.a /* c */",
        "`${this.a}` + `",
    ] {
        let kinds = error_kinds(&wrap(&format!("<b t-out=\"{expr}\"/>")));
        assert!(
            kinds.iter().any(|k| matches!(
                k,
                DiagnosticKind::ExpressionSyntax | DiagnosticKind::ForbiddenSyntax
            )),
            "{expr:?}: {kinds:?}"
        );
    }
    // A line separator in an expression is JavaScript whitespace: the expression is re-printed,
    // so it does not reach the module.
    let code = module_of(&wrap("<b t-out=\"this.a\u{2028}+ 1\"/>"));
    assert!(!code.contains('\u{2028}'), "{code}");
    // Names that would become object keys or code.
    for (body, kind) in [
        ("<Foo __proto__=\"1\"/>", DiagnosticKind::InvalidName),
        (
            "<Card><t t-set-slot=\"__proto__\">x</t></Card>",
            DiagnosticKind::InvalidName,
        ),
        (
            "<li t-foreach=\"this.l\" t-as=\"__proto__\" t-key=\"1\"/>",
            DiagnosticKind::InvalidName,
        ),
        (
            "<b t-out=\"({__proto__: this.a})\"/>",
            DiagnosticKind::ForbiddenSyntax,
        ),
        (
            "<button t-on-clïck=\"this.go\"/>",
            DiagnosticKind::InvalidName,
        ),
        (
            "<b t-att-on\u{e9}=\"this.a\"/>",
            DiagnosticKind::InvalidName,
        ),
    ] {
        let kinds = error_kinds(&wrap(body));
        assert!(kinds.contains(&kind), "{body}: {kinds:?}");
    }
    // `constructor` is an ordinary member and prop name: emitted as a member read and a string key.
    let code = module_of(&wrap("<Foo constructor=\"this.constructor\"/>"));
    assert!(
        code.contains("\"constructor\": () => $self.constructor"),
        "{code}"
    );
    // Quotes in an XML attribute name cannot reach us: the XML is refused first.
    assert_eq!(
        error_kinds("<templates><t t-name=\"web.T\"><b x\" onload=\"alert(1)\"/></t></templates>"),
        [DiagnosticKind::Xml]
    );
}

/// Rust review (test quality): the walk to dynamic positions. A static sibling between two
/// referenced ones is passed with `.nextSibling`, text and static elements are not referenced, the
/// last child of an element is appended (`null` marker), and a leading marker of a multi-root
/// block is walked to.
#[test]
fn walks_reach_every_dynamic_position() {
    let code = module_of(&wrap(
        "<div><b t-out=\"this.a\"/><i>x</i><span t-att-title=\"this.t\"/>text<u t-out=\"this.b\"/></div>",
    ));
    let expected_body = concat!(
        "  const $n0 = $t0();\n",
        "  const $n1 = $n0.firstChild;\n",
        "  $.insert($n1, () => $self.a, null);\n",
        "  const $n2 = $n1.nextSibling.nextSibling;\n",
        "  $.effect(() => $.attr($n2, \"title\", $self.t));\n",
        "  const $n3 = $n2.nextSibling.nextSibling;\n",
        "  $.insert($n3, () => $self.b, null);\n",
        "  return $n0;\n",
    );
    assert!(code.contains(expected_body), "{code}");
    let code = module_of(&wrap("<t t-out=\"this.a\"/><p>x</p>"));
    assert!(
        code.contains(concat!(
            "  const $n0 = $t0();\n",
            "  const $n1 = $n0.firstChild;\n",
            "  $.insert($n0, () => $self.a, $n1);\n",
        )),
        "{code}"
    );
    assert!(code.contains("\\u003c!\\u003e\\u003cp\\u003ex"), "{code}");
    // Adjacent texts (here through a plain <t>) are one text node.
    let code = module_of(&wrap("<p>a<t>b</t>c</p>"));
    assert!(
        code.contains("\\u003cp\\u003eabc\\u003c\\/p\\u003e"),
        "{code}"
    );
}

fn siblings(n: usize) -> String {
    wrap(&format!(
        "<div>{}<b t-out=\"this.a\"/><i/></div>",
        "<i/>".repeat(n)
    ))
}

/// Security review S4: a position after many static siblings is reached in hops of at most
/// 16 `.nextSibling`, each hop a named node, so no generated expression is deep.
#[test]
fn long_sibling_runs_walk_in_short_hops() {
    let code = module_of(&siblings(20_000));
    let longest = code
        .lines()
        .map(|l| l.matches(".nextSibling").count())
        .max()
        .unwrap_or(0);
    assert_eq!(longest, 16, "longest chain");
    assert!(
        code.contains("$.insert($n1250, () => $self.a, null);"),
        "hops"
    );
}

/// Security review S4: a template whose module would pass `module_bytes` stops being generated
/// once it does, and fails with the typed limit.
#[test]
fn huge_sibling_runs_stop_at_the_module_limit() {
    // Keep the sibling run within provenance admission, then lower only the output limit.
    // Otherwise a larger input can correctly hit the provenance budget before codegen runs.
    let xml = siblings(20_000);
    let module_limit = module_of(&xml).len() / 2;
    let limits = erp_ui_compiler::CompilerLimits::default()
        .lowered(LimitKind::ModuleBytes, module_limit)
        .unwrap();
    let err = compile(
        &set_of(&xml),
        &CompileOptions::new(BuildMode::Prod).with_limits(limits),
    )
    .unwrap_err();
    assert_eq!(err.errors().len(), 1, "{:?}", err.errors());
    let diagnostic = &err.errors()[0];
    assert_eq!(diagnostic.kind(), DiagnosticKind::Limit);
    let hit = diagnostic.limit().unwrap();
    assert_eq!(hit.kind(), LimitKind::ModuleBytes);
    assert_eq!(hit.limit(), module_limit);
    assert!(hit.actual() > hit.limit());
    assert_eq!(
        diagnostic.message(),
        format!(
            "module_bytes limit exceeded: maximum {module_limit}, got {} (generation stopped once the module passed the limit)",
            hit.actual()
        )
    );
}

/// Security review S4: a t-if chain is a flat list of `if` statements, so thousands of branches
/// keep the module shallow (and it passes the module check inside `compile`).
#[test]
fn long_branch_chains_stay_flat() {
    let mut body = String::from("<div><b t-if=\"this.a\"/>");
    for _ in 0..5000 {
        body.push_str("<b t-elif=\"this.a\"/>");
    }
    body.push_str("<b t-else=\"\"/></div>");
    let code = module_of(&wrap(&body));
    assert!(
        code.contains("() => { if ($self.a) return 0; if ($self.a) return 1;"),
        "{}",
        &code[..code.len().min(2000)]
    );
    assert!(code.contains(" return 5001; }, ["), "else branch");
}

/// A template that reads a `t-set` local compiles and emits local variable assignment.
#[test]
fn t_set_reads_compile_successfully() {
    let xml = wrap("<t t-set=\"v\" t-value=\"1\"/><b t-out=\"v\"/><i t-att-title=\"v\"/>");
    let code = module_of(&xml);
    assert!(code.contains("const $v0 = 1;"), "{code}");
    assert!(code.contains("$.insert("), "{code}");
    assert!(code.contains("$.effect(() => $.attr("), "{code}");
}

/// Round-two review L1: a module is charged once for its bytes, also when blocks nest, so a
/// nested template fits a `module_bytes` limit equal to its own size.
#[test]
fn nested_blocks_are_charged_once() {
    let mut body = String::new();
    for i in 0..12 {
        body.push_str(&format!(
            "<div t-if=\"this.c{i}\"><span t-out=\"this.v{i}\"/>"
        ));
    }
    for _ in 0..12 {
        body.push_str("</div>");
    }
    let xml = wrap(&body);
    let size = module_of(&xml).len();
    let limits = erp_ui_compiler::CompilerLimits::default()
        .lowered(LimitKind::ModuleBytes, size)
        .unwrap();
    let fits = compile(
        &set_of(&xml),
        &CompileOptions::new(BuildMode::Prod).with_limits(limits),
    );
    assert!(
        fits.is_ok(),
        "{:?}",
        fits.err().map(|e| e.errors().to_vec())
    );
    let limits = erp_ui_compiler::CompilerLimits::default()
        .lowered(LimitKind::ModuleBytes, size / 2)
        .unwrap();
    let Err(over) = compile(
        &set_of(&xml),
        &CompileOptions::new(BuildMode::Prod).with_limits(limits),
    ) else {
        panic!("half the module size must not fit");
    };
    assert!(
        over.errors().iter().any(|d| d
            .limit()
            .is_some_and(|l| l.kind() == LimitKind::ModuleBytes)),
        "{:?}",
        over.errors()
    );
}

/// Static call cycle without an intervening `t-if` guard is detected and rejected.
#[test]
fn static_call_cycle_detection() {
    let xml = "<templates>\
        <t t-name=\"web.A\"><t t-call=\"web.B\"/></t>\
        <t t-name=\"web.B\"><t t-call=\"web.A\"/></t>\
        </templates>";
    let mut set = TemplateSet::new();
    set.push(TemplateSource::addon_file(
        AddonName::new("web").unwrap(),
        TemplatePath::new("static/src/t.xml").unwrap(),
        xml,
    ));
    let err = compile(&set, &CompileOptions::new(BuildMode::Prod)).unwrap_err();
    let cycles: Vec<_> = err
        .errors()
        .iter()
        .filter(|d| d.kind() == DiagnosticKind::Cycle)
        .collect();
    assert!(!cycles.is_empty(), "{:?}", err.errors());
    assert!(
        cycles[0].message().contains("static call cycle detected"),
        "{}",
        cycles[0].message()
    );

    // With a t-if guard, the cycle check allows recursive templates
    let guarded = "<templates>\
        <t t-name=\"web.Tree\">\
            <div>\
                <t t-if=\"this.hasChildren\">\
                    <t t-call=\"web.Tree\"/>\
                </t>\
            </div>\
        </t>\
        </templates>";
    let mut set_guarded = TemplateSet::new();
    set_guarded.push(TemplateSource::addon_file(
        AddonName::new("web").unwrap(),
        TemplatePath::new("static/src/t.xml").unwrap(),
        guarded,
    ));
    let compiled = compile(&set_guarded, &CompileOptions::new(BuildMode::Prod));
    assert!(compiled.is_ok(), "{:?}", compiled.err());
}

/// t-call renders sub-templates with caller scope and callee body 0.
#[test]
fn t_call_scope_and_caller_body() {
    let xml = "<templates>\
        <t t-name=\"web.Callee\"><div><t t-out=\"0\"/></div></t>\
        <t t-name=\"web.Caller\"><t t-call=\"web.Callee\"><p>Injected</p></t></t>\
        </templates>";
    let mut set = TemplateSet::new();
    set.push(TemplateSource::addon_file(
        AddonName::new("web").unwrap(),
        TemplatePath::new("static/src/t.xml").unwrap(),
        xml,
    ));
    let c = compile(&set, &CompileOptions::new(BuildMode::Prod)).unwrap();
    let callee_code = c.modules()[0].code();
    assert!(
        callee_code.contains("$.out($n0, () => ($b ? $b($self, $c) : null), null, () => null);"),
        "{callee_code}"
    );
    assert!(
        callee_code.contains(
            "if ($d > 64) { throw Error(\"t-call depth limit (64) exceeded: web.Callee\"); }"
        ),
        "{callee_code}"
    );
    let caller_code = c.modules()[1].code();
    assert!(caller_code.contains("$.keyed("), "{caller_code}");
}

/// t-portal projects rendered content to target container via document.querySelector.
#[test]
fn t_portal_projects_content() {
    let code = module_of(&wrap("<div t-portal=\"'body'\"><p>Modal</p></div>"));
    assert!(
        code.contains(
            "const $p = typeof $target === \"string\" ? document.querySelector($target) : $target;"
        ),
        "{code}"
    );
}

/// Dynamic components with t-component and component event handlers emit callHandler.
#[test]
fn dynamic_component_and_events() {
    let code = module_of(&wrap(
        "<t t-component=\"this.dynamicComp\" t-on-change=\"this.handleChange\"/>",
    ));
    assert!(code.contains("$.component("), "{code}");
    assert!(code.contains("$self.dynamicComp"), "{code}");
    assert!(
        code.contains("\"onChange\": () => ($e) => $.callHandler($self.handleChange, $self, $e)"),
        "{code}"
    );
}

/// SourceMap is populated with line mappings, version 3, and x_provenance.
#[test]
fn source_maps_carry_provenance() {
    let xml = wrap("<p>Hello <b>World</b></p>");
    let compiled = compile(&set_of(&xml), &CompileOptions::new(BuildMode::Prod)).unwrap();
    let mod_entry = &compiled.modules()[0];
    let sm = mod_entry.source_map();
    assert_eq!(sm.version(), 3);
    assert_eq!(sm.file(), "web.T.js");
    assert!(!sm.mappings().is_empty());
    assert!(!sm.provenance().is_empty());
    assert_eq!(sm.provenance()[0].addon, "web");
    assert_eq!(sm.provenance()[0].template, "web.T");

    let json = sm.to_json();
    assert!(json.contains("\"version\":3"), "{json}");
    assert!(json.contains("\"file\":\"web.T.js\""), "{json}");
    assert!(json.contains("\"x_provenance\":["), "{json}");
    assert!(json.contains("\"addon\":\"web\""), "{json}");
}

/// Lowering LimitKind::Effects triggers DiagnosticKind::Limit when reactive effects exceed the limit.
#[test]
fn effect_limit_is_enforced() {
    let xml = wrap(
        "<div>\
        <b t-att-a=\"this.a\"/>\
        <b t-att-b=\"this.b\"/>\
        <b t-att-c=\"this.c\"/>\
        </div>",
    );
    let limits = erp_ui_compiler::CompilerLimits::default()
        .lowered(LimitKind::Effects, 2)
        .unwrap();
    let err = compile(
        &set_of(&xml),
        &CompileOptions::new(BuildMode::Prod).with_limits(limits),
    )
    .unwrap_err();
    let hits: Vec<_> = err.errors().iter().filter_map(|d| d.limit()).collect();
    assert_eq!(hits.len(), 1, "{:?}", err.errors());
    assert_eq!(hits[0].kind(), LimitKind::Effects);
    assert_eq!(hits[0].limit(), 2);
}

/// Codegen injection safety: attack payloads in positions such as constructor, __proto__,
/// prototype, comments, and line separators stay safe or are refused.
#[test]
fn codegen_injection_extended_corpus() {
    for prop in ["constructor", "__proto__", "prototype"] {
        let code = module_of(&wrap(&format!("<b t-out=\"this['{prop}']\"/>")));
        assert!(!code.contains(&format!("$self.{prop}")), "{code}");
        assert!(code.contains(&format!("$self[\"{prop}\"]")), "{code}");
    }
}
