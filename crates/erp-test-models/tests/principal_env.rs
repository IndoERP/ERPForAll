//! The principal carried by environments: the harness runs as
//! `System` by default and keeps today's `(uid, su)` behaviour; `as_user` switches it. No spec
//! case of its own (ENV-001/003/004/005 in `env.rs` must stay green unchanged).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use support::currency_fixture;

mod support;

use erp_orm::{
    Actor, Elevation, PrincipalHint, PrincipalKind, ResolutionEpoch, SUPERUSER_ID, StaticPolicy,
};
use erp_sdk::{Context, Domain, ErrorKind};
use erp_test_models::{base_users, test_orm};
use erp_testkit::{Harness, HarnessError};

fn orm_error(e: HarnessError) -> erp_sdk::Error {
    match e {
        HarnessError::Orm(e) => e,
        other => panic!("expected an ORM error, got {other}"),
    }
}

/// harness_runs_as_system_by_default
#[tokio::test]
async fn harness_runs_as_system_by_default() {
    let h = support::orm().await;
    assert_eq!(h.principal(), PrincipalHint::system());
    let tx = h.begin().await.unwrap();
    let env = tx.env();
    let p = env.principal();
    assert_eq!(p.kind(), PrincipalKind::System);
    assert_eq!(p.subject(), SUPERUSER_ID);
    assert_eq!(p.elevation(), &Elevation::Full);
    assert!(tx.env().su());
    assert_eq!(tx.env().uid(), 1);
    assert_eq!(tx.env().user_id(), SUPERUSER_ID);
    // the same principal as `Orm::env` and `Orm::begin` give for the superuser id
    assert_eq!(
        tx.env().principal(),
        h.orm().env(SUPERUSER_ID, Context::new()).principal()
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// harness_as_user_switches_begin_and_run
#[tokio::test]
async fn harness_as_user_switches_begin_and_run() {
    let h = Harness::builder([&base_users::ADDON, &test_orm::ADDON])
        .with_static_decls(crate::currency_fixture::decls())
        .policy(StaticPolicy::SuperuserOnly)
        .as_user(5)
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    let env = tx.env();
    let p = env.principal();
    assert_eq!(
        (p.kind(), p.subject()),
        (PrincipalKind::User, erp_orm::UserId::new(5).unwrap())
    );
    assert_eq!(p.resolution().epoch(), ResolutionEpoch::UNRESOLVED);
    assert!(!env.su());
    // the static policy still decides on `su()` alone: the user is refused, sudo is allowed
    let partners = env.records("test_orm.partner").unwrap();
    let err = partners.search(&mut tx, &Domain::TRUE).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access);
    assert_eq!(
        err.message(),
        "You are not allowed to read 'Test ORM Partner' (test_orm.partner) records."
    );
    drop(tx);
    let mut tx = h.begin().await.unwrap();
    let found = partners
        .sudo(true)
        .search(&mut tx, &Domain::TRUE)
        .await
        .unwrap();
    assert!(found.is_empty());
    drop(tx);
    let uid = h.run(async |tx| Ok(tx.env().uid())).await.unwrap();
    assert_eq!(uid, 5);
    h.cleanup().await.unwrap();
}

/// harness_refuses_uid_zero
#[tokio::test]
async fn harness_refuses_uid_zero() {
    let err = Harness::builder([&base_users::ADDON, &test_orm::ADDON])
        .with_static_decls(crate::currency_fixture::decls())
        .as_user(0)
        .build()
        .await
        .err()
        .expect("uid 0 is refused");
    let err = orm_error(err);
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(err.message(), "invalid user id 0: ids are non-zero");

    let h = support::orm().await;
    let err = orm_error(h.begin_as(0).await.err().expect("uid 0 is refused"));
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(err.message(), "invalid user id 0: ids are non-zero");
    h.cleanup().await.unwrap();
}

/// with_user_records_the_impersonator
#[tokio::test]
async fn with_user_records_the_impersonator() {
    let h = support::orm().await;
    let tx = h.begin().await.unwrap();
    let system = tx.env();
    let as7 = system.with_user(7);
    assert_eq!(as7.uid(), 7);
    assert_eq!(
        as7.principal().actor(),
        Some(&Actor::Impersonator(Box::new(system.principal().clone())))
    );
    // equal parameters give equal environments; the actor is a parameter
    assert_eq!(as7, system.with_user(7));
    let direct = h.begin_as(7).await.unwrap();
    assert_ne!(as7, direct.env(), "reached from another principal");
    assert_eq!(
        as7.with_user(8).principal().actor(),
        as7.principal().actor()
    );
    // with_user takes an i64 or a UserId
    assert_eq!(system.with_user(SUPERUSER_ID), system.with_user(1));
    drop((tx, direct));
    h.cleanup().await.unwrap();
}

/// sudo_false_keeps_the_superuser_elevated
#[tokio::test]
async fn sudo_false_keeps_the_superuser_elevated() {
    let h = support::orm().await;
    let tx = h.begin().await.unwrap();
    let system = tx.env();
    assert!(system.su());
    let off = system.sudo(false);
    assert!(off.su());
    assert_eq!(off.principal().elevation(), &Elevation::Full);
    assert_eq!(off, system);
    let switched_back = system.with_user(7).with_user(SUPERUSER_ID).sudo(false);
    assert!(switched_back.su());
    // the original principal comes back, not a System principal impersonated by itself
    assert_eq!(switched_back, system);
    // a user still drops elevation
    let user = h.begin_as(5).await.unwrap();
    assert!(!user.env().sudo(true).sudo(false).su());
    drop((tx, user));
    h.cleanup().await.unwrap();
}
