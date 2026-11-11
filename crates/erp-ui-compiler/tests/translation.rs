//! Compiler-only localization. Serving and runtime language selection have separate deliveries.
#![allow(
    clippy::unwrap_used,
    reason = "test fixture construction and assertions"
)]
use erp_ui_compiler::{
    AddonName, BuildMode, CompileOptions, DiagnosticKind, TemplatePath, TemplateSet,
    TemplateSource, TranslationCatalogue, TranslationError, compile, compile_localized,
    extract_translations,
};

fn options() -> CompileOptions {
    CompileOptions::new(BuildMode::Prod)
}
fn sources(files: &[(&str, &str, &str)]) -> TemplateSet {
    let mut set = TemplateSet::new();
    for &(addon, path, xml) in files {
        set.push(TemplateSource::addon_file(
            AddonName::new(addon).unwrap(),
            TemplatePath::new(path).unwrap(),
            xml,
        ));
    }
    set
}
fn one(xml: &str) -> TemplateSet {
    sources(&[("web", "static/src/a.xml", xml)])
}
fn catalogue(terms: &[(&str, &str, &str)]) -> TranslationCatalogue {
    let mut result = TranslationCatalogue::new("fr_FR").unwrap();
    for &(addon, source, translated) in terms {
        result = result
            .with_term(AddonName::new(addon).unwrap(), source, translated)
            .unwrap();
    }
    result
}
fn code(set: &TemplateSet, terms: &[(&str, &str, &str)]) -> String {
    compile_localized(set, &options(), &catalogue(terms))
        .unwrap()
        .modules()[0]
        .code()
        .to_owned()
}

#[test]
fn translates_literals_and_preserves_nontranslatable_attributes() {
    let set = one(
        r#"<templates><t t-name="web.A"><p title="Hint" data-id="Hint">  Hello  </p></t></templates>"#,
    );
    let translated = code(
        &set,
        &[("web", "Hello", "Bonjour"), ("web", "Hint", "Aide")],
    );
    assert!(translated.contains("Bonjour"), "{translated}");
    assert!(translated.contains("Aide"), "{translated}");
    assert!(
        translated.contains("Hint"),
        "data-id must stay literal: {translated}"
    );
    let original = compile(&set, &options()).unwrap();
    assert!(original.modules()[0].code().contains("Hello"));
    assert!(
        original
            .warnings()
            .iter()
            .any(|d| d.kind() == DiagnosticKind::Skipped && d.directive() == Some("translation"))
    );
    let localized = compile_localized(&set, &options(), &catalogue(&[])).unwrap();
    assert_eq!(localized.modules()[0].code(), original.modules()[0].code());
    assert!(
        !localized
            .warnings()
            .iter()
            .any(|d| d.directive() == Some("translation"))
    );
}

#[test]
fn context_and_off_are_lexical_and_do_not_leak_to_siblings() {
    let set = one(
        r#"<templates><t t-name="web.A"><div><span t-translation-context="other">Same</span><span t-translation="off" title="Same"><b>Same</b></span><span>Same</span></div></t></templates>"#,
    );
    let result = code(
        &set,
        &[("other", "Same", "Autre"), ("web", "Same", "Pareil")],
    );
    for literal in ["Autre", "Same", "Pareil"] {
        assert!(result.contains(literal), "missing {literal}: {result}");
    }
    let extracted = extract_translations(&set, &options()).unwrap();
    let keys: Vec<_> = extracted
        .terms()
        .iter()
        .map(|term| (term.addon().as_str(), term.source()))
        .collect();
    assert_eq!(keys, [("other", "Same"), ("web", "Same")]);
}

#[test]
fn attribute_context_overrides_enclosing_context() {
    let set = one(
        r#"<templates><t t-name="web.A"><p t-translation-context="other" t-translation-context-title="web" title="Same">Same</p></t></templates>"#,
    );
    let result = code(
        &set,
        &[("other", "Same", "Autre"), ("web", "Same", "Pareil")],
    );
    assert!(result.contains("Autre"), "{result}");
    assert!(result.contains("Pareil"), "{result}");
    let error = compile(&set, &options()).unwrap_err();
    assert!(
        error
            .errors()
            .iter()
            .any(|d| d.kind() == DiagnosticKind::NotSupportedUntil)
    );
}

#[test]
fn translates_component_literals_without_translating_expressions() {
    let set = one(
        r#"<templates><t t-name="web.A"><Foo label.translate="Hello" title="this.Hello"/></t></templates>"#,
    );
    let result = code(
        &set,
        &[
            ("web", "Hello", "Bonjour"),
            ("web", "this.Hello", "BROKEN_EXPRESSION"),
        ],
    );
    assert!(result.contains("Bonjour"), "{result}");
    assert!(
        result.contains("$self.Hello"),
        "expression must stay unchanged: {result}"
    );
    assert!(!result.contains("BROKEN_EXPRESSION"));
}

#[test]
fn xml_whitespace_is_trimmed_only_for_lookup_and_nbsp_remains_significant() {
    let set = one(
        "<templates><t t-name=\"web.A\"><p>  Hello  </p><b>\u{a0}Hello\u{a0}</b></t></templates>",
    );
    let extracted = extract_translations(&set, &options()).unwrap();
    let keys: Vec<_> = extracted.terms().iter().map(|term| term.source()).collect();
    assert_eq!(keys, ["Hello", "\u{a0}Hello\u{a0}"]);
    let result = code(
        &set,
        &[
            ("web", "Hello", "Bonjour"),
            ("web", "\u{a0}Hello\u{a0}", "Salut"),
        ],
    );
    assert!(result.contains("Bonjour"), "{result}");
    assert!(result.contains("Salut"), "{result}");
}

#[test]
fn ordinary_calls_and_provider_slot_content_keep_declaration_ownership() {
    let set = sources(&[
        (
            "web",
            "static/src/a.xml",
            r#"<templates><t t-name="web.A"><t t-call="other.Callee"/><Foo><span>Hello</span></Foo></t></templates>"#,
        ),
        (
            "other",
            "static/src/b.xml",
            r#"<templates><t t-name="other.Callee"><p>Hello</p></t></templates>"#,
        ),
    ]);
    let compiled = compile_localized(
        &set,
        &options(),
        &catalogue(&[("web", "Hello", "Provider"), ("other", "Hello", "Callee")]),
    )
    .unwrap();
    assert!(compiled.modules()[0].code().contains("Provider"));
    assert!(compiled.modules()[1].code().contains("Callee"));
    let extracted = extract_translations(&set, &options()).unwrap();
    assert_eq!(extracted.terms().len(), 2);
}

#[test]
fn translated_expansion_still_obeys_compiler_module_limit() {
    let set = one(r#"<templates><t t-name="web.A"><p>Hello</p></t></templates>"#);
    let options = options().with_limits(
        erp_ui_compiler::CompilerLimits::default()
            .lowered(erp_ui_compiler::LimitKind::ModuleBytes, 2048)
            .unwrap(),
    );
    assert!(
        compile(&set, &options).is_ok(),
        "source must fit before translation"
    );
    let errors = compile_localized(
        &set,
        &options,
        &catalogue(&[("web", "Hello", &"x".repeat(4096))]),
    )
    .unwrap_err();
    assert_eq!(errors.errors()[0].kind(), DiagnosticKind::Limit);
    assert_eq!(
        errors.errors()[0].limit().unwrap().kind(),
        erp_ui_compiler::LimitKind::ModuleBytes
    );
    assert!(
        errors.errors()[0].message().contains("2048"),
        "actual emitted byte count varies with codegen; the limit identity is separately asserted"
    );
}

#[test]
fn extraction_is_sorted_deduplicated_and_retains_source_locations() {
    let set = sources(&[
        (
            "z",
            "static/src/z.xml",
            "<templates><t t-name=\"z.A\"><p>Zulu</p><p>Zulu</p></t></templates>",
        ),
        (
            "a",
            "static/src/a.xml",
            "<templates><t t-name=\"a.A\"><p>Alpha</p></t></templates>",
        ),
    ]);
    let first = extract_translations(&set, &options()).unwrap();
    let second = extract_translations(&set, &options()).unwrap();
    assert_eq!(first, second);
    let keys: Vec<_> = first
        .terms()
        .iter()
        .map(|term| (term.addon().as_str(), term.source()))
        .collect();
    assert_eq!(keys, [("a", "Alpha"), ("z", "Zulu")]);
    assert_eq!(first.terms()[1].locations().len(), 2);
    for term in first.terms() {
        for location in term.locations() {
            assert_eq!(location.origin().addon(), term.addon());
            assert!(location.position().is_some());
        }
    }
}

#[test]
fn inherited_literals_use_contributor_not_root_addon() {
    let set = sources(&[
        (
            "web",
            "static/src/a.xml",
            r#"<templates><t t-name="web.A"><div><span>Same</span></div></t></templates>"#,
        ),
        (
            "other",
            "static/src/b.xml",
            r#"<templates><t t-inherit="web.A" t-inherit-mode="extension"><xpath expr="//div" position="inside"><b>Same</b></xpath></t></templates>"#,
        ),
    ]);
    let result = code(
        &set,
        &[("web", "Same", "Pareil"), ("other", "Same", "Autre")],
    );
    assert!(result.contains("Pareil"), "{result}");
    assert!(result.contains("Autre"), "{result}");
    let extracted = extract_translations(&set, &options()).unwrap();
    assert_eq!(extracted.terms().len(), 2);
    for term in extracted.terms() {
        assert_eq!(term.locations()[0].origin().addon(), term.addon());
    }
}

#[test]
fn source_fallback_and_global_precedence_are_explicit() {
    let set = one(
        r#"<templates><t t-name="web.A"><p>Missing</p><p>Global</p><p>Local</p></t></templates>"#,
    );
    let dictionary = catalogue(&[("web", "Local", "Module")])
        .with_global_term("Global", "Partage")
        .unwrap()
        .with_global_term("Local", "Global shadow")
        .unwrap();
    let result = compile_localized(&set, &options(), &dictionary).unwrap();
    let js = result.modules()[0].code();
    for literal in ["Missing", "Partage", "Module"] {
        assert!(js.contains(literal), "{js}");
    }
    assert!(!js.contains("Global shadow"));
}

#[test]
fn catalogue_rejects_invalid_languages_and_conflicting_duplicates() {
    let error = TranslationCatalogue::new("../fr").unwrap_err();
    assert!(matches!(error, TranslationError::InvalidLanguage));
    assert_eq!(
        error.to_string(),
        "invalid translation language: expected 1..64 ASCII letters/digits separated by '_' or '-'"
    );
    let dictionary = catalogue(&[("web", "Hello", "Bonjour")]);
    assert_eq!(dictionary.language(), "fr_FR");
    assert_eq!(
        dictionary
            .clone()
            .with_term(AddonName::new("web").unwrap(), "Hello", "Bonjour")
            .unwrap(),
        dictionary
    );
    let error = dictionary
        .with_term(AddonName::new("web").unwrap(), "Hello", "Different")
        .unwrap_err();
    assert!(matches!(error, TranslationError::ConflictingTerm { .. }));
    assert_eq!(
        error.to_string(),
        "conflicting translation for addon 'web', source 'Hello'"
    );
}

#[test]
fn catalogue_bounds_sources_before_storing_them() {
    let error = TranslationCatalogue::new("fr_FR")
        .unwrap()
        .with_term(AddonName::new("web").unwrap(), &"a".repeat(65_537), "b")
        .unwrap_err();
    assert!(matches!(error, TranslationError::Limit { .. }));
    assert_eq!(error.to_string(), "translation source exceeds 65536 bytes");
}

#[test]
fn catalogue_rejects_overlong_translations() {
    let error = TranslationCatalogue::new("fr_FR")
        .unwrap()
        .with_global_term("Hello", &"b".repeat(65_537))
        .unwrap_err();
    assert!(matches!(error, TranslationError::Limit { .. }));
    assert_eq!(error.to_string(), "translation value exceeds 65536 bytes");
}

#[test]
fn catalogue_has_an_aggregate_admission_budget() {
    let mut dictionary = TranslationCatalogue::new("fr_FR").unwrap();
    let value = "x".repeat(65_536);
    let mut sequence = 0;
    let error = loop {
        assert!(
            sequence < 257,
            "catalogue failed to enforce its aggregate byte cap"
        );
        // Unique source keys ensure this exercises aggregate accounting rather than conflicts.
        let key = format!("term_{sequence}");
        sequence += 1;
        match dictionary.with_global_term(&key, &value) {
            Ok(next) => dictionary = next,
            Err(error) => break error,
        }
    };
    assert!(matches!(error, TranslationError::Limit { .. }));
    assert_eq!(
        error.to_string(),
        "translation catalogue exceeds 16777216 charged bytes"
    );
}

#[test]
fn catalogue_has_a_term_count_budget() {
    let mut dictionary = TranslationCatalogue::new("fr_FR").unwrap();
    for index in 0..100_000 {
        dictionary = dictionary
            .with_global_term(&format!("term_{index}"), "v")
            .unwrap();
    }
    let error = dictionary
        .with_global_term("one_too_many", "v")
        .unwrap_err();
    assert!(matches!(error, TranslationError::Limit { .. }));
    assert_eq!(
        error.to_string(),
        "translation catalogue exceeds 100000 terms"
    );
}

#[test]
fn caller_scope_specialization_is_refused_explicitly() {
    let set = sources(&[
        (
            "web",
            "static/src/caller.xml",
            r#"<templates><t t-name="web.A" t-translation-context="other"><t t-call="other.Callee"/></t></templates>"#,
        ),
        (
            "other",
            "static/src/callee.xml",
            r#"<templates><t t-name="other.Callee"><p>Hello</p></t></templates>"#,
        ),
    ]);
    let errors = compile_localized(&set, &options(), &catalogue(&[])).unwrap_err();
    assert_eq!(errors.errors()[0].kind(), DiagnosticKind::NotSupportedUntil);
    assert_eq!(
        errors.errors()[0].message(),
        "caller translation context/off propagation into t-call is not supported until a later release"
    );
    // Existing source compilation retains its historical behavior.
    assert!(compile(&set, &options()).is_ok());
}

#[test]
fn repeated_translation_expansion_cannot_bypass_semantic_admission() {
    let xml = format!(
        "<templates><t t-name=\"web.A\"><section>{}</section></t></templates>",
        "<p>Hello</p>".repeat(32)
    );
    let options = options().with_limits(
        erp_ui_compiler::CompilerLimits::default()
            .lowered(erp_ui_compiler::LimitKind::OutputBytes, 8192)
            .unwrap(),
    );
    let set = one(&xml);
    assert!(
        compile(&set, &options).is_ok(),
        "untranslated source must be admitted"
    );
    assert!(
        compile_localized(&set, &options, &catalogue(&[])).is_ok(),
        "occurrence metadata alone must be admitted"
    );
    let errors = compile_localized(
        &set,
        &options,
        &catalogue(&[("web", "Hello", &"x".repeat(512))]),
    )
    .unwrap_err();
    let error = &errors.errors()[0];
    assert_eq!(error.kind(), DiagnosticKind::Limit);
    assert_eq!(
        error.limit().unwrap().kind(),
        erp_ui_compiler::LimitKind::OutputBytes
    );
    assert_eq!(
        error.message(),
        "output_bytes limit exceeded: maximum 8192, got 8193"
    );
}

#[test]
fn inherited_translation_validation_error_names_the_extension_source() {
    let set = sources(&[
        (
            "web",
            "static/src/a.xml",
            r#"<templates><t t-name="web.A"><div/></t></templates>"#,
        ),
        (
            "other",
            "static/src/b.xml",
            r#"<templates><t t-inherit="web.A" t-inherit-mode="extension"><xpath expr="//div" position="inside"><span t-translation-context="../bad">Hello</span></xpath></t></templates>"#,
        ),
    ]);
    let errors = compile_localized(&set, &options(), &catalogue(&[])).unwrap_err();
    let error = &errors.errors()[0];
    assert_eq!(error.kind(), DiagnosticKind::InvalidDirectiveValue);
    assert_eq!(error.location().unwrap().origin().addon().as_str(), "other");
    assert_eq!(
        error.location().unwrap().origin().path().as_str(),
        "static/src/b.xml"
    );
    assert_eq!(
        error.message(),
        "t-translation-context=\"../bad\": expected an addon name (inheritance spec 1 at other/static/src/b.xml:1:60; node <span> at other/static/src/b.xml:1:98)"
    );
}

#[test]
fn inherited_translation_attribute_error_names_the_extension_source() {
    let set = sources(&[
        (
            "web",
            "static/src/a.xml",
            r#"<templates><t t-name="web.A"><div/></t></templates>"#,
        ),
        (
            "other",
            "static/src/b.xml",
            r#"<templates><t t-inherit="web.A" t-inherit-mode="extension"><xpath expr="//div" position="inside"><span title="Hello" t-translation-context-title="../bad">Hello</span></xpath></t></templates>"#,
        ),
    ]);
    let errors = compile_localized(&set, &options(), &catalogue(&[])).unwrap_err();
    let error = &errors.errors()[0];
    assert_eq!(error.kind(), DiagnosticKind::InvalidDirectiveValue);
    assert_eq!(error.location().unwrap().origin().addon().as_str(), "other");
    assert_eq!(
        error.location().unwrap().origin().path().as_str(),
        "static/src/b.xml"
    );
    assert_eq!(
        error.message(),
        "t-translation-context-title=\"../bad\": expected an addon name (inheritance spec 1 at other/static/src/b.xml:1:60; node <span> at other/static/src/b.xml:1:98)"
    );
}

#[test]
fn inherited_translation_specialization_error_names_the_extension_source() {
    let set = sources(&[
        (
            "web",
            "static/src/a.xml",
            r#"<templates><t t-name="web.A"><div/></t><t t-name="web.Target"><p>Hello</p></t></templates>"#,
        ),
        (
            "other",
            "static/src/b.xml",
            r#"<templates><t t-inherit="web.A" t-inherit-mode="extension"><xpath expr="//div" position="inside"><t t-call="web.Target" t-translation-context="other"/></xpath></t></templates>"#,
        ),
    ]);
    let errors = compile_localized(&set, &options(), &catalogue(&[])).unwrap_err();
    let error = &errors.errors()[0];
    assert_eq!(error.kind(), DiagnosticKind::NotSupportedUntil);
    assert_eq!(error.location().unwrap().origin().addon().as_str(), "other");
    assert_eq!(
        error.location().unwrap().origin().path().as_str(),
        "static/src/b.xml"
    );
    assert_eq!(
        error.message(),
        "caller translation context/off propagation into t-call is not supported until a later release (inheritance spec 1 at other/static/src/b.xml:1:60; node <t> at other/static/src/b.xml:1:98)"
    );
}

#[test]
fn mutated_inherited_literal_refuses_unproven_ownership() {
    let set = sources(&[
        (
            "web",
            "static/src/a.xml",
            r#"<templates><t t-name="web.A"><p title="Old">Stable</p></t></templates>"#,
        ),
        (
            "other",
            "static/src/b.xml",
            r#"<templates><t t-inherit="web.A" t-inherit-mode="extension"><xpath expr="//p" position="attributes"><attribute name="title">New</attribute></xpath></t></templates>"#,
        ),
    ]);
    let errors = compile_localized(&set, &options(), &catalogue(&[])).unwrap_err();
    let error = &errors.errors()[0];
    assert_eq!(error.kind(), DiagnosticKind::Inheritance);
    let expected = "translation literal has no unique inherited source owner (inheritance source unresolved; candidates: inheritance spec n/a (declaration); node <t> at web/static/src/a.xml:1:12 | inheritance spec 1 at other/static/src/b.xml:1:60; node <xpath> at other/static/src/b.xml:1:60)";
    assert_eq!(error.message(), expected);
    assert!(
        error.location().is_none(),
        "unproven source must not be reported as exact"
    );
    let extraction_errors = extract_translations(&set, &options()).unwrap_err();
    assert_eq!(
        extraction_errors.errors()[0].kind(),
        DiagnosticKind::Inheritance
    );
    assert_eq!(extraction_errors.errors()[0].message(), expected);
    assert!(
        compile(&set, &options()).is_ok(),
        "source compilation must retain its old behavior"
    );
}

#[test]
fn malicious_catalogue_values_are_not_generated_as_executable_code() {
    let set = one(r#"<templates><t t-name="web.A"><p title="Hint">Hello</p></t></templates>"#);
    let payload =
        "</p><script>globalThis.injected=true</script><p>\"` ${globalThis.injected=true} ";
    let result = code(&set, &[("web", "Hello", payload), ("web", "Hint", payload)]);
    assert!(
        !result.contains("<script>"),
        "raw markup breakout: {result}"
    );
    assert!(
        !result.contains("globalThis.injected=true;"),
        "executable breakout: {result}"
    );
    // Native browser acceptance separately asserts DOM text, attributes and no execution.
}

#[test]
fn malformed_templates_are_rejected_by_extraction() {
    let errors = extract_translations(
        &one(r#"<templates><t t-name="web.A" t-translation="on">Hello</t></templates>"#),
        &options(),
    )
    .unwrap_err();
    assert_eq!(
        errors.errors()[0].kind(),
        DiagnosticKind::InvalidDirectiveValue
    );
    assert_eq!(
        errors.errors()[0].message(),
        "t-translation=\"on\": the only value is \"off\""
    );
}

fn review_inherited(body: &str, operation: &str) -> TemplateSet {
    sources(&[
        (
            "web",
            "static/src/a.xml",
            &format!(r#"<templates><t t-name="web.A">{body}</t></templates>"#),
        ),
        (
            "other",
            "static/src/b.xml",
            &format!(
                r#"<templates><t t-inherit="web.A" t-inherit-mode="extension">{operation}</t></templates>"#
            ),
        ),
    ])
}

#[test]
fn review_r1_raw_inherited_text_never_uses_containing_element_owner() {
    for (body, operation) in [
        ("<p/>", r#"<xpath expr="//p">Hello</xpath>"#),
        (
            "<p/>",
            r#"<xpath expr="//p" position="inside"><i/>Hello</xpath>"#,
        ),
        (
            "<p>Old</p>",
            r#"<xpath expr="//p" position="inside">Hello</xpath>"#,
        ),
        (
            "<p>Hello</p>",
            r#"<xpath expr="//p" position="replace" mode="inner">Hello</xpath>"#,
        ),
        (
            "<p/>",
            "<xpath expr=\"//p\" position=\"inside\">\u{a0}</xpath>",
        ),
        (
            "<p/>",
            r#"<data><xpath expr="//p" position="inside">Hello</xpath></data>"#,
        ),
        (
            "<div><i/>Hello<p>Stable</p></div>",
            r#"<xpath expr="//i" position="replace"><b/></xpath>"#,
        ),
        (
            "<div><p>Hello</p></div>",
            r#"<xpath expr="//p" position="replace"><section>$0</section></xpath>"#,
        ),
        (
            "<div><p>Hello</p><b/></div>",
            r#"<xpath expr="//b" position="inside"><xpath expr="//p" position="move"/></xpath>"#,
        ),
    ] {
        let set = review_inherited(body, operation);
        assert!(
            compile(&set, &options()).is_ok(),
            "source fixture: {operation}"
        );
        let terms = catalogue(&[
            ("web", "Hello", "WRONG_ROOT"),
            ("other", "Hello", "CONTRIBUTOR"),
        ]);
        let localized = compile_localized(&set, &options(), &terms).unwrap_err();
        let extracted = extract_translations(&set, &options()).unwrap_err();
        for errors in [&localized, &extracted] {
            let error = &errors.errors()[0];
            assert_eq!(error.kind(), DiagnosticKind::Inheritance, "{operation}");
            let column = 60 + operation.find("<xpath").unwrap();
            let expected = format!(
                "translation text ownership after inheritance mutation is not supported until a later release (inheritance source unresolved; candidates: inheritance spec n/a (declaration); node <t> at web/static/src/a.xml:1:12 | inheritance spec 1 at other/static/src/b.xml:1:{column}; node <xpath> at other/static/src/b.xml:1:{column})"
            );
            assert_eq!(error.message(), expected);
            assert!(error.location().is_none());
        }
        assert_eq!(
            localized.errors()[0].message(),
            extracted.errors()[0].message()
        );
    }
}

#[test]
fn review_r2_translation_cannot_erase_source_branch_errors() {
    let body = r#"<p t-if="this.ok">Yes</p>BAD<span t-else="">No</span>"#;
    let direct = one(&format!(
        r#"<templates><t t-name="web.A">{body}</t></templates>"#
    ));
    let inherited = review_inherited(
        "<div/>",
        &format!(r#"<xpath expr="//div" position="inside"><section>{body}</section></xpath>"#),
    );
    for set in [&direct, &inherited] {
        let original = compile(set, &options()).unwrap_err();
        let expected = original
            .errors()
            .iter()
            .find(|e| e.kind() == DiagnosticKind::TextBetweenBranches)
            .unwrap();
        assert!(
            expected
                .message()
                .starts_with("text is not allowed between branching directives")
        );
        for replacement in ["", " \t\n"] {
            let dictionary =
                catalogue(&[("web", "BAD", replacement), ("other", "BAD", replacement)]);
            let localized = compile_localized(set, &options(), &dictionary).unwrap_err();
            let extracted = extract_translations(set, &options()).unwrap_err();
            for errors in [&localized, &extracted] {
                let error = errors
                    .errors()
                    .iter()
                    .find(|e| e.kind() == expected.kind())
                    .unwrap();
                assert_eq!(error.message(), expected.message());
                assert_eq!(error.location(), expected.location());
            }
        }
    }
}

#[test]
fn review_r2_translation_cannot_erase_html_structure_admission() {
    let set = one(
        r#"<templates><t t-name="web.A"><table>BAD<tbody><tr><td>Good</td></tr></tbody></table></t></templates>"#,
    );
    let original = compile(&set, &options()).unwrap_err();
    let expected = original
        .errors()
        .iter()
        .find(|e| e.kind() == DiagnosticKind::HtmlStructure)
        .unwrap();
    for replacement in ["", " \t\n"] {
        let localized =
            compile_localized(&set, &options(), &catalogue(&[("web", "BAD", replacement)]))
                .unwrap_err();
        let extracted = extract_translations(&set, &options()).unwrap_err();
        for errors in [&localized, &extracted] {
            let error = errors
                .errors()
                .iter()
                .find(|e| e.kind() == expected.kind())
                .unwrap();
            assert_eq!(error.message(), expected.message());
        }
    }
}

#[test]
fn review_r2_valid_empty_and_blank_translations_remain_supported() {
    let set = one(r#"<templates><t t-name="web.A"><p>Hello</p></t></templates>"#);
    for replacement in ["", " \t\n"] {
        assert!(
            compile_localized(
                &set,
                &options(),
                &catalogue(&[("web", "Hello", replacement)])
            )
            .is_ok()
        );
    }
}

#[test]
fn review_r3_attribute_writes_cannot_reuse_an_equal_original_signature() {
    for (body, operations) in [
        (
            r#"<p title="Hello"/>"#,
            r#"<xpath expr="//p" position="attributes"><attribute name="title">Hello</attribute></xpath>"#,
        ),
        (
            r#"<Foo label.translate="Hello"/>"#,
            r#"<xpath expr="//Foo" position="attributes"><attribute name="label.translate">Hello</attribute></xpath>"#,
        ),
        (
            r#"<p title="Hello"/>"#,
            r#"<xpath expr="//p" position="attributes"><attribute name="title">Changed</attribute></xpath><xpath expr="//p" position="attributes"><attribute name="title">Hello</attribute></xpath>"#,
        ),
        (
            r#"<p title="Hello"/>"#,
            r#"<xpath expr="//p" position="attributes"><attribute name="title"/></xpath><xpath expr="//p" position="attributes"><attribute name="title">Hello</attribute></xpath>"#,
        ),
        (
            r#"<p title="Hello"/>"#,
            r#"<data><xpath expr="//p" position="attributes"><attribute name="title">Hello</attribute></xpath></data>"#,
        ),
        (
            r#"<t t-call="web.Target" label.translate="Hello"/>"#,
            r#"<xpath expr="//t[@t-call='web.Target']" position="attributes"><attribute name="label.translate">Hello</attribute></xpath>"#,
        ),
    ] {
        let mut set = review_inherited(body, operations);
        if body.contains("t-call") {
            set.push(TemplateSource::addon_file(
                AddonName::new("web").unwrap(),
                TemplatePath::new("static/src/target.xml").unwrap(),
                r#"<templates><t t-name="web.Target"><p>Stable</p></t></templates>"#,
            ));
        }
        assert!(
            compile(&set, &options()).is_ok(),
            "source fixture: {operations}"
        );
        let dictionary = catalogue(&[
            ("web", "Hello", "WRONG_ROOT"),
            ("other", "Hello", "CONTRIBUTOR"),
        ]);
        let localized = compile_localized(&set, &options(), &dictionary).unwrap_err();
        let extracted = extract_translations(&set, &options()).unwrap_err();
        let mut expected = "translation attribute ownership after inheritance mutation is not supported until a later release (inheritance source unresolved; candidates: inheritance spec n/a (declaration); node <t> at web/static/src/a.xml:1:12".to_owned();
        for (index, (offset, _)) in operations.match_indices("<xpath").enumerate() {
            let column = 60 + offset;
            expected.push_str(&format!(" | inheritance spec {} at other/static/src/b.xml:1:{column}; node <xpath> at other/static/src/b.xml:1:{column}", index + 1));
        }
        expected.push(')');
        for errors in [&localized, &extracted] {
            let error = &errors.errors()[0];
            assert_eq!(error.kind(), DiagnosticKind::Inheritance);
            assert_eq!(error.message(), expected);
            assert!(error.location().is_none());
        }
    }
}

#[test]
fn review_r3_attribute_restoration_across_extensions_is_not_proven_original() {
    let set = sources(&[
        (
            "web",
            "static/src/a.xml",
            r#"<templates><t t-name="web.A"><p title="Hello"/></t></templates>"#,
        ),
        (
            "other",
            "static/src/b.xml",
            r#"<templates><t t-inherit="web.A" t-inherit-mode="extension"><xpath expr="//p" position="attributes"><attribute name="title">Changed</attribute></xpath></t></templates>"#,
        ),
        (
            "third",
            "static/src/c.xml",
            r#"<templates><t t-inherit="web.A" t-inherit-mode="extension"><xpath expr="//p" position="attributes"><attribute name="title">Hello</attribute></xpath></t></templates>"#,
        ),
    ]);
    let original = compile(&set, &options()).unwrap();
    assert!(original.modules()[0].code().contains("Hello"));
    assert!(!original.modules()[0].code().contains("Changed"));
    let dictionary = catalogue(&[
        ("web", "Hello", "WRONG_ROOT"),
        ("third", "Hello", "CONTRIBUTOR"),
    ]);
    let localized = compile_localized(&set, &options(), &dictionary).unwrap_err();
    let extracted = extract_translations(&set, &options()).unwrap_err();
    let expected = "translation attribute ownership after inheritance mutation is not supported until a later release (inheritance source unresolved; candidates: inheritance spec n/a (declaration); node <t> at web/static/src/a.xml:1:12 | inheritance spec 1 at other/static/src/b.xml:1:60; node <xpath> at other/static/src/b.xml:1:60 | inheritance spec 1 at third/static/src/c.xml:1:60; node <xpath> at third/static/src/c.xml:1:60)";
    for errors in [&localized, &extracted] {
        let error = &errors.errors()[0];
        assert_eq!(error.kind(), DiagnosticKind::Inheritance);
        assert_eq!(error.message(), expected);
        assert!(error.location().is_none());
    }
}

#[test]
fn catalogue_rejects_nul_before_html_can_silently_alter_the_literal() {
    for (source, value, subject, offset) in [
        ("Hello", "A\0B", "value", 1),
        ("A\0B", "Bonjour", "source", 1),
        ("Hello", "\0", "value", 0),
        ("é\0", "Bonjour", "source", 2),
    ] {
        let module = TranslationCatalogue::new("fr_FR")
            .unwrap()
            .with_term(AddonName::new("web").unwrap(), source, value)
            .unwrap_err();
        let global = TranslationCatalogue::new("fr_FR")
            .unwrap()
            .with_global_term(source, value)
            .unwrap_err();
        for error in [&module, &global] {
            match error {
                TranslationError::Nul {
                    subject: actual_subject,
                    offset: actual_offset,
                } => {
                    assert_eq!(*actual_subject, subject);
                    assert_eq!(*actual_offset, offset);
                }
                other => panic!("expected typed Nul error, got {other:?}"),
            }
            assert_eq!(
                error.to_string(),
                format!("translation {subject} contains a NUL byte at offset {offset}")
            );
        }
    }
}
