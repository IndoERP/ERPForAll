//! ENV cases of S1: environments, the shared transaction cache, commit/rollback, create cache.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

mod support;

use erp_sdk::{Cell, Context, SUPERUSER_ID, Vals, Value};
use erp_test_models::test_orm::{Category, Mixed, Partner};
use erp_testkit::assert_query_count;
use support::{map, one};

fn ctx(pairs: &[(&str, Value)]) -> Context {
    Context::from_pairs(pairs.iter().map(|(k, v)| (*k, v.clone())))
}

#[tokio::test]
async fn spec_env_001_equal_parameters_give_equal_environments() {
    let h = support::orm().await;
    let tx = h.begin().await.unwrap();
    let base = tx.env().records("base").unwrap();
    let e1 = base
        .with_context([("test_stuff", Value::Bool(false))])
        .env()
        .clone();
    let e2 = base
        .with_context([("test_stuff", Value::Bool(false))])
        .env()
        .clone();
    assert_eq!(e1, e2);
    assert_ne!(e1, tx.env());
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_env_003_superuser_id_forces_superuser_mode() {
    let h = support::orm().await;
    let tx = h.begin_as(5).await.unwrap();
    let env = tx.env();
    assert!(!env.su());
    assert!(h.orm().env(SUPERUSER_ID, Context::new()).su());
    assert!(env.with_user(SUPERUSER_ID).su());
    let recs = env.records("test_orm.partner").unwrap();
    assert!(recs.with_user(SUPERUSER_ID).env().su());
    assert!(!recs.sudo(true).with_user(5).env().su());
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_env_004_sudo_semantics() {
    let h = support::orm().await;
    let tx = h.begin_as(5).await.unwrap();
    let env = tx.env().with_context([
        ("default_name", Value::str("x")),
        ("lang", Value::str("fr_FR")),
    ]);
    let records = env
        .records("test_orm.partner")
        .unwrap()
        .browse([1, 2])
        .unwrap();
    let r2 = records.sudo(true);
    assert!(r2.env().su());
    assert_eq!(r2.env().uid(), 5);
    assert_eq!(
        r2.env().context().to_map(),
        map(&[("lang", Value::str("fr_FR"))])
    );
    let r3 = r2.sudo(true);
    assert_eq!(r3.env(), r2.env());
    let r4 = r2.sudo(false);
    assert!(!r4.env().su());
    assert_eq!(r4.env().context(), r2.env().context());
    for r in [&r2, &r3, &r4] {
        assert_eq!(r.prefetch_ids(), records.prefetch_ids());
    }
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_env_005_with_user() {
    let h = support::orm().await;
    let tx = h.begin().await.unwrap();
    let records = tx
        .env()
        .records("test_orm.partner")
        .unwrap()
        .browse([3, 4])
        .unwrap();
    assert_eq!(records.with_user(0).env(), records.env());
    let other = records.with_user(7);
    assert_eq!(other.env().uid(), 7);
    assert!(!other.env().su());
    assert_eq!(other.env().context(), records.env().context());
    assert_eq!(other.rec_ids(), records.rec_ids());
    assert_eq!(other.prefetch_ids(), records.prefetch_ids());
    // records reached from the new recordset keep its environment
    assert!(other.iter().all(|r| r.env() == other.env()));
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_env_006_with_context_merge_replace_sticky_companies() {
    let h = support::orm().await;
    let tx = h.begin().await.unwrap();
    let start = tx
        .env()
        .with_context_replaced(ctx(&[("nothing_to_see_here", Value::Bool(true))]));
    let ids = |v: &[i64]| Value::list(v.iter().map(|i| Value::Int(*i)));
    let e1 = start.with_context([("allowed_company_ids", ids(&[1]))]);
    assert_eq!(
        e1.context().to_map(),
        map(&[
            ("nothing_to_see_here", Value::Bool(true)),
            ("allowed_company_ids", ids(&[1]))
        ])
    );
    let e2 = e1.with_context([("allowed_company_ids", ids(&[2]))]);
    assert_eq!(
        e2.context().to_map(),
        map(&[
            ("nothing_to_see_here", Value::Bool(true)),
            ("allowed_company_ids", ids(&[2]))
        ])
    );
    let e3 = e2.with_context_replaced(ctx(&[("allowed_company_ids", ids(&[1]))]));
    assert_eq!(
        e3.context().to_map(),
        map(&[("allowed_company_ids", ids(&[1]))])
    );
    let e4 = e3.with_context_replaced(ctx(&[("nothing_to_see_here", Value::Bool(true))]));
    assert_eq!(
        e4.context().to_map(),
        map(&[
            ("nothing_to_see_here", Value::Bool(true)),
            ("allowed_company_ids", ids(&[1]))
        ])
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_env_010_lang_and_tz() {
    let h = support::orm().await;
    let tx = h.begin().await.unwrap();
    let env = tx.env();
    assert_eq!(env.lang(), None);
    assert_eq!(
        env.with_context([("tz", Value::str("Not/AZone"))]).tz(),
        "UTC"
    );
    assert_eq!(
        env.with_context([("tz", Value::str("Asia/Jakarta"))]).tz(),
        "Asia/Jakarta"
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_env_012_one_cache_shared_by_all_environments() {
    let h = support::orm().await;
    let mut tx = h.begin().await.unwrap();
    let partners = tx.env().model::<Partner>().unwrap();
    let created = partners
        .create(&mut tx, vec![Vals::new(), Vals::new()])
        .await
        .unwrap();
    let (foo1, bar1) = (created.at(0).unwrap(), created.at(1).unwrap());
    let demo_uid = tx
        .env()
        .records("res.users")
        .unwrap()
        .create_values(&mut tx, &[map(&[("name", Value::str("Demo"))])])
        .await
        .unwrap()
        .ids()[0];
    let demo = created.with_user(demo_uid);
    let (foo2, bar2) = (demo.at(0).unwrap(), demo.at(1).unwrap());
    for (rec, name, email) in [(&foo1, "foo", "foo@x"), (&bar1, "bar", "bar@x")] {
        rec.write(
            &mut tx,
            Vals::new()
                .set(Partner::NAME, name)
                .set(Partner::EMAIL, email),
        )
        .await
        .unwrap();
    }
    assert_query_count!(tx, <= 0, {
        assert_eq!(foo2.get(&mut tx, Partner::NAME).await.unwrap().as_deref(), Some("foo"));
        assert_eq!(bar2.get(&mut tx, Partner::EMAIL).await.unwrap().as_deref(), Some("bar@x"));
    });
    for (rec, name, email) in [(&foo2, "FOO", "foo@y"), (&bar2, "BAR", "bar@y")] {
        rec.write(
            &mut tx,
            Vals::new()
                .set(Partner::NAME, name)
                .set(Partner::EMAIL, email),
        )
        .await
        .unwrap();
    }
    assert_eq!(
        foo1.get(&mut tx, Partner::NAME).await.unwrap().as_deref(),
        Some("FOO")
    );
    tx.invalidate_model("test_orm.partner", Some(&["name"]))
        .await
        .unwrap();
    tx.invalidate_records(&foo1, Some(&["email"]))
        .await
        .unwrap();
    assert!(!tx.is_cached(&foo1, "name").unwrap() && !tx.is_cached(&bar1, "name").unwrap());
    assert!(!tx.is_cached(&foo1, "email").unwrap());
    assert!(tx.is_cached(&bar1, "email").unwrap());
    tx.invalidate_all().await.unwrap();
    assert!(!tx.is_cached(&bar1, "email").unwrap());
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_env_013_commit_and_rollback_empty_the_cache() {
    let h = support::orm().await;
    let mut tx = h.begin().await.unwrap();
    let rec = tx
        .env()
        .model::<Partner>()
        .unwrap()
        .create(&mut tx, vec![Vals::new().set(Partner::NAME, "committed")])
        .await
        .unwrap();
    assert!(tx.is_cached(&rec, "name").unwrap());
    tx.commit().await.unwrap();
    assert!(!tx.is_cached(&rec, "name").unwrap());
    let name = assert_query_count!(tx, <= 1, { rec.get(&mut tx, Partner::NAME).await.unwrap() });
    assert_eq!(name.as_deref(), Some("committed"));

    rec.write(&mut tx, Vals::new().set(Partner::NAME, "uncommitted"))
        .await
        .unwrap();
    tx.flush().await.unwrap();
    tx.rollback().await.unwrap();
    assert!(!tx.is_cached(&rec, "name").unwrap());
    assert_eq!(
        rec.get(&mut tx, Partner::NAME).await.unwrap().as_deref(),
        Some("committed")
    );
    assert_eq!(
        one(&mut tx, "SELECT name FROM test_orm_partner")
            .await
            .as_deref(),
        Some("committed")
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_env_019_cache_after_create_equals_the_database() {
    let h = support::orm().await;
    let mut tx = h.begin().await.unwrap();
    let rec = tx
        .env()
        .model::<Mixed>()
        .unwrap()
        .create(&mut tx, vec![Vals::new()])
        .await
        .unwrap();
    let cached = assert_query_count!(tx, <= 0, { rec.cell(&mut tx, "boolean").await.unwrap() });
    tx.invalidate_all().await.unwrap();
    let fetched = rec.cell(&mut tx, "boolean").await.unwrap();
    assert_eq!(cached, Cell::Null);
    assert_eq!(cached, fetched);
    assert!(!rec.get(&mut tx, Mixed::BOOLEAN).await.unwrap());
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_env_014_environment_pool_stays_bounded() {
    let h = support::orm().await;
    let tx = h.begin().await.unwrap();
    let base = tx.env().records("base").unwrap();
    // Equal parameters give equal environments
    let env_a = base
        .with_user(42)
        .with_context([("k", Value::Int(1))])
        .env()
        .clone();
    let env_b = base
        .with_user(42)
        .with_context([("k", Value::Int(1))])
        .env()
        .clone();
    assert_eq!(env_a, env_b);

    let env_diff_uid = base
        .with_user(43)
        .with_context([("k", Value::Int(1))])
        .env()
        .clone();
    assert_ne!(env_a, env_diff_uid);

    let env_diff_ctx = base
        .with_user(42)
        .with_context([("k", Value::Int(2))])
        .env()
        .clone();
    assert_ne!(env_a, env_diff_ctx);

    assert_eq!(tx.env().uid(), erp_sdk::SUPERUSER_ID);
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_env_025_cache_only_fields_vanish_on_invalidation() {
    let h = support::orm().await;
    let mut tx = h.begin().await.unwrap();
    let m = tx.env().model::<Category>().unwrap();
    let rec = m
        .create(
            &mut tx,
            vec![
                Vals::new()
                    .set(Category::NAME, "X")
                    .set(Category::DUMMY, "v"),
            ],
        )
        .await
        .unwrap();
    assert_eq!(
        rec.get(&mut tx, Category::DUMMY).await.unwrap().as_deref(),
        Some("v")
    );
    tx.invalidate_all().await.unwrap();
    assert_eq!(rec.get(&mut tx, Category::DUMMY).await.unwrap(), None);

    rec.set(&mut tx, Category::DUMMY, "w").await.unwrap();
    assert_eq!(
        rec.get(&mut tx, Category::DUMMY).await.unwrap().as_deref(),
        Some("w")
    );
    tx.invalidate_all().await.unwrap();
    assert_eq!(rec.get(&mut tx, Category::DUMMY).await.unwrap(), None);

    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_env_007_with_company_reorders_allowed_companies() {
    let h = support::orm().await;
    let tx = h.begin().await.unwrap();
    let base_env = tx.env().with_context([(
        "allowed_company_ids",
        Value::list([Value::Int(1), Value::Int(2)]),
    )]);
    assert_eq!(
        base_env.context().allowed_company_ids().unwrap(),
        Some([1, 2].as_slice())
    );

    // with_company(2) puts company 2 first
    let env_c = base_env.with_company(2).unwrap();
    assert_eq!(
        env_c.context().allowed_company_ids().unwrap(),
        Some([2, 1].as_slice())
    );

    // with_company(1) puts company 1 first again
    let env_a = env_c.with_company(1).unwrap();
    assert_eq!(
        env_a.context().allowed_company_ids().unwrap(),
        Some([1, 2].as_slice())
    );

    // with_company(0) leaves context unchanged
    let env_zero = env_a.with_company(0).unwrap();
    assert_eq!(
        env_zero.context().allowed_company_ids().unwrap(),
        Some([1, 2].as_slice())
    );

    drop(tx);
    h.cleanup().await.unwrap();
}
