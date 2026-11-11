//! Capture canonical source compilation before localization implementation; a test oracle only.
use erp_ui_compiler::{
    AddonName, BuildMode, CompileOptions, TemplatePath, TemplateSet, TemplateSource, compile,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let inputs: Vec<(String, String, String)> = serde_json::from_str(include_str!(
        "../tests/fixtures/translation-source-inputs.json"
    ))?;
    let mut set = TemplateSet::new();
    for (addon, path, xml) in inputs {
        set.push(TemplateSource::addon_file(
            AddonName::new(&addon)?,
            TemplatePath::new(&path)?,
            xml,
        ));
    }
    let mut output = String::new();
    compile(&set, &CompileOptions::new(BuildMode::Prod))?.write_canonical(&mut output);
    print!("{output}");
    Ok(())
}
