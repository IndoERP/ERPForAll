//! Manifest and default data verification for addon `uom`.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

use std::fs;
use std::path::Path;

#[test]
fn uom_manifest_and_access_rules_are_valid() {
    let manifest_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("manifest.toml");
    let content = fs::read_to_string(&manifest_path).expect("uom manifest.toml");
    let parsed: toml::Table = toml::from_str(&content).expect("parse manifest.toml");

    assert_eq!(
        parsed.get("name").and_then(|v| v.as_str()),
        Some("Units of Measure")
    );
    assert_eq!(
        parsed.get("license").and_then(|v| v.as_str()),
        Some("MIT OR Apache-2.0")
    );
    let depends = parsed
        .get("depends")
        .and_then(|v| v.as_array())
        .expect("depends list");
    assert!(depends.iter().any(|d| d.as_str() == Some("base")));

    let data_files = parsed
        .get("data")
        .and_then(|v| v.as_array())
        .expect("data files list");
    let file_names: Vec<&str> = data_files.iter().filter_map(|v| v.as_str()).collect();
    assert!(file_names.contains(&"security/ir.access.csv"));
    assert!(file_names.contains(&"data/uom_data.xml"));

    let access_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("security/ir.access.csv");
    let access = fs::read_to_string(&access_path).expect("uom security/ir.access.csv");
    assert!(
        access.contains(
            "access_uom_uom_manager,uom.uom.manager,model_uom_uom,base.group_system,crud,"
        )
    );
    assert!(access.contains("access_uom_uom_user,uom.uom.user,model_uom_uom,base.group_user,r,"));
}

#[test]
fn uom_default_data_declares_precision_and_thirty_standard_units() {
    let data_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("data/uom_data.xml");
    let content = fs::read_to_string(&data_path).expect("uom data/uom_data.xml");

    // Must declare decimal.precision 'Product Unit' with digits 2
    assert!(content.contains(r#"id="decimal_product_uom""#));
    assert!(content.contains(r#"<field name="name">Product Unit</field>"#));
    assert!(
        content.contains(r#"<field name="digits" eval="2"/>"#)
            || content.contains(r#"<field name="digits">2</field>"#)
    );

    // Must declare all standard units across dimensions
    let expected_units = [
        // Units
        "product_uom_unit",
        "product_uom_pack_6",
        "product_uom_dozen",
        // Working Time
        "product_uom_hour",
        "product_uom_day",
        "product_uom_minute",
        // Length
        "product_uom_millimeter",
        "product_uom_cm",
        "product_uom_meter",
        "product_uom_km",
        // Surface
        "product_uom_square_meter",
        // Volume
        "product_uom_milliliter",
        "product_uom_litre",
        "product_uom_cubic_meter",
        // Weight
        "product_uom_gram",
        "product_uom_kgm",
        "product_uom_ton",
        // American units
        "product_uom_oz",
        "product_uom_lb",
        "product_uom_inch",
        "product_uom_foot",
        "product_uom_yard",
        "product_uom_mile",
        "product_uom_square_foot",
        "product_uom_floz",
        "product_uom_qt",
        "product_uom_gal",
        "product_uom_cubic_inch",
        "product_uom_cubic_foot",
        // Energy
        "product_uom_kwh",
    ];

    assert_eq!(expected_units.len(), 30, "exactly 30 standard units");
    for unit_id in expected_units {
        let needle = format!(r#"id="{unit_id}""#);
        assert!(content.contains(&needle), "missing expected unit {unit_id}");
    }
}
