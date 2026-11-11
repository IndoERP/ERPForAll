//! The principal resolver (
//! §2.1, §2.3, §3.2, §3.4, §7 "Re-resolution after a membership change: 1 statement", §8.3):
//! one statement per transaction start, groups closed through the snapshot, companies validated,
//! inactive or missing subjects refused with an `Access` error naming the kind.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod support;

use std::collections::BTreeMap;
use std::num::NonZeroU64;

use erp_fields::ErrorKind;
use erp_orm::{
    CompanyId, GroupId, GroupSet, PrincipalHint, PrincipalKind, PrincipalResolver, SUPERUSER_ID,
    Transaction, UserId,
};
use erp_security::resolver::{Resolver, SUBJECT_SQL, SUBJECTS_SQL};
use erp_sql::param::{Param, Scalar};
use erp_testkit::Harness;
use erp_testkit::tokio_postgres::Row;
use support::{harness, snapshot};

fn gid(id: i64) -> GroupId {
    GroupId::new(id).unwrap()
}

fn cid(id: i64) -> CompanyId {
    CompanyId::new(id).unwrap()
}

fn uid(id: i64) -> UserId {
    UserId::new(id).unwrap()
}

/// Ids of what `seed` committed.
struct Seeded {
    user: i64,
    bob: i64,
    archived: i64,
    employee: i64,
    internal: i64,
    everyone: i64,
    main: i64,
    branch: i64,
}

/// Groups employee → internal → everyone, two companies, an active user in `employee` with both
/// companies (default `branch`), an active user without groups or companies, an archived user.
/// Committed.
async fn seed(h: &Harness) -> Seeded {
    let mut tx = h.begin().await.unwrap();
    let one = |rows: Vec<Row>| -> i64 { rows[0].get(0) };
    let mut ids = Vec::new();
    for name in ["Everyone", "Internal", "Employee"] {
        ids.push(one(tx
            .query(
                "INSERT INTO res_groups (name) VALUES ($1) RETURNING id::int8",
                &[&name],
            )
            .await
            .unwrap()));
    }
    let (everyone, internal, employee) = (ids[0], ids[1], ids[2]);
    tx.execute(
        "INSERT INTO res_groups_implied_rel (gid, hid) VALUES ($1, $2), ($2, $3)",
        &[&employee, &internal, &everyone],
    )
    .await
    .unwrap();
    let currency = erp_testkit::fixtures::seed_currency_in(&mut tx)
        .await
        .unwrap();
    let main = one(tx
        .query(
            "INSERT INTO res_company (name, currency_id) VALUES ('Main', $1) RETURNING id::int8",
            &[&currency],
        )
        .await
        .unwrap());
    let branch = one(tx
        .query(
            "INSERT INTO res_company (name, currency_id) VALUES ('Branch', $1) RETURNING id::int8",
            &[&currency],
        )
        .await
        .unwrap());
    let user = one(tx
        .query(
            "INSERT INTO res_users (name, login, active, company_id) \
             VALUES ('Ann', 'ann', true, $1) RETURNING id::int8",
            &[&branch],
        )
        .await
        .unwrap());
    let archived = one(tx
        .query(
            "INSERT INTO res_users (name, login, active) VALUES ('Old', 'old', false) \
             RETURNING id::int8",
            &[],
        )
        .await
        .unwrap());
    let bob = one(tx
        .query(
            "INSERT INTO res_users (name, login, active) VALUES ('Bob', 'bob', true) \
             RETURNING id::int8",
            &[],
        )
        .await
        .unwrap());
    tx.execute(
        "INSERT INTO res_groups_users_rel (uid, gid) VALUES ($1, $2)",
        &[&user, &employee],
    )
    .await
    .unwrap();
    tx.execute(
        "INSERT INTO res_company_users_rel (user_id, cid) VALUES ($1, $2), ($1, $3)",
        &[&user, &main, &branch],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    Seeded {
        user,
        bob,
        archived,
        employee,
        internal,
        everyone,
        main,
        branch,
    }
}

/// The snapshot's group graph of `seed`.
fn graph(s: &Seeded) -> BTreeMap<GroupId, Vec<GroupId>> {
    BTreeMap::from([
        (gid(s.everyone), vec![]),
        (gid(s.internal), vec![gid(s.everyone)]),
        (gid(s.employee), vec![gid(s.internal)]),
    ])
}

/// Run the resolver's statement for `hint` the way the transaction start does.
async fn subject_rows(tx: &mut Transaction, hint: &PrincipalHint) -> Vec<Row> {
    let statement = Resolver::id_order().statement(hint).unwrap();
    assert_eq!(statement.sql(), SUBJECT_SQL);
    let [Param::Scalar(Scalar::Int8(id))] = statement.params() else {
        panic!("one int8 parameter: {:?}", statement.params());
    };
    tx.query(statement.sql(), &[id]).await.unwrap()
}

/// DN §3.2: one statement reads the subject, its direct groups and its companies; `finish`
/// closes the groups through the snapshot and makes the default company context (the subject's
/// companies, its default company first).
#[tokio::test]
async fn a_user_principal_is_resolved_in_one_statement() {
    let h = harness().await;
    let s = seed(&h).await;
    let policy = snapshot(graph(&s));
    let mut tx = h.begin().await.unwrap();
    let hint = PrincipalHint::user(uid(s.user));
    let rows = erp_testkit::assert_query_count!(tx, >= 1, <= 1, {
        subject_rows(&mut tx, &hint).await
    });
    let principal = Resolver::id_order()
        .finish(&hint, &rows, policy.as_ref())
        .unwrap();
    assert_eq!(principal.kind(), PrincipalKind::User);
    assert_eq!(principal.subject(), uid(s.user));
    let resolution = principal.resolution();
    assert!(resolution.is_resolved());
    assert_eq!(resolution.epoch().get(), 1);
    assert_eq!(
        resolution.groups(),
        &GroupSet::new([gid(s.employee), gid(s.internal), gid(s.everyone)])
    );
    assert_eq!(resolution.all_companies(), [cid(s.main), cid(s.branch)]);
    assert_eq!(
        principal.companies().allowed(),
        [cid(s.branch), cid(s.main)]
    );
    assert_eq!(principal.companies().current(), Some(cid(s.branch)));
    h.cleanup().await.unwrap();
}

/// R-S15: a missing or archived subject is an `Access` error naming the principal kind.
#[tokio::test]
async fn missing_and_archived_subjects_cannot_act() {
    let h = harness().await;
    let s = seed(&h).await;
    let policy = snapshot(graph(&s));
    let mut tx = h.begin().await.unwrap();
    for (user, message) in [
        (
            s.archived,
            format!(
                "The user principal cannot act: user {} is archived.",
                s.archived
            ),
        ),
        (
            987_654,
            "The user principal cannot act: user 987654 does not exist.".to_owned(),
        ),
    ] {
        let hint = PrincipalHint::user(uid(user));
        let rows = subject_rows(&mut tx, &hint).await;
        let err = Resolver::id_order()
            .finish(&hint, &rows, policy.as_ref())
            .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Access);
        assert_eq!(err.message(), message);
    }
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn unknown_groups_and_foreign_default_companies_are_refused() {
    let h = harness().await;
    let s = seed(&h).await;
    let mut tx = h.begin().await.unwrap();
    let hint = PrincipalHint::user(uid(s.user));
    let rows = subject_rows(&mut tx, &hint).await;
    let without_employee = snapshot(BTreeMap::from([
        (gid(s.everyone), vec![]),
        (gid(s.internal), vec![gid(s.everyone)]),
    ]));
    let err = Resolver::id_order()
        .finish(&hint, &rows, without_employee.as_ref())
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        format!("the test snapshot has no group {}", s.employee)
    );

    tx.execute(
        "DELETE FROM res_company_users_rel WHERE user_id = $1 AND cid = $2",
        &[&s.user, &s.branch],
    )
    .await
    .unwrap();
    let rows = subject_rows(&mut tx, &hint).await;
    let err = Resolver::id_order()
        .finish(&hint, &rows, snapshot(graph(&s)).as_ref())
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access);
    assert_eq!(
        err.message(),
        "Access to unauthorized or invalid companies."
    );
    h.cleanup().await.unwrap();
}

/// The `System` principal is built by the core and never resolved.
#[test]
fn the_system_principal_is_not_resolved() {
    let err = Resolver::id_order()
        .statement(&PrincipalHint::from(SUPERUSER_ID))
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "the system principal is built by the core and never resolved from the database"
    );
}

/// DN §3.4, §7: the subjects bound in a transaction are re-read with one statement
/// (`uid = ANY($1)`) in the transaction's own view, in the order given, at the epoch given; a
/// subject archived in the transaction can no longer act.
#[tokio::test]
async fn re_resolution_is_one_statement_in_the_transactions_view() {
    let h = harness().await;
    let s = seed(&h).await;
    let policy = snapshot(graph(&s));
    let mut tx = h.begin().await.unwrap();
    // uncommitted: Ann leaves the employee group and the main company
    tx.execute(
        "DELETE FROM res_groups_users_rel WHERE uid = $1",
        &[&s.user],
    )
    .await
    .unwrap();
    tx.execute(
        "INSERT INTO res_groups_users_rel (uid, gid) VALUES ($1, $2)",
        &[&s.user, &s.internal],
    )
    .await
    .unwrap();
    tx.execute(
        "DELETE FROM res_company_users_rel WHERE user_id = $1 AND cid = $2",
        &[&s.user, &s.main],
    )
    .await
    .unwrap();
    let other = uid(s.bob);
    let epoch = NonZeroU64::new(7).unwrap();
    tx.capture_statements(true);
    let resolutions = erp_testkit::assert_query_count!(tx, >= 1, <= 1, {
        Resolver::id_order()
            .resolve_subjects(&mut tx, &[uid(s.user), other, uid(s.user)], policy.as_ref(), epoch)
            .await
            .unwrap()
    });
    assert_eq!(tx.captured(), [SUBJECTS_SQL.to_owned()]);
    assert_eq!(resolutions.len(), 3);
    assert_eq!(
        resolutions[0].groups(),
        &GroupSet::new([gid(s.internal), gid(s.everyone)])
    );
    assert_eq!(resolutions[0].all_companies(), [cid(s.branch)]);
    assert_eq!(resolutions[0].epoch().get(), 7);
    assert!(resolutions[1].groups().is_empty() && resolutions[1].all_companies().is_empty());
    assert_eq!(resolutions[2], resolutions[0]);

    tx.execute(
        "UPDATE res_users SET active = false WHERE id = $1",
        &[&s.user],
    )
    .await
    .unwrap();
    let err = Resolver::id_order()
        .resolve_subjects(&mut tx, &[uid(s.user)], policy.as_ref(), epoch)
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access);
    assert_eq!(
        err.message(),
        format!(
            "The user principal cannot act: user {} is archived.",
            s.user
        )
    );
    let err = Resolver::id_order()
        .resolve_subjects(&mut tx, &[uid(424_242)], policy.as_ref(), epoch)
        .await
        .unwrap_err();
    assert_eq!(
        err.message(),
        "The user principal cannot act: user 424242 does not exist."
    );
    h.cleanup().await.unwrap();
}

/// Base declares company name and active state, without sequence. Its resolver orders by
/// name/id and filters archives; synthetic missing-column cases remain in company_env.rs.
#[tokio::test]
async fn for_registry_with_company_active_filters_archived_without_sequence() {
    let h = harness().await;
    let resolver = Resolver::for_registry(h.registry()).unwrap();
    let statement = resolver.statement(&PrincipalHint::user(uid(5))).unwrap();
    assert!(
        statement.sql().contains("ORDER BY rc.name, rc.id)"),
        "{}",
        statement.sql()
    );
    assert!(statement.sql().contains("rc.active"), "{}", statement.sql());
    h.cleanup().await.unwrap();
}
