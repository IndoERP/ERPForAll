//! The compiled native addon receipt is available without runtime source-tree trust.

include!(concat!(env!("OUT_DIR"), "/linked_addon_identities.rs"));

#[test]
fn linked_addon_receipts_are_per_addon_and_versioned() {
    let names: Vec<_> = LINKED_ADDON_IDENTITIES
        .iter()
        .map(|(name, _)| *name)
        .collect();
    assert_eq!(
        names,
        [
            "acceptance_notes",
            "base",
            "barcodes",
            "mail",
            "product",
            "product_mail",
            "theme_office_blue",
            "uom",
            "web"
        ]
    );
    for (_, identity) in LINKED_ADDON_IDENTITIES {
        let encoded = identity
            .strip_prefix("addon-v1:")
            .expect("versioned receipt");
        assert_eq!(encoded.len(), 64);
        assert!(
            encoded
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        );
    }
    assert_ne!(LINKED_ADDON_IDENTITIES[0].1, LINKED_ADDON_IDENTITIES[1].1);
}

#[test]
fn compiled_native_receipts_match_the_admitted_source_bytes() {
    let workspace = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap();
    for (name, identity) in LINKED_ADDON_IDENTITIES {
        let snapshot = erp_modules::artifact_identity::ArtifactSnapshot::capture(
            name,
            &workspace.join("addons").join(name),
        )
        .unwrap();
        assert_eq!(
            snapshot.digest(),
            *identity,
            "compiled source receipt for {name}"
        );
        snapshot.verify_unchanged().unwrap();
    }
}
