//! The SPEC-UI cases of the template compiler through the public API (design note §12).
//! Expected messages are
//! OWL's texts from the cited `owl.js` lines (facts), the rest are properties of our output.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use erp_ui_compiler::{
    AddonName, Analysis, BuildMode, CompileOptions, CompiledSet, DiagnosticKind, Severity,
    TemplatePath, TemplateSet, TemplateSource, Verdict, analyze, compile,
};

fn set_of(files: &[(&str, &str)]) -> TemplateSet {
    let web = AddonName::new("web").unwrap();
    let mut set = TemplateSet::new();
    for (path, xml) in files {
        set.push(TemplateSource::addon_file(
            web.clone(),
            TemplatePath::new(path).unwrap(),
            *xml,
        ));
    }
    set
}

fn options() -> CompileOptions {
    CompileOptions::new(BuildMode::Prod)
}

fn analysis(xml: &str) -> Analysis {
    analyze(&set_of(&[("static/src/t.xml", xml)]), &options())
}

/// Every error diagnostic of the set, as `(kind, message)`, in report order.
fn errors(xml: &str) -> Vec<(DiagnosticKind, String)> {
    let a = analysis(xml);
    a.set_diagnostics()
        .iter()
        .chain(a.templates().iter().flat_map(|t| t.diagnostics()))
        .filter(|d| d.severity() == Severity::Error)
        .map(|d| (d.kind(), d.message().to_owned()))
        .collect()
}

fn one_error(xml: &str) -> (DiagnosticKind, String) {
    let mut e = errors(xml);
    assert_eq!(e.len(), 1, "expected one error, got {e:?}");
    e.remove(0)
}

fn wrap(body: &str) -> String {
    format!("<templates><t t-name=\"web.T\">{body}</t></templates>")
}

fn compiled(xml: &str) -> CompiledSet {
    match compile(&set_of(&[("static/src/t.xml", xml)]), &options()) {
        Ok(c) => c,
        Err(e) => panic!("compile failed: {:?}", e.errors()),
    }
}

fn module_of(xml: &str) -> String {
    let c = compiled(xml);
    assert_eq!(c.modules().len(), 1);
    c.modules()[0].code().to_owned()
}

#[test]
fn malformed_branches_fail_with_owls_messages() {
    assert_eq!(
        one_error(&wrap("<div t-elif=\"this.a\"/>")),
        (
            DiagnosticKind::BranchWithoutIf,
            "t-elif and t-else directives must be preceded by a t-if or t-elif directive"
                .to_owned()
        )
    );
    assert_eq!(
        one_error(&wrap("<div t-if=\"this.a\"/>text<div t-else=\"\"/>")),
        (
            DiagnosticKind::TextBetweenBranches,
            "text is not allowed between branching directives".to_owned()
        )
    );
    assert_eq!(
        one_error(&wrap("<div t-if=\"this.a\" t-else=\"\"/>")),
        (
            DiagnosticKind::MultipleBranchDirectives,
            "Only one conditional branching directive is allowed per node".to_owned()
        )
    );
    // Whitespace between branches is allowed, and the chain compiles.
    let code = module_of(&wrap(
        "<b t-if=\"this.a\">1</b>\n   <i t-elif=\"this.b\">2</i>\n <u t-else=\"\">3</u>",
    ));
    assert!(code.contains("$.cond("), "{code}");
}

#[test]
fn if_with_foreach_cannot_chain() {
    assert_eq!(
        one_error(&wrap(
            "<div t-if=\"this.a\" t-foreach=\"this.l\" t-as=\"x\" t-key=\"x\"/><div t-else=\"\"/>"
        )),
        (
            DiagnosticKind::IfWithForeachChained,
            "t-if cannot stay at the same level as t-foreach when using t-elif or t-else"
                .to_owned()
        )
    );
    // Without a following branch, t-if and t-foreach on one node are fine.
    assert!(
        errors(&wrap(
            "<div t-if=\"this.a\" t-foreach=\"this.l\" t-as=\"x\" t-key=\"x\"/>"
        ))
        .is_empty()
    );
}

#[test]
fn word_operators_compile_to_javascript_operators() {
    let code = module_of(&wrap(
        "<i t-if=\"this.a gt 1 and this.b lte 2\">x</i>\
         <i t-if=\"this.c gte 3 or this.d lt 4\">y</i>\
         <i t-att-title=\"this.e.and\">z</i>",
    ));
    for op in [
        "$self.a > 1",
        "$self.b <= 2",
        "$self.c >= 3",
        "$self.d < 4",
        " && ",
        " || ",
    ] {
        assert!(code.contains(op), "{op} missing in {code}");
    }
    // A member named like a word stays a member.
    assert!(code.contains("$self.e.and"), "{code}");
    for word in [" gt ", " lte ", " gte ", " lt ", " and ", " or "] {
        assert!(!code.contains(word), "{word} left in {code}");
    }
}

#[test]
fn event_directive_without_a_name_fails() {
    for attr in ["t-on", "t-on-"] {
        assert_eq!(
            one_error(&wrap(&format!("<button {attr}=\"this.go\"/>"))),
            (
                DiagnosticKind::MissingEventName,
                "Missing event name with t-on directive".to_owned()
            ),
            "{attr}"
        );
    }
}

#[test]
fn t_ref_on_a_component_fails() {
    assert_eq!(
        one_error(&wrap("<Foo t-ref=\"this.fooRef\"/>")),
        (
            DiagnosticKind::RefOnComponent,
            "t-ref is no longer supported on components. Consider exposing only the public part of the component's API through a callback prop.".to_owned()
        )
    );
}

#[test]
fn foreach_requires_a_key() {
    assert_eq!(
        one_error(&wrap("<li t-foreach=\"this.items\" t-as=\"item\">x</li>")),
        (
            DiagnosticKind::MissingTKey,
            "\"Directive t-foreach should always be used with a t-key!\" (expression: t-foreach=\"this.items\" t-as=\"item\")".to_owned()
        )
    );
    let code = module_of(&wrap(
        "<ul><li t-foreach=\"this.items\" t-as=\"item\" t-key=\"item.id\"><t t-out=\"item.name\"/></li></ul>",
    ));
    assert!(code.contains("$.list("), "{code}");
}

#[test]
fn sub_template_invocation_with_t_call() {
    let xml = "<templates><t t-name=\"web.A\"><b/></t><t t-name=\"web.T\"><div t-call=\"web.A\"/></t></templates>";
    assert_eq!(
        one_error(xml),
        (
            DiagnosticKind::DirectiveNotAllowedHere,
            "Directive 't-call' can only be used on <t> nodes (used on a <div>)".to_owned()
        )
    );
    let ok = "<templates><t t-name=\"web.A\"><b t-out=\"param\"/></t><t t-name=\"web.T\"><t t-call=\"web.A\" param=\"1\"/></t></templates>";
    let c = compiled(ok);
    assert_eq!(c.modules().len(), 2);
    let mod_t = &c.modules()[1];
    assert!(mod_t.code().contains("$.keyed("), "{}", mod_t.code());
    assert!(mod_t.code().contains("\"param\": 1"), "{}", mod_t.code());
}

#[test]
fn variable_definition_and_assignment_with_t_set() {
    let xml = wrap(
        "<t t-set=\"greeting\" t-value=\"'hello'\"/><t t-set=\"block\"><p>text</p></t><span t-out=\"greeting\"/><span t-out=\"block\"/>",
    );
    let code = module_of(&xml);
    assert!(code.contains("const $v0 = \"hello\";"), "{code}");
    assert!(code.contains("const $v1 = () => {"), "{code}");
    assert!(code.contains("$.insert("), "{code}");
}

#[test]
fn set_slot_only_on_t_nodes_inside_a_component() {
    assert_eq!(
        one_error(&wrap("<Card><div t-set-slot=\"header\">h</div></Card>")),
        (
            DiagnosticKind::DirectiveNotAllowedHere,
            "Directive 't-set-slot' can only be used on <t> nodes (used on a <div>)".to_owned()
        )
    );
    // Outside a component OWL ignores the attribute on a `<t>` and renders the content in place:
    // a warning, same output.
    let outside = wrap("<span><t t-set-slot=\"x\"><b>in place</b></t></span>");
    let a = analysis(&outside);
    let d = a.templates()[0].diagnostics();
    assert_eq!(d.len(), 1, "{d:?}");
    assert_eq!(
        (d[0].severity(), d[0].kind()),
        (Severity::Warning, DiagnosticKind::IgnoredAttribute)
    );
    assert!(module_of(&outside).contains("in place"));
    let code = module_of(&wrap(
        "<Card><t t-set-slot=\"header\">h</t><p>body</p></Card>",
    ));
    assert!(code.contains("$.component("), "{code}");
    assert!(code.contains("\"header\""), "{code}");
    assert!(code.contains("\"default\""), "{code}");
}

#[test]
fn event_modifiers_are_checked() {
    assert_eq!(
        one_error(&wrap("<button t-on-click.later=\"this.go\"/>")),
        (
            DiagnosticKind::UnknownEventModifier,
            "Unknown event modifier: 'later'".to_owned()
        )
    );
    let code = module_of(&wrap(
        "<button t-on-click.stop.prevent.self.capture.synthetic=\"this.go\"/>",
    ));
    assert!(code.contains("$.on("), "{code}");
    let code = module_of(&wrap(
        "<button t-on-touchstart.stop.self.capture.synthetic.passive=\"this.go\"/>",
    ));
    assert!(code.contains("$.on("), "{code}");
}

#[test]
fn prevent_with_passive_is_refused() {
    for attr in [
        "t-on-touchstart.prevent.passive",
        "t-on-touchstart.passive.prevent",
        "t-on-wheel.synthetic.prevent.capture.passive",
    ] {
        assert_eq!(
            one_error(&wrap(&format!("<div {attr}=\"this.go\"/>"))),
            (
                DiagnosticKind::ConflictingEventModifiers,
                "the modifiers 'prevent' and 'passive' cannot be combined: a passive listener cannot prevent the default action; drop one of them".to_owned()
            ),
            "{attr}"
        );
    }
    assert_eq!(
        DiagnosticKind::ConflictingEventModifiers.code(),
        "conflicting_event_modifiers"
    );
}

#[test]
fn dom_element_references_with_t_ref() {
    assert_eq!(
        one_error(&wrap("<Foo t-ref=\"this.r\"/>")).0,
        DiagnosticKind::RefOnComponent
    );
    let code = module_of(&wrap("<input t-ref=\"this.inputRef\"/>"));
    assert!(
        code.contains("$.effect(() => { const $r = $self.inputRef;"),
        "{code}"
    );
}

#[test]
fn inherit_modes() {
    let xml = "<templates>\
        <t t-name=\"web.P\"><div class=\"p\"/></t>\
        <t t-name=\"web.C\" t-inherit=\"web.P\" t-inherit-mode=\"sideways\"><xpath expr=\"//div\" position=\"inside\"><b/></xpath></t>\
        </templates>";
    assert_eq!(
        one_error(xml),
        (
            DiagnosticKind::InvalidDirectiveValue,
            "Invalid inherit mode 'sideways': expected 'primary' or 'extension'".to_owned()
        )
    );
    let xml = "<templates>\
        <t t-name=\"web.P\"><div class=\"p\"/></t>\
        <t t-inherit=\"web.P\" t-inherit-mode=\"extension\"><xpath expr=\"//div\" position=\"inside\"><i>e</i></xpath></t>\
        <t t-name=\"web.C\" t-inherit=\"web.P\"><xpath expr=\"//div\" position=\"inside\"><b>c</b></xpath></t>\
        </templates>";
    let a = analysis(xml);
    assert!(
        a.is_clean(),
        "{:?}",
        a.templates()
            .iter()
            .map(|t| t.diagnostics())
            .collect::<Vec<_>>()
    );
    assert_eq!(a.extensions(), 1);
    let names: Vec<&str> = a.templates().iter().map(|t| t.name().as_str()).collect();
    assert_eq!(names, ["web.P", "web.C"], "an extension adds no template");
    let c = compiled(xml);
    let p = c.modules()[0].code();
    let child = c.modules()[1].code();
    // The extension modifies the parent in place; the primary child gets it too, then its own.
    assert!(p.contains("\\u003ci\\u003ee\\u003c\\/i\\u003e"), "{p}");
    assert!(!p.contains("\\u003cb\\u003ec"), "{p}");
    assert!(
        child.contains("\\u003ci\\u003ee\\u003c\\/i\\u003e\\u003cb\\u003ec\\u003c\\/b\\u003e"),
        "{child}"
    );
    assert!(child.contains("export const name = \"web.C\""), "{child}");
}

#[test]
fn primary_root_tag_and_attributes_are_merged() {
    let xml = r#"<templates>
        <div t-name="web.P" class="parent" data-kept="yes"><span/></div>
        <section t-name="web.C" t-inherit="web.P" class="child">
            <xpath expr="//span" position="inside"><i/></xpath>
        </section>
    </templates>"#;
    let c = compiled(xml);
    let parent = c.modules()[0].code();
    let child = c.modules()[1].code();
    assert!(parent.contains("parent"), "{parent}");
    assert!(child.contains("section"), "{child}");
    assert!(child.contains("child"), "{child}");
    assert!(child.contains("data-kept"), "{child}");
    assert!(!child.contains("parent"), "{child}");
}

#[test]
fn primary_root_merge_is_visible_to_grandchildren() {
    let c = compiled(
        r#"<templates>
        <div t-name="web.P" data-kept="yes"><span/></div>
        <section t-name="web.C" t-inherit="web.P" class="child"/>
        <article t-name="web.G" t-inherit="web.C">
            <xpath expr="//section" position="attributes"><attribute name="data-grandchild">yes</attribute></xpath>
        </article>
    </templates>"#,
    );
    let grandchild = c.modules()[2].code();
    assert!(grandchild.contains("article"), "{grandchild}");
    assert!(grandchild.contains("data-grandchild"), "{grandchild}");
    assert!(grandchild.contains("data-kept"), "{grandchild}");
    assert!(grandchild.contains("child"), "{grandchild}");
}

#[test]
fn anchor_operations_target_the_declared_anchor() {
    let c = compiled(
        r#"<templates>
        <t t-name="web.P"><div><span t-anchor="row.actions">before</span></div></t>
        <t t-inherit="web.P" t-inherit-mode="extension">
            <anchor name="row.actions" position="after"><i>after</i></anchor>
        </t>
    </templates>"#,
    );
    let parent = c.modules()[0].code();
    assert!(parent.contains("before"), "{parent}");
    assert!(parent.contains("after"), "{parent}");
    assert!(
        !parent.contains("data-t-anchor"),
        "production strips anchor metadata: {parent}"
    );
}

#[test]
fn duplicate_anchors_fail_with_both_nodes_named() {
    assert_eq!(
        one_error(
            r#"<templates><t t-name="web.P"><span t-anchor="row.actions"/><i t-anchor="row.actions"/></t></templates>"#
        ),
        (
            DiagnosticKind::InvalidDirectiveValue,
            "duplicate t-anchor 'row.actions': <span> and <i> declare the same anchor".to_owned(),
        )
    );
}

#[test]
fn primary_child_sees_later_addon_extensions() {
    let mut set = TemplateSet::new();
    for (addon, path, xml) in [
        (
            "a",
            "static/src/one.xml",
            r#"<templates><t t-inherit="a.P" t-inherit-mode="extension"><xpath expr="//div" position="inside"><i>first</i></xpath></t><t t-name="a.P"><div/></t></templates>"#,
        ),
        (
            "a",
            "static/src/two.xml",
            r#"<templates><t t-inherit="a.P" t-inherit-mode="extension"><xpath expr="//div" position="inside"><i>second</i></xpath></t></templates>"#,
        ),
        (
            "b",
            "static/src/one.xml",
            r#"<templates><t t-name="b.C" t-inherit="a.P"><xpath expr="//div" position="inside"><i>child</i></xpath></t></templates>"#,
        ),
        (
            "b",
            "static/src/two.xml",
            r#"<templates><t t-inherit="b.C" t-inherit-mode="extension"><xpath expr="//div" position="inside"><i>childext</i></xpath></t></templates>"#,
        ),
        (
            "c",
            "static/src/one.xml",
            r#"<templates><t t-inherit="a.P" t-inherit-mode="extension"><xpath expr="//div" position="inside"><i>third</i></xpath></t></templates>"#,
        ),
        (
            "c",
            "static/src/two.xml",
            r#"<templates><t t-inherit="a.P" t-inherit-mode="extension"><xpath expr="//div" position="inside"><i>fourth</i></xpath></t></templates>"#,
        ),
    ] {
        set.push(TemplateSource::addon_file(
            AddonName::new(addon).unwrap(),
            TemplatePath::new(path).unwrap(),
            xml,
        ));
    }
    let compiled = compile(&set, &options()).unwrap();
    let child = compiled.modules()[1].code();
    let markers = ["first", "second", "third", "fourth", "child", "childext"];
    let positions: Vec<_> = markers.iter().map(|m| child.find(m).unwrap()).collect();
    assert!(positions.windows(2).all(|p| p[0] < p[1]), "{child}");
}

#[test]
fn inherited_condition_attributes_compile_as_conjunction() {
    let c = compiled(
        r#"<templates>
        <t t-name="web.P"><div t-if="this.first"/></t>
        <t t-inherit="web.P" t-inherit-mode="extension"><xpath expr="//div" position="attributes"><attribute name="t-if" add="this.second"/></xpath></t>
    </templates>"#,
    );
    let parent = c.modules()[0].code();
    assert!(parent.contains("$self.first"), "{parent}");
    assert!(parent.contains("$self.second"), "{parent}");
    assert!(parent.contains("&&"), "{parent}");
}

#[test]
fn inherited_replace_wraps_the_target_with_dollar_zero() {
    let c = compiled(
        r#"<templates>
        <t t-name="web.P"><div><span>original</span></div></t>
        <t t-inherit="web.P" t-inherit-mode="extension"><xpath expr="//span" position="replace"><section>$0<i>added</i></section></xpath></t>
    </templates>"#,
    );
    let parent = c.modules()[0].code();
    assert!(
        parent.find("original").unwrap() < parent.find("added").unwrap(),
        "{parent}"
    );
    assert!(parent.contains("section"), "{parent}");
}

#[test]
fn anchor_locator_rejects_quotes_and_oversized_names() {
    for name in ["row&apos;actions".to_owned(), "x".repeat(129)] {
        let xml = format!(
            r#"<templates><t t-name="web.P"><span t-anchor="row.actions"/></t><t t-inherit="web.P" t-inherit-mode="extension"><anchor name="{name}" position="after"><i/></anchor></t></templates>"#
        );
        let decoded = name.replace("&apos;", "'");
        assert_eq!(
            one_error(&xml),
            (
                DiagnosticKind::Inheritance,
                format!(
                    "template inheritance failed: anchor name '{decoded}' must be [a-z0-9_.@-]+ and at most 128 bytes"
                ),
            )
        );
    }
}

#[test]
fn anchor_names_are_template_local_and_dev_metadata_is_kept() {
    let xml = r#"<templates><t t-name="web.A"><span t-anchor="row.actions"/></t><t t-name="web.B"><i t-anchor="row.actions"/></t></templates>"#;
    let set = set_of(&[("static/src/t.xml", xml)]);
    let dev = compile(&set, &CompileOptions::new(BuildMode::Dev)).unwrap();
    assert_eq!(dev.modules().len(), 2);
    assert!(
        dev.modules()
            .iter()
            .all(|m| m.code().contains("data-t-anchor"))
    );
    let prod = compile(&set, &options()).unwrap();
    assert!(
        prod.modules()
            .iter()
            .all(|m| !m.code().contains("data-t-anchor"))
    );
}

#[test]
fn inherited_duplicate_anchor_keeps_both_source_origins() {
    let mut set = TemplateSet::new();
    set.push(TemplateSource::addon_file(
        AddonName::new("a").unwrap(),
        TemplatePath::new("static/src/base.xml").unwrap(),
        r#"<templates><t t-name="a.P"><span t-anchor="row.actions"/></t></templates>"#,
    ));
    set.push(TemplateSource::addon_file(AddonName::new("b").unwrap(), TemplatePath::new("static/src/extension.xml").unwrap(),
        r#"<templates><t t-inherit="a.P" t-inherit-mode="extension"><xpath expr="//span" position="after"><i t-anchor="row.actions"/></xpath></t></templates>"#));
    let error = compile(&set, &options()).unwrap_err();
    let diagnostics = error.errors();
    assert_eq!(diagnostics.len(), 1);
    let diagnostic = &diagnostics[0];
    assert_eq!(diagnostic.kind(), DiagnosticKind::Inheritance);
    assert_eq!(
        diagnostic.message(),
        "template inheritance failed: duplicate t-anchor 'row.actions': <span> from a/static/src/base.xml:1:12 (entry line 1) and <i> from b/static/src/extension.xml:1:12 (entry line 1) declare the same anchor"
    );
    assert_eq!(
        diagnostic.location().unwrap().origin().addon().as_str(),
        "b"
    );
    assert_eq!(
        diagnostic.location().unwrap().origin().path().as_str(),
        "static/src/extension.xml"
    );
}

#[test]
fn anchor_locator_rejects_missing_name_and_unknown_attributes() {
    for (operation, expected) in [
        (
            "<anchor position=\"after\"/>",
            "<anchor> requires a 'name' attribute",
        ),
        (
            "<anchor name=\"x\" expr=\"//span\"/>",
            "<anchor> does not accept attribute 'expr'",
        ),
        (
            "<anchor name=\"x\" position=\"sideways\"/>",
            "Invalid position attribute: 'sideways'",
        ),
    ] {
        let xml = format!(
            "<templates><t t-name=\"web.P\"><span t-anchor=\"x\"/></t><t t-inherit=\"web.P\" t-inherit-mode=\"extension\">{operation}</t></templates>"
        );
        assert_eq!(
            one_error(&xml),
            (
                DiagnosticKind::Inheritance,
                format!("template inheritance failed: {expected}")
            )
        );
    }
}

#[test]
fn anchor_rewrite_obeys_lowered_output_bound() {
    let operations = "<anchor name=\"x\" position=\"after\"/>".repeat(20);
    let xml = format!(
        "<templates><t t-name=\"web.P\"><span t-anchor=\"x\"/></t><t t-inherit=\"web.P\" t-inherit-mode=\"extension\">{operations}</t></templates>"
    );
    let limits = erp_ui_compiler::CompilerLimits::default()
        .lowered(erp_ui_compiler::LimitKind::FileBytes, xml.len())
        .unwrap();
    let errors = compile(
        &set_of(&[("static/src/t.xml", &xml)]),
        &options().with_limits(limits),
    )
    .unwrap_err();
    assert_eq!(errors.errors().len(), 1);
    assert_eq!(errors.errors()[0].kind(), DiagnosticKind::Inheritance);
    assert_eq!(
        errors.errors()[0].message(),
        format!(
            "template inheritance failed: anchor rewrite exceeds file_bytes limit: maximum {}",
            xml.len()
        )
    );
}

#[test]
fn unmatched_xpath_reports_the_offending_spec_source() {
    let mut set = TemplateSet::new();
    set.push(TemplateSource::addon_file(
        AddonName::new("a").unwrap(),
        TemplatePath::new("static/src/base.xml").unwrap(),
        "<templates>\n<t t-name=\"a.P\"><div><span/></div></t>\n</templates>",
    ));
    set.push(TemplateSource::addon_file(
        AddonName::new("b").unwrap(), TemplatePath::new("static/src/extension.xml").unwrap(),
        "<templates>\n  <t t-inherit=\"a.P\" t-inherit-mode=\"extension\">\n    <xpath expr=\"//span\" position=\"attributes\"><attribute name=\"class\">kept</attribute></xpath>\n    <data>\n\n    <xpath expr=\"//gone\" position=\"inside\"><i/></xpath>\n    </data>\n  </t>\n</templates>",
    ));
    let errors = compile(&set, &options()).unwrap_err();
    assert_eq!(errors.errors().len(), 1);
    let diagnostic = &errors.errors()[0];
    assert_eq!(diagnostic.kind(), DiagnosticKind::Inheritance);
    assert_eq!(
        diagnostic.message(),
        "template inheritance failed: error in view 'extension of a.P' (id 2): inheritance spec 2 at entry line 5 <xpath expr=\"//gone\">: Element '<xpath expr=\"//gone\">' cannot be located in parent view"
    );
    assert_eq!(diagnostic.template().unwrap().as_str(), "a.P");
    let location = diagnostic.location().unwrap();
    assert_eq!(location.origin().addon().as_str(), "b");
    assert_eq!(
        location.origin().path().as_str(),
        "static/src/extension.xml"
    );
    assert_eq!(location.position().unwrap().line().get(), 6);
    assert_eq!(location.position().unwrap().column().get(), 5);
}

#[test]
fn declaration_errors_keep_template_and_source_metadata() {
    for (xml, name, kind, message) in [
        (
            "<templates>\n  <t t-name=\"web.C\" t-inherit=\"web.Gone\"/>\n</templates>",
            "web.C",
            DiagnosticKind::MissingTemplate,
            "template parent 'web.Gone' not found (t-inherit of 'web.C')",
        ),
        (
            "<templates>\n  <t t-inherit=\"web.Gone\" t-inherit-mode=\"extension\"/>\n</templates>",
            "web.Gone",
            DiagnosticKind::MissingTemplate,
            "template parent 'web.Gone' not found (t-inherit of an extension)",
        ),
        (
            "<templates>\n  <t t-name=\"web.P\"><i/></t>\n  <t t-name=\"web.P\"><b/></t>\n</templates>",
            "web.P",
            DiagnosticKind::DuplicateTemplate,
            "template 'web.P' is already defined at web/static/src/t.xml:2:3",
        ),
    ] {
        let errors = compile(&set_of(&[("static/src/t.xml", xml)]), &options()).unwrap_err();
        assert_eq!(errors.errors().len(), 1);
        let diagnostic = &errors.errors()[0];
        assert_eq!(diagnostic.kind(), kind);
        assert_eq!(diagnostic.message(), message);
        assert_eq!(diagnostic.template().unwrap().as_str(), name);
        let location = diagnostic.location().unwrap();
        assert_eq!(location.origin().addon().as_str(), "web");
        assert_eq!(location.origin().path().as_str(), "static/src/t.xml");
        assert!(location.position().is_some());
    }
}

#[test]
fn inherited_expression_error_reports_its_inserting_source_and_spec() {
    let mut set = TemplateSet::new();
    set.push(TemplateSource::addon_file(
        AddonName::new("a").unwrap(),
        TemplatePath::new("static/src/base.xml").unwrap(),
        "<templates>\n  <t t-name=\"a.P\"><div><span/></div></t>\n</templates>",
    ));
    set.push(TemplateSource::addon_file(AddonName::new("b").unwrap(), TemplatePath::new("static/src/extension.xml").unwrap(),
        "<templates>\n  <t t-inherit=\"a.P\" t-inherit-mode=\"extension\">\n    <xpath expr=\"//span\" position=\"after\">\n      <i t-if=\"\"/>\n    </xpath>\n  </t>\n</templates>"));
    let errors = compile(&set, &options()).unwrap_err();
    assert_eq!(errors.errors().len(), 1);
    let diagnostic = &errors.errors()[0];
    assert_eq!(diagnostic.kind(), DiagnosticKind::ExpressionSyntax);
    assert_eq!(
        diagnostic.message(),
        "empty expression (inheritance spec 1 at b/static/src/extension.xml:3:5; node <i> at b/static/src/extension.xml:4:7)"
    );
    assert_eq!(diagnostic.template().unwrap().as_str(), "a.P");
    let location = diagnostic.location().unwrap();
    assert_eq!(location.origin().addon().as_str(), "b");
    assert_eq!(
        location.origin().path().as_str(),
        "static/src/extension.xml"
    );
    assert_eq!(location.position().unwrap().line().get(), 4);
    assert_eq!(location.position().unwrap().column().get(), 7);
}

#[test]
fn missing_parent_fails_at_assembly() {
    let xml = "<templates><t t-name=\"web.C\" t-inherit=\"web.Gone\"><xpath expr=\"//div\" position=\"inside\"/></t></templates>";
    let (kind, message) = one_error(xml);
    assert_eq!(kind, DiagnosticKind::MissingTemplate);
    assert_eq!(
        message,
        "template parent 'web.Gone' not found (t-inherit of 'web.C')"
    );
    let xml = "<templates><t t-inherit=\"web.Gone\" t-inherit-mode=\"extension\"><xpath expr=\"//div\" position=\"inside\"/></t></templates>";
    assert_eq!(
        one_error(xml).1,
        "template parent 'web.Gone' not found (t-inherit of an extension)"
    );
}

#[test]
fn generated_modules_never_evaluate_code() {
    let code = module_of(&wrap(
        "<div t-att-title=\"this.t\" t-on-click=\"() => this.go(1)\">\
         &lt;/script&gt;${x}`\u{2028}</div>",
    ));
    // `${` and backticks stay inside double-quoted strings, where they are plain text.
    for bad in [
        "eval",
        "Function(",
        "new ",
        "import(",
        "</script",
        "\u{2028}",
    ] {
        assert!(!code.contains(bad), "{bad:?} in {code}");
    }
    let imports: Vec<&str> = code.lines().filter(|l| l.starts_with("import ")).collect();
    assert_eq!(
        imports,
        [format!(
            "import * as $ from \"{}\";",
            erp_ui_compiler::ABI_MODULE
        )]
    );
}

#[test]
fn dynamic_tag_evaluation_with_t_tag() {
    assert_eq!(
        one_error(&wrap("<block-text/>")),
        (
            DiagnosticKind::InvalidName,
            "Invalid tag name: 'block-text'".to_owned()
        )
    );
    let code = module_of(&wrap("<t t-tag=\"this.myTag\"><span/></t>"));
    assert!(
        code.contains("document.createElement($self.myTag)"),
        "{code}"
    );
}

#[test]
fn custom_directives_must_be_registered() {
    for attr in ["t-custom", "t-custom-"] {
        assert_eq!(
            one_error(&wrap(&format!("<div {attr}=\"1\"/>"))).1,
            "Missing custom directive name with t-custom directive",
            "{attr}"
        );
    }
    assert_eq!(
        one_error(&wrap("<div t-custom-tooltip=\"this.tip\"/>")),
        (
            DiagnosticKind::CustomDirectiveNotDefined,
            "Custom directive \"tooltip\" is not defined".to_owned()
        )
    );
    let opts = CompileOptions::new(BuildMode::Prod)
        .with_custom_directive("tooltip")
        .unwrap();
    let a = analyze(
        &set_of(&[(
            "static/src/t.xml",
            &wrap("<div t-custom-tooltip.wide=\"this.tip\"/>"),
        )]),
        &opts,
    );
    assert!(a.is_clean(), "{:?}", a.templates()[0].diagnostics());
}

#[test]
fn two_way_input_binding_with_t_model() {
    assert_eq!(
        one_error(&wrap("<div t-model=\"this.state.v\"/>")),
        (
            DiagnosticKind::ModelOnWrongElement,
            "The t-model directive only works with <input>, <textarea> and <select>".to_owned()
        )
    );
    for el in ["input", "textarea", "select"] {
        assert!(
            errors(&wrap(&format!("<{el} t-model.trim=\"this.state.v\"/>"))).is_empty(),
            "{el}"
        );
    }
    let code = module_of(&wrap(
        "<input t-model.trim.number=\"this.num\"/><select t-model.lazy=\"this.sel\"><option value=\"1\">1</option></select>",
    ));
    assert!(code.contains("parseFloat($e.target.value) || 0"), "{code}");
    assert!(code.contains("$.prop("), "{code}");
    assert!(code.contains("\"change\""), "{code}");
}

#[test]
fn prop_suffixes() {
    assert_eq!(
        one_error(&wrap("<Foo label.upper=\"this.l\"/>")),
        (
            DiagnosticKind::InvalidPropSuffix,
            "Invalid prop suffix: upper".to_owned()
        )
    );
    let code = module_of(&wrap(
        "<Foo onPick.bind=\"this.pick\" label.translate=\"Add a line\" same.alike=\"this.s\" plain=\"\"/>",
    ));
    // `.translate` is a literal string, not an expression.
    assert!(code.contains("\"Add a line\""), "{code}");
    assert!(code.contains("$self.pick"), "{code}");
    assert!(code.contains("$self.s"), "{code}");
}

#[test]
fn subtemplate_context_parameterization_with_t_call_context() {
    let xml = "<templates><t t-name=\"web.Sub\"><b t-out=\"this.title\"/></t><t t-name=\"web.T\"><t t-call=\"web.Sub\" t-call-context=\"this.subCtx\"/></t></templates>";
    let c = compiled(xml);
    assert_eq!(c.modules().len(), 2);
    let code = c.modules()[1].code();
    assert!(
        code.contains("Object.assign({}, $self.subCtx, {this: $self.subCtx}"),
        "{code}"
    );
}

#[test]
fn this_reads_the_component_and_bare_names_are_locals() {
    let code = module_of(&wrap(
        "<ul><li t-foreach=\"this.rows\" t-as=\"row\" t-key=\"row.id\"><t t-out=\"row.label\"/></li></ul>",
    ));
    assert!(code.contains("$self.rows"), "{code}");
    assert!(
        !code.contains("this."),
        "component reads go through $self: {code}"
    );
    let (kind, message) = one_error(&wrap("<b t-out=\"label\"/>"));
    assert_eq!(kind, DiagnosticKind::UnknownIdentifier);
    assert!(
        message.contains("'label'") && message.contains("this.label"),
        "{message}"
    );
}

#[test]
fn dynamic_subtemplate_resolution_with_t_call() {
    let xml = "<templates><t t-name=\"web.A\"><b/></t><t t-name=\"web.T\"><t t-call=\"{{this.targetTpl}}\"/></t></templates>";
    let c = compiled(xml);
    assert_eq!(c.modules().len(), 2);
    let code = c.modules()[1].code();
    assert!(code.contains("const $target = $self.targetTpl;"), "{code}");
    assert!(
        code.contains("Object.assign(Object.create($c || null)"),
        "{code}"
    );
}

#[test]
fn t_valuef_is_refused() {
    let (kind, message) = one_error(&wrap("<t t-set=\"cls\" t-valuef=\"table-striped\"/>"));
    assert_eq!(kind, DiagnosticKind::RejectedDirective);
    assert!(
        message.contains("BUG-063") && message.contains("t-value=\"'…'\""),
        "{message}"
    );
}

/// BUG-064 (`Parity: divergent — bug fix BUG-064`): a non-breaking space is not XML whitespace and
/// survives the whitespace rule.
#[test]
fn non_breaking_spaces_are_kept() {
    let code = module_of(&wrap("<span>#&#160;Lines</span>"));
    assert!(code.contains("#\u{a0}Lines"), "{code}");
}

#[test]
fn template_roots_hold_templates() {
    let a =
        analysis("<template><t t-name=\"web.A\"><b/></t><t t-name=\"web.B\"><i/></t></template>");
    assert_eq!(a.templates().len(), 2);
    assert!(a.templates().iter().all(|t| t.verdict() == Verdict::Passed));
}

/// Parity: divergent — bug fix BUG-065.
#[test]
fn plain_attributes_on_t_nodes() {
    let a = analysis(&wrap("<t owl=\"1\"><span t-out=\"this.n\"/></t>"));
    assert!(a.is_clean());
    let warnings: Vec<DiagnosticKind> = a.templates()[0]
        .diagnostics()
        .iter()
        .map(|d| d.kind())
        .collect();
    assert_eq!(warnings, [DiagnosticKind::IgnoredAttribute]);
    let (kind, message) = one_error(&wrap("<t t-out=\"this.n\" class=\"me-1\"/>"));
    assert_eq!(kind, DiagnosticKind::DirectiveNotAllowedHere);
    assert!(
        message.starts_with("attribute 'class' is not allowed on a <t t-out> node"),
        "{message}"
    );
    let xml = "<templates>\
        <t t-name=\"web.P\"><div/></t>\
        <t t-inherit=\"web.P\" t-inherit-mode=\"extension\" owl=\"1\"><xpath expr=\"//div\" position=\"inside\"><i>e</i></xpath></t>\
        </templates>";
    let a = analysis(xml);
    assert!(a.is_clean(), "{:?}", a.set_diagnostics());
    assert!(
        a.set_diagnostics()
            .iter()
            .any(|d| d.kind() == DiagnosticKind::IgnoredAttribute)
    );
    for root_attr in ["class=\"x\"", "t-if=\"this.a\""] {
        let xml = format!(
            "<templates><t t-name=\"web.P\"><div/></t>\
             <t t-inherit=\"web.P\" t-inherit-mode=\"extension\" {root_attr}><xpath expr=\"//div\" position=\"inside\"><i>e</i></xpath></t>\
             </templates>"
        );
        let kinds: Vec<DiagnosticKind> = errors(&xml).into_iter().map(|(k, _)| k).collect();
        assert_eq!(
            kinds,
            [
                DiagnosticKind::DirectiveNotAllowedHere,
                DiagnosticKind::Inheritance
            ],
            "{root_attr}"
        );
    }
}

/// Parity: divergent — (every template is combined and compiled at assembly, and
/// any error fails it).
#[test]
fn an_extension_with_errors_fails_its_parent() {
    let xml = "<templates>\
        <t t-name=\"web.P\"><div/></t>\
        <t t-name=\"web.C\" t-inherit=\"web.P\"><xpath expr=\"//div\" position=\"inside\"><b>c</b></xpath></t>\
        <t t-name=\"web.X\" t-inherit=\"web.P\" t-inherit-mode=\"extension\"><xpath expr=\"//div\" position=\"inside\"><i>e</i></xpath></t>\
        <t t-name=\"web.Other\"><p/></t>\
        </templates>";
    let a = analysis(xml);
    // The extension's own error is a set diagnostic.
    let set: Vec<(DiagnosticKind, &str)> = a
        .set_diagnostics()
        .iter()
        .filter(|d| d.severity() == Severity::Error)
        .map(|d| (d.kind(), d.message()))
        .collect();
    assert_eq!(
        set,
        [(
            DiagnosticKind::InvalidDirectiveValue,
            "an extension (t-inherit-mode=\"extension\") modifies its parent and has no t-name"
        )]
    );
    // The parent and its primary child fail; the unrelated template passes.
    let verdicts: Vec<(&str, Verdict)> = a
        .templates()
        .iter()
        .map(|t| (t.name().as_str(), t.verdict()))
        .collect();
    assert_eq!(
        verdicts,
        [
            ("web.P", Verdict::Rejected),
            ("web.C", Verdict::Rejected),
            ("web.Other", Verdict::Passed)
        ]
    );
    let p = &a.templates()[0].diagnostics()[0];
    assert_eq!(p.kind(), DiagnosticKind::Inheritance);
    assert!(
        p.message().starts_with(
            "the template is not compiled without its inheritance: the extension at web/static/src/t.xml:1:"
        ),
        "{}",
        p.message()
    );
    let c = &a.templates()[1].diagnostics()[0];
    assert!(
        c.message().contains("its parent 'web.P'"),
        "{}",
        c.message()
    );
    assert!(compile(&set_of(&[("static/src/t.xml", xml)]), &options()).is_err());
}

#[test]
fn changed_node_is_not_misattributed_to_an_identical_same_line_peer() {
    let mut set = TemplateSet::new();
    let base = "<templates><t t-name=\"a.P\"><div><span t-if=\"this.ok\"/><span t-if=\"this.x = 1\"/></div></t></templates>";
    set.push(TemplateSource::addon_file(
        AddonName::new("a").unwrap(),
        TemplatePath::new("static/src/base.xml").unwrap(),
        base,
    ));
    set.push(TemplateSource::addon_file(AddonName::new("b").unwrap(), TemplatePath::new("static/src/extension.xml").unwrap(), "<templates>\n  <t t-inherit=\"a.P\" t-inherit-mode=\"extension\">\n    <xpath expr=\"//span[1]\" position=\"attributes\"><attribute name=\"t-if\">this.x = 1</attribute></xpath>\n  </t>\n</templates>"));
    let errors = compile(&set, &options()).unwrap_err();
    assert_eq!(errors.errors().len(), 2);
    let message = "assignments and updates are only allowed in t-on-* handlers and inside arrow functions, not in an expression evaluated while rendering (the check is syntactic: a method call that changes state is not detected)";
    let first = &errors.errors()[0];
    assert_eq!(first.kind(), DiagnosticKind::WriteDuringRender);
    assert_eq!(
        first.message(),
        format!(
            "{message} (inheritance source unresolved; candidates: inheritance spec n/a (declaration); node <t> at a/static/src/base.xml:1:12 | inheritance spec 1 at b/static/src/extension.xml:3:5; node <xpath> at b/static/src/extension.xml:3:5)"
        )
    );
    assert!(
        first.location().is_none(),
        "a changed node has no verified literal source location"
    );
    let second = &errors.errors()[1];
    let original_column = base.find("<span t-if=\"this.x = 1\"").unwrap() + 1;
    assert_eq!(second.kind(), DiagnosticKind::WriteDuringRender);
    assert_eq!(
        second.message(),
        format!(
            "{message} (inheritance spec n/a (declaration); node <span> at a/static/src/base.xml:1:{original_column})"
        )
    );
    assert_eq!(
        second
            .location()
            .unwrap()
            .position()
            .unwrap()
            .column()
            .get() as usize,
        original_column
    );
}
