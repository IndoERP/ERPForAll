//! Until installed-only schema filtering exists, the linked registry refuses a schema part of an
//! addon other than `mail` ((c).4). Process-wide, so a binary of its own.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

use erp_addon_mail::store_fetch::{SchemaFragment, SchemaPart, TopicRegistry};
use erp_sdk::store_wire::{FieldKind, FieldSpec};

fn other(fragment: &mut SchemaFragment) {
    fragment.field("other.model", "flag", FieldSpec::new(FieldKind::Bool));
}

erp_addon_mail::store_schema_part!(OTHER_PART = SchemaPart::new("other", other));

#[test]
fn a_schema_part_of_another_addon_is_refused_until_the_follow_up() {
    // `from_parts` (tests, unit checks) is not affected
    TopicRegistry::from_parts(&[], &[&OTHER_PART], &[]).unwrap();
    let error = TopicRegistry::linked().unwrap_err();
    assert_eq!(error.kind(), erp_sdk::ErrorKind::Programming);
    assert_eq!(
        error.message(),
        "store schema part of addon 'other' is not supported until a later release: \
         the schema topic would show it in databases where the addon is not installed"
    );
}
