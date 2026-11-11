//! RSET cases of S1: construction, container protocol, set algebra, ensure_one, display_name.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::mutable_key_type,
    reason = "tests"
)]

mod support;

use std::collections::{HashMap, HashSet};

use erp_sdk::{Base, ErrorKind, RecordSet, Transaction, Vals, Value};
use erp_test_models::test_orm::{Country, NoName, Partner};
use erp_testkit::assert_query_count;
use support::map;

async fn partners(tx: &mut Transaction, n: usize) -> RecordSet<Partner> {
    let vals = (0..n)
        .map(|i| Vals::new().set(Partner::NAME, format!("p{i}").as_str()))
        .collect();
    tx.env()
        .model::<Partner>()
        .unwrap()
        .create(tx, vals)
        .await
        .unwrap()
}

#[tokio::test]
async fn spec_rset_001_browse_input_forms() {
    let h = support::orm().await;
    let tx = h.begin().await.unwrap();
    let m = tx.env().model::<Partner>().unwrap();
    let (a, b) = (11, 12);
    assert_query_count!(tx, <= 0, {
        assert!(m.browse([]).unwrap().is_empty());
        assert_eq!(m.browse([a]).unwrap().ids(), [a]);
        assert_eq!(m.browse([a, b, a]).unwrap().ids(), [a, b, a]);
        assert_eq!(m.browse([b, a].into_iter()).unwrap().ids(), [b, a]);
        assert_eq!(m.browse([999_999_999]).unwrap().len(), 1);
    });
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_rset_002_browse_rejects_falsy_real_ids() {
    let h = support::orm().await;
    let tx = h.begin().await.unwrap();
    let m = tx.env().model::<Partner>().unwrap();
    let err = m.browse([5, 0]).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(err.message(), "Invalid falsy real id");
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_rset_003_ids_id_and_int() {
    let h = support::orm().await;
    let mut tx = h.begin().await.unwrap();
    let rs = partners(&mut tx, 2).await;
    let (p, q) = (rs.at(0).unwrap(), rs.at(1).unwrap());
    let ppq = &(&p + &q) + &p;
    assert_eq!(ppq.ids(), [p.ids()[0], q.ids()[0], p.ids()[0]]);
    let empty = rs.browse([]).unwrap();
    assert!(empty.ids().is_empty());
    assert_eq!(p.id().unwrap().unwrap().get(), p.ids()[0]);
    assert_eq!(empty.id().unwrap(), None);
    let err = (&p + &q).id().unwrap_err();
    assert_eq!(err.kind(), erp_sdk::ErrorKind::Programming);
    assert_eq!(
        err.message(),
        format!(
            "Expected singleton: test_orm.partner({}, {})",
            p.ids()[0],
            q.ids()[0]
        )
    );
    assert_eq!(p.int().unwrap(), p.ids()[0]);
    assert_eq!(empty.int().unwrap(), 0);
    assert!((&p + &q).int().is_err());
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_rset_004_recordsets_are_immutable_id_snapshots() {
    let h = support::orm().await;
    let mut tx = h.begin().await.unwrap();
    let rs = partners(&mut tx, 3).await;
    let ids = rs.ids();
    rs.set(&mut tx, Partner::ACTIVE, false).await.unwrap();
    tx.flush().await.unwrap();
    assert_eq!(rs.ids(), ids);
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_rset_005_derived_records_keep_the_environment() {
    let h = support::orm().await;
    let mut tx = h.begin().await.unwrap();
    let rs = partners(&mut tx, 3).await;
    let env = rs.env().clone();
    assert_eq!(rs.at(0).unwrap().env(), &env);
    assert!(rs.iter().all(|r| r.env() == &env));
    let demo = rs.with_user(7);
    let demo_env = env.with_user(7);
    assert_eq!(demo.env(), &demo_env);
    assert_eq!(demo.at(0).unwrap().env(), &demo_env);
    assert!(demo.iter().all(|r| r.env() == &demo_env));
    assert_eq!(rs.env(), &env);
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_rset_006_007_008_length_iteration_indexing() {
    let h = support::orm().await;
    let mut tx = h.begin().await.unwrap();
    let rs = partners(&mut tx, 5).await;
    let ids = rs.ids();
    let r = |i: usize| rs.at(i as isize).unwrap();
    let empty = rs.browse([]).unwrap();
    // 006
    assert!(empty.is_empty());
    assert_eq!(empty.len(), 0);
    assert!(!r(0).is_empty());
    assert_eq!((&(&r(0) + &r(0)) + &r(1)).len(), 3);
    // 007
    let pqp = &(&r(0) + &r(1)) + &r(0);
    let forward: Vec<_> = pqp.iter().map(|x| x.ids()[0]).collect();
    assert_eq!(forward, [ids[0], ids[1], ids[0]]);
    let backward: Vec<_> = pqp.iter().rev().map(|x| x.ids()[0]).collect();
    assert_eq!(backward, [ids[0], ids[1], ids[0]]);
    assert!(pqp.iter().all(|x| x.prefetch_ids() == pqp.prefetch_ids()));
    assert_eq!(empty.iter().count(), 0);
    assert_eq!(r(0).iter().count(), 1);
    // 008
    assert_query_count!(tx, <= 0, {
        assert_eq!(rs.at(-1).unwrap().ids(), [ids[4]]);
        assert_eq!(rs.slice(Some(1), Some(3), 1).unwrap().ids(), &ids[1..3]);
        let rev: Vec<_> = ids.iter().rev().copied().collect();
        assert_eq!(rs.slice(None, None, -1).unwrap().ids(), rev);
        assert_eq!(rs.slice(None, None, 2).unwrap().ids(), [ids[0], ids[2], ids[4]]);
        assert!(rs.slice(Some(10), None, 1).unwrap().is_empty());
        assert_eq!(rs.at(5).unwrap_err().kind(), ErrorKind::Programming);
        assert!(empty.at(0).is_err());
    });
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_rset_009_string_indexing_reads_and_assigns() {
    let h = support::orm().await;
    let mut tx = h.begin().await.unwrap();
    let typed = partners(&mut tx, 2).await;
    let rs = typed.as_records().clone();
    let p = rs.at(0).unwrap();
    let e = rs.browse([]).unwrap();
    assert_eq!(p.value(&mut tx, "name").await.unwrap(), Value::str("p0"));
    assert_eq!(e.value(&mut tx, "name").await.unwrap(), Value::False);
    assert_eq!(
        rs.value(&mut tx, "name").await.unwrap_err().kind(),
        ErrorKind::Programming
    );
    let err = p.value(&mut tx, "nope").await.unwrap_err();
    assert_eq!(err.kind(), erp_sdk::ErrorKind::Programming);
    assert!(err.message().contains("'nope'"), "{err}");
    tx.capture_statements(true);
    rs.set_value(&mut tx, "email", Value::str("x@y"))
        .await
        .unwrap();
    tx.flush().await.unwrap();
    assert_eq!(tx.captured().len(), 1, "one UPDATE for both records");
    assert_eq!(
        rs.values(&mut tx, "email").await.unwrap(),
        [Value::str("x@y"), Value::str("x@y")]
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_rset_010_membership() {
    let h = support::orm().await;
    let mut tx = h.begin().await.unwrap();
    let partners_rs = partners(&mut tx, 3).await;
    let p = partners_rs.at(0).unwrap();
    assert!(partners_rs.contains(&p).unwrap());
    assert!(
        !partners_rs
            .contains(&partners_rs.browse([]).unwrap())
            .unwrap()
    );
    assert!(
        !partners_rs
            .contains(&(&p + &partners_rs.at(1).unwrap()))
            .unwrap()
    );
    assert!(partners_rs.has_field("name"));
    assert!(!partners_rs.has_field("nope"));
    let country = tx.env().records("test_orm.country").unwrap();
    let err = partners_rs.as_records().contains(&country).unwrap_err();
    assert_eq!(err.kind(), erp_sdk::ErrorKind::Programming);
    assert!(
        err.message()
            .starts_with("inconsistent models in: test_orm.country() in test_orm.partner("),
        "{err}"
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_rset_012_015_equality_subsets_hash_repr() {
    let h = support::orm().await;
    let mut tx = h.begin().await.unwrap();
    let rs = partners(&mut tx, 3).await;
    let (p, q) = (rs.at(0).unwrap(), rs.at(1).unwrap());
    // 012
    assert_eq!(&p + &q, &q + &p);
    assert_eq!(&p + &p, p);
    assert_eq!(p.sudo(true), p);
    let empty_country = tx.env().records("test_orm.country").unwrap();
    assert_ne!(rs.browse([]).unwrap().as_records(), &empty_country);
    // 013
    let a = &p + &q;
    let b = &q + &rs.at(2).unwrap();
    assert!((&a - &b).is_subset(&a).unwrap());
    assert!((&a & &b).is_subset(&b).unwrap());
    assert!(a.is_subset(&(&a | &b)).unwrap());
    assert!(!a.is_strict_subset(&a).unwrap());
    assert!(a.is_subset(&a).unwrap());
    assert!(rs.browse([]).unwrap().is_subset(&a).unwrap());
    assert!(a.as_records().is_subset(&empty_country).is_err());
    // 014
    let mut map: HashMap<RecordSet<Partner>, i32> = HashMap::new();
    map.insert(&p + &q, 1);
    assert_eq!(map.get(&(&q + &p)), Some(&1));
    let set: HashSet<RecordSet<Partner>> = [p.clone(), &p + &p, p.sudo(true)].into_iter().collect();
    assert_eq!(set.len(), 1);
    // 015
    let ids = rs.ids();
    assert_eq!(
        format!("{}", rs.slice(Some(0), Some(2), 1).unwrap()),
        format!("test_orm.partner({}, {})", ids[0], ids[1])
    );
    assert_eq!(format!("{p}"), format!("test_orm.partner({},)", ids[0]));
    assert_eq!(format!("{}", rs.browse([]).unwrap()), "test_orm.partner()");
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_rset_016_020_set_algebra() {
    let h = support::orm().await;
    let mut tx = h.begin().await.unwrap();
    let rs = partners(&mut tx, 3).await;
    let ids = rs.ids();
    let (p, q, r) = (rs.at(0).unwrap(), rs.at(1).unwrap(), rs.at(2).unwrap());
    let empty = rs.browse([]).unwrap();
    // 016
    let a = &p + &q;
    let b = &q + &r;
    assert_eq!((&a + &b).ids(), [ids[0], ids[1], ids[1], ids[2]]);
    let same = &a + &empty;
    assert_eq!(same.ids(), a.ids());
    assert_eq!(same.prefetch_ids(), a.prefetch_ids());
    // 017
    assert_eq!((&a | &b).ids(), [ids[0], ids[1], ids[2]]);
    assert_eq!((&(&q + &p) | &(&p + &r)).ids(), [ids[1], ids[0], ids[2]]);
    assert_eq!((&(&p + &p) | &empty).ids(), [ids[0], ids[0]]);
    assert_eq!((&(&p + &p) | &q).ids(), [ids[0], ids[1]]);
    // 018
    let ppq = &(&p + &q) + &p;
    assert_eq!((&ppq & &(&p + &r)).ids(), [ids[0]]);
    assert_eq!((&ppq - &r).ids(), [ids[0], ids[1], ids[0]]);
    assert!((&a - &a).is_empty());
    // 019 (untyped)
    let country = tx.env().records("test_orm.country").unwrap();
    for (result, op) in [
        (a.as_records().concat(&country), "+"),
        (a.as_records().difference(&country), "-"),
        (a.as_records().intersection(&country), "&"),
        (a.as_records().union(&country), "|"),
    ] {
        let err = result.unwrap_err();
        assert_eq!(err.kind(), erp_sdk::ErrorKind::Programming);
        assert!(
            err.message()
                .contains(&format!(") {op} test_orm.country()")),
            "{op}: {err}"
        );
    }
    assert_ne!(a.as_records(), &country);
    // 020
    assert_eq!(
        empty.concat_all([&a, &empty, &b]).unwrap().ids(),
        [ids[0], ids[1], ids[1], ids[2]]
    );
    assert_eq!(
        empty.union_all([&a, &b]).unwrap().ids(),
        [ids[0], ids[1], ids[2]]
    );
    assert_eq!(empty.union_all([&(&p + &p)]).unwrap().ids(), [ids[0]]);
    assert!(empty.as_records().concat_all([&country]).is_err());
    let _ = Country::NAME;
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_rset_022_ensure_one() {
    let h = support::orm().await;
    let mut tx = h.begin().await.unwrap();
    let rs = partners(&mut tx, 3).await;
    let ids = rs.ids();
    let err = rs.ensure_one().unwrap_err();
    assert_eq!(err.kind(), erp_sdk::ErrorKind::Programming);
    assert_eq!(
        err.message(),
        format!(
            "Expected singleton: test_orm.partner({}, {}, {})",
            ids[0], ids[1], ids[2]
        )
    );
    let first = rs.at(0).unwrap();
    assert_eq!(first.ensure_one().unwrap(), &first);
    assert_eq!(
        rs.browse([]).unwrap().ensure_one().unwrap_err().message(),
        "Expected singleton: test_orm.partner()"
    );
    let pp = &first + &first;
    assert_eq!(
        pp.ensure_one().unwrap_err().message(),
        format!("Expected singleton: test_orm.partner({0}, {0})", ids[0])
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_rset_051_default_display_name() {
    let h = support::orm().await;
    let mut tx = h.begin().await.unwrap();
    let rs = partners(&mut tx, 2).await;
    let first = rs.at(0).unwrap();
    assert_eq!(
        first
            .get(&mut tx, Base::DISPLAY_NAME)
            .await
            .unwrap()
            .as_deref(),
        Some("p0")
    );
    let labels = rs.mapped(&mut tx, Base::DISPLAY_NAME).await.unwrap();
    assert_eq!(labels.len(), 2);
    let noname = tx
        .env()
        .model::<NoName>()
        .unwrap()
        .create(&mut tx, vec![Vals::new()])
        .await
        .unwrap();
    assert_eq!(
        noname
            .get(&mut tx, Base::DISPLAY_NAME)
            .await
            .unwrap()
            .as_deref(),
        Some(format!("test_orm.noname,{}", noname.ids()[0]).as_str())
    );
    let blank = tx
        .env()
        .records("test_orm.partner")
        .unwrap()
        .create_values(&mut tx, &[map(&[("name", Value::str(""))])])
        .await
        .unwrap();
    assert_eq!(
        blank.value(&mut tx, "display_name").await.unwrap(),
        Value::False
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_rset_052_display_name_on_several_records() {
    let h = support::orm().await;
    let mut tx = h.begin().await.unwrap();
    let rs = partners(&mut tx, 2).await;
    let err = rs.get(&mut tx, Base::DISPLAY_NAME).await.unwrap_err();
    assert_eq!(err.kind(), erp_sdk::ErrorKind::Programming);
    assert!(err.message().starts_with("Expected singleton: "), "{err}");
    drop(tx);
    h.cleanup().await.unwrap();
}
