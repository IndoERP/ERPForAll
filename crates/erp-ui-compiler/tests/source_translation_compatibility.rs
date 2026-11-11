//! condition: original compilation must keep its pre-localization canonical output.
use erp_ui_compiler::{
    AddonName, BuildMode, CompileOptions, TemplatePath, TemplateSet, TemplateSource, compile,
};

#[test]
fn source_compilation_keeps_pre_localization_output() {
    let inputs: Vec<(String, String, String)> =
        serde_json::from_str(include_str!("fixtures/translation-source-inputs.json")).unwrap();
    let mut set = TemplateSet::new();
    for (addon, path, xml) in inputs {
        set.push(TemplateSource::addon_file(
            AddonName::new(&addon).unwrap(),
            TemplatePath::new(&path).unwrap(),
            xml,
        ));
    }
    let mut actual = String::new();
    compile(&set, &CompileOptions::new(BuildMode::Prod))
        .unwrap()
        .write_canonical(&mut actual);
    assert_eq!(
        actual,
        include_str!("fixtures/translation-source-baseline.txt")
    );
}
