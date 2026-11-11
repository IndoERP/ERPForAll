//! `ir.model.data` for the data loader: the external-id cache that follows
//! `update_xmlids` and `env_ref`, and the moved `load_records`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use erp_data::model_data::{
    XidCache, env_ref, load_records, load_records_cached, lookup, lookup_cached, update_xmlids,
    update_xmlids_cached,
};
use erp_data::{LoadRecord, XidBinding, XmlId};
use erp_sdk::{ErrorKind, RawVals, Records, Transaction, Value};
use erp_test_models::{base_users, test_convert};
use erp_testkit::{Harness, assert_query_count};

async fn harness() -> Harness {
    Harness::builder([
        &base_users::ADDON,
        &test_convert::ADDON,
        &erp_data::model_data::ADDON,
    ])
    .with_decls([erp_data::ir_model_data_decl()])
    .build()
    .await
    .expect("harness")
}

const MODEL: &str = "test_convert.record";

fn xid(text: &str) -> XmlId {
    XmlId::parse(text).unwrap()
}

/// `n` new records of the fixture model, in creation order.
async fn make_records(tx: &mut Transaction, n: usize) -> Vec<i64> {
    let records = tx.env().records(MODEL).unwrap();
    let vals: Vec<RawVals> = (0..n)
        .map(|i| {
            records
                .vals_from_values([(&"name".to_owned(), &Value::str(format!("r{i}")))])
                .unwrap()
        })
        .collect();
    records.create(tx, vals).await.unwrap().ids()
}

/// What `ir.model.data` says about `x`, straight from the database (no cache).
async fn stored(tx: &mut Transaction, x: &XmlId) -> Option<(String, Option<i64>, bool)> {
    let env = tx.env();
    lookup(tx, &env, std::slice::from_ref(x))
        .await
        .unwrap()
        .remove(x)
        .map(|r| (r.model, r.res_id, r.noupdate))
}

/// What the cache says about `x`, asserting that asking costs no statement.
async fn cached(
    tx: &mut Transaction,
    cache: &mut XidCache,
    x: &XmlId,
) -> Option<(String, Option<i64>, bool)> {
    let env = tx.env();
    let mut found = assert_query_count!(tx, >= 0, <= 0, {
        lookup_cached(tx, &env, cache, std::slice::from_ref(x))
            .await
            .unwrap()
    });
    found.remove(x).map(|r| (r.model, r.res_id, r.noupdate))
}

fn binding(x: &XmlId, id: i64, noupdate: bool) -> XidBinding {
    XidBinding::new(x.clone(), MODEL, id, noupdate)
}

#[tokio::test]
async fn spec_data_009_update_xmlids_is_one_statement_and_the_cache_follows_it() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let env = tx.env();
    let r = make_records(&mut tx, 7).await;
    let mut cache = XidCache::new();
    let x = xid("m.x");

    // a new external id assigned to r0: one statement; the cache has it, asking costs nothing
    assert_query_count!(tx, >= 1, <= 1, {
        update_xmlids_cached(&mut tx, &env, &mut cache, &[binding(&x, r[0], false)], false)
            .await
            .unwrap();
    });
    assert_eq!(
        cached(&mut tx, &mut cache, &x).await,
        Some((MODEL.to_owned(), Some(r[0]), false))
    );
    assert_eq!(
        stored(&mut tx, &x).await,
        cached(&mut tx, &mut cache, &x).await
    );

    // r1 with update=True, then r2 without: each call is one statement, the cache follows
    for (target, update) in [(r[1], true), (r[2], false)] {
        assert_query_count!(tx, >= 1, <= 1, {
            update_xmlids_cached(&mut tx, &env, &mut cache, &[binding(&x, target, false)], update)
                .await
                .unwrap();
        });
        assert_eq!(
            cached(&mut tx, &mut cache, &x).await,
            Some((MODEL.to_owned(), Some(target), false)),
            "update={update}"
        );
        assert_eq!(
            stored(&mut tx, &x).await,
            cached(&mut tx, &mut cache, &x).await
        );
    }

    // a noupdate external id: update=True ignores the reassignment, update=False follows it
    let nu = xid("m.nu");
    update_xmlids_cached(
        &mut tx,
        &env,
        &mut cache,
        &[binding(&nu, r[3], true)],
        false,
    )
    .await
    .unwrap();
    for (target, update, expect) in [(r[4], true, r[3]), (r[5], false, r[5])] {
        assert_query_count!(tx, >= 1, <= 1, {
            update_xmlids_cached(&mut tx, &env, &mut cache, &[binding(&nu, target, true)], update)
                .await
                .unwrap();
        });
        assert_eq!(
            cached(&mut tx, &mut cache, &nu).await,
            Some((MODEL.to_owned(), Some(expect), true)),
            "update={update}"
        );
        assert_eq!(
            stored(&mut tx, &nu).await,
            cached(&mut tx, &mut cache, &nu).await
        );
    }
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// A cache entry that was read from a row the statement then left alone stays; an id the cache
/// never saw stays unknown (and costs one statement to ask for).
#[tokio::test]
async fn cache_keeps_what_it_read_and_does_not_guess_what_it_did_not() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let env = tx.env();
    let r = make_records(&mut tx, 3).await;
    let nu = xid("m.nu");
    // the row exists (noupdate), the cache has never seen it
    update_xmlids(&mut tx, &env, &[binding(&nu, r[0], true)], false)
        .await
        .unwrap();
    let mut cache = XidCache::new();
    assert!(cache.is_empty());
    // the guarded update returns nothing: nothing is cached for the unseen id
    update_xmlids_cached(&mut tx, &env, &mut cache, &[binding(&nu, r[1], true)], true)
        .await
        .unwrap();
    assert!(cache.get(&nu).is_none());
    // asking costs one statement and reads the truth (r0, not the refused r1)
    let found = assert_query_count!(tx, >= 1, <= 1, {
        lookup_cached(&mut tx, &env, &mut cache, std::slice::from_ref(&nu))
            .await
            .unwrap()
    });
    assert_eq!(found[&nu].res_id, Some(r[0]));
    // now it is cached: the next guarded update keeps the entry that was read
    update_xmlids_cached(&mut tx, &env, &mut cache, &[binding(&nu, r[2], true)], true)
        .await
        .unwrap();
    assert_eq!(
        cached(&mut tx, &mut cache, &nu).await,
        Some((MODEL.to_owned(), Some(r[0]), true))
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_data_031_env_ref_errors_and_raise_if_not_found_false() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let env = tx.env();
    let r = make_records(&mut tx, 2).await;
    let present = xid("m.present");
    let deleted = xid("m.deleted");
    let none = xid("m.none");
    update_xmlids(
        &mut tx,
        &env,
        &[
            binding(&present, r[0], false),
            binding(&deleted, r[1], false),
        ],
        false,
    )
    .await
    .unwrap();
    // `m.deleted`: the record is deleted behind the external id, directly in SQL
    let deleted_id = r[1];
    tx.execute(
        "DELETE FROM test_convert_record WHERE id = $1",
        &[&deleted_id],
    )
    .await
    .unwrap();

    // the record behind a live external id
    let found: Records = env_ref(&mut tx, &env, &present, true)
        .await
        .unwrap()
        .unwrap();
    assert_eq!((found.model_name(), found.ids()), (MODEL, vec![r[0]]));

    // no row: the external id is not in the system
    let err = env_ref(&mut tx, &env, &none, true).await.unwrap_err();
    assert_eq!(
        (err.kind(), err.message()),
        (
            ErrorKind::Missing,
            "External ID not found in the system: m.none"
        )
    );
    // a row whose record is gone
    let err = env_ref(&mut tx, &env, &deleted, true).await.unwrap_err();
    assert_eq!(
        (err.kind(), err.message()),
        (
            ErrorKind::Missing,
            "No record found for unique ID m.deleted. It may have been deleted."
        )
    );
    // without raising, both are `None`
    assert!(
        env_ref(&mut tx, &env, &none, false)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        env_ref(&mut tx, &env, &deleted, false)
            .await
            .unwrap()
            .is_none()
    );
    // an unqualified external id without a module part
    let bare = xid("bare");
    let err = env_ref(&mut tx, &env, &bare, true).await.unwrap_err();
    assert_eq!(
        (err.kind(), err.message()),
        (
            ErrorKind::Missing,
            "External ID not found in the system: bare"
        )
    );
    assert!(
        env_ref(&mut tx, &env, &bare, false)
            .await
            .unwrap()
            .is_none()
    );
    // a lookup and an existence check, nothing more
    assert_query_count!(tx, >= 2, <= 2, {
        env_ref(&mut tx, &env, &present, true).await.unwrap();
    });
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// `lookup_cached` asks the database once for what the cache lacks, and never for what it has.
#[tokio::test]
async fn lookup_cached_costs_one_statement_for_the_misses_and_none_for_the_rest() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let env = tx.env();
    let r = make_records(&mut tx, 2).await;
    let (a, b, missing) = (xid("m.a"), xid("m.b"), xid("m.missing"));
    update_xmlids(
        &mut tx,
        &env,
        &[binding(&a, r[0], false), binding(&b, r[1], false)],
        false,
    )
    .await
    .unwrap();
    let mut cache = XidCache::new();
    let wanted = [a.clone(), b.clone(), missing.clone()];
    let first = assert_query_count!(tx, >= 1, <= 1, {
        lookup_cached(&mut tx, &env, &mut cache, &wanted).await.unwrap()
    });
    assert_eq!(first.len(), 2, "the missing id has no row");
    assert_eq!(cache.len(), 2, "only rows are cached");
    // the two present ids are free now; the missing one costs a statement each time
    let second = assert_query_count!(tx, >= 0, <= 0, {
        lookup_cached(&mut tx, &env, &mut cache, &wanted[..2]).await.unwrap()
    });
    assert_eq!(second, first);
    assert_query_count!(tx, >= 1, <= 1, {
        lookup_cached(&mut tx, &env, &mut cache, &wanted).await.unwrap();
    });
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

fn loadable(x: Option<&XmlId>, name: &str, noupdate: bool) -> LoadRecord {
    let vals = RawVals::new();
    let vals = {
        let mut v = vals;
        v.insert(
            "name".into(),
            erp_sdk::Cell::Str(std::sync::Arc::from(name)),
        );
        v
    };
    LoadRecord::new(x.cloned(), vals, noupdate)
}

/// `load_records` creates, finds and updates by external id, in batch order, and the cached
/// variant leaves the cache in step with the rows it wrote.
#[tokio::test]
async fn load_records_creates_then_updates_by_external_id_and_feeds_the_cache() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let env = tx.env();
    let model = env.records(MODEL).unwrap();
    let (a, b) = (xid("m.a"), xid("m.b"));
    let mut cache = XidCache::new();

    let created = load_records_cached(
        &mut tx,
        &model,
        vec![
            loadable(Some(&a), "A", false),
            loadable(Some(&b), "B", true),
        ],
        false,
        &mut cache,
    )
    .await
    .unwrap();
    let ids = created.ids();
    assert_eq!(ids.len(), 2);
    assert_eq!(
        cached(&mut tx, &mut cache, &a).await,
        Some((MODEL.to_owned(), Some(ids[0]), false))
    );
    assert_eq!(
        cached(&mut tx, &mut cache, &b).await,
        Some((MODEL.to_owned(), Some(ids[1]), true))
    );
    assert_eq!(
        stored(&mut tx, &b).await,
        cached(&mut tx, &mut cache, &b).await
    );

    // loading them again (update mode) writes `a` and leaves the noupdate `b` alone, in order
    let again = load_records(
        &mut tx,
        &model,
        vec![
            loadable(Some(&b), "B2", false),
            loadable(Some(&a), "A2", false),
        ],
        true,
    )
    .await
    .unwrap();
    assert_eq!(again.ids(), [ids[1], ids[0]]);
    tx.invalidate_all().await.unwrap();
    let names = model
        .browse(ids.clone())
        .unwrap()
        .cells(&mut tx, "name")
        .await
        .unwrap();
    assert_eq!(
        names,
        [
            erp_sdk::Cell::Str("A2".into()),
            erp_sdk::Cell::Str("B".into())
        ]
    );

    // update mode without an id of any kind is refused
    let err = load_records(&mut tx, &model, vec![loadable(None, "nobody", false)], true)
        .await
        .unwrap_err();
    assert_eq!(
        (err.kind(), err.message()),
        (
            ErrorKind::Validation,
            "Cannot update a record without specifying its id or xml_id"
        )
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// Tracked batches are internal loader state; a missing module is a contract failure.
#[tokio::test]
async fn a_tracked_batch_without_its_module_is_an_invariant() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let model = tx
        .env()
        .records(MODEL)
        .unwrap()
        .with_context([(erp_data::load::load_state::TRACK_BATCH, Value::Bool(true))]);
    let error = load_records(&mut tx, &model, vec![loadable(None, "A", false)], false)
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.message(),
        "context['install_track_batch'] is set without install_module text"
    );
    assert!(error.is_invariant_violation());
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn model_data_invariants_report_internal_error_kind() {
    let err = erp_fields::Error::internal("broken invariant");
    assert_eq!(err.kind(), ErrorKind::Internal);
    assert_eq!(err.message(), "broken invariant");
    assert!(err.is_internal());
    assert_eq!(err.kind().http_status(), 500);
}

#[test]
fn qualified_xid_binding() {
    use erp_data::{QualifiedXmlId, XidBinding};

    let q = QualifiedXmlId::parse("test_mod.test_rec").unwrap();
    let binding = XidBinding::qualified(q.clone(), "test.model", 42, false);
    assert_eq!(binding.xid.module(), Some("test_mod"));
    assert_eq!(binding.xid.name(), "test_rec");
    assert_eq!(binding.qualified_xid().unwrap(), q);
}

#[test]
fn qualified_xml_id_newtype_validations_and_conversions() {
    use erp_data::QualifiedXmlId;

    let q = QualifiedXmlId::parse("base.res_partner").unwrap();
    assert_eq!(q.module(), "base");
    assert_eq!(q.name(), "res_partner");
    assert_eq!(q.to_string(), "base.res_partner");

    let xid: XmlId = q.clone().into();
    assert_eq!(xid.module(), Some("base"));
    assert_eq!(xid.name(), "res_partner");
    assert_eq!(xid.require_qualified().unwrap(), q);

    let bare = XmlId::parse("unqualified").unwrap();
    assert!(bare.as_qualified().is_none());
    let err = bare.require_qualified().unwrap_err();
    assert_eq!(
        err.to_string(),
        "external ID \"unqualified\" has no module: external IDs must be in the form module.name"
    );

    assert!(QualifiedXmlId::parse("unqualified").is_err());
}
