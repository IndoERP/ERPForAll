// SPDX-License-Identifier: MIT OR Apache-2.0
#![allow(clippy::unwrap_used, reason = "tests")]
use erp_fields::ErrorKind;
use erp_orm::node::SectionDigest;
use erp_rpc::{ReadAuditPolicy, RpcConfig};

fn digest(policy: ReadAuditPolicy) -> erp_orm::node::Digest {
    SectionDigest::of(
        &RpcConfig::builder()
            .read_audit_policy(policy)
            .build()
            .unwrap(),
    )
    .unwrap()
    .digest()
}

#[test]
fn read_audit_policy_is_bounded_positive_and_canonical() {
    let groups =
        ReadAuditPolicy::user_groups(&["base.group_user", "base.group_system", "base.group_user"])
            .unwrap();
    let sorted = ReadAuditPolicy::user_groups(&["base.group_system", "base.group_user"]).unwrap();
    assert_eq!(groups, sorted);
    assert_eq!(digest(groups), digest(sorted));
    assert_eq!(
        digest(ReadAuditPolicy::user_groups(&[]).unwrap()),
        digest(ReadAuditPolicy::default())
    );
    assert_ne!(
        digest(ReadAuditPolicy::all_users()),
        digest(ReadAuditPolicy::default())
    );
    assert_ne!(
        digest(ReadAuditPolicy::user_groups(&["base.group_user"]).unwrap()),
        digest(ReadAuditPolicy::default())
    );
    for bad in [
        "!base.group_user",
        "base.group_user,!base.group_system",
        "base.group_user,base.group_system",
        ".",
        "base.",
        "base.g roup",
        "base.group!user",
        "base.group|other",
        "base.group(other)",
        "base.group&other",
    ] {
        let error = ReadAuditPolicy::user_groups(&[bad]).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Programming);
        assert_eq!(
            error.message(),
            "audit_read_policy.user_groups must contain positive qualified group XML IDs without expression syntax (maximum 256 UTF-8 bytes each)"
        );
    }
    let long = format!("x.{}", "é".repeat(128));
    let error = ReadAuditPolicy::user_groups(&[&long]).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.message(),
        "audit_read_policy.user_groups must contain positive qualified group XML IDs without expression syntax (maximum 256 UTF-8 bytes each)"
    );
    let boundary = format!("x.{}", "é".repeat(127));
    ReadAuditPolicy::user_groups(&[&boundary]).unwrap();
    ReadAuditPolicy::user_groups(&["base.group_user"; 256]).unwrap();
    let error = ReadAuditPolicy::user_groups(&["base.group_user"; 257]).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.message(),
        "audit_read_policy.user_groups has more than 256 entries"
    );
}

#[test]
fn maximum_read_audit_policy_fits_shared_settings() {
    let names: Vec<String> = (0..256)
        .map(|n| format!("x.g{n:03}{}", "a".repeat(250)))
        .collect();
    assert!(names.iter().all(|name| name.len() == 256));
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    let policy = ReadAuditPolicy::user_groups(&refs).unwrap();
    assert_ne!(digest(policy), digest(ReadAuditPolicy::default()));
}
