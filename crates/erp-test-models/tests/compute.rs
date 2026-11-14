//! Computed fields, recompute and flush for scalar dependencies (M1/S4a): COMP, CRUD and ENV
//! cases. Fixtures: `erp_test_models::test_compute`.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

use support::currency_fixture;

mod support;

use std::sync::Arc;

use erp_sdk::{ErrorKind, Operation, OrmConfig, RecordSet, Transaction, Value};
use erp_test_models::test_compute::{
    self as tc, Cascade, ComputeInverse, ComputeRaises, ComputeReadonly, ComputeReadwrite,
    ComputeSudo, ComputeUnassigned, Constrained, Counted, Created, Creator, CtxKey, DependsExt,
    MultiComputeInverse, PerfBase, PingPong, Precompute, PrecomputeCombo, PrecomputeEditable,
    PrecomputeReadonly, RelatedSingle, RequiredLate, Undeclared,
};
use erp_test_models::test_orm::{Country, Mixed, Partner};
use erp_test_models::{base_users, test_depends, test_orm};
use erp_testkit::{DomainPolicy, Harness, Otherwise, assert_query_count};

async fn harness() -> Harness {
    Harness::builder([&base_users::ADDON, &test_orm::ADDON, &tc::ADDON])
        .with_static_decls(crate::currency_fixture::decls())
        .build()
        .await
        .expect("harness")
}

/// `(field, value)` pairs given to a create.
type Given = &'static [(&'static str, &'static str)];

fn s(v: Option<Arc<str>>) -> Option<String> {
    v.map(|s| s.to_string())
}

/// A tag for the fixture log, unique per test.
fn tagged<M: erp_sdk::Model>(rs: &RecordSet<M>, tag: &str) -> RecordSet<M> {
    rs.with_context([("log", Value::str(tag))])
}

#[tokio::test]
async fn comp_002_dependencies_accumulate_across_overrides() {
    let h = Harness::builder([&tc::ADDON, &tc::ext::ADDON])
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    assert_eq!(
        tx.field_depends("test_orm.depends_ext", "x").unwrap(),
        ["a", "b"],
        "base first"
    );
    let model = tx.env().model::<DependsExt>().unwrap();
    let rec = model
        .create(
            &mut tx,
            vec![model.vals().set(DependsExt::A, "A").set(DependsExt::B, "B")],
        )
        .await
        .unwrap();
    assert_eq!(
        s(rec.get(&mut tx, DependsExt::X).await.unwrap()).as_deref(),
        Some("A/B")
    );
    // writing `b` (a dependency added by the override) invalidates `x`
    rec.set(&mut tx, DependsExt::B, "C").await.unwrap();
    assert!(!tx.is_cached(&rec, "x").unwrap());
    assert_eq!(
        s(rec.get(&mut tx, DependsExt::X).await.unwrap()).as_deref(),
        Some("A/C")
    );
    drop(tx);
    h.cleanup().await.unwrap();

    // without the extension: only `a`
    let h = Harness::builder([&tc::ADDON]).build().await.unwrap();
    let tx = h.begin().await.unwrap();
    assert_eq!(
        tx.field_depends("test_orm.depends_ext", "x").unwrap(),
        ["a"]
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn comp_003_explicit_depends_replace_the_methods() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    assert_eq!(
        tx.field_depends("test_orm.depends_ext", "y").unwrap(),
        ["b"]
    );
    let model = tx.env().model::<DependsExt>().unwrap();
    let rec = model
        .create(
            &mut tx,
            vec![model.vals().set(DependsExt::A, "a").set(DependsExt::B, "b")],
        )
        .await
        .unwrap();
    assert_eq!(
        s(rec.get(&mut tx, DependsExt::Y).await.unwrap()).as_deref(),
        Some("a+b")
    );
    // `a` is only read: writing it marks nothing
    rec.set(&mut tx, DependsExt::A, "A").await.unwrap();
    assert!(tx.records_to_compute(&rec, "y").unwrap().is_empty());
    rec.set(&mut tx, DependsExt::B, "B").await.unwrap();
    assert_eq!(tx.records_to_compute(&rec, "y").unwrap(), *rec.as_records());
    assert_eq!(
        s(rec.get(&mut tx, DependsExt::Y).await.unwrap()).as_deref(),
        Some("A+B")
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn comp_010_transitive_dependents_are_updated() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let model = tx.env().model::<Cascade>().unwrap();
    let rec = model
        .create(&mut tx, vec![model.vals().set(Cascade::FOO, "Hi")])
        .await
        .unwrap();
    assert_eq!(
        s(rec.get(&mut tx, Cascade::BAZ).await.unwrap()).as_deref(),
        Some("<[Hi]>")
    );
    rec.set(&mut tx, Cascade::FOO, "Ho").await.unwrap();
    assert_eq!(
        s(rec.get(&mut tx, Cascade::BAZ).await.unwrap()).as_deref(),
        Some("<[Ho]>")
    );
    tx.invalidate_all().await.unwrap();
    assert_eq!(
        s(rec.get(&mut tx, Cascade::BAZ).await.unwrap()).as_deref(),
        Some("<[Ho]>"),
        "stored in the database"
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn comp_014_stored_dependents_are_marked_non_stored_invalidated() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let model = tx.env().model::<Cascade>().unwrap();
    let rec = model
        .create(&mut tx, vec![model.vals().set(Cascade::FOO, "a")])
        .await
        .unwrap();
    assert_eq!(
        s(rec.get(&mut tx, Cascade::BAR).await.unwrap()).as_deref(),
        Some("[a]")
    );
    assert!(tx.is_cached(&rec, "bar").unwrap());
    rec.set(&mut tx, Cascade::FOO, "b").await.unwrap();
    assert!(!tx.is_cached(&rec, "bar").unwrap());
    assert_eq!(
        tx.records_to_compute(&rec, "baz").unwrap(),
        *rec.as_records()
    );
    assert_eq!(
        s(rec.get(&mut tx, Cascade::BAR).await.unwrap()).as_deref(),
        Some("[b]")
    );
    assert_eq!(
        s(rec.get(&mut tx, Cascade::BAZ).await.unwrap()).as_deref(),
        Some("<[b]>")
    );
    assert!(tx.records_to_compute(&rec, "baz").unwrap().is_empty());
    drop(tx);
    h.cleanup().await.unwrap();
}

/// budget for `modified()` on an empty cache: 0.
#[tokio::test]
async fn comp_015_modified_costs_no_query() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let model = tx.env().model::<Cascade>().unwrap();
    let rec = model
        .create(&mut tx, vec![model.vals().set(Cascade::FOO, "a")])
        .await
        .unwrap();
    tx.invalidate_all().await.unwrap();
    assert_query_count!(tx, <= 0, {
        rec.set(&mut tx, Cascade::FOO, "Other").await.unwrap();
    });
    assert_eq!(
        s(rec.get(&mut tx, Cascade::BAZ).await.unwrap()).as_deref(),
        Some("<[Other]>")
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn comp_021_protection_prevents_recomputation() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let model = tx.env().model::<ComputeReadonly>().unwrap();
    let rec = model
        .create(
            &mut tx,
            vec![model.vals().set(ComputeReadonly::FOO, "unprotected #1")],
        )
        .await
        .unwrap();
    assert_eq!(
        s(rec.get(&mut tx, ComputeReadonly::BAR).await.unwrap()).as_deref(),
        Some("unprotected #1")
    );
    let other = rec.clone();
    let records = rec.as_records().clone();
    tx.protecting(&["bar"], &records, async |tx: &mut Transaction| {
        other.set(tx, ComputeReadonly::FOO, "protected").await?;
        assert_eq!(
            s(other.get(tx, ComputeReadonly::BAR).await?).as_deref(),
            Some("unprotected #1")
        );
        // nested, with duplicated fields
        let inner = other.clone();
        let records = other.as_records().clone();
        tx.protecting(&["bar", "bar"], &records, async |tx: &mut Transaction| {
            inner
                .set(tx, ComputeReadonly::FOO, "protected again")
                .await?;
            assert_eq!(
                s(inner.get(tx, ComputeReadonly::BAR).await?).as_deref(),
                Some("unprotected #1")
            );
            Ok(())
        })
        .await?;
        // a record created inside the protection of another one is computed normally
        let model = other.env().model::<ComputeReadonly>()?;
        let created = model
            .create(tx, vec![model.vals().set(ComputeReadonly::FOO, "free")])
            .await?;
        assert_eq!(
            s(created.get(tx, ComputeReadonly::BAR).await?).as_deref(),
            Some("free")
        );
        Ok(())
    })
    .await
    .unwrap();
    rec.set(&mut tx, ComputeReadonly::FOO, "unprotected #2")
        .await
        .unwrap();
    assert_eq!(
        s(rec.get(&mut tx, ComputeReadonly::BAR).await.unwrap()).as_deref(),
        Some("unprotected #2")
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn comp_024_env_017_pull_recompute_and_fetch_keeps_pending_values() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let model = tx.env().model::<PerfBase>().unwrap();
    let vals = (1..=3)
        .map(|i| {
            model
                .vals()
                .set(PerfBase::NAME, format!("r{i}").as_str())
                .set(PerfBase::VALUE, i)
        })
        .collect();
    let recs = model.create(&mut tx, vals).await.unwrap();
    tx.invalidate_all().await.unwrap();
    let second = recs.at(1).unwrap();
    // the write itself needs no query
    assert_query_count!(tx, <= 0, {
        second.set(&mut tx, PerfBase::VALUE, 42).await.unwrap();
    });
    tx.capture_statements(true);
    // one SELECT of the prefetchable columns, no UPDATE before it
    assert_query_count!(tx, >= 1, <= 1, {
        for r in recs.iter() {
            r.get(&mut tx, PerfBase::NAME).await.unwrap();
        }
    });
    assert!(
        tx.captured()[0].starts_with("SELECT"),
        "{:?}",
        tx.captured()
    );
    let values = assert_query_count!(tx, <= 0, {
        let mut v = Vec::new();
        for r in recs.iter() {
            v.push(r.get(&mut tx, PerfBase::VALUE_PC).await.unwrap());
        }
        v
    });
    assert_eq!(values, [0.01, 0.42, 0.03]);
    assert_eq!(second.get(&mut tx, PerfBase::VALUE).await.unwrap(), 42);
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn comp_026_non_stored_compute_runs_once_per_prefetch_set() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let model = tx.env().model::<Counted>().unwrap();
    let vals = (0..100).map(|i| model.vals().set(Counted::N, i)).collect();
    let recs = tagged(&model.create(&mut tx, vals).await.unwrap(), "c026");
    let mut doubles = Vec::new();
    for r in recs.iter() {
        doubles.push(r.get(&mut tx, Counted::DOUBLE).await.unwrap());
    }
    assert_eq!(doubles, (0..100).map(|i| i * 2).collect::<Vec<i32>>());
    assert_eq!(tc::take_log("c026"), ["double x100"]);
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn comp_027_compute_marks_done_first_and_restores_on_error() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let model = tx.env().model::<ComputeRaises>().unwrap();
    let rec = model
        .create(&mut tx, vec![model.vals().set(ComputeRaises::FOO, "a")])
        .await
        .unwrap();
    assert_eq!(
        s(rec.get(&mut tx, ComputeRaises::BAR).await.unwrap()).as_deref(),
        Some("a")
    );
    rec.set(&mut tx, ComputeRaises::FOO, "boom").await.unwrap();
    let err = rec.get(&mut tx, ComputeRaises::BAR).await.unwrap_err();
    assert_eq!(err.message(), "boom");
    assert_eq!(
        tx.records_to_compute(&rec, "bar").unwrap(),
        *rec.as_records(),
        "pending again"
    );
    assert_eq!(tx.flush().await.unwrap_err().message(), "boom");
    // the compute reads the field's old value without recursing
    rec.set(&mut tx, ComputeRaises::FOO, "b").await.unwrap();
    assert_eq!(
        s(rec.get(&mut tx, ComputeRaises::BAR).await.unwrap()).as_deref(),
        Some("a|b")
    );
    tx.flush().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn comp_028_compute_sudo_keeps_the_uid() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let users = tx.env().model::<erp_sdk::ResUsers>().unwrap();
    let demo = users
        .create(
            &mut tx,
            vec![users.vals().set(erp_sdk::ResUsers::NAME, "Demo")],
        )
        .await
        .unwrap();
    let demo_uid = demo.ids()[0];
    let model = tx.env().model::<ComputeSudo>().unwrap();
    let rec = model.create(&mut tx, vec![model.vals()]).await.unwrap();
    let as_demo = tagged(&rec.with_user(demo_uid), "c028");
    assert!(!as_demo.env().su());
    assert_eq!(
        s(as_demo
            .get(&mut tx, ComputeSudo::NAME_FOR_UID)
            .await
            .unwrap())
        .as_deref(),
        Some("Demo")
    );
    assert_eq!(tc::take_log("c028"), ["sudo"]);
    // the cache key is the uid alone for a compute_sudo field
    let key = tx
        .cache_key(as_demo.env(), "test_orm.compute.sudo", "name_for_uid")
        .unwrap();
    assert_eq!(key, [Value::Int(demo_uid)]);
    assert_eq!(
        s(rec.get(&mut tx, ComputeSudo::NAME_FOR_UID).await.unwrap()).as_deref(),
        Some("__system__"),
        "another uid, another cached value"
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn comp_029_unassigned_computed_values() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let model = tx.env().model::<ComputeUnassigned>().unwrap();
    let rec = model.create(&mut tx, vec![model.vals()]).await.unwrap();
    let err = rec.get(&mut tx, ComputeUnassigned::BAR).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        format!(
            "Compute method failed to assign test_orm.compute.unassigned({},).bar",
            rec.ids()[0]
        )
    );
    assert_eq!(
        rec.get(&mut tx, ComputeUnassigned::BARE).await.unwrap(),
        None
    );
    assert_eq!(
        rec.get(&mut tx, ComputeUnassigned::BARS).await.unwrap(),
        None
    );
    assert_eq!(
        rec.get(&mut tx, ComputeUnassigned::BARES).await.unwrap(),
        None
    );
    // new record
    let new = model.new_record(&mut tx, model.vals()).await.unwrap();
    let err = new.get(&mut tx, ComputeUnassigned::BAR).await.unwrap_err();
    assert!(
        err.message().starts_with("Compute method failed to assign"),
        "{err}"
    );
    assert_eq!(
        new.get(&mut tx, ComputeUnassigned::BARE).await.unwrap(),
        None
    );
    assert_eq!(
        new.get(&mut tx, ComputeUnassigned::BARS).await.unwrap(),
        None
    );
    assert_eq!(
        new.get(&mut tx, ComputeUnassigned::BARES).await.unwrap(),
        None
    );
    // assigned
    let rec = model
        .create(
            &mut tx,
            vec![model.vals().set(ComputeUnassigned::FOO, "assign")],
        )
        .await
        .unwrap();
    assert_eq!(
        s(rec.get(&mut tx, ComputeUnassigned::BAR).await.unwrap()).as_deref(),
        Some("assign")
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn comp_031_editable_computed_field_keeps_the_written_value() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let model = tx.env().model::<ComputeReadwrite>().unwrap();
    model.create(&mut tx, vec![model.vals()]).await.unwrap(); // warmup
    let r1 = assert_query_count!(tx, >= 1, <= 1, {
        model
            .create(&mut tx, vec![model.vals().set(ComputeReadwrite::FOO, "Foo")])
            .await
            .unwrap()
    });
    let r2 = assert_query_count!(tx, >= 1, <= 1, {
        model
            .create(
                &mut tx,
                vec![
                    model
                        .vals()
                        .set(ComputeReadwrite::FOO, "Foo")
                        .set(ComputeReadwrite::BAR, "Bar"),
                ],
            )
            .await
            .unwrap()
    });
    let with_default = model.with_context([("default_bar", Value::str("Def"))]);
    let r3 = assert_query_count!(tx, >= 1, <= 1, {
        with_default
            .create(&mut tx, vec![model.vals().set(ComputeReadwrite::FOO, "Foo")])
            .await
            .unwrap()
    });
    assert_query_count!(tx, <= 0, { tx.flush().await.unwrap() });
    tx.invalidate_all().await.unwrap();
    for (rec, bar) in [(&r1, "Foo"), (&r2, "Bar"), (&r3, "Def")] {
        assert_eq!(
            s(rec.get(&mut tx, ComputeReadwrite::BAR).await.unwrap()).as_deref(),
            Some(bar)
        );
    }
    // the written value is kept until a dependency changes
    r2.set(&mut tx, ComputeReadwrite::FOO, "Changed")
        .await
        .unwrap();
    assert_eq!(
        s(r2.get(&mut tx, ComputeReadwrite::BAR).await.unwrap()).as_deref(),
        Some("Changed")
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn comp_032_readonly_computed_field_overrides_the_given_value() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let model = tx.env().model::<ComputeReadonly>().unwrap();
    model.create(&mut tx, vec![model.vals()]).await.unwrap(); // warmup
    let with_default = model.with_context([("default_bar", Value::str("Def"))]);
    let mut recs = Vec::new();
    for m in [&model, &with_default] {
        for given in [false, true] {
            let mut vals = model.vals().set(ComputeReadonly::FOO, "Foo");
            if given {
                vals = vals.set(ComputeReadonly::BAR, "Bar");
            }
            recs.push(assert_query_count!(tx, >= 1, <= 1, {
                m.create(&mut tx, vec![vals]).await.unwrap()
            }));
        }
    }
    assert_query_count!(tx, <= 0, { tx.flush().await.unwrap() });
    tx.invalidate_all().await.unwrap();
    for rec in &recs {
        assert_eq!(
            s(rec.get(&mut tx, ComputeReadonly::BAR).await.unwrap()).as_deref(),
            Some("Foo")
        );
    }
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn comp_033_pending_sibling_recompute_is_forced_before_an_inverse_write() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let model = tagged(&tx.env().model::<MultiComputeInverse>().unwrap(), "c033");
    let rec = model.create(&mut tx, vec![model.vals()]).await.unwrap();
    tc::take_log("c033");
    rec.set(&mut tx, MultiComputeInverse::FOO, "1/2/3")
        .await
        .unwrap();
    assert!(tc::take_log("c033").is_empty());
    rec.set(&mut tx, MultiComputeInverse::BAR3, "X")
        .await
        .unwrap();
    assert_eq!(tc::take_log("c033"), ["compute", "inverse23"]);
    assert_eq!(
        s(rec.get(&mut tx, MultiComputeInverse::FOO).await.unwrap()).as_deref(),
        Some("1/2/X")
    );
    rec.set(&mut tx, MultiComputeInverse::FOO, "A/B/C")
        .await
        .unwrap();
    rec.write(
        &mut tx,
        rec.vals()
            .set(MultiComputeInverse::BAR1, "X")
            .set(MultiComputeInverse::BAR2, "Y")
            .set(MultiComputeInverse::BAR3, "Z"),
    )
    .await
    .unwrap();
    assert_eq!(tc::take_log("c033"), ["inverse1", "inverse23"]);
    assert_eq!(
        s(rec.get(&mut tx, MultiComputeInverse::FOO).await.unwrap()).as_deref(),
        Some("X/Y/Z")
    );
    tx.flush().await.unwrap();
    assert!(
        tc::take_log("c033").is_empty(),
        "nothing recomputed at flush"
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn comp_034_inverse_is_called_instead_of_compute() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let model = tagged(&tx.env().model::<ComputeInverse>().unwrap(), "c034");
    let rec = model
        .create(&mut tx, vec![model.vals().set(ComputeInverse::FOO, "Hi")])
        .await
        .unwrap();
    assert_eq!(tc::take_log("c034"), ["compute"]);
    let rec2 = model
        .create(&mut tx, vec![model.vals().set(ComputeInverse::BAR, "Hi")])
        .await
        .unwrap();
    assert_eq!(tc::take_log("c034"), ["inverse"]);
    assert_eq!(
        s(rec2.get(&mut tx, ComputeInverse::FOO).await.unwrap()).as_deref(),
        Some("Hi")
    );
    rec.set(&mut tx, ComputeInverse::FOO, "Ho").await.unwrap();
    assert_eq!(
        s(rec.get(&mut tx, ComputeInverse::BAR).await.unwrap()).as_deref(),
        Some("Ho")
    );
    assert_eq!(tc::take_log("c034"), ["compute"]);
    rec.set(&mut tx, ComputeInverse::BAR, "Ha").await.unwrap();
    assert_eq!(tc::take_log("c034"), ["inverse"]);
    assert_eq!(
        s(rec.get(&mut tx, ComputeInverse::FOO).await.unwrap()).as_deref(),
        Some("Ha")
    );
    // one inverse call for several records
    let both = rec.concat(&rec2).unwrap();
    both.set(&mut tx, ComputeInverse::BAR, "Both")
        .await
        .unwrap();
    assert_eq!(tc::take_log("c034"), ["inverse"]);
    assert_eq!(
        both.mapped(&mut tx, ComputeInverse::FOO).await.unwrap(),
        [Some(Arc::from("Both")), Some(Arc::from("Both"))]
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn comp_035_inverse_create_and_write_queries() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let model = tx.env().model::<ComputeInverse>().unwrap();
    model.create(&mut tx, vec![model.vals()]).await.unwrap(); // warmup
    tx.flush().await.unwrap();
    let r1 = assert_query_count!(tx, >= 1, <= 1, {
        let r = model
            .create(&mut tx, vec![model.vals().set(ComputeInverse::FOO, "Foo")])
            .await
            .unwrap();
        tx.flush().await.unwrap();
        r
    });
    let r2 = assert_query_count!(tx, >= 2, <= 2, {
        let r = model
            .create(
                &mut tx,
                vec![
                    model
                        .vals()
                        .set(ComputeInverse::FOO, "Foo")
                        .set(ComputeInverse::BAR, "Bar"),
                ],
            )
            .await
            .unwrap();
        tx.flush().await.unwrap();
        r
    });
    let with_default = model.with_context([("default_bar", Value::str("Def"))]);
    let r3 = assert_query_count!(tx, >= 2, <= 2, {
        let r = with_default
            .create(&mut tx, vec![model.vals().set(ComputeInverse::FOO, "Foo")])
            .await
            .unwrap();
        tx.flush().await.unwrap();
        r
    });
    tx.invalidate_all().await.unwrap();
    for (rec, value) in [(&r1, "Foo"), (&r2, "Bar"), (&r3, "Def")] {
        assert_eq!(
            s(rec.get(&mut tx, ComputeInverse::FOO).await.unwrap()).as_deref(),
            Some(value)
        );
        assert_eq!(
            s(rec.get(&mut tx, ComputeInverse::BAR).await.unwrap()).as_deref(),
            Some(value)
        );
    }
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn crud_018_constraint_and_inverse_ordering() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let model = tx.env().model::<ComputeInverse>().unwrap().with_context([
        ("log", Value::str("c018")),
        ("log_constraint", Value::Bool(true)),
    ]);
    let sorted = |mut v: Vec<String>| {
        v.sort();
        v
    };
    let rec = model
        .create(&mut tx, vec![model.vals().set(ComputeInverse::BAZ, "Hi")])
        .await
        .unwrap();
    assert_eq!(tc::take_log("c018"), ["compute", "constraint"]);
    rec.set(&mut tx, ComputeInverse::BAZ, "Ho").await.unwrap();
    assert_eq!(tc::take_log("c018"), ["constraint"]);
    let rec = model
        .create(&mut tx, vec![model.vals().set(ComputeInverse::BAR, "Hi")])
        .await
        .unwrap();
    assert_eq!(sorted(tc::take_log("c018")), ["constraint", "inverse"]);
    rec.set(&mut tx, ComputeInverse::BAR, "Ho").await.unwrap();
    assert_eq!(tc::take_log("c018"), ["inverse", "constraint"]);
    let rec = model
        .create(
            &mut tx,
            vec![
                model
                    .vals()
                    .set(ComputeInverse::BAR, "Hi")
                    .set(ComputeInverse::BAZ, "Hi"),
            ],
        )
        .await
        .unwrap();
    assert_eq!(sorted(tc::take_log("c018")), ["constraint", "inverse"]);
    rec.write(
        &mut tx,
        rec.vals()
            .set(ComputeInverse::BAR, "Ho")
            .set(ComputeInverse::BAZ, "Ho"),
    )
    .await
    .unwrap();
    assert_eq!(tc::take_log("c018"), ["inverse", "constraint"]);
    drop(tx);
    h.cleanup().await.unwrap();
}

/// columns, no UPDATE.
#[tokio::test]
async fn crud_011_precomputed_fields_go_into_the_insert() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let model = tx.env().model::<Precompute>().unwrap();
    model
        .create(&mut tx, vec![model.vals().set(Precompute::NAME, "warm")])
        .await
        .unwrap();
    tx.capture_statements(true);
    let rec = assert_query_count!(tx, >= 1, <= 1, {
        let r = model
            .create(&mut tx, vec![model.vals().set(Precompute::NAME, "Foo")])
            .await
            .unwrap();
        tx.flush().await.unwrap();
        r
    });
    let insert = &tx.captured()[0];
    for col in ["\"name\"", "\"lower\"", "\"upper\"", "\"lowup\""] {
        assert!(insert.contains(col), "{insert}");
    }
    tx.invalidate_all().await.unwrap();
    assert_eq!(
        (
            s(rec.get(&mut tx, Precompute::LOWER).await.unwrap()),
            s(rec.get(&mut tx, Precompute::UPPER).await.unwrap()),
            s(rec.get(&mut tx, Precompute::LOWUP).await.unwrap()),
        ),
        (
            Some("foo".into()),
            Some("FOO".into()),
            Some("fooFOO".into())
        )
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn crud_012_readonly_versus_editable_precomputed_fields() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();

    // editable chain
    let m = tx.env().model::<PrecomputeEditable>().unwrap();
    type E = PrecomputeEditable;
    let cases: [(Given, [&str; 3]); 4] = [
        (&[], ["COMPUTED", "COMPUTED", "COMPUTED"]),
        (&[("bar", "bar")], ["bar", "COMPUTED", "COMPUTED"]),
        (&[("baz", "baz")], ["COMPUTED", "baz", "baz"]),
        (&[("bar", "bar"), ("baz", "baz")], ["bar", "baz", "baz"]),
    ];
    for (given, expected) in cases {
        let mut vals = m.vals().set(E::FOO, "foo");
        for (f, v) in given {
            vals = match *f {
                "bar" => vals.set(E::BAR, *v),
                _ => vals.set(E::BAZ, *v),
            };
        }
        let r = m.create(&mut tx, vec![vals]).await.unwrap();
        let got = [
            s(r.get(&mut tx, E::BAR).await.unwrap()),
            s(r.get(&mut tx, E::BAZ).await.unwrap()),
            s(r.get(&mut tx, E::BAZ2).await.unwrap()),
        ];
        assert_eq!(got, expected.map(|e| Some(e.to_owned())), "{given:?}");
    }
    // editable multi: one field of a group prevents the others
    let cases: [(Given, [Option<&str>; 3]); 4] = [
        (&[], [Some("COMPUTED"), Some("COMPUTED"), Some("COMPUTED")]),
        (&[("boo1", "boo1")], [Some("boo1"), None, None]),
        (&[("boo2", "boo2")], [None, Some("boo2"), Some("boo2")]),
        (
            &[("boo1", "boo1"), ("boo2", "boo2")],
            [Some("boo1"), Some("boo2"), Some("boo2")],
        ),
    ];
    for (given, expected) in cases {
        let mut vals = m.vals().set(E::FOO, "foo");
        for (f, v) in given {
            vals = match *f {
                "boo1" => vals.set(E::BOO1, *v),
                _ => vals.set(E::BOO2, *v),
            };
        }
        let r = m.create(&mut tx, vec![vals]).await.unwrap();
        let got = [
            s(r.get(&mut tx, E::BOO1).await.unwrap()),
            s(r.get(&mut tx, E::BOO2).await.unwrap()),
            s(r.get(&mut tx, E::CHOO).await.unwrap()),
        ];
        assert_eq!(got, expected.map(|e| e.map(str::to_owned)), "{given:?}");
    }
    // readonly: a given value is ignored
    let m = tx.env().model::<PrecomputeReadonly>().unwrap();
    type R = PrecomputeReadonly;
    let r = m
        .create(
            &mut tx,
            vec![m.vals().set(R::FOO, "foo").set(R::BAR, "bar")],
        )
        .await
        .unwrap();
    assert_eq!(
        s(r.get(&mut tx, R::BAR).await.unwrap()).as_deref(),
        Some("COMPUTED")
    );
    assert_eq!(
        s(r.get(&mut tx, R::BAZ).await.unwrap()).as_deref(),
        Some("COMPUTED")
    );
    let r = m
        .create(
            &mut tx,
            vec![m.vals().set(R::FOO, "foo").set(R::BAZ, "baz")],
        )
        .await
        .unwrap();
    assert_eq!(
        s(r.get(&mut tx, R::BAR).await.unwrap()).as_deref(),
        Some("COMPUTED")
    );
    assert_eq!(
        s(r.get(&mut tx, R::BAZ).await.unwrap()).as_deref(),
        Some("baz")
    );

    // combo: always one INSERT; the inverse runs when `setter` is given
    let m = tx.env().model::<PrecomputeCombo>().unwrap();
    type C = PrecomputeCombo;
    m.create(&mut tx, vec![m.vals()]).await.unwrap(); // warmup
    let tagged_m = tagged(&m, "c012");
    let r = assert_query_count!(tx, >= 1, <= 1, {
        tagged_m.create(&mut tx, vec![m.vals().set(C::NAME, "A")]).await.unwrap()
    });
    assert!(tc::take_log("c012").is_empty());
    let a = Some("A".to_owned());
    assert_eq!(
        [
            s(r.get(&mut tx, C::READER).await.unwrap()),
            s(r.get(&mut tx, C::EDITER).await.unwrap()),
            s(r.get(&mut tx, C::SETTER).await.unwrap())
        ],
        [a.clone(), a.clone(), a.clone()]
    );
    let defaults = tagged_m.with_context([
        ("default_reader", Value::str("X")),
        ("default_editer", Value::str("Y")),
        ("default_setter", Value::str("Z")),
    ]);
    let r1 = assert_query_count!(tx, >= 1, <= 1, {
        defaults.create(&mut tx, vec![m.vals().set(C::NAME, "A")]).await.unwrap()
    });
    assert_eq!(tc::take_log("c012"), ["inverse_setter"]);
    let r2 = assert_query_count!(tx, >= 1, <= 1, {
        tagged_m
            .create(
                &mut tx,
                vec![
                    m.vals()
                        .set(C::NAME, "A")
                        .set(C::READER, "X")
                        .set(C::EDITER, "Y")
                        .set(C::SETTER, "Z"),
                ],
            )
            .await
            .unwrap()
    });
    assert_eq!(tc::take_log("c012"), ["inverse_setter"]);
    for r in [&r1, &r2] {
        assert_eq!(
            [
                s(r.get(&mut tx, C::READER).await.unwrap()),
                s(r.get(&mut tx, C::EDITER).await.unwrap()),
                s(r.get(&mut tx, C::SETTER).await.unwrap())
            ],
            [a.clone(), Some("Y".into()), Some("Z".into())]
        );
    }
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn comp_006_crud_017_019_constraints_on_create_and_after_recompute() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let model = tx.env().model::<Constrained>().unwrap();
    // create: the precomputed value is checked
    let err = model
        .create(&mut tx, vec![model.vals().set(Constrained::NAME, "bad")])
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert!(
        tx.fields_to_compute().is_empty(),
        "{:?}",
        tx.fields_to_compute()
    );
    tx.rollback().await.unwrap();

    let users = tx.env().model::<erp_sdk::ResUsers>().unwrap();
    let demo = users
        .create(
            &mut tx,
            vec![users.vals().set(erp_sdk::ResUsers::NAME, "Demo")],
        )
        .await
        .unwrap();
    let as_demo = tagged(&model.with_user(demo.ids()[0]), "c019");
    let rec = as_demo
        .create(&mut tx, vec![model.vals().set(Constrained::NAME, "ok")])
        .await
        .unwrap();
    // constraints run as superuser
    assert_eq!(tc::take_log("c019"), ["check su=true"]);
    // after a recompute of the stored field
    rec.set(&mut tx, Constrained::NAME, "bad").await.unwrap();
    assert!(tc::take_log("c019").is_empty(), "`name` is not constrained");
    let err = tx.flush().await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(err.message(), "upper may not be BAD");
    tx.rollback().await.unwrap();
    // new records never run constraints
    let model = tx.env().model::<Constrained>().unwrap();
    let new = model
        .new_record(&mut tx, model.vals().set(Constrained::NAME, "bad"))
        .await
        .unwrap();
    assert_eq!(
        s(new.get(&mut tx, Constrained::UPPER).await.unwrap()).as_deref(),
        Some("BAD")
    );
    new.set(&mut tx, Constrained::NOTE, "bad").await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn comp_055_flush_fields_and_flush_model_scope() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let partners = tx.env().model::<Partner>().unwrap();
    let countries = tx.env().model::<Country>().unwrap();
    let r = partners
        .create(&mut tx, vec![partners.vals()])
        .await
        .unwrap();
    let q = countries
        .create(&mut tx, vec![countries.vals()])
        .await
        .unwrap();
    tx.flush().await.unwrap();
    r.write(
        &mut tx,
        r.vals().set(Partner::NAME, "n").set(Partner::EMAIL, "e"),
    )
    .await
    .unwrap();
    q.set(&mut tx, Country::NAME, "c").await.unwrap();
    let spec = tx.registry().model("test_orm.partner").unwrap();
    let (pid, email, website) = (
        spec.id,
        spec.field_id("email").unwrap(),
        spec.field_id("website").unwrap(),
    );
    tx.capture_statements(true);
    assert_query_count!(tx, >= 1, <= 1, {
        tx.flush_fields(pid, &[email]).await.unwrap();
    });
    let update = tx.captured()[0].clone();
    assert!(
        update.contains("\"test_orm_partner\"") && update.contains("\"email\""),
        "{update}"
    );
    assert!(!update.contains("\"name\" ="), "per-field flush: {update}");
    assert!(tx.is_dirty(&q, "name").unwrap(), "other models untouched");
    assert!(tx.is_dirty(&r, "name").unwrap());
    assert_query_count!(tx, <= 0, {
        tx.flush_fields(pid, &[website]).await.unwrap();
    });
    assert_query_count!(tx, >= 1, <= 1, {
        tx.flush_model(pid).await.unwrap();
    });
    assert!(!tx.is_dirty(&r, "name").unwrap());
    assert!(tx.is_dirty(&q, "name").unwrap());
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn comp_056_flush_reaches_a_fixed_point() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let model = tx.env().model::<Creator>().unwrap();
    let rec = model
        .create(&mut tx, vec![model.vals().set(Creator::NAME, "foo")])
        .await
        .unwrap();
    assert_eq!(
        tx.fields_to_compute(),
        ["test_orm.compute.creator.created_id"]
    );
    tx.flush().await.unwrap();
    assert!(tx.fields_to_compute().is_empty());
    tx.invalidate_all().await.unwrap();
    let created_id = rec
        .get(&mut tx, Creator::CREATED_ID)
        .await
        .unwrap()
        .unwrap();
    let created = tx
        .env()
        .model::<Created>()
        .unwrap()
        .browse([created_id.get()])
        .unwrap();
    assert_eq!(created.get(&mut tx, Created::VALUE).await.unwrap(), 3);
    drop(tx);
    h.cleanup().await.unwrap();
}

/// COMP §7 D5: a flush that keeps creating work fails instead of committing half-computed data
/// (is n/a-by-construction for its log lines).
#[tokio::test]
async fn flush_that_does_not_converge_is_an_error() {
    let h = Harness::builder([&tc::ADDON])
        .config(OrmConfig {
            max_flush_rounds: 3,
            ..OrmConfig::default()
        })
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    let model = tx.env().model::<PingPong>().unwrap();
    model.create(&mut tx, vec![model.vals()]).await.unwrap();
    let err = tx.flush().await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert!(
        err.message().contains("did not converge after 3 rounds"),
        "{err}"
    );
    assert!(err.message().contains("test_orm.pingpong.m"), "{err}");
    assert!(tx.commit().await.is_err(), "commit flushes and fails too");
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn comp_061_raw_sql_does_not_flush() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let partners = tx.env().model::<Partner>().unwrap();
    let r = partners
        .create(&mut tx, vec![partners.vals().set(Partner::NAME, "old")])
        .await
        .unwrap();
    r.set(&mut tx, Partner::NAME, "new").await.unwrap();
    let sql = format!(
        "SELECT name FROM test_orm_partner WHERE id = {}",
        r.ids()[0]
    );
    assert_eq!(support::one(&mut tx, &sql).await.as_deref(), Some("old"));
    let spec = tx.registry().model("test_orm.partner").unwrap();
    let (pid, name) = (spec.id, spec.field_id("name").unwrap());
    tx.flush_fields(pid, &[name]).await.unwrap();
    assert_eq!(support::one(&mut tx, &sql).await.as_deref(), Some("new"));
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn comp_063_equal_writes_do_not_notify_dependents() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let model = tagged(&tx.env().model::<PerfBase>().unwrap(), "c063");
    let rec = model
        .create(&mut tx, vec![model.vals().set(PerfBase::VALUE, 5)])
        .await
        .unwrap();
    tx.commit().await.unwrap();
    rec.get(&mut tx, PerfBase::VALUE_PC).await.unwrap();
    tc::take_log("c063");
    rec.set(&mut tx, PerfBase::VALUE, 5).await.unwrap();
    assert!(!tx.is_dirty(&rec, "value").unwrap());
    assert!(tx.records_to_compute(&rec, "value_pc").unwrap().is_empty());
    tx.capture_statements(true);
    tx.flush().await.unwrap();
    let update = tx.captured().join("\n");
    assert!(!update.contains("\"value\" ="), "{update}");
    assert!(update.contains("\"write_uid\""), "{update}");
    assert!(tc::take_log("c063").is_empty(), "nothing recomputed");
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn comp_066_new_records_are_not_recomputed_by_flush() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let model = tx.env().model::<PerfBase>().unwrap();
    let real = model
        .create(&mut tx, vec![model.vals().set(PerfBase::VALUE, 1)])
        .await
        .unwrap();
    let new = model
        .new_record(&mut tx, model.vals().set(PerfBase::VALUE, 7))
        .await
        .unwrap();
    real.set(&mut tx, PerfBase::VALUE, 2).await.unwrap();
    new.set(&mut tx, PerfBase::VALUE, 8).await.unwrap();
    tx.capture_statements(true);
    tx.flush().await.unwrap();
    assert!(tx.captured().iter().any(|q| q.contains("\"value_pc\"")));
    // only the new record is still to compute
    assert_eq!(
        tx.records_to_compute(&real, "value_pc").unwrap(),
        *new.as_records()
    );
    assert_eq!(new.get(&mut tx, PerfBase::VALUE_PC).await.unwrap(), 0.08);
    assert!(!tx.is_dirty(&new, "value_pc").unwrap());
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn crud_022_write_with_recomputation_is_one_update() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let model = tx.env().model::<PerfBase>().unwrap();
    let vals = (0..5)
        .map(|i| model.vals().set(PerfBase::VALUE, i))
        .collect();
    let recs = model.create(&mut tx, vals).await.unwrap();
    tx.flush().await.unwrap();
    tx.capture_statements(true);
    assert_query_count!(tx, >= 1, <= 1, {
        recs.set(&mut tx, PerfBase::VALUE, 42).await.unwrap();
        tx.flush().await.unwrap();
    });
    let update = &tx.captured()[0];
    assert!(
        update.contains("\"value\"") && update.contains("\"value_pc\""),
        "{update}"
    );
    tx.invalidate_all().await.unwrap();
    assert_eq!(
        recs.mapped(&mut tx, PerfBase::VALUE_PC).await.unwrap(),
        [0.42; 5]
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn budget_create_with_stored_computes_is_one_statement() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let model = tx.env().model::<PerfBase>().unwrap();
    model.create(&mut tx, vec![model.vals()]).await.unwrap(); // warmup
    let rec = assert_query_count!(tx, >= 1, <= 1, {
        let r = model
            .create(
                &mut tx,
                vec![model.vals().set(PerfBase::NAME, "X").set(PerfBase::VALUE, 7)],
            )
            .await
            .unwrap();
        tx.flush().await.unwrap();
        r
    });
    tx.invalidate_all().await.unwrap();
    assert_eq!(rec.get(&mut tx, PerfBase::VALUE_PC).await.unwrap(), 0.07);
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn env_015_context_dependent_fields_are_cached_per_key() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let model = tx.env().model::<PerfBase>().unwrap();
    let vals = (0..5).map(|_| model.vals()).collect();
    let recs = model.create(&mut tx, vals).await.unwrap();
    for key in [1, 2] {
        let in_ctx = recs.with_context([("key", Value::Int(key))]);
        let values = assert_query_count!(tx, >= 1, <= 1, {
            in_ctx.mapped(&mut tx, PerfBase::VALUE_CTX).await.unwrap()
        });
        assert_eq!(values, [i32::try_from(key).unwrap(); 5]);
    }
    let in_ctx = recs.with_context([("key", Value::Int(3))]);
    assert_query_count!(tx, >= 1, <= 1, {
        for r in in_ctx.iter() {
            assert_eq!(r.get(&mut tx, PerfBase::VALUE_CTX).await.unwrap(), 3);
        }
    });
    let again = recs.with_context([("key", Value::Int(1))]);
    assert_query_count!(tx, <= 0, {
        assert_eq!(again.mapped(&mut tx, PerfBase::VALUE_CTX).await.unwrap(), [1; 5]);
    });
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn env_016_cache_key_rules() {
    let h = harness().await;
    let tx = h.begin().await.unwrap();
    let env = tx.env().with_user(7).with_context([
        ("allowed_company_ids", Value::list([Value::Int(3)])),
        ("lang", Value::str("fr_FR")),
        ("k", Value::list([Value::Int(1), Value::Int(2)])),
    ]);
    assert_eq!(
        tx.cache_key(&env, "test_orm.ctx_key", "all").unwrap(),
        [
            Value::Int(3),
            Value::list([Value::Int(7), Value::Bool(false)]),
            Value::str("fr_FR"),
            Value::Bool(true),
            Value::list([Value::Int(1), Value::Int(2)]),
        ]
    );
    assert_eq!(
        tx.cache_key(&env, "test_orm.compute.sudo", "name_for_uid")
            .unwrap(),
        [Value::Int(7)],
        "compute_sudo: the uid alone"
    );
    let _ = CtxKey::ALL;
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn env_018_dirty_marking_scope() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let partners = tx.env().model::<Partner>().unwrap();
    let real = partners
        .create(&mut tx, vec![partners.vals()])
        .await
        .unwrap();
    let new = partners.new_record(&mut tx, partners.vals()).await.unwrap();
    real.set(&mut tx, Partner::NAME, "x").await.unwrap();
    new.set(&mut tx, Partner::NAME, "y").await.unwrap();
    assert!(tx.is_dirty(&real, "name").unwrap());
    assert!(tx.is_cached(&new, "name").unwrap());
    assert!(!tx.is_dirty(&new, "name").unwrap());
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn env_022_023_invalidation_scopes() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let partners = tx.env().model::<Partner>().unwrap();
    let vals = (0..2)
        .map(|i| {
            partners
                .vals()
                .set(Partner::EMAIL, format!("e{i}").as_str())
        })
        .collect();
    let rs = partners.create(&mut tx, vals).await.unwrap();
    tx.flush().await.unwrap();
    let (r1, r2) = (rs.at(0).unwrap(), rs.at(1).unwrap());
    r1.set(&mut tx, Partner::NAME, "pending").await.unwrap();
    // (a)
    assert_query_count!(tx, <= 0, {
        tx.invalidate_records(&r1, Some(&["email"])).await.unwrap();
    });
    assert!(!tx.is_cached(&r1, "email").unwrap());
    assert!(tx.is_cached(&r2, "email").unwrap());
    assert!(tx.is_dirty(&r1, "name").unwrap());
    // (b)
    assert_query_count!(tx, <= 0, {
        tx.invalidate_model("test_orm.partner", Some(&["email"])).await.unwrap();
    });
    assert!(!tx.is_cached(&r2, "email").unwrap());
    assert_eq!(
        s(r1.get(&mut tx, Partner::NAME).await.unwrap()).as_deref(),
        Some("pending")
    );
    // edge: invalidating the dirty field writes it first
    assert_query_count!(tx, >= 1, <= 1, {
        tx.invalidate_records(&r1, Some(&["name"])).await.unwrap();
    });
    assert!(!tx.is_cached(&r1, "name").unwrap());
    // (c)
    r2.set(&mut tx, Partner::NAME, "other").await.unwrap();
    assert_query_count!(tx, >= 1, <= 1, {
        tx.invalidate_all().await.unwrap();
    });
    assert!(!tx.is_cached(&r2, "name").unwrap());
    assert_eq!(
        rs.mapped(&mut tx, Partner::NAME).await.unwrap(),
        [Some(Arc::from("pending")), Some(Arc::from("other"))]
    );
    let model = tx.env().model::<PerfBase>().unwrap();
    let rec = model.create(&mut tx, vec![model.vals()]).await.unwrap();
    for key in [1, 2] {
        rec.with_context([("key", Value::Int(key))])
            .get(&mut tx, PerfBase::VALUE_CTX)
            .await
            .unwrap();
    }
    tx.invalidate_records(&rec, Some(&["value_ctx"]))
        .await
        .unwrap();
    for key in [1, 2] {
        let r = rec.with_context([("key", Value::Int(key))]);
        assert_query_count!(tx, >= 1, <= 1, {
            r.get(&mut tx, PerfBase::VALUE_CTX).await.unwrap();
        });
    }
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn env_040_new_creates_in_memory_records() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let mixed = tx.env().model::<Mixed>().unwrap();
    let m = assert_query_count!(tx, <= 0, {
        let m = mixed
            .new_record(&mut tx, mixed.vals().set(Mixed::CHAR, "XXX")).await
            .unwrap();
        assert!(m.rec_ids()[0].is_new());
        assert_eq!(s(m.get(&mut tx, Mixed::CHAR).await.unwrap()).as_deref(), Some("XXX"));
        // defaults apply to fields not given
        assert_eq!(m.get(&mut tx, Mixed::FLOAT_DEFAULT).await.unwrap(), 1.25);
        m
    });
    assert!(m.ids().is_empty(), "no origin, no id");
    tx.flush().await.unwrap();
    assert_eq!(
        support::one(&mut tx, "SELECT count(*)::text FROM test_orm_mixed")
            .await
            .as_deref(),
        Some("0")
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn env_041_042_new_id_equality_origin_and_ids() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let model = tx.env().model::<RelatedSingle>().unwrap();
    let reals = model
        .create(
            &mut tx,
            vec![
                model.vals().set(RelatedSingle::NAME, "a"),
                model.vals().set(RelatedSingle::NAME, "b"),
            ],
        )
        .await
        .unwrap();
    let (a, b) = (reals.at(0).unwrap(), reals.at(1).unwrap());
    async fn n(
        tx: &mut Transaction,
        model: &RecordSet<RelatedSingle>,
        origin: Option<&RecordSet<RelatedSingle>>,
        r: Option<&str>,
    ) -> RecordSet<RelatedSingle> {
        model.new_with(tx, model.vals(), origin, r).await.unwrap()
    }
    let xs = [
        n(&mut tx, &model, None, None).await,
        n(&mut tx, &model, None, Some("a")).await,
        n(&mut tx, &model, None, Some("b")).await,
        n(&mut tx, &model, None, Some("b")).await,
    ];
    for (i, x) in xs.iter().enumerate() {
        for (j, y) in xs.iter().enumerate() {
            assert_eq!(x == y, i == j || (i >= 2 && j >= 2), "{i} {j}");
        }
    }
    let ys = [
        n(&mut tx, &model, Some(&a), None).await,
        n(&mut tx, &model, Some(&b), None).await,
        n(&mut tx, &model, Some(&b), None).await,
    ];
    assert_ne!(ys[0], ys[1]);
    assert_eq!(ys[1], ys[2]);
    assert_ne!(
        n(&mut tx, &model, None, Some("a")).await,
        n(&mut tx, &model, Some(&b), None).await
    );
    let all = ys[0].concat(&ys[1]).unwrap().concat(&ys[2]).unwrap();
    assert_eq!(all.origin().ids(), [a.ids()[0], b.ids()[0], b.ids()[0]]);
    assert_eq!(all.origin().origin(), all.origin());
    // a new record with an origin copies the origin's stored values
    assert_eq!(
        s(ys[0].get(&mut tx, RelatedSingle::NAME).await.unwrap()).as_deref(),
        Some("a")
    );
    let mixed = a.concat(&ys[0]).unwrap().concat(&xs[0]).unwrap();
    assert_eq!(mixed.ids(), [a.ids()[0], a.ids()[0]]);
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn env_043_reads_on_new_records_follow_origin_compute_default() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let model = tx.env().model::<RelatedSingle>().unwrap();
    let real = model
        .create(&mut tx, vec![model.vals().set(RelatedSingle::NAME, "Foo")])
        .await
        .unwrap();
    let in_ctx = model.with_context([
        ("default_dummy", Value::str("WTF")),
        ("default_color", Value::Int(42)),
    ]);
    let real = real.with_env(in_ctx.env().clone());
    let new_origin = in_ctx
        .new_with(
            &mut tx,
            in_ctx.vals().set(RelatedSingle::NAME, "Bar"),
            Some(&real),
            None,
        )
        .await
        .unwrap();
    let new_record = in_ctx
        .new_record(&mut tx, in_ctx.vals().set(RelatedSingle::NAME, "Baz"))
        .await
        .unwrap();
    let three = [&real, &new_origin, &new_record];
    let mut dummies = Vec::new();
    let mut colors = Vec::new();
    let mut names = Vec::new();
    for r in three {
        dummies.push(s(r.get(&mut tx, RelatedSingle::DUMMY).await.unwrap()));
        colors.push(r.get(&mut tx, RelatedSingle::COLOR).await.unwrap());
        names.push(s(r
            .get(&mut tx, erp_sdk::Base::DISPLAY_NAME)
            .await
            .unwrap()));
    }
    assert_eq!(dummies, vec![Some("WTF".to_owned()); 3]);
    assert_eq!(colors, [0, 0, 42]);
    assert_eq!(
        names,
        [
            Some("Foo".to_owned()),
            Some("Bar".to_owned()),
            Some("Baz".to_owned())
        ]
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn comp_038_one_segment_related_read_and_write() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let model = tx.env().model::<RelatedSingle>().unwrap();
    let rec = model
        .create(&mut tx, vec![model.vals().set(RelatedSingle::NAME, "Foo")])
        .await
        .unwrap();
    assert_eq!(
        s(rec.get(&mut tx, RelatedSingle::RELATED_NAME).await.unwrap()).as_deref(),
        Some("Foo")
    );
    rec.set(&mut tx, RelatedSingle::NAME, "Bar").await.unwrap();
    assert_eq!(
        s(rec.get(&mut tx, RelatedSingle::RELATED_NAME).await.unwrap()).as_deref(),
        Some("Bar")
    );
    rec.set(&mut tx, RelatedSingle::RELATED_NAME, "Baz")
        .await
        .unwrap();
    assert_eq!(
        s(rec.get(&mut tx, RelatedSingle::NAME).await.unwrap()).as_deref(),
        Some("Baz")
    );
    let created = model
        .create(
            &mut tx,
            vec![model.vals().set(RelatedSingle::RELATED_NAME, "Qux")],
        )
        .await
        .unwrap();
    tx.invalidate_all().await.unwrap();
    assert_eq!(
        rec.concat(&created)
            .unwrap()
            .mapped(&mut tx, RelatedSingle::NAME)
            .await
            .unwrap(),
        [Some(Arc::from("Baz")), Some(Arc::from("Qux"))]
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// a sync compute reading an undeclared field is a `Programming` error.
#[tokio::test]
async fn undeclared_read_in_a_sync_compute_is_an_error() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let model = tx.env().model::<Undeclared>().unwrap();
    let rec = model.create(&mut tx, vec![model.vals()]).await.unwrap();
    let err = rec.get(&mut tx, Undeclared::C).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert!(err.message().contains("test_orm.undeclared.b"), "{err}");
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn env_047_savepoint_rollback_clears_the_cache() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let partners = tx.env().model::<Partner>().unwrap();
    let countries = tx.env().model::<Country>().unwrap();
    let r = partners
        .create(&mut tx, vec![partners.vals()])
        .await
        .unwrap();
    let c = countries
        .create(&mut tx, vec![countries.vals().set(Country::NAME, "C")])
        .await
        .unwrap();
    tx.flush().await.unwrap();
    r.set(&mut tx, Partner::NAME, "A").await.unwrap();
    tx.capture_statements(true);
    let r2 = r.clone();
    let err = tx
        .savepoint(async |tx: &mut Transaction| -> erp_sdk::Result<()> {
            r2.set(tx, Partner::NAME, "B").await?;
            Err(erp_sdk::Error::user("X"))
        })
        .await
        .unwrap_err();
    assert_eq!(err.message(), "X");
    let log = tx.captured().to_vec();
    let update = log.iter().position(|q| q.starts_with("UPDATE")).unwrap();
    let savepoint = log.iter().position(|q| q.starts_with("SAVEPOINT")).unwrap();
    assert!(update < savepoint, "{log:?}");
    assert!(log.iter().any(|q| q.starts_with("ROLLBACK TO SAVEPOINT")));
    assert!(
        !tx.is_cached(&c, "name").unwrap(),
        "no value of any model is cached"
    );
    assert!(!tx.is_cached(&r, "name").unwrap());
    assert!(!tx.is_dirty(&r, "name").unwrap());
    assert_eq!(
        s(r.get(&mut tx, Partner::NAME).await.unwrap()).as_deref(),
        Some("A")
    );
    tx.commit().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn env_048_released_savepoint_keeps_state() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let partners = tx.env().model::<Partner>().unwrap();
    let r = partners
        .create(&mut tx, vec![partners.vals()])
        .await
        .unwrap();
    tx.flush().await.unwrap();
    tx.capture_statements(true);
    let r2 = r.clone();
    tx.savepoint(async |tx: &mut Transaction| r2.set(tx, Partner::NAME, "B").await)
        .await
        .unwrap();
    let log = tx.captured().to_vec();
    let update = log.iter().position(|q| q.starts_with("UPDATE")).unwrap();
    let release = log.iter().position(|q| q.starts_with("RELEASE")).unwrap();
    assert!(update < release, "{log:?}");
    assert_query_count!(tx, <= 0, {
        assert_eq!(s(r.get(&mut tx, Partner::NAME).await.unwrap()).as_deref(), Some("B"));
    });
    // a flush error at release rolls the savepoint back and is returned
    let gone = partners
        .create(&mut tx, vec![partners.vals()])
        .await
        .unwrap();
    tx.flush().await.unwrap();
    tx.execute(
        &format!("DELETE FROM test_orm_partner WHERE id = {}", gone.ids()[0]),
        &[],
    )
    .await
    .unwrap();
    let g = gone.clone();
    let err = tx
        .savepoint(async |tx: &mut Transaction| g.set(tx, Partner::NAME, "x").await)
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Missing);
    assert_eq!(
        s(r.get(&mut tx, Partner::NAME).await.unwrap()).as_deref(),
        Some("B")
    );
    tx.commit().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn env_049_050_051_ormcache() {
    use erp_sdk::OrmCacheKey;
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let key = OrmCacheKey::new("ir.model.data", "_xmlid_lookup", vec![Value::str("base.x")]);
    assert!(tx.ormcache_get("default", &key).await.unwrap().is_none());
    for _ in 0..3 {
        let v = tx
            .ormcache("default", key.clone(), async |_tx: &mut Transaction| {
                Ok(Value::Int(7))
            })
            .await
            .unwrap();
        assert_eq!(v, Value::Int(7));
    }
    assert!(tx.ormcache_get("default", &key).await.unwrap().is_some());
    assert_eq!(tx.ormcache_stats("ir.model.data", "_xmlid_lookup"), (2, 1));
    let other = OrmCacheKey::new("m", "f", vec![]);
    assert_eq!(tx.ormcache_get("default", &other).await.unwrap(), None);
    tx.ormcache_add("default", other.clone(), Value::str("v"))
        .await
        .unwrap();
    let v = tx
        .ormcache("default", other.clone(), async |_tx: &mut Transaction| {
            Err(erp_sdk::Error::programming("must not be called"))
        })
        .await
        .unwrap();
    assert_eq!(v, Value::str("v"));
    let k = OrmCacheKey::new("m", "g", vec![]);
    tx.ormcache_add("default", k.clone(), Value::str("val1"))
        .await
        .unwrap();
    let k2 = k.clone();
    let _ = tx
        .savepoint(async |tx: &mut Transaction| -> erp_sdk::Result<()> {
            tx.ormcache_add("default", k2.clone(), Value::str("sp1"))
                .await
                .unwrap();
            tx.invalidate_ormcache("default");
            assert_eq!(tx.ormcache_get("default", &k2).await.unwrap(), None);
            Err(erp_sdk::Error::user("rollback"))
        })
        .await;
    assert_eq!(
        tx.ormcache_get("default", &k).await.unwrap(),
        Some(Value::str("val1"))
    );
    let k2 = k.clone();
    tx.savepoint(async |tx: &mut Transaction| {
        tx.ormcache_add("default", k2.clone(), Value::str("sp2"))
            .await
            .unwrap();
        Ok(())
    })
    .await
    .unwrap();
    assert_eq!(
        tx.ormcache_get("default", &k).await.unwrap(),
        Some(Value::str("sp2"))
    );
    let _ = tx
        .savepoint(async |tx: &mut Transaction| -> erp_sdk::Result<()> {
            tx.invalidate_ormcache("default");
            Err(erp_sdk::Error::user("rollback"))
        })
        .await;
    assert_eq!(
        tx.ormcache_get("default", &k).await.unwrap(),
        Some(Value::str("sp2"))
    );
    tx.savepoint(async |tx: &mut Transaction| {
        tx.invalidate_ormcache("default");
        Ok(())
    })
    .await
    .unwrap();
    assert_eq!(
        tx.ormcache_get("default", &k).await.unwrap(),
        None,
        "invalidation merged"
    );
    // committed values are visible to the next transaction
    tx.ormcache_add("default", k.clone(), Value::str("committed"))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let mut tx2 = h.begin().await.unwrap();
    assert_eq!(
        tx2.ormcache_get("default", &k).await.unwrap(),
        Some(Value::str("committed"))
    );
    drop(tx2);
    drop(tx);
    h.cleanup().await.unwrap();
}

/// readonly precomputed fields are excluded from default resolution on the fallback path.
#[tokio::test]
async fn readonly_precompute_excluded_from_defaults_fallback_path() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let m = tx.env().model::<PrecomputeReadonly>().unwrap();
    type R = PrecomputeReadonly;
    // BAR is a char precomputed readonly field.
    // Setting default_bar to an invalid shape (list of int) would fail with ErrorKind::Validation
    // if default_get was asked for it. Because readonly precomputes are excluded upfront from
    // candidate fields on the fallback path, default_get is never asked for BAR, so no validation
    // error occurs, and precompute computes it normally.
    let ctx_m = m.with_context([
        ("default_bar", Value::list([Value::Int(123)])),
        ("default_unrelated_key", Value::str("trigger_fallback")),
    ]);
    let r = ctx_m
        .create(&mut tx, vec![ctx_m.vals().set(R::FOO, "foo")])
        .await
        .unwrap();
    // BAR is readonly precomputed, so the context default was excluded and precompute computed it
    assert_eq!(
        s(r.get(&mut tx, R::BAR).await.unwrap()).as_deref(),
        Some("COMPUTED")
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_comp_030_batched_recompute_partially_unreadable() {
    let policy = DomainPolicy::builder(Otherwise::Allow)
        .deny("test_orm.compute.unassigned", Operation::Read)
        .build()
        .unwrap();
    let h = Harness::builder([&tc::ADDON, &test_orm::ADDON, &base_users::ADDON])
        .with_static_decls(crate::currency_fixture::decls())
        .policy(policy)
        .build()
        .await
        .expect("harness");
    let mut tx = h.begin().await.unwrap();
    tx.batch("INSERT INTO res_users (id, name, login, active) VALUES (2, 'demo', 'demo', true)")
        .await
        .unwrap();
    let model = tx.env().model::<ComputeUnassigned>().unwrap();
    let r1 = model.create(&mut tx, vec![model.vals()]).await.unwrap();
    let r2 = model.create(&mut tx, vec![model.vals()]).await.unwrap();
    tx.flush().await.unwrap();
    tx.invalidate_all().await.unwrap();

    const DEMO: i64 = 2;
    let r1_demo = r1.with_user(DEMO);
    let r2_demo = r2.with_user(DEMO);

    // Records are not accessible for read by demo user
    let err1 = r1_demo
        .get(&mut tx, ComputeUnassigned::BARS)
        .await
        .unwrap_err();
    assert_eq!(err1.kind(), ErrorKind::Access);
    assert_eq!(
        err1.message(),
        "Your access rights do not let you read 'Model with computed fields left unassigned' (test_orm.compute.unassigned) records."
    );
    let err2 = r2_demo
        .get(&mut tx, ComputeUnassigned::BARS)
        .await
        .unwrap_err();
    assert_eq!(err2.kind(), ErrorKind::Access);
    assert_eq!(
        err2.message(),
        "Your access rights do not let you read 'Model with computed fields left unassigned' (test_orm.compute.unassigned) records."
    );

    // Modifying records and flushing should not trigger an access error in either order:
    // Order 1: r1 assigned, r2 unassigned
    r1_demo
        .set(&mut tx, ComputeUnassigned::FOO, "assign")
        .await
        .unwrap();
    r2_demo
        .set(&mut tx, ComputeUnassigned::FOO, "x")
        .await
        .unwrap();
    tx.flush().await.unwrap();
    tx.invalidate_all().await.unwrap();
    assert_eq!(
        s(r1.get(&mut tx, ComputeUnassigned::BARS).await.unwrap()).as_deref(),
        Some("assign")
    );
    assert_eq!(
        r2.get(&mut tx, ComputeUnassigned::BARS).await.unwrap(),
        None
    );

    // Order 2: r3 unassigned, r4 assigned
    let r3 = model.create(&mut tx, vec![model.vals()]).await.unwrap();
    let r4 = model.create(&mut tx, vec![model.vals()]).await.unwrap();
    tx.flush().await.unwrap();
    tx.invalidate_all().await.unwrap();
    let r3_demo = r3.with_user(DEMO);
    let r4_demo = r4.with_user(DEMO);
    r3_demo
        .set(&mut tx, ComputeUnassigned::FOO, "x")
        .await
        .unwrap();
    r4_demo
        .set(&mut tx, ComputeUnassigned::FOO, "assign")
        .await
        .unwrap();
    tx.flush().await.unwrap();
    tx.invalidate_all().await.unwrap();
    assert_eq!(
        r3.get(&mut tx, ComputeUnassigned::BARS).await.unwrap(),
        None
    );
    assert_eq!(
        s(r4.get(&mut tx, ComputeUnassigned::BARS).await.unwrap()).as_deref(),
        Some("assign")
    );

    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_comp_069_write_uid_recompute_only() {
    let h = Harness::builder([&test_depends::ADDON, &base_users::ADDON])
        .build()
        .await
        .expect("harness");
    let mut tx = h.begin().await.unwrap();
    tx.batch(
        "INSERT INTO res_users (id, name, login, active) VALUES (2, 'demo', 'demo', true), (3, 'user_b', 'user_b', true)",
    )
    .await
    .unwrap();
    let partners = tx.env().model::<test_depends::Partner>().unwrap();
    let orders = tx.env().model::<test_depends::Order>().unwrap();

    let p = partners
        .create(
            &mut tx,
            vec![
                partners
                    .vals()
                    .set(test_depends::Partner::NAME, "Partner P"),
            ],
        )
        .await
        .unwrap();
    let o = orders
        .create(
            &mut tx,
            vec![
                orders
                    .vals()
                    .set(test_depends::Order::NAME, "Order 1")
                    .set(test_depends::Order::PARTNER_ID, p.rec_ids()[0])
                    .set(test_depends::Order::AMOUNT_TOTAL, 100.0),
            ],
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();

    const USER_A: i64 = 2;
    const USER_B: i64 = 3;

    // Begin transaction with USER_B environment (default environment for commit/flush)
    let mut tx = h.begin_as(USER_B).await.unwrap();
    let o_user_a = o.with_user(USER_A);
    // User A modifies a dependency
    o_user_a
        .set(&mut tx, test_depends::Order::AMOUNT_TOTAL, 250.0)
        .await
        .unwrap();

    // Recompute is flushed in the transaction's environment (USER_B)
    tx.flush().await.unwrap();
    tx.commit().await.unwrap();

    let mut tx = h.begin().await.unwrap();
    // Order was directly modified by User A -> write_uid = USER_A
    let order_wu = support::one(
        &mut tx,
        &format!(
            "SELECT write_uid::text FROM test_dep_order WHERE id = {}",
            o.ids()[0]
        ),
    )
    .await;
    assert_eq!(order_wu.as_deref(), Some("2"));

    // Partner was updated ONLY by the recompute -> write_uid = USER_B
    let partner_wu = support::one(
        &mut tx,
        &format!(
            "SELECT write_uid::text FROM test_dep_partner WHERE id = {}",
            p.ids()[0]
        ),
    )
    .await;
    assert_eq!(partner_wu.as_deref(), Some("3"));

    // Partner total_amount was recomputed to 250.0
    assert_eq!(
        p.get(&mut tx, test_depends::Partner::TOTAL_AMOUNT)
            .await
            .unwrap(),
        250.0
    );

    drop(tx);
    h.cleanup().await.unwrap();
}

/// (the note on): a required stored compute that is computed after the
/// INSERT and not given is refused before any statement of the create, with the NOT NULL text and
/// the reason; a given value is inserted, then recomputed at the flush. Fails on the code before
/// PC-1, where the INSERT is sent and PostgreSQL's NOT NULL violation (SQLSTATE 23502, no reason)
/// comes back.
#[tokio::test]
async fn a_required_stored_compute_computed_after_the_insert_is_refused_before_it() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let model = tx.env().model::<RequiredLate>().unwrap();
    let given = model
        .create(
            &mut tx,
            vec![
                model
                    .vals()
                    .set(RequiredLate::NAME, "n")
                    .set(RequiredLate::CODE, "given"),
            ],
        )
        .await
        .unwrap();
    tx.flush().await.unwrap();
    tx.invalidate_all().await.unwrap();
    assert_eq!(
        s(given.get(&mut tx, RequiredLate::CODE).await.unwrap()).as_deref(),
        Some("code:n"),
        "recomputed at the flush"
    );
    // one row gives the value, the other does not: the whole call is refused, nothing is sent
    let err = assert_query_count!(tx, <= 0, {
        model
            .create(
                &mut tx,
                vec![
                    model
                        .vals()
                        .set(RequiredLate::NAME, "a")
                        .set(RequiredLate::CODE, "given"),
                    model.vals().set(RequiredLate::NAME, "b"),
                ],
            )
            .await
            .unwrap_err()
    });
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.sqlstate(),
        None,
        "refused by the ORM, not by PostgreSQL"
    );
    assert_eq!(
        err.message(),
        "The database refused the change: Missing required field 'Code' (code) for model \
         'Required late' (test_orm.compute.required_late): it is computed after the INSERT \
         because it is declared precompute = false"
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}
