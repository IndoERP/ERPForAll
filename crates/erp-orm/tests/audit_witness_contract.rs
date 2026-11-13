// SPDX-License-Identifier: MIT OR Apache-2.0
#![allow(clippy::unwrap_used, reason = "tests")]
use erp_orm::audit::seal::{Anchor, AnchorRange, SealIdentity, WitnessError};

#[test]
fn witness_contract_validates_identity_version_and_range() {
    let identity = SealIdentity::new([1; 16], i64::MAX as u64, 2).unwrap();
    let anchor = Anchor::new(identity, [9; 32], 1).unwrap();
    assert_eq!(anchor.identity(), identity);
    assert_eq!(anchor.seal_hash(), &[9; 32]);
    assert_eq!(anchor.format_version(), 1);
    assert_eq!(identity.incarnation(), &[1; 16]);
    assert_eq!(identity.epoch(), i64::MAX as u64);
    assert_eq!(identity.batch_no(), 2);
    let error = Anchor::new(identity, [9; 32], 2).unwrap_err();
    assert_eq!(error, WitnessError::UnsupportedVersion);
    assert_eq!(
        error.to_string(),
        "audit witness: unsupported format version"
    );
    for (first, last) in [(0, 1), (2, 1), (1, u64::MAX)] {
        let error = AnchorRange::new(first, last).unwrap_err();
        assert_eq!(error, WitnessError::InvalidRange);
        assert_eq!(error.to_string(), "audit witness: invalid batch range");
    }
    let range = AnchorRange::new(2, i64::MAX as u64).unwrap();
    assert!(range.contains(2));
    assert!(range.contains(i64::MAX as u64));
    assert!(!range.contains(1));
}
