//! Policies come from the
//! testkit's `DomainPolicy` (record predicates per model and operation, no `erp-security`).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod support;

use std::collections::BTreeMap;
use std::num::NonZeroU64;

use erp_orm::{
    AccessCtx, AccessCtxParts, AccessDomainChains, AccessDomainImpl, AccessSuper, BeginOptions,
    CompanyId, ElevationKind, GroupId, GroupLookup, GroupSet, IntoPolicyProvider, KindClass,
    Operation, Principal, PrincipalBuilder, QualifiedXmlId, Resolution, UserId,
};
use erp_registry::{FieldDecl, ModelDecl};
use erp_sdk::domain::{NowKind, PrincipalParam, UserPath};
use erp_sdk::{Command, Domain, ErrorKind, Operator, RawVals, Records, Transaction, Value};
use erp_test_models::{base_users, test_access};
use erp_testkit::{DomainPolicy, Harness, Otherwise, assert_query_count};
use support::dom;

const OBJ: &str = "test_access.some_obj";
const DEMO: i64 = 2;
const OTHER: i64 = 3;

async fn harness_with(
    policy: impl IntoPolicyProvider,
    decls: impl IntoIterator<Item = ModelDecl>,
) -> Harness {
    let h = Harness::builder([&base_users::ADDON, &test_access::ADDON])
        .with_decls(decls)
        .policy(policy)
        .build()
        .await
        .expect("harness");
    let mut tx = h.begin().await.unwrap();
    tx.batch(
        "INSERT INTO res_users (id, name, login, active) VALUES (2, 'demo', 'demo', true), \
         (3, 'other', 'other', true); \
         SELECT setval(pg_get_serial_sequence('res_users', 'id'), 3)",
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    h
}

async fn harness(policy: impl IntoPolicyProvider) -> Harness {
    harness_with(policy, []).await
}

fn company(id: i64) -> CompanyId {
    CompanyId::new(id).unwrap()
}

/// A user principal resolved with companies 1, 2 and 3, `allowed` active.
fn principal(uid: i64, allowed: &[i64]) -> Principal {
    PrincipalBuilder::user(UserId::new(uid).unwrap())
        .resolution(
            Resolution::in_id_order(
                GroupSet::default(),
                [company(1), company(2), company(3)],
                NonZeroU64::MIN,
            )
            .unwrap(),
        )
        .companies(allowed.iter().map(|c| company(*c)).collect())
        .build()
        .unwrap()
}

async fn begin_as(h: &Harness, uid: i64, allowed: &[i64]) -> Transaction {
    h.orm()
        .begin_opts(BeginOptions::for_principal(principal(uid, allowed)))
        .await
        .unwrap()
}

fn row(pairs: &[(&str, Value)]) -> BTreeMap<String, Value> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), v.clone()))
        .collect()
}

/// Records created as superuser.
async fn create(h: &Harness, model: &str, rows: &[BTreeMap<String, Value>]) -> Vec<i64> {
    let mut tx = h.begin().await.unwrap();
    let ids = tx
        .env()
        .records(model)
        .unwrap()
        .create_values(&mut tx, rows)
        .await
        .unwrap()
        .ids();
    tx.commit().await.unwrap();
    ids
}

fn records(tx: &Transaction, model: &str, ids: &[i64]) -> Records {
    tx.env()
        .records(model)
        .unwrap()
        .browse(ids.iter().copied())
        .unwrap()
}

fn cond(field: &str, op: Operator, value: impl Into<erp_sdk::domain::Value>) -> Domain {
    Domain::condition(field, op, value).unwrap()
}

fn param(p: PrincipalParam) -> erp_sdk::domain::Value {
    erp_sdk::domain::Value::Param(p)
}

/// Every value of `field` on `records`, read as their environment.
async fn ints(tx: &mut Transaction, records: &Records, field: &str) -> Vec<i64> {
    records
        .cells(tx, field)
        .await
        .unwrap()
        .into_iter()
        .map(|c| match c {
            erp_sdk::Cell::Int(i) => i64::from(i),
            erp_sdk::Cell::Id(i) => i,
            other => panic!("not an integer: {other:?}"),
        })
        .collect()
}

#[track_caller]
fn assert_access(error: &erp_sdk::Error, message: &str) {
    assert_eq!(error.kind(), ErrorKind::Access, "{error}");
    assert_eq!(error.message(), message);
}

fn denied(verb: &str, id: i64, uid: i64) -> String {
    format!(
        "Your access rights do not let you {verb} 'Object For Test Access Right' (test_access.some_obj) \
         records.\n\nRecords: test_access.some_obj({id},), User: {uid}"
    )
}

async fn objs(h: &Harness) -> Vec<i64> {
    let names = ["Mario", "Luigi", "Peach", "Toad", "Yoshi", "Bowser"];
    let rows: Vec<_> = names
        .iter()
        .zip(1..)
        .map(|(n, v)| row(&[("name", Value::str(*n)), ("val", Value::Int(v))]))
        .collect();
    create(h, OBJ, &rows).await
}

#[tokio::test]
async fn spec_sec_007_superuser_bypass() {
    let h = harness(DomainPolicy::builder(Otherwise::Deny).build().unwrap()).await;
    let ids = objs(&h).await;
    let mut tx = begin_as(&h, DEMO, &[]).await;
    let objs = records(&tx, OBJ, &ids);
    // without access: nothing
    for op in Operation::ALL {
        assert!(!objs.has_access(&mut tx, op).await.unwrap(), "{op:?}");
    }
    assert!(
        objs.filtered_access(&mut tx, Operation::Read)
            .await
            .unwrap()
            .is_empty()
    );
    let err = objs
        .check_access(&mut tx, Operation::Read)
        .await
        .unwrap_err();
    assert_access(
        &err,
        "Your access rights do not let you read 'Object For Test Access Right' (test_access.some_obj) \
         records.",
    );
    // in superuser mode: everything, without a statement
    let su = objs.sudo(true);
    assert_query_count!(tx, <= 0, {
        for op in Operation::ALL {
            assert!(su.has_access(&mut tx, op).await.unwrap(), "{op:?}");
            su.check_access(&mut tx, op).await.unwrap();
        }
        assert_eq!(
            su.filtered_access(&mut tx, Operation::Unlink).await.unwrap().ids(),
            ids
        );
    });
    let all = su.search(&mut tx, &Domain::TRUE).await.unwrap();
    assert_eq!(all.ids(), ids);
    su.write_values(&mut tx, &row(&[("val", Value::Int(0))]))
        .await
        .unwrap();
    su.unlink(&mut tx).await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_sec_012_search_applies_the_read_access_domain() {
    // "Forbid negatives" as the read predicate of the objects
    let policy = DomainPolicy::builder(Otherwise::Allow)
        .rule(OBJ, Operation::Read, cond("val", Operator::Gt, 0))
        .build()
        .unwrap();
    let h = harness(policy).await;
    let ids = create(
        &h,
        OBJ,
        &[
            row(&[("name", Value::str("allowed")), ("val", Value::Int(1))]),
            row(&[("name", Value::str("forbidden")), ("val", Value::Int(-1))]),
        ],
    )
    .await;
    let (allowed, forbidden) = (ids[0], ids[1]);
    let mut tx = begin_as(&h, DEMO, &[]).await;
    let objs = tx.env().records(OBJ).unwrap();
    assert_eq!(
        ints(&mut tx, &objs.browse([allowed]).unwrap(), "val").await,
        [1]
    );
    let err = objs
        .browse([forbidden])
        .unwrap()
        .cells(&mut tx, "val")
        .await
        .unwrap_err();
    assert_access(&err, &denied("access", forbidden, DEMO));
    assert_eq!(
        objs.search(&mut tx, &Domain::TRUE).await.unwrap().ids(),
        [allowed]
    );
    // a new record without origin is readable, one with the forbidden origin is not
    let fresh = tx
        .new_record(&objs, RawVals::new(), None, None)
        .await
        .unwrap();
    fresh.cells(&mut tx, "val").await.unwrap();
    let shadow = tx
        .new_record(
            &objs,
            RawVals::new(),
            Some(erp_sdk::RecId::from_real(forbidden).unwrap()),
            None,
        )
        .await
        .unwrap();
    let err = shadow.cells(&mut tx, "val").await.unwrap_err();
    assert_access(&err, &denied("access", forbidden, DEMO));
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_sec_014_relational_values_filtered_by_comodel_access() {
    let policy = DomainPolicy::builder(Otherwise::Allow)
        .rule(OBJ, Operation::Read, cond("val", Operator::Gt, 0))
        .rule(
            "test_access.child",
            Operation::Read,
            cond("val", Operator::Gt, 0),
        )
        .build()
        .unwrap();
    let h = harness(policy).await;
    let ids = create(
        &h,
        OBJ,
        &[
            row(&[("name", Value::str("allowed")), ("val", Value::Int(1))]),
            row(&[("name", Value::str("forbidden")), ("val", Value::Int(-1))]),
        ],
    )
    .await;
    let (allowed, forbidden) = (ids[0], ids[1]);
    let container = create(
        &h,
        "test_access.container",
        &[row(&[
            ("name", Value::str("box")),
            (
                "some_ids",
                Value::list([Value::Int(allowed), Value::Int(forbidden)]),
            ),
        ])],
    )
    .await[0];
    let children = create(
        &h,
        "test_access.child",
        &[
            row(&[
                ("val", Value::Int(1)),
                ("container_id", Value::Int(container)),
            ]),
            row(&[
                ("val", Value::Int(-1)),
                ("container_id", Value::Int(container)),
            ]),
        ],
    )
    .await;
    let mut tx = begin_as(&h, DEMO, &[]).await;
    let boxed = records(&tx, "test_access.container", &[container]);
    assert_eq!(
        boxed.related(&mut tx, "some_ids").await.unwrap().ids(),
        [allowed]
    );
    assert_eq!(
        boxed.related(&mut tx, "child_ids").await.unwrap().ids(),
        [children[0]]
    );
    // linking a record the user cannot read is refused
    let err = boxed
        .write(
            &mut tx,
            RawVals::new().set_commands(
                "some_ids".to_owned(),
                vec![Command::Set(vec![
                    erp_sdk::RecId::from_real(allowed).unwrap(),
                    erp_sdk::RecId::from_real(forbidden).unwrap(),
                ])],
            ),
        )
        .await
        .unwrap_err();
    assert_access(
        &err,
        &format!(
            "Failed to write field test_access.container.some_ids\n{}",
            denied("access", forbidden, DEMO)
        ),
    );
    tx.rollback().await.unwrap();
    // clearing removes only the links the user can read
    let mut tx = begin_as(&h, DEMO, &[]).await;
    let boxed = records(&tx, "test_access.container", &[container]);
    boxed
        .write(
            &mut tx,
            RawVals::new().set_commands("some_ids".to_owned(), vec![Command::Clear]),
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let mut su = h.begin().await.unwrap();
    let boxed = records(&su, "test_access.container", &[container]);
    assert_eq!(
        boxed.related(&mut su, "some_ids").await.unwrap().ids(),
        [forbidden]
    );
    drop(su);
    h.cleanup().await.unwrap();

    // no read access on the comodel at all: the value is empty and `has_access` is false
    let policy = DomainPolicy::builder(Otherwise::Allow)
        .deny(OBJ, Operation::Read)
        .build()
        .unwrap();
    let h = harness(policy).await;
    let ids = objs(&h).await;
    let container = create(
        &h,
        "test_access.container",
        &[row(&[(
            "some_ids",
            Value::list(ids.iter().map(|i| Value::Int(*i))),
        )])],
    )
    .await[0];
    let mut tx = begin_as(&h, DEMO, &[]).await;
    let boxed = records(&tx, "test_access.container", &[container]);
    assert!(boxed.related(&mut tx, "some_ids").await.unwrap().is_empty());
    let objs = records(&tx, OBJ, &ids);
    assert!(!objs.has_access(&mut tx, Operation::Read).await.unwrap());
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_sec_017_access_memo_performance_and_consistency() {
    let policy = DomainPolicy::builder(Otherwise::Allow)
        .rule(OBJ, Operation::Read, cond("val", Operator::Gt, 0))
        .build()
        .unwrap();
    let h = harness(policy).await;
    let allowed = objs(&h).await;
    let forbidden_rows: Vec<_> = (0..14)
        .map(|i| {
            row(&[
                ("name", Value::str(format!("f{i}"))),
                ("val", Value::Int(-1)),
            ])
        })
        .collect();
    let forbidden = create(&h, OBJ, &forbidden_rows).await;
    let mut tx = begin_as(&h, DEMO, &[]).await;
    let objs = records(&tx, OBJ, &allowed);
    assert_query_count!(tx, >= 1, <= 1, {
        objs.check_access(&mut tx, Operation::Read).await.unwrap();
    });
    // warm: the memo answers
    assert_query_count!(tx, <= 0, {
        objs.check_access(&mut tx, Operation::Read).await.unwrap();
        assert_eq!(
            objs.filtered_access(&mut tx, Operation::Read).await.unwrap().ids(),
            allowed
        );
        let empty = tx.env().records(OBJ).unwrap();
        assert!(empty.has_access(&mut tx, Operation::Read).await.unwrap());
        objs.fetch(&mut tx, Some(&["id"])).await.unwrap();
    });
    // a denial is never memoized: every later access still raises
    let hidden = records(&tx, OBJ, &forbidden);
    for _ in 0..2 {
        let err = hidden
            .check_access(&mut tx, Operation::Read)
            .await
            .unwrap_err();
        assert_access(&err, &denied("access", forbidden[0], DEMO));
    }
    for id in [forbidden[0], forbidden[13]] {
        let err = records(&tx, OBJ, &[id])
            .cells(&mut tx, "name")
            .await
            .unwrap_err();
        assert_access(&err, &denied("access", id, DEMO));
    }
    // a write to a field the predicate reads drops the memo: the record is checked again
    let su = records(&tx, OBJ, &allowed[..1]).sudo(true);
    su.write_values(&mut tx, &row(&[("val", Value::Int(-5))]))
        .await
        .unwrap();
    let err = records(&tx, OBJ, &allowed[..1])
        .check_access(&mut tx, Operation::Read)
        .await
        .unwrap_err();
    assert_access(&err, &denied("access", allowed[0], DEMO));
    drop(tx);
    h.cleanup().await.unwrap();
}

/// A read over records the memo knows and records it does not still runs the access predicate
/// (PERF.ACCESS-SERVE: the read path asks whether *any* id is unknown, never copying the ids).
#[tokio::test]
async fn a_read_mixing_memoized_and_unchecked_records_is_still_restricted() {
    let policy = DomainPolicy::builder(Otherwise::Allow)
        .rule(OBJ, Operation::Read, cond("val", Operator::Gt, 0))
        .build()
        .unwrap();
    let h = harness(policy).await;
    let allowed = objs(&h).await;
    let forbidden = create(
        &h,
        OBJ,
        &[row(&[("name", Value::str("f")), ("val", Value::Int(-1))])],
    )
    .await[0];
    let mut tx = begin_as(&h, DEMO, &[]).await;
    records(&tx, OBJ, &allowed)
        .check_access(&mut tx, Operation::Read)
        .await
        .unwrap();
    // known ids and one never checked: the forbidden record is refused, not read
    let mixed = records(&tx, OBJ, &[allowed[0], forbidden]);
    let err = mixed.cells(&mut tx, "name").await.unwrap_err();
    assert_access(&err, &denied("access", forbidden, DEMO));
    // the denial is not memoized: a second mixed read (another known id, not cached yet) is
    // refused again
    let mixed = records(&tx, OBJ, &[allowed[1], forbidden]);
    let err = mixed.cells(&mut tx, "name").await.unwrap_err();
    assert_access(&err, &denied("access", forbidden, DEMO));
    // known ids only: read as before
    let known = records(&tx, OBJ, &allowed);
    assert_eq!(ints(&mut tx, &known, "val").await, [1, 2, 3, 4, 5, 6]);
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn invalidation_forgets_memoized_checks() {
    let policy = DomainPolicy::builder(Otherwise::Allow)
        .rule(OBJ, Operation::Read, cond("val", Operator::Gt, 0))
        .build()
        .unwrap();
    let h = harness(policy).await;
    let ids = objs(&h).await;
    let mut tx = begin_as(&h, DEMO, &[]).await;
    let hide = |id: i64| format!("UPDATE test_access_some_obj SET val = -1 WHERE id = {id}");

    // invalidate_model: only the fields a predicate reads drop its entries
    let first = records(&tx, OBJ, &ids[..1]);
    first.check_access(&mut tx, Operation::Read).await.unwrap();
    tx.batch(&hide(ids[0])).await.unwrap();
    tx.invalidate_model(OBJ, Some(&["name"])).await.unwrap();
    assert_query_count!(tx, <= 0, {
        first.check_access(&mut tx, Operation::Read).await.unwrap();
    });
    tx.invalidate_model(OBJ, Some(&["val"])).await.unwrap();
    let err = first
        .check_access(&mut tx, Operation::Read)
        .await
        .unwrap_err();
    assert_access(&err, &denied("access", ids[0], DEMO));

    // invalidate_records
    let second = records(&tx, OBJ, &ids[1..2]);
    second.check_access(&mut tx, Operation::Read).await.unwrap();
    assert_query_count!(tx, <= 0, {
        second.check_access(&mut tx, Operation::Read).await.unwrap();
    });
    tx.batch(&hide(ids[1])).await.unwrap();
    tx.invalidate_records(&second, Some(&["val"]))
        .await
        .unwrap();
    let err = second
        .check_access(&mut tx, Operation::Read)
        .await
        .unwrap_err();
    assert_access(&err, &denied("access", ids[1], DEMO));

    // invalidate_all
    let third = records(&tx, OBJ, &ids[2..3]);
    third.check_access(&mut tx, Operation::Read).await.unwrap();
    assert_query_count!(tx, <= 0, {
        third.check_access(&mut tx, Operation::Read).await.unwrap();
    });
    tx.batch(&hide(ids[2])).await.unwrap();
    tx.invalidate_all().await.unwrap();
    let err = third
        .check_access(&mut tx, Operation::Read)
        .await
        .unwrap_err();
    assert_access(&err, &denied("access", ids[2], DEMO));
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_sec_020_create_checked_after_insertion_write_before() {
    let policy = DomainPolicy::builder(Otherwise::Allow)
        .rule_all(OBJ, &cond("val", Operator::Gt, 0))
        .build()
        .unwrap();
    let h = harness(policy).await;
    let one = create(&h, OBJ, &[row(&[("val", Value::Int(1))])]).await[0];
    let mut tx = begin_as(&h, DEMO, &[]).await;
    let objs = tx.env().records(OBJ).unwrap();
    // the new record does not satisfy the predicate: refused after the INSERT, transaction
    // poisoned for the caller to roll back
    let err = objs
        .create_values(&mut tx, &[row(&[("val", Value::Int(-1))])])
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access);
    let next = one + 1;
    assert_eq!(err.message(), denied("create", next, DEMO));
    assert!(tx.is_poisoned());
    tx.rollback().await.unwrap();
    // the record matched before the write: allowed, though it no longer matches after it
    let mut tx = begin_as(&h, DEMO, &[]).await;
    let rec = records(&tx, OBJ, &[one]);
    rec.write_values(&mut tx, &row(&[("val", Value::Int(-1))]))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let mut su = h.begin().await.unwrap();
    let written = records(&su, OBJ, &[one]);
    assert_eq!(ints(&mut su, &written, "val").await, [-1]);
    // and now the write is refused (the record does not match before the write)
    let mut tx = begin_as(&h, DEMO, &[]).await;
    let err = records(&tx, OBJ, &[one])
        .write_values(&mut tx, &row(&[("val", Value::Int(2))]))
        .await
        .unwrap_err();
    assert_access(&err, &denied("modify", one, DEMO));
    drop((tx, su));
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_sec_048_unlink_checks_every_record_before_deleting() {
    let policy = DomainPolicy::builder(Otherwise::Allow)
        .rule(OBJ, Operation::Unlink, cond("val", Operator::Lt, 4))
        .build()
        .unwrap();
    let h = harness(policy).await;
    let ids = objs(&h).await;
    let mut tx = begin_as(&h, DEMO, &[]).await;
    let all = records(&tx, OBJ, &ids);
    tx.capture_statements(true);
    let err = all.unlink(&mut tx).await.unwrap_err();
    assert_access(&err, &denied("delete", ids[3], DEMO));
    assert!(
        !tx.captured()
            .iter()
            .any(|s| s.trim_start().starts_with("DELETE")),
        "nothing is deleted: {:?}",
        tx.captured()
    );
    tx.capture_statements(false);
    assert!(!tx.is_poisoned(), "refused before any change");
    let count = all
        .sudo(true)
        .search_count(&mut tx, &Domain::TRUE, None)
        .await
        .unwrap();
    assert_eq!(count, 6);
    // the allowed subset goes, with one check statement for the batch more than superuser mode
    let before = tx.query_count();
    records(&tx, OBJ, &ids[4..])
        .sudo(true)
        .unlink(&mut tx)
        .await
        .unwrap();
    let as_superuser = tx.query_count() - before;
    let before = tx.query_count();
    records(&tx, OBJ, &ids[..3]).unlink(&mut tx).await.unwrap();
    assert_eq!(tx.query_count() - before, as_superuser + 1);
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_sec_049_batch_evaluation_with_prefetch() {
    let policy = DomainPolicy::builder(Otherwise::Allow)
        .rule(OBJ, Operation::Read, cond("val", Operator::Lt, 4))
        .build()
        .unwrap();
    let h = harness(policy).await;
    let ids = objs(&h).await;
    // one record of the prefetch set disappears
    let mut su = h.begin().await.unwrap();
    records(&su, OBJ, &ids[1..2]).unlink(&mut su).await.unwrap();
    su.commit().await.unwrap();
    let mut tx = begin_as(&h, DEMO, &[]).await;
    let all = records(&tx, OBJ, &ids);
    // the batch is evaluated in one statement; the missing record of the prefetch set does not
    // fail a read of the others
    let readable = assert_query_count!(tx, >= 1, <= 1, {
        all.filtered_access(&mut tx, Operation::Read).await.unwrap()
    });
    assert_eq!(readable.ids(), [ids[0], ids[2]]);
    let first = all.browse_ids(all.rec_ids()[..1].iter().copied());
    assert_eq!(ints(&mut tx, &first, "val").await, [1]);
    // the queried recordset intersecting the missing record does fail
    let missing = all.browse_ids(all.rec_ids()[1..2].iter().copied());
    let err = missing.cells(&mut tx, "val").await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Missing, "{err}");
    assert_eq!(
        err.message(),
        format!(
            "Record does not exist or has been deleted.\n(Record: test_access.some_obj({},), User: \
             {DEMO})",
            ids[1]
        )
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Principal parameters end to end: `user.id`, `company_ids`, a user path and "now" are bound
/// per statement; the memoized text serves every principal (one render for three principals).
#[tokio::test]
async fn principal_parameters_are_bound_per_statement() {
    let rule = Domain::and([
        cond("user_id", Operator::Eq, param(PrincipalParam::Uid)),
        Domain::or([
            cond("company_id", Operator::Eq, false),
            cond(
                "company_id",
                Operator::In,
                param(PrincipalParam::CompanyIds),
            ),
        ]),
    ]);
    let policy = DomainPolicy::builder(Otherwise::Allow)
        .rule(OBJ, Operation::Read, rule)
        .build()
        .unwrap();
    let h = harness(policy).await;
    let companies = create(
        &h,
        "test_access.company",
        &[
            row(&[("name", Value::str("c1"))]),
            row(&[("name", Value::str("c2"))]),
        ],
    )
    .await;
    let (c1, c2) = (companies[0], companies[1]);
    let ids = create(
        &h,
        OBJ,
        &[
            row(&[
                ("user_id", Value::Int(DEMO)),
                ("company_id", Value::Int(c1)),
            ]),
            row(&[
                ("user_id", Value::Int(OTHER)),
                ("company_id", Value::Int(c1)),
            ]),
            row(&[
                ("user_id", Value::Int(DEMO)),
                ("company_id", Value::Int(c2)),
            ]),
            row(&[("user_id", Value::Int(DEMO))]),
        ],
    )
    .await;
    let visible = |uid: i64, allowed: Vec<i64>| {
        let h = &h;
        async move {
            let mut tx = begin_as(h, uid, &allowed).await;
            let found = tx
                .env()
                .records(OBJ)
                .unwrap()
                .search(&mut tx, &Domain::TRUE)
                .await
                .unwrap()
                .ids();
            drop(tx);
            found
        }
    };
    let before = h.orm().search_memo_stats();
    assert_eq!(visible(DEMO, vec![c1]).await, [ids[0], ids[3]]);
    assert_eq!(visible(OTHER, vec![c1]).await, [ids[1]]);
    assert_eq!(visible(DEMO, vec![c1, c2]).await, [ids[0], ids[2], ids[3]]);
    assert_eq!(visible(DEMO, vec![]).await, [ids[3]]);
    let after = h.orm().search_memo_stats();
    assert!(
        after.misses - before.misses <= 2,
        "one text for every principal: {before:?} -> {after:?}"
    );
    h.cleanup().await.unwrap();
}

/// A user path and "now" in predicates (lowered to a sub-select bound by the subject and to the
/// transaction's start time).
#[tokio::test]
async fn user_paths_and_now_in_predicates() {
    let rule = Domain::and([
        cond(
            "partner_id",
            Operator::Eq,
            param(PrincipalParam::UserPath(
                UserPath::parse("access_partner_id.id").unwrap(),
            )),
        ),
        Domain::or([
            cond("deadline", Operator::Eq, false),
            cond(
                "deadline",
                Operator::Ge,
                param(PrincipalParam::Now(NowKind::Date)),
            ),
        ]),
    ]);
    let policy = DomainPolicy::builder(Otherwise::Allow)
        .rule(OBJ, Operation::Read, rule)
        .build()
        .unwrap();
    let h = harness(policy).await;
    let partners = create(
        &h,
        "test_access.partner",
        &[
            row(&[("name", Value::str("p1"))]),
            row(&[("name", Value::str("p2"))]),
        ],
    )
    .await;
    let mut su = h.begin().await.unwrap();
    su.batch(&format!(
        "UPDATE res_users SET access_partner_id = {} WHERE id = {DEMO}",
        partners[0]
    ))
    .await
    .unwrap();
    su.commit().await.unwrap();
    let ids = create(
        &h,
        OBJ,
        &[
            row(&[("partner_id", Value::Int(partners[0]))]),
            row(&[("partner_id", Value::Int(partners[1]))]),
            row(&[
                ("partner_id", Value::Int(partners[0])),
                ("deadline", Value::str("2000-01-01")),
            ]),
            row(&[
                ("partner_id", Value::Int(partners[0])),
                ("deadline", Value::str("2999-01-01")),
            ]),
            row(&[]),
        ],
    )
    .await;
    let mut tx = begin_as(&h, DEMO, &[]).await;
    let found = tx
        .env()
        .records(OBJ)
        .unwrap()
        .search(&mut tx, &Domain::TRUE)
        .await
        .unwrap();
    assert_eq!(found.ids(), [ids[0], ids[3]]);
    drop(tx);
    let mut tx = begin_as(&h, OTHER, &[]).await;
    let found = tx
        .env()
        .records(OBJ)
        .unwrap()
        .search(&mut tx, &Domain::TRUE)
        .await
        .unwrap();
    assert_eq!(found.ids(), [ids[4]]);
    drop(tx);
    h.cleanup().await.unwrap();
}

/// `#[access_domain]`: the override narrows the policy's predicate
/// through `super`, in searches and record checks; superuser mode is not affected.
#[tokio::test]
async fn access_domain_overrides_narrow_the_policy() {
    let registry = Harness::builder([&base_users::ADDON, &test_access::ADDON])
        .registry_only()
        .unwrap();
    let chains = AccessDomainChains::build(&registry, &[]).unwrap();
    assert!(chains.has_overrides(registry.model_id("test_access.note").unwrap()));
    let policy = DomainPolicy::builder(Otherwise::Allow)
        .rule(
            "test_access.note",
            Operation::Read,
            cond("name", Operator::Ne, "secret"),
        )
        .overrides(registry, chains)
        .build()
        .unwrap();
    let h = harness(policy).await;
    let ids = create(
        &h,
        "test_access.note",
        &[
            row(&[("name", Value::str("plain"))]),
            row(&[("name", Value::str("secret"))]),
            row(&[("name", Value::str("classified"))]),
        ],
    )
    .await;
    let mut tx = begin_as(&h, DEMO, &[]).await;
    let notes = tx.env().records("test_access.note").unwrap();
    assert_eq!(
        notes.search(&mut tx, &Domain::TRUE).await.unwrap().ids(),
        [ids[0]]
    );
    // writes are not narrowed by the override
    records(&tx, "test_access.note", &ids[2..])
        .write_values(&mut tx, &row(&[("name", Value::str("classified!"))]))
        .await
        .unwrap();
    let all = notes
        .sudo(true)
        .search(&mut tx, &Domain::TRUE)
        .await
        .unwrap();
    assert_eq!(all.ids(), ids);
    drop(tx);
    h.cleanup().await.unwrap();
}

/// `AccessCtx::has_group` in an override (`test_access.categ`): members of the manager group see
/// every category, others not the internal ones, in searches and in record checks.
#[tokio::test]
async fn access_domain_overrides_ask_the_groups() {
    let registry = Harness::builder([&base_users::ADDON, &test_access::ADDON])
        .registry_only()
        .unwrap();
    let chains = AccessDomainChains::build(&registry, &[]).unwrap();
    let manager = GroupId::new(7).unwrap();
    let policy = DomainPolicy::builder(Otherwise::Allow)
        .group_xmlid("test_access.group_manager", manager)
        .overrides(registry, chains)
        .build()
        .unwrap();
    let h = harness(policy).await;
    let ids = create(
        &h,
        "test_access.categ",
        &[
            row(&[("name", Value::str("public"))]),
            row(&[("name", Value::str("internal"))]),
        ],
    )
    .await;
    let as_manager = PrincipalBuilder::user(UserId::new(DEMO).unwrap())
        .resolution(
            Resolution::in_id_order(GroupSet::new([manager]), [company(1)], NonZeroU64::MIN)
                .unwrap(),
        )
        .build()
        .unwrap();
    let mut tx = h
        .orm()
        .begin_opts(BeginOptions::for_principal(as_manager))
        .await
        .unwrap();
    let categs = tx.env().records("test_access.categ").unwrap();
    assert_eq!(
        categs.search(&mut tx, &Domain::TRUE).await.unwrap().ids(),
        ids
    );
    drop(tx);
    let mut tx = begin_as(&h, DEMO, &[]).await;
    let categs = tx.env().records("test_access.categ").unwrap();
    assert_eq!(
        categs.search(&mut tx, &Domain::TRUE).await.unwrap().ids(),
        [ids[0]]
    );
    let internal = records(&tx, "test_access.categ", &ids[1..]);
    let err = internal
        .check_access(&mut tx, Operation::Read)
        .await
        .unwrap_err();
    assert_access(
        &err,
        &format!(
            "Your access rights do not let you access 'Access Category' (test_access.categ) records.\n\n\
             Records: test_access.categ({},), User: {DEMO}",
            ids[1]
        ),
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

fn keep(ctx: &AccessCtx<'_>, op: Operation, sup: AccessSuper<'_>) -> erp_sdk::Result<Domain> {
    sup.call(ctx, op)
}

fn leak(addon: &'static str, model: &'static str, line: u32) -> &'static AccessDomainImpl {
    Box::leak(Box::new(AccessDomainImpl {
        addon,
        model,
        func: keep,
        file: "extra.rs",
        line,
    }))
}

/// A policy without the group's external id.
struct NoXmlIds;

impl GroupLookup for NoXmlIds {
    fn group_id(&self, _xmlid: &QualifiedXmlId) -> erp_sdk::Result<Option<GroupId>> {
        Ok(None)
    }
}

/// The override chains refuse an addon overriding one model twice and an override without a
/// declaration of its model in its addon (`OrmBuilder::build` runs the same check at startup);
/// overrides of addons that are not installed take no part. `has_group` with an unknown or a
/// malformed external id is a `Programming` error, never `false`.
#[test]
fn access_domain_chains_fail_loudly() {
    let registry = Harness::builder([&base_users::ADDON, &test_access::ADDON])
        .registry_only()
        .unwrap();
    let linked = erp_orm::ACCESS_DOMAIN_IMPLS
        .iter()
        .find(|i| i.model == "test_access.note")
        .unwrap();
    let err = AccessDomainChains::build(&registry, &[leak("test_access", "test_access.note", 1)])
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        format!(
            "#[access_domain] of model 'test_access.note' is implemented twice by addon \
             'test_access': {}:{} (addon 'test_access') and extra.rs:1 (addon 'test_access')",
            linked.file, linked.line
        )
    );
    let err = AccessDomainChains::build(&registry, &[leak("base_users", "test_access.note", 2)])
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "#[access_domain] at extra.rs:2 (addon 'base_users') has no declaration of model \
         'test_access.note' in addon 'base_users': add an #[extend] (or #[model]) for it in that \
         addon"
    );
    let company = registry.model_id("test_access.company").unwrap();
    let chains = AccessDomainChains::build(
        &registry,
        &[leak("not_installed", "test_access.company", 3)],
    )
    .unwrap();
    assert!(!chains.has_overrides(company));

    let chains = AccessDomainChains::build(&registry, &[]).unwrap();
    let categ = registry.model("test_access.categ").unwrap();
    let groups = GroupSet::default();
    let ctx = AccessCtx::new(AccessCtxParts::new(
        &groups,
        KindClass::Internal,
        ElevationKind::None,
        categ,
        &registry,
        &NoXmlIds,
    ));
    let err = chains
        .apply(&ctx, Operation::Read, &Domain::TRUE)
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "has_group('test_access.group_manager') in an access override of 'test_access.categ': \
         no such group"
    );
    let err = ctx.has_group("group_manager").unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "invalid XML id 'group_manager': it has no module part"
    );
}

/// External domains can name no principal parameter, and the in-memory evaluator refuses one
/// (access predicates are decided in SQL only, R-S6).
#[tokio::test]
async fn parameters_never_reach_memory() {
    let h = harness(DomainPolicy::builder(Otherwise::Allow).build().unwrap()).await;
    let ids = objs(&h).await;
    let mut tx = begin_as(&h, DEMO, &[]).await;
    let objs = records(&tx, OBJ, &ids);
    let mine = cond("user_id", Operator::Eq, param(PrincipalParam::Uid));
    let err = objs.filtered_domain(&mut tx, &mine).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "filtered_domain cannot evaluate principal parameters (user.id, company_ids, …) in \
         [('user_id', '=', user.id)]: access predicates are decided in SQL only"
    );
    // the same domain in a search is bound in SQL
    let found = tx
        .env()
        .records(OBJ)
        .unwrap()
        .search(&mut tx, &mine)
        .await
        .unwrap();
    assert!(found.is_empty());
    // JSON has no parameter form
    let err = dom(erp_sdk::serde_json::json!([["user_id", "=", "user.id"]]));
    assert_eq!(err.to_list().len(), 1, "a plain string, not a parameter");
    drop(tx);
    h.cleanup().await.unwrap();
}

fn refused(description: &str, model: &str, id: i64) -> String {
    format!(
        "Your access rights do not let you access '{description}' ({model}) records.\n\nRecords: {model}({id},), User: {DEMO}"
    )
}

async fn team_fixture(rule_field: &str, policy_member: &str) -> (Harness, i64, i64) {
    let policy = DomainPolicy::builder(Otherwise::Allow)
        .rule(
            "test_access.team",
            Operation::Read,
            dom(serde_json::json!([[
                rule_field,
                "any",
                [["name", "=", policy_member]]
            ]])),
        )
        .build()
        .unwrap();
    let h = harness(policy).await;
    let members = create(
        &h,
        "test_access.member",
        &[row(&[("name", Value::str("vip"))])],
    )
    .await;
    let teams = create(
        &h,
        "test_access.team",
        &[row(&[
            ("name", Value::str("t")),
            ("member_ids", Value::list([Value::Int(members[0])])),
        ])],
    )
    .await;
    (h, teams[0], members[0])
}

/// A rule over a many2many forgets the memoized decision when the link changes: through the
/// field itself (an unlink command) and through the mirror many2many on the comodel (
/// review D-1).
#[tokio::test]
async fn many2many_rules_forget_memoized_checks_when_links_change() {
    const TEAM: &str = "test_access.team";
    // (a) the link is removed through the rule's own field
    let (h, team, member) = team_fixture("member_ids", "vip").await;
    let mut tx = begin_as(&h, DEMO, &[]).await;
    let t = records(&tx, TEAM, &[team]);
    t.check_access(&mut tx, Operation::Read).await.unwrap();
    assert_query_count!(tx, <= 0, {
        t.check_access(&mut tx, Operation::Read).await.unwrap();
    });
    t.write(
        &mut tx,
        RawVals::new().set_commands(
            "member_ids".to_owned(),
            vec![Command::Unlink(erp_sdk::RecId::from_real(member).unwrap())],
        ),
    )
    .await
    .unwrap();
    let err = t.check_access(&mut tx, Operation::Read).await.unwrap_err();
    assert_access(&err, &refused("Access Team", TEAM, team));
    let err = t.cells(&mut tx, "name").await.unwrap_err();
    assert_access(&err, &refused("Access Team", TEAM, team));
    drop(tx);
    h.cleanup().await.unwrap();

    // (c) the rule reads a second field of the same model over the same relation table
    let (h, team, member) = team_fixture("alt_member_ids", "vip").await;
    let mut tx = begin_as(&h, DEMO, &[]).await;
    let t = records(&tx, TEAM, &[team]);
    t.check_access(&mut tx, Operation::Read).await.unwrap();
    t.write(
        &mut tx,
        RawVals::new().set_commands(
            "member_ids".to_owned(),
            vec![Command::Unlink(erp_sdk::RecId::from_real(member).unwrap())],
        ),
    )
    .await
    .unwrap();
    let err = t.check_access(&mut tx, Operation::Read).await.unwrap_err();
    assert_access(&err, &refused("Access Team", TEAM, team));
    drop(tx);
    h.cleanup().await.unwrap();

    // (b) the link is removed through the mirror field of the member, under sudo
    let (h, team, member) = team_fixture("member_ids", "vip").await;
    let mut tx = begin_as(&h, DEMO, &[]).await;
    let t = records(&tx, TEAM, &[team]);
    t.check_access(&mut tx, Operation::Read).await.unwrap();
    let m = records(&tx, "test_access.member", &[member]).sudo(true);
    m.write(
        &mut tx,
        RawVals::new().set_commands(
            "team_ids".to_owned(),
            vec![Command::Unlink(erp_sdk::RecId::from_real(team).unwrap())],
        ),
    )
    .await
    .unwrap();
    let err = t.check_access(&mut tx, Operation::Read).await.unwrap_err();
    assert_access(&err, &refused("Access Team", TEAM, team));
    drop(tx);
    h.cleanup().await.unwrap();
}

/// A write that reads the same model inside (the delegated parent of an `_inherits` child)
/// cannot leave a decision memoized from the state before the buffered values (review F-4).
#[tokio::test]
async fn write_with_delegated_values_leaves_no_stale_memo() {
    const HOLDER: &str = "test_access.holder";
    let policy = DomainPolicy::builder(Otherwise::Allow)
        .rule(HOLDER, Operation::Read, cond("level", Operator::Gt, 0))
        .build()
        .unwrap();
    let h = harness(policy).await;
    let ids = create(
        &h,
        HOLDER,
        &[row(&[("name", Value::str("a")), ("level", Value::Int(1))])],
    )
    .await;
    let mut tx = begin_as(&h, DEMO, &[]).await;
    let held = records(&tx, HOLDER, &ids);
    held.check_access(&mut tx, Operation::Read).await.unwrap();
    held.write_values(
        &mut tx,
        &row(&[("level", Value::Int(-1)), ("name", Value::str("b"))]),
    )
    .await
    .unwrap();
    let err = held
        .check_access(&mut tx, Operation::Read)
        .await
        .unwrap_err();
    assert_access(&err, &refused("Access Holder", HOLDER, ids[0]));
    drop(tx);
    h.cleanup().await.unwrap();
}

/// An id-only rule over a many2many (`member_ids != False`, reading no column of the checked
/// model's table) forgets the memoized decision when the other side of the link is deleted: the
/// database cascades the relation rows away, in both directions (review, unlink).
#[tokio::test]
async fn many2many_rules_forget_memoized_checks_when_a_linked_record_is_deleted() {
    const TEAM: &str = "test_access.team";
    const MEMBER: &str = "test_access.member";
    // (model checked, its link field, the other model)
    for (checked, field, other, description) in [
        (TEAM, "member_ids", MEMBER, "Access Team"),
        (MEMBER, "team_ids", TEAM, "Access Member"),
    ] {
        let policy = DomainPolicy::builder(Otherwise::Allow)
            .rule(checked, Operation::Read, cond(field, Operator::Ne, false))
            .build()
            .unwrap();
        let h = harness(policy).await;
        let members = create(&h, MEMBER, &[row(&[("name", Value::str("m"))])]).await;
        let teams = create(
            &h,
            TEAM,
            &[row(&[
                ("name", Value::str("t")),
                ("member_ids", Value::list([Value::Int(members[0])])),
            ])],
        )
        .await;
        let (checked_id, other_id) = if checked == TEAM {
            (teams[0], members[0])
        } else {
            (members[0], teams[0])
        };
        let mut tx = begin_as(&h, DEMO, &[]).await;
        let rec = records(&tx, checked, &[checked_id]);
        rec.check_access(&mut tx, Operation::Read).await.unwrap();
        records(&tx, other, &[other_id])
            .sudo(true)
            .unlink(&mut tx)
            .await
            .unwrap();
        let err = rec
            .check_access(&mut tx, Operation::Read)
            .await
            .unwrap_err();
        assert_access(&err, &refused(description, checked, checked_id));
        drop(tx);
        h.cleanup().await.unwrap();
    }
}

#[tokio::test]
async fn spec_rel_040_many2many_writes_keep_unreadable_links() {
    let policy = DomainPolicy::builder(Otherwise::Allow)
        .rule(OBJ, Operation::Read, cond("val", Operator::Gt, 0))
        .build()
        .unwrap();
    let h = harness(policy).await;
    let ids = create(
        &h,
        OBJ,
        &[
            row(&[("name", Value::str("t1")), ("val", Value::Int(1))]),
            row(&[("name", Value::str("t2")), ("val", Value::Int(-1))]),
            row(&[("name", Value::str("t3")), ("val", Value::Int(1))]),
        ],
    )
    .await;
    let (t1, t2, t3) = (ids[0], ids[1], ids[2]);
    let container = create(
        &h,
        "test_access.container",
        &[row(&[
            ("name", Value::str("box")),
            ("some_ids", Value::list([Value::Int(t1), Value::Int(t2)])),
        ])],
    )
    .await[0];

    // u writes [SET([t3.id])]: database pairs are t3 and t2
    let mut tx = begin_as(&h, DEMO, &[]).await;
    let boxed = records(&tx, "test_access.container", &[container]);
    boxed
        .write(
            &mut tx,
            RawVals::new().set_commands(
                "some_ids".to_owned(),
                vec![Command::Set(vec![erp_sdk::RecId::from_real(t3).unwrap()])],
            ),
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();

    let mut su = h.begin().await.unwrap();
    let boxed_su = records(&su, "test_access.container", &[container]);
    let current_ids = boxed_su.related(&mut su, "some_ids").await.unwrap().ids();
    assert_eq!(current_ids.len(), 2);
    assert!(current_ids.contains(&t2) && current_ids.contains(&t3));
    drop(su);

    // u writes [UNLINK(t2.id)]: changes nothing
    let mut tx = begin_as(&h, DEMO, &[]).await;
    let boxed = records(&tx, "test_access.container", &[container]);
    boxed
        .write(
            &mut tx,
            RawVals::new().set_commands(
                "some_ids".to_owned(),
                vec![Command::Unlink(erp_sdk::RecId::from_real(t2).unwrap())],
            ),
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();

    let mut su = h.begin().await.unwrap();
    let boxed_su = records(&su, "test_access.container", &[container]);
    let current_ids = boxed_su.related(&mut su, "some_ids").await.unwrap().ids();
    assert_eq!(current_ids.len(), 2);
    assert!(current_ids.contains(&t2) && current_ids.contains(&t3));
    drop(su);

    // u writes [CLEAR]: leaves t2
    let mut tx = begin_as(&h, DEMO, &[]).await;
    let boxed = records(&tx, "test_access.container", &[container]);
    boxed
        .write(
            &mut tx,
            RawVals::new().set_commands("some_ids".to_owned(), vec![Command::Clear]),
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();

    let mut su = h.begin().await.unwrap();
    let boxed_su = records(&su, "test_access.container", &[container]);
    assert_eq!(
        boxed_su.related(&mut su, "some_ids").await.unwrap().ids(),
        [t2]
    );
    drop(su);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_rel_041_many2many_targets_readable_and_match_domain() {
    let domain_box = ModelDecl::new("test_access", "test_access.domain_box")
        .table("test_access_domain_box")
        .fields(
            const {
                &[
                    FieldDecl::char("name"),
                    FieldDecl::many2many("filtered_ids", "test_access.some_obj")
                        .domain(r#"[["name", "ilike", "a"]]"#),
                ]
            },
        );

    let policy = DomainPolicy::builder(Otherwise::Allow)
        .rule(OBJ, Operation::Read, cond("val", Operator::Gt, 0))
        .build()
        .unwrap();
    let h = harness_with(policy, [domain_box]).await;

    let ids = create(
        &h,
        OBJ,
        &[
            row(&[("name", Value::str("allowed")), ("val", Value::Int(1))]),
            // 'forbidden a' matches the domain but demo cannot read it, so the readability check is what refuses it
            row(&[("name", Value::str("forbidden a")), ("val", Value::Int(-1))]),
            row(&[("name", Value::str("foo")), ("val", Value::Int(1))]),
        ],
    )
    .await;
    let (allowed, forbidden, foo) = (ids[0], ids[1], ids[2]);

    let dbox = create(
        &h,
        "test_access.domain_box",
        &[row(&[("name", Value::str("dbox"))])],
    )
    .await[0];

    // 1. Positive path: superuser links allowed (readable and matches domain 'ilike a')
    let mut su = h.begin().await.unwrap();
    let dbox_su = records(&su, "test_access.domain_box", &[dbox]);
    dbox_su
        .write(
            &mut su,
            RawVals::new().set_commands(
                "filtered_ids".to_owned(),
                vec![Command::Link(erp_sdk::RecId::from_real(allowed).unwrap())],
            ),
        )
        .await
        .unwrap();
    // read back and assert target was actually linked
    let current = dbox_su.related(&mut su, "filtered_ids").await.unwrap();
    assert_eq!(current.ids(), [allowed]);
    drop(su);

    // 2. Positive path: user demo links allowed (readable by demo and matches domain)
    let mut tx = begin_as(&h, DEMO, &[]).await;
    let dbox_demo = records(&tx, "test_access.domain_box", &[dbox]);
    dbox_demo
        .write(
            &mut tx,
            RawVals::new().set_commands(
                "filtered_ids".to_owned(),
                vec![Command::Link(erp_sdk::RecId::from_real(allowed).unwrap())],
            ),
        )
        .await
        .unwrap();
    // read back and assert target was linked as the demo user
    let current_demo = dbox_demo.related(&mut tx, "filtered_ids").await.unwrap();
    assert_eq!(current_demo.ids(), [allowed]);
    drop(tx);

    // 3. User demo links forbidden (target matches domain but is unreadable by demo): AccessError
    let mut tx = begin_as(&h, DEMO, &[]).await;
    let dbox_demo = records(&tx, "test_access.domain_box", &[dbox]);
    let err = dbox_demo
        .write(
            &mut tx,
            RawVals::new().set_commands(
                "filtered_ids".to_owned(),
                vec![Command::Link(erp_sdk::RecId::from_real(forbidden).unwrap())],
            ),
        )
        .await
        .unwrap_err();
    assert_access(
        &err,
        &format!(
            "Failed to write field test_access.domain_box.filtered_ids\n{}",
            denied("access", forbidden, DEMO)
        ),
    );
    assert!(
        dbox_demo
            .sudo(true)
            .related(&mut tx, "filtered_ids")
            .await
            .unwrap()
            .ids()
            .is_empty()
    );
    drop(tx);

    let mut su = h.begin().await.unwrap();
    let dbox_rec = records(&su, "test_access.domain_box", &[dbox]);
    let err = dbox_rec
        .write(
            &mut su,
            RawVals::new().set_commands(
                "filtered_ids".to_owned(),
                vec![Command::Link(erp_sdk::RecId::from_real(foo).unwrap())],
            ),
        )
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "Cannot link inaccessible records in test_access.domain_box.filtered_ids \
         ([('name', 'ilike', 'a')])"
    );
    assert!(
        dbox_rec
            .related(&mut su, "filtered_ids")
            .await
            .unwrap()
            .ids()
            .is_empty()
    );
    drop(su);
    h.cleanup().await.unwrap();
}

/// Probe both scalar cache hits and explicit fetch/row boundaries with the same cache state.
async fn cached_read(tx: &mut Transaction, recs: &Records, path: &str) -> erp_sdk::Result<()> {
    match path {
        "cell" => {
            recs.cell(tx, "name").await?;
        }
        "cells" => {
            recs.cells(tx, "name").await?;
        }
        "value" => {
            recs.value(tx, "name").await?;
        }
        "values" => {
            recs.values(tx, "name").await?;
        }
        "related" => {
            recs.related(tx, "categ_id").await?;
        }
        "mapped_path" => {
            recs.mapped_path(tx, "name").await?;
        }
        "web_read" => {
            recs.web_read(tx, erp_orm::Spec::new().field("name"))
                .await?;
        }
        "many2one" => {
            recs.cell(tx, "categ_id").await?;
        }
        "id" => {
            recs.cell(tx, "id").await?;
        }
        "display_name" => {
            recs.cell(tx, "display_name").await?;
        }
        "read" => {
            recs.read(tx, &["name"]).await?;
        }
        "read_rows" => {
            recs.read_rows(tx, &["name"], erp_orm::Load::Raw).await?;
        }
        "read_id" => {
            recs.read(tx, &["id"]).await?;
        }
        "fetch" => {
            recs.fetch(tx, Some(&["name"])).await?;
        }
        _ => panic!("unknown cached read probe: {path}"),
    }
    Ok(())
}

const CACHED_READ_PATHS: &[&str] = &[
    "cell",
    "cells",
    "value",
    "values",
    "id",
    "related",
    "mapped_path",
    "web_read",
    "many2one",
    "display_name",
    "read",
    "read_rows",
    "read_id",
    "fetch",
];

#[tokio::test]
async fn cached_reads_recheck_model_access_after_dropping_sudo() {
    let h = harness(DomainPolicy::builder(Otherwise::Deny).build().unwrap()).await;
    let ids = create(&h, OBJ, &[row(&[("name", Value::str("secret"))])]).await;
    for path in CACHED_READ_PATHS {
        let mut tx = begin_as(&h, DEMO, &[]).await;
        let restricted = records(&tx, OBJ, &ids);
        let elevated = restricted.sudo(true);
        assert_eq!(
            elevated.value(&mut tx, "name").await.unwrap(),
            Value::str("secret")
        );
        let err = cached_read(&mut tx, &elevated.sudo(false), path)
            .await
            .expect_err(path);
        assert_access(
            &err,
            "Your access rights do not let you read 'Object For Test Access Right' (test_access.some_obj) records.",
        );
        // Denial never destroys the value or authorizes the restricted identity.
        assert_eq!(
            elevated.value(&mut tx, "name").await.unwrap(),
            Value::str("secret")
        );
        assert_access(
            &cached_read(&mut tx, &restricted, path)
                .await
                .expect_err(path),
            err.message(),
        );
        drop(tx);
    }
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn cached_reads_recheck_record_rules_after_switching_user() {
    let policy = DomainPolicy::builder(Otherwise::Allow)
        .rule(
            OBJ,
            Operation::Read,
            cond("val", Operator::Eq, param(PrincipalParam::Uid)),
        )
        .build()
        .unwrap();
    let h = harness(policy).await;
    let ids = create(
        &h,
        OBJ,
        &[row(&[
            ("name", Value::str("private")),
            ("val", Value::Int(DEMO)),
        ])],
    )
    .await;
    for path in CACHED_READ_PATHS {
        let mut tx = begin_as(&h, DEMO, &[]).await;
        let allowed = records(&tx, OBJ, &ids);
        assert_eq!(
            allowed.value(&mut tx, "name").await.unwrap(),
            Value::str("private")
        );
        allowed
            .write_values(&mut tx, &row(&[("name", Value::str("pending private"))]))
            .await
            .unwrap();
        // made by System: demo has no `principal.impersonate` (A4); the cache is the
        // transaction's either way
        let restricted = allowed.with_env(
            h.orm()
                .env(erp_orm::SUPERUSER_ID, erp_orm::Context::new())
                .with_user(OTHER),
        );
        let err = cached_read(&mut tx, &restricted, path)
            .await
            .expect_err(path);
        assert_access(&err, &denied("access", ids[0], OTHER));
        assert_eq!(
            allowed.value(&mut tx, "name").await.unwrap(),
            Value::str("pending private")
        );
        assert_access(
            &cached_read(&mut tx, &restricted, path)
                .await
                .expect_err(path),
            &denied("access", ids[0], OTHER),
        );
        drop(tx);
    }
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn cached_reads_recheck_record_rules_after_narrowing_companies() {
    let policy = DomainPolicy::builder(Otherwise::Allow)
        .rule(
            OBJ,
            Operation::Read,
            cond(
                "company_id",
                Operator::In,
                param(PrincipalParam::CompanyIds),
            ),
        )
        .build()
        .unwrap();
    let h = harness(policy).await;
    let companies = create(
        &h,
        "test_access.company",
        &[
            row(&[("name", Value::str("A"))]),
            row(&[("name", Value::str("B"))]),
        ],
    )
    .await;
    let ids = create(
        &h,
        OBJ,
        &[row(&[
            ("name", Value::str("company private")),
            ("company_id", Value::Int(companies[1])),
        ])],
    )
    .await;
    for path in CACHED_READ_PATHS {
        let mut tx = begin_as(&h, DEMO, &companies).await;
        let allowed = records(&tx, OBJ, &ids);
        assert_eq!(
            allowed.value(&mut tx, "name").await.unwrap(),
            Value::str("company private")
        );
        let restricted = allowed.with_context([(
            "allowed_company_ids",
            Value::list([Value::Int(companies[0])]),
        )]);
        let err = cached_read(&mut tx, &restricted, path)
            .await
            .expect_err(path);
        // This fixture's custom rule has no generated multi-company hint.
        assert_access(&err, &denied("access", ids[0], DEMO));
        assert_eq!(
            allowed.value(&mut tx, "name").await.unwrap(),
            Value::str("company private")
        );
        drop(tx);
    }
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn cached_reads_recheck_the_origin_of_a_new_record() {
    let policy = DomainPolicy::builder(Otherwise::Allow)
        .rule(OBJ, Operation::Read, cond("val", Operator::Gt, 0))
        .build()
        .unwrap();
    let h = harness(policy).await;
    let ids = create(
        &h,
        OBJ,
        &[row(&[
            ("name", Value::str("hidden origin")),
            ("val", Value::Int(-1)),
        ])],
    )
    .await;
    let mut tx = begin_as(&h, DEMO, &[]).await;
    let elevated = records(&tx, OBJ, &ids).sudo(true);
    let shadow = tx
        .new_record(
            &elevated,
            RawVals::new(),
            Some(erp_sdk::RecId::from_real(ids[0]).unwrap()),
            None,
        )
        .await
        .unwrap();
    assert_eq!(
        shadow.value(&mut tx, "name").await.unwrap(),
        Value::str("hidden origin")
    );
    let restricted = shadow.sudo(false);
    let err = restricted.value(&mut tx, "name").await.unwrap_err();
    assert_access(&err, &denied("access", ids[0], DEMO));
    assert_eq!(
        shadow.value(&mut tx, "name").await.unwrap(),
        Value::str("hidden origin")
    );
    drop(tx);
    h.cleanup().await.unwrap();
}
