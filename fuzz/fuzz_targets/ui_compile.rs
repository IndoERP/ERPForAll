#![no_main]

use erp_ui_compiler::{
    AddonName, BuildMode, CompileOptions, TemplatePath, TemplateSet, TemplateSource, analyze,
    compile,
};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(xml) = std::str::from_utf8(data) else {
        return;
    };

    let addon = AddonName::new("web").expect("valid constant addon name 'web'");
    let path = TemplatePath::new("fuzz.xml").expect("valid constant template path 'fuzz.xml'");

    // 1. Fuzz raw XML input directly
    fuzz_xml(&addon, &path, xml);

    // 2. If the input does not already contain <templates> tags, test wrapped as a template body
    if !xml.contains("<templates") && !xml.contains("</templates>") {
        let wrapped = format!("<templates><t t-name=\"web.Fuzz\">{xml}</t></templates>");
        fuzz_xml(&addon, &path, &wrapped);
    }
});

fn fuzz_xml(addon: &AddonName, path: &TemplatePath, xml: &str) {
    let mut set = TemplateSet::new();
    set.push(TemplateSource::addon_file(addon.clone(), path.clone(), xml));

    for mode in [BuildMode::Dev, BuildMode::Prod] {
        let options = CompileOptions::new(mode);

        // Analyze must never panic and must be deterministic
        let analysis1 = analyze(&set, &options);
        let mut canon1 = String::new();
        analysis1.write_canonical(&mut canon1);

        let analysis2 = analyze(&set, &options);
        let mut canon2 = String::new();
        analysis2.write_canonical(&mut canon2);
        assert_eq!(
            canon1, canon2,
            "analyze canonical representation must be deterministic"
        );

        // Compile must never panic and must be deterministic
        let outcome1 = compile(&set, &options);
        let mut comp1 = String::new();
        match &outcome1 {
            Ok(c) => c.write_canonical(&mut comp1),
            Err(e) => e.write_canonical(&mut comp1),
        }

        let outcome2 = compile(&set, &options);
        let mut comp2 = String::new();
        match &outcome2 {
            Ok(c) => c.write_canonical(&mut comp2),
            Err(e) => e.write_canonical(&mut comp2),
        }
        assert_eq!(
            comp1, comp2,
            "compile canonical representation must be deterministic"
        );
    }
}
