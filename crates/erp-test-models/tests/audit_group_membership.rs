// SPDX-License-Identifier: MIT OR Apache-2.0
#![allow(clippy::unwrap_used, reason = "tests")]
use erp_orm::QualifiedXmlId;
use erp_testkit::{FieldPolicy, Harness};

#[tokio::test]
async fn audit_group_membership_resolves_all_ids_and_ignores_debug() {
    let policy = FieldPolicy::new().member(10, ["base.group_user", "base.group_no_one"]);
    let h = Harness::builder([&erp_test_models::base_users::ADDON])
        .policy(policy)
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    for debug in [false, true] {
        let env = tx.env().with_user(10).with_debug(debug);
        let membership = tx.group_membership(&env).await.unwrap();
        assert!(!membership.has_any_effective_xmlid(&[]).unwrap());
        let too_many = vec![QualifiedXmlId::parse("base.group_user").unwrap(); 257];
        let error = membership.has_any_effective_xmlid(&too_many).unwrap_err();
        assert_eq!(error.kind(), erp_fields::ErrorKind::Programming);
        assert_eq!(
            error.message(),
            "audit read policy has more than 256 group references"
        );
        assert!(
            membership
                .has_any_effective_xmlid(&[QualifiedXmlId::parse("base.group_no_one").unwrap()])
                .unwrap()
        );
        let error = membership
            .has_any_effective_xmlid(&[
                QualifiedXmlId::parse("base.group_user").unwrap(),
                QualifiedXmlId::parse("x.unknown").unwrap(),
            ])
            .unwrap_err();
        assert_eq!(error.kind(), erp_fields::ErrorKind::Programming);
        assert_eq!(
            error.message(),
            "audit read policy names unresolved group 'x.unknown'"
        );
    }
    let env = tx.env().with_user(11);
    assert!(
        !tx.group_membership(&env)
            .await
            .unwrap()
            .has_any_effective_xmlid(&[QualifiedXmlId::parse("base.group_user").unwrap()])
            .unwrap()
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn audit_group_selection_refuses_unsupported_resolver() {
    let h = Harness::builder([&erp_test_models::base_users::ADDON])
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    let env = tx.env().with_user(10);
    let error = tx
        .group_membership(&env)
        .await
        .unwrap()
        .has_any_effective_xmlid(&[QualifiedXmlId::parse("base.group_user").unwrap()])
        .unwrap_err();
    assert_eq!(error.kind(), erp_fields::ErrorKind::Programming);
    assert_eq!(
        error.message(),
        "this access policy does not resolve group references (resolve_group_ref): use a compiled policy snapshot"
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}
