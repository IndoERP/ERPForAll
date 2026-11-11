#![allow(clippy::unwrap_used, reason = "safety assertions")]
use crate::h18_storage::btree_storage_bytes;
#[test]
fn pinned_layout_requires_explicit_toolchain_reaudit() {
    assert!(
        include_str!("../../../rust-toolchain.toml")
            .lines()
            .any(|line| line == "channel = \"1.98.1\""),
        "Re-audit the BTree envelope against the new pinned standard library"
    );
    assert_eq!(
        btree_storage_bytes::<String, serde_json::Value>(0).unwrap(),
        0
    );
    for count in 1..=11 {
        assert_eq!(
            btree_storage_bytes::<String, serde_json::Value>(count).unwrap(),
            btree_storage_bytes::<String, serde_json::Value>(1).unwrap()
        );
    }
    assert!(
        btree_storage_bytes::<String, serde_json::Value>(12).unwrap()
            > btree_storage_bytes::<String, serde_json::Value>(11).unwrap()
    );
    let error = btree_storage_bytes::<String, serde_json::Value>(usize::MAX).unwrap_err();
    assert_eq!(error.kind(), erp_sdk::ErrorKind::Validation);
    assert_eq!(error.message(), "H18 BTree storage size overflow");
}
