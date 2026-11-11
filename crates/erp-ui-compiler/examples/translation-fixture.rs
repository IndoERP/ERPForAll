//! Emit real localized modules for the native-browser safety test; no serving path is added.
use erp_ui_compiler::{
    AddonName, BuildMode, CompileOptions, TemplatePath, TemplateSet, TemplateSource,
    TranslationCatalogue, compile_localized, extract_translations,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut set = TemplateSet::new();
    set.push(TemplateSource::addon_file(
        AddonName::new("web")?,
        TemplatePath::new("static/src/localized.xml")?,
        r#"<templates><t t-name="web.Main"><section><p id="translated" title="Hint">Hello</p><Child label.translate="Hello"/><span id="original">Same</span><div id="inherited"/></section></t><t t-name="web.Child"><output id="component" t-out="this.props.label"/></t></templates>"#,
    ));
    set.push(TemplateSource::addon_file(
        AddonName::new("other")?,
        TemplatePath::new("static/src/extension.xml")?,
        r#"<templates><t t-inherit="web.Main" t-inherit-mode="extension"><xpath expr="//*[@id='inherited']" position="inside"><b id="contributor">Same</b></xpath></t></templates>"#,
    ));
    let payload =
        "</p><script>globalThis.injected=true</script><p>\"` ${globalThis.injected=true} \u{2028}";
    let catalogue = TranslationCatalogue::new("fr_FR")?
        .with_term(AddonName::new("web")?, "Hello", payload)?
        .with_term(AddonName::new("web")?, "Hint", payload)?
        .with_term(AddonName::new("web")?, "Same", "Pareil")?
        .with_term(AddonName::new("other")?, "Same", "Autre")?;
    let options = CompileOptions::new(BuildMode::Prod);
    let compiled = compile_localized(&set, &options, &catalogue)?;
    let extracted = extract_translations(&set, &options)?;
    let modules: Vec<_> = compiled
        .modules()
        .iter()
        .map(|module| serde_json::json!({"name": module.name().as_str(), "code": module.code()}))
        .collect();
    let terms: Vec<_> = extracted.terms().iter()
        .map(|term| serde_json::json!({"addon": term.addon().as_str(), "source": term.source(), "locations": term.locations().iter().map(ToString::to_string).collect::<Vec<_>>()})).collect();
    println!(
        "{}",
        serde_json::json!({"modules": modules, "payload": payload, "terms": terms})
    );
    Ok(())
}
