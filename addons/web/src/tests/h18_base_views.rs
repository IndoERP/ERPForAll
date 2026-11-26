// SPDX-License-Identifier: MIT OR Apache-2.0
// The views the base addon ships, lowered as the web client asks for them; included into the `h18_lower` test module.

/// Each primary view of a base `*_views.xml` data file as (file, xml id, model, arch).
fn base_primary_views() -> Vec<(String, String, String, String)> {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../base/data");
    let mut files: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.to_string_lossy().ends_with("_views.xml"))
        .collect();
    files.sort();
    assert!(!files.is_empty(), "no view files under {}", dir.display());
    let mut views = Vec::new();
    for path in files {
        let text = std::fs::read_to_string(&path).unwrap();
        let document = roxmltree::Document::parse(&text).unwrap();
        let file = path.file_name().unwrap().to_string_lossy().into_owned();
        for record in document.descendants().filter(|node| {
            node.has_tag_name("record") && node.attribute("model") == Some("ir.ui.view")
        }) {
            let field = |name: &str| {
                record
                    .children()
                    .find(|node| node.has_tag_name("field") && node.attribute("name") == Some(name))
            };
            // An extension view is lowered combined with the view it extends, not on its own.
            if field("inherit_id").is_some() {
                continue;
            }
            let id = record.attribute("id").unwrap_or_default().to_owned();
            let model = field("model")
                .and_then(|node| node.text())
                .unwrap()
                .trim()
                .to_owned();
            let root = field("arch")
                .unwrap()
                .children()
                .find(|node| node.is_element())
                .unwrap();
            views.push((file.clone(), id, model, text[root.range()].to_owned()));
        }
    }
    views
}

/// Kernel models the registry crate declares itself: a fixture registry of addons does not hold them.
const KERNEL_MODELS: [&str; 2] = ["ir.model.data", "ir.module.module"];

#[test]
fn every_base_primary_view_is_admitted_by_the_web_client() {
    let views = base_primary_views();
    let registry = RegistryBuilder::new([&erp_addon_base::ADDON, &crate::ADDON])
        .build()
        .unwrap();
    let refused: Vec<String> = views
        .iter()
        .filter_map(|(file, id, model, arch)| {
            if KERNEL_MODELS.contains(&model.as_str()) {
                return None;
            }
            if registry.model(model).is_none() {
                return Some(format!(
                    "{file} {id}: model {model} is not in the base and web registry"
                ));
            }
            let kind = roxmltree::Document::parse(arch)
                .unwrap()
                .root_element()
                .tag_name()
                .name()
                .to_owned();
            fixture_model(model, arch, &kind, &DescriptionBudget::new())
                .err()
                .map(|error| format!("{file} {id} ({model} {kind}): {error}"))
        })
        .collect();
    assert!(
        refused.is_empty(),
        "{} of {} base views are refused:\n{}",
        refused.len(),
        views.len(),
        refused.join("\n")
    );
}
