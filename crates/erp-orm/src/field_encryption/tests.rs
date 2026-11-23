// SPDX-License-Identifier: MIT OR Apache-2.0
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

use erp_registry::{AddonDescriptor, FieldDecl, ModelDecl, Registry, RegistryBuilder};

use super::*;

/// Two keys, `k2` seals; the bytes are fixed test values, never a deployment's keys.
const TWO_KEYS: &str = "# test keys\n\
    k2 AgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgI=\n\
    \n\
    k1 AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE=\n";
const ONLY_K1: &str = "k1 AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE=\n";
const OTHER_K1: &str = "k1 AwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwM=\n";

fn keys(text: &str) -> FieldKeys {
    FieldKeys::parse(text).unwrap()
}

#[test]
fn a_sealed_value_opens_to_the_clear_text_and_does_not_contain_it() {
    let keys = keys(TWO_KEYS);
    let clear = "3201-0123-4567-CANARY";
    let envelope = keys
        .seal("hr.employee", "national_id", clear.as_bytes())
        .unwrap();
    assert!(
        !envelope.windows(clear.len()).any(|w| w == clear.as_bytes()),
        "the envelope holds the clear text"
    );
    assert_eq!(&envelope[..3], b"EF\x01");
    let opened = keys.open("hr.employee", "national_id", &envelope).unwrap();
    assert_eq!(opened.as_slice(), clear.as_bytes());
}

#[test]
fn every_seal_uses_a_new_nonce() {
    let keys = keys(TWO_KEYS);
    let a = keys.seal("m", "f", b"same").unwrap();
    let b = keys.seal("m", "f", b"same").unwrap();
    assert_ne!(a, b, "two seals of one value must differ");
}

#[test]
fn the_first_key_seals_and_every_key_opens() {
    let rotated = keys(TWO_KEYS);
    assert_eq!(rotated.sealing_id(), "k2");
    assert_eq!(rotated.ids().collect::<Vec<_>>(), ["k2", "k1"]);
    // a value sealed before the rotation (with k1 first) still opens after it
    let before = keys(ONLY_K1).seal("m", "f", b"old value").unwrap();
    assert_eq!(
        rotated.open("m", "f", &before).unwrap().as_slice(),
        b"old value"
    );
    // and a new value names k2
    let after = rotated.seal("m", "f", b"new value").unwrap();
    assert_eq!(&after[3..6], b"\x02k2");
}

#[test]
fn a_value_moved_to_another_field_or_model_does_not_open() {
    let keys = keys(TWO_KEYS);
    let envelope = keys.seal("hr.employee", "national_id", b"x").unwrap();
    for (model, field) in [
        ("hr.employee", "bank_account"),
        ("res.partner", "national_id"),
    ] {
        let error = keys.open(model, field, &envelope).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Database);
        assert!(
            error.message().contains("does not open"),
            "{}",
            error.message()
        );
    }
}

#[test]
fn altered_bytes_and_an_edited_key_id_are_refused() {
    let keys = keys(TWO_KEYS);
    let envelope = keys.seal("m", "f", b"value").unwrap();
    let mut flipped = envelope.clone();
    let last = flipped.len() - 1;
    flipped[last] ^= 1;
    assert!(keys.open("m", "f", &flipped).is_err());
    // k2 -> k1: the id is part of the associated data
    let mut renamed = envelope.clone();
    renamed[5] = b'1';
    let error = keys.open("m", "f", &renamed).unwrap_err();
    assert!(
        error.message().contains("does not open with the key 'k1'"),
        "{}",
        error.message()
    );
    assert!(
        keys.open("m", "f", &envelope[..envelope.len() - 1])
            .is_err()
    );
    assert!(keys.open("m", "f", &envelope[..20]).is_err());
}

#[test]
fn a_key_with_the_same_id_but_other_bytes_does_not_open() {
    let envelope = keys(ONLY_K1).seal("m", "f", b"value").unwrap();
    let error = keys(OTHER_K1).open("m", "f", &envelope).unwrap_err();
    assert!(
        error.message().contains("does not open"),
        "{}",
        error.message()
    );
}

#[test]
fn an_unknown_key_and_clear_text_are_refused_by_name() {
    let envelope = keys(TWO_KEYS).seal("m", "f", b"value").unwrap();
    let error = keys(ONLY_K1).open("m", "f", &envelope).unwrap_err();
    assert!(
        error.message().contains("sealed with the key 'k2'"),
        "{}",
        error.message()
    );
    let error = keys(ONLY_K1).open("m", "f", b"clear text").unwrap_err();
    assert!(
        error.message().contains("m.f is not an encrypted value"),
        "{}",
        error.message()
    );
    assert!(!error.message().contains("clear text"));
}

#[test]
fn cells_round_trip_and_null_stays_null() {
    let keys = keys(TWO_KEYS);
    assert_eq!(keys.seal_cell("m", "f", Cell::Null).unwrap(), Cell::Null);
    let sealed = keys.seal_cell("m", "f", Cell::Str("é".into())).unwrap();
    assert!(matches!(sealed, Cell::Bytes(_)));
    assert_eq!(
        keys.open_cell("m", "f", sealed).unwrap(),
        Cell::Str("é".into())
    );
    assert!(keys.seal_cell("m", "f", Cell::Int(1)).is_err());
}

#[test]
fn key_files_are_checked_without_echoing_their_content() {
    for (text, expected) in [
        ("", "holds no key"),
        ("# only a comment\n", "holds no key"),
        ("k1\n", "line 1 is not"),
        ("k1 AQEB extra\n", "line 1 is not"),
        (
            "bad/id AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE=\n",
            "line 1: a key id",
        ),
        ("k1 not-base64!\n", "line 1: the key is not valid base64"),
        ("k1 AQEBAQ==\n", "line 1: a key is 32 bytes"),
        (
            "k1 AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE=\n\
             k1 AgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgI=\n",
            "line 2: key id 'k1' is listed twice",
        ),
    ] {
        let error = FieldKeys::parse(text).unwrap_err();
        assert!(
            error.message().contains(expected),
            "{text:?}: {}",
            error.message()
        );
        assert!(!error.message().contains("AQEB"), "{}", error.message());
    }
}

#[test]
fn debug_shows_key_ids_only() {
    let shown = format!("{:?}", keys(TWO_KEYS));
    assert!(shown.contains("k2") && shown.contains("k1"), "{shown}");
    assert!(
        !shown.contains("AgIC") && !shown.contains("AQEB"),
        "{shown}"
    );
}

#[test]
fn the_fingerprint_ignores_comments_and_tells_key_sets_apart() {
    let a = keys(ONLY_K1).fingerprint();
    assert_eq!(a, keys(&format!("# note\n\n{ONLY_K1}")).fingerprint());
    assert_ne!(a, keys(OTHER_K1).fingerprint());
    assert_ne!(a, keys(TWO_KEYS).fingerprint());
}

static ADDON: AddonDescriptor = AddonDescriptor {
    name: "crypt_test",
    depends: &["base"],
    exact_only: false,
};

fn registry(fields: &'static [FieldDecl]) -> erp_fields::Result<Registry> {
    RegistryBuilder::new([&ADDON])
        .with_decls([&*Box::leak(Box::new(
            ModelDecl::new("crypt_test", "crypt_test.person").fields(fields),
        ))])
        .build()
}

#[test]
fn a_registry_with_an_encrypted_field_needs_keys() {
    let with = registry(const { &[FieldDecl::char("national_id").encrypted(true)] }).unwrap();
    let error = require_keys(&with, None).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert!(
        error
            .message()
            .contains("crypt_test.person.national_id is encrypted")
            && error.message().contains("field_key_file"),
        "{}",
        error.message()
    );
    require_keys(&with, Some(&keys(ONLY_K1))).unwrap();
    let without = registry(const { &[FieldDecl::char("name")] }).unwrap();
    require_keys(&without, None).unwrap();
}

#[test]
fn an_encrypted_field_has_a_bytea_column_and_text_in_the_cache() {
    let registry = registry(const { &[FieldDecl::text("note").encrypted(true)] }).unwrap();
    let model = registry.model("crypt_test.person").unwrap();
    let field = model.field("note").unwrap();
    assert!(field.encrypted);
    assert_eq!(field.column(), Some(erp_fields::ColumnType::Bytea));
    assert_eq!(field.storage(), erp_fields::Storage::Str);
    assert!(Sealed::of(model, field).is_some());
    assert!(Sealed::of(model, model.field("name").unwrap()).is_none());
}

#[test]
fn declarations_an_encrypted_field_cannot_have_are_refused() {
    for (fields, expected) in [
        (
            const { &[FieldDecl::integer("n").encrypted(true)] } as &'static [FieldDecl],
            "only char and text fields can be encrypted",
        ),
        (
            const {
                &[FieldDecl::char("n")
                    .encrypted(true)
                    .index(Some(erp_fields::IndexKind::Btree))]
            },
            "cannot be indexed",
        ),
        (
            const { &[FieldDecl::char("n").encrypted(true).translate(true)] },
            "a translated field cannot be encrypted",
        ),
        (
            const { &[FieldDecl::char("n").encrypted(true).tracking(10)] },
            "tracking an encrypted field is refused",
        ),
        (
            const {
                &[FieldDecl::char("n")
                    .encrypted(true)
                    .store(false)
                    .compute("_c")]
            },
            "must be stored",
        ),
    ] {
        let error = registry(fields).unwrap_err();
        assert!(error.message().contains(expected), "{}", error.message());
    }
}
