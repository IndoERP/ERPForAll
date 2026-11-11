//! Compile and compose generated-column declarations through the public addon macro surface.
#![allow(clippy::unwrap_used, reason = "acceptance tests")]

use erp_registry::GeneratedStorage;
use erp_sdk::{AddonDescriptor, RegistryBuilder, model};

erp_sdk::addon!(name = "test_sqlfields_macro", depends = ["base"]);

/// Addon fixture exercising every storage spelling on real macro output.
#[model(name = "sample.generated_macro")]
pub struct GeneratedMacro {
    #[field(integer)]
    pub value: i32,
    #[field(integer, generated = "value + 1")]
    pub auto_total: i32,
    #[field(integer, generated = "value + 2", generated_storage = "virtual")]
    pub virtual_total: i32,
    #[field(integer, generated = "value + 3", generated_storage = "stored")]
    pub stored_total: i32,
}

#[test]
fn generated_storage_macro_compiles_and_composes() {
    static BASE: AddonDescriptor = AddonDescriptor {
        name: "base",
        depends: &[],
        exact_only: false,
    };
    let registry = RegistryBuilder::new([&BASE, &ADDON]).build().unwrap();
    let model = registry.model("sample.generated_macro").unwrap();
    let source = model.field("value").unwrap();
    for (name, storage, expression) in [
        ("auto_total", GeneratedStorage::Virtual, "value + 1"),
        ("virtual_total", GeneratedStorage::Virtual, "value + 2"),
        ("stored_total", GeneratedStorage::Stored, "value + 3"),
    ] {
        let field = model.field(name).unwrap();
        let generated = field.generated().unwrap();
        assert_eq!(generated.storage(), storage);
        assert_eq!(generated.source(), expression);
        assert_eq!(generated.dependencies(), &[source.id]);
        assert!(field.has_column());
        assert!(field.readonly);
        assert!(!field.is_assignment_eligible());
    }
}
