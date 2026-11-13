//! on more than one node (SCALE.TEST style, 3, 5): two
//! ORM instances over one database and one shared store stand for two application nodes. Dedup by
//! content hash has no race, garbage collection is a leased job any node may run and never removes
//! content another node is linking, and a node-local store is refused for a second live node.
//!
//! Real separate processes are `SCALE.TEST`'s; these are two independent ORM
//! generations with their own pools and runtimes in one process.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod attachment_support;
mod support;

use std::sync::{Arc, Mutex};

use attachment_support::*;
use erp_addon_base::attachments::gc::{
    GC_HOLD_LOCK, hold, mark_unreferenced, sweep_mark, sweep_remove,
};
use erp_addon_base::attachments::{
    self, AttachmentContent, NewAttachment, StageRequest, Uploader, verify_topology,
};
use erp_blob::{AttachmentConfig, AttachmentRuntime, AttachmentStore};
use erp_orm::{BeginOptions, Isolation, Orm, PrincipalHint};
use erp_sdk::{Context, ErrorKind};
use erp_test_models::base_users;
use erp_testkit::Harness;

/// A second application node: the same database and store, its own ORM generation and runtime.
fn second_node(f: &Fixture, store: Arc<dyn AttachmentStore>) -> Orm {
    let rt = Arc::new(AttachmentRuntime::single("main", store, external_limits()));
    f.h.orm()
        .rebuild(Arc::clone(f.h.registry()))
        .generation_attachment(erp_orm::GenerationAttachment::new(
            Arc::clone(f.h.registry()),
            rt,
        ))
        .build()
        .unwrap()
}

async fn env_of(orm: &Orm) -> erp_sdk::Env {
    let tx = orm.begin(PrincipalHint::system()).await.unwrap();
    tx.env()
}

/// Stage `bytes` on `orm` and link them as a standalone attachment in a retried transaction.
async fn upload_and_link(orm: &Orm, name: &str, bytes: &[u8]) -> i64 {
    let env = env_of(orm).await;
    let mut body = bytes;
    let up = Uploader::new(orm)
        .stage(
            &env,
            &mut body,
            StageRequest::named(name).with_declared_length(bytes.len() as u64),
        )
        .await
        .unwrap();
    let ids = orm
        .run(PrincipalHint::system(), Context::new(), async |tx| {
            attachments::create(
                tx,
                vec![NewAttachment::new(
                    name,
                    AttachmentContent::Upload(up.id.clone()),
                )],
            )
            .await
        })
        .await
        .unwrap();
    ids[0].get()
}

async fn gc_pass(orm: &Orm) {
    // as the job kind runs: READ COMMITTED, the system principal
    let rc = || BeginOptions::new(PrincipalHint::system()).isolation(Isolation::ReadCommitted);
    let mut tx = orm.begin_opts(rc()).await.unwrap();
    mark_unreferenced(&mut tx, 100).await.unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let mut tx = orm.begin_opts(rc()).await.unwrap();
    let ids = sweep_mark(&mut tx, 0, 100).await.unwrap().ids;
    tx.commit().await.unwrap();
    drop(tx);
    if ids.is_empty() {
        return;
    }
    let mut tx = orm.begin_opts(rc()).await.unwrap();
    let env = tx.env();
    match sweep_remove(&mut tx, &env, &ids, 0).await {
        Ok(_) => tx.commit().await.unwrap(),
        Err(_) => {
            let _ = tx.rollback().await;
        }
    }
}

/// Whether a transaction waits for the hold lock (`GC_HOLD_LOCK`), read from `pg_locks` on a
/// connection of its own.
async fn waits_for_hold_lock(observer: &erp_testkit::tokio_postgres::Client) -> bool {
    let (class, key) = GC_HOLD_LOCK;
    let n: i64 = observer
        .query_one(
            "SELECT count(*) FROM pg_locks WHERE locktype = 'advisory' AND NOT granted \
             AND classid = $1::int4::oid AND objid = $2::int4::oid AND objsubid = 2",
            &[&class, &key],
        )
        .await
        .unwrap()
        .get(0);
    n > 0
}

/// starts after its hold commits never references a key the sweep is removing)
#[tokio::test]
async fn a_hold_on_one_node_waits_for_a_sweep_running_on_another() {
    let f = fixture(external_limits()).await;
    admit_cleanup(f.h.orm()).await;
    let node_b = second_node(&f, Arc::clone(&f.store) as Arc<dyn AttachmentStore>);
    let node_a = f.h.orm().clone();
    let rc = || BeginOptions::new(PrincipalHint::system()).isolation(Isolation::ReadCommitted);
    let first = upload_and_link(&node_a, "h1.txt", b"removed by the running sweep").await;
    let second = upload_and_link(&node_a, "h2.txt", b"kept by the hold afterwards").await;
    let mut tx = node_a.begin(PrincipalHint::system()).await.unwrap();
    tx.execute("DELETE FROM ir_attachment WHERE id = $1", &[&first])
        .await
        .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let mut tx = node_a.begin_opts(rc()).await.unwrap();
    mark_unreferenced(&mut tx, 100).await.unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let mut tx = node_a.begin_opts(rc()).await.unwrap();
    let ids = sweep_mark(&mut tx, 0, 100).await.unwrap().ids;
    tx.commit().await.unwrap();
    drop(tx);
    assert_eq!(ids.len(), 1);
    // node A's sweep: past the hold check, its key removed, not committed yet
    let mut sweep = node_a.begin_opts(rc()).await.unwrap();
    let env = sweep.env();
    let report = sweep_remove(&mut sweep, &env, &ids, 0).await.unwrap();
    assert_eq!((report.rows, report.keys), (1, 1));
    // node B asks for a hold through its own pool: it waits and does not return while A is open
    let observer = f.h.db().client().await.unwrap();
    let take_hold = async {
        let mut tx = node_b.begin(PrincipalHint::system()).await.unwrap();
        let id = hold(&mut tx, 600, "backup running").await.unwrap();
        tx.commit().await.unwrap();
        id
    };
    tokio::pin!(take_hold);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        tokio::select! {
            _ = &mut take_hold => panic!("the hold returned while another node's sweep held the lock"),
            () = tokio::time::sleep(std::time::Duration::from_millis(20)) => {}
        }
        if waits_for_hold_lock(&observer).await {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the hold never waited for the lock"
        );
    }
    sweep.commit().await.unwrap();
    drop(sweep);
    assert!(take_hold.await > 0);
    // a sweep on node A after the hold committed on node B sees it and removes nothing
    let mut tx = node_a.begin(PrincipalHint::system()).await.unwrap();
    tx.execute("DELETE FROM ir_attachment WHERE id = $1", &[&second])
        .await
        .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let mut tx = node_a.begin_opts(rc()).await.unwrap();
    mark_unreferenced(&mut tx, 100).await.unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let mut tx = node_a.begin_opts(rc()).await.unwrap();
    assert!(
        sweep_mark(&mut tx, 0, 100).await.unwrap().ids.is_empty(),
        "held"
    );
    tx.commit().await.unwrap();
    assert_eq!(f.store.content_keys().len(), 1);
}

#[tokio::test]
async fn two_nodes_link_identical_content_into_one_blob() {
    let f = fixture(external_limits()).await;
    admit_cleanup(f.h.orm()).await;
    let store: Arc<dyn AttachmentStore> = Arc::clone(&f.store) as _;
    let node_b = second_node(&f, store);
    let node_a = f.h.orm().clone();
    let mut tasks: Vec<std::pin::Pin<Box<dyn std::future::Future<Output = i64>>>> = Vec::new();
    for i in 0..12 {
        let orm = if i % 2 == 0 {
            node_a.clone()
        } else {
            node_b.clone()
        };
        tasks.push(Box::pin(async move {
            upload_and_link(
                &orm,
                &format!("f{i}.txt"),
                b"identical content on two nodes",
            )
            .await
        }));
    }
    let ids: Vec<i64> = futures_util::future::join_all(tasks).await;
    assert_eq!(ids.len(), 12);
    assert_eq!(
        scalar(&f.h, "SELECT count(*) FROM ir_blob").await,
        1,
        "one row per digest"
    );
    assert_eq!(
        scalar(&f.h, "SELECT count(DISTINCT blob_id) FROM ir_attachment").await,
        1
    );
    assert_eq!(
        f.store.content_keys().len(),
        1,
        "one object in the shared store"
    );
    // either node reads what the other linked
    let mut tx = node_b.begin(PrincipalHint::system()).await.unwrap();
    let handle = attachments::open(&mut tx, ids[0]).await.unwrap();
    assert_eq!(
        handle.read_to_vec(100).await.unwrap(),
        b"identical content on two nodes"
    );
}

/// nodes; the crash points are in base_attachment_gc). Oracle: every committed reference has
/// bytes whose SHA-256 matches its key (`read_to_vec` checks it); after quiescence and a final
/// collection no key exists without a row.
#[tokio::test]
async fn linkers_unlinkers_and_collectors_on_two_nodes_never_lose_referenced_content() {
    let f = fixture(external_limits()).await;
    admit_cleanup(f.h.orm()).await;
    let store: Arc<dyn AttachmentStore> = Arc::clone(&f.store) as _;
    let node_b = second_node(&f, store);
    let node_a = f.h.orm().clone();
    let live: Arc<Mutex<Vec<i64>>> = Arc::new(Mutex::new(Vec::new()));
    let contents: [&[u8]; 3] = [
        b"content number one",
        b"content number two",
        b"content number three",
    ];
    let mut tasks: Vec<std::pin::Pin<Box<dyn std::future::Future<Output = ()>>>> = Vec::new();
    for worker in 0..4u64 {
        let orm = if worker % 2 == 0 {
            node_a.clone()
        } else {
            node_b.clone()
        };
        let live = Arc::clone(&live);
        tasks.push(Box::pin(async move {
            let mut seed = 0x9E37_79B9_7F4A_7C15u64 ^ (worker + 1);
            for round in 0..8 {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                let bytes = contents[(seed % 3) as usize];
                let id = upload_and_link(&orm, &format!("w{worker}r{round}.txt"), bytes).await;
                live.lock().unwrap().push(id);
            }
        }));
    }
    for worker in 0..2u64 {
        let orm = if worker == 0 {
            node_a.clone()
        } else {
            node_b.clone()
        };
        let live = Arc::clone(&live);
        tasks.push(Box::pin(async move {
            let mut seed = 0xD1B5_4A32_D192_ED03u64 ^ (worker + 7);
            for _ in 0..10 {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                let victim = {
                    let mut live = live.lock().unwrap();
                    if live.is_empty() {
                        None
                    } else {
                        let at = (seed % live.len() as u64) as usize;
                        Some(live.swap_remove(at))
                    }
                };
                if let Some(id) = victim {
                    orm.run(PrincipalHint::system(), Context::new(), async |tx| {
                        tx.env()
                            .records("ir.attachment")?
                            .browse([id])?
                            .unlink(tx)
                            .await
                    })
                    .await
                    .unwrap();
                }
                tokio::task::yield_now().await;
            }
        }));
    }
    for worker in 0..2u64 {
        let orm = if worker == 0 {
            node_a.clone()
        } else {
            node_b.clone()
        };
        tasks.push(Box::pin(async move {
            for _ in 0..10 {
                gc_pass(&orm).await;
                tokio::task::yield_now().await;
            }
        }));
    }
    futures_util::future::join_all(tasks).await;
    // every surviving reference reads back with a verified digest
    let survivors: Vec<i64> = live.lock().unwrap().clone();
    let mut tx = f.h.begin().await.unwrap();
    for id in &survivors {
        let handle = attachments::open(&mut tx, *id).await.unwrap();
        let bytes = handle.read_to_vec(100).await.unwrap();
        assert!(contents.contains(&bytes.as_slice()), "attachment {id}");
    }
    tx.rollback().await.unwrap();
    drop(tx);
    // remove everything and collect: nothing is left in the store without a row
    let mut tx = f.h.begin().await.unwrap();
    tx.execute("DELETE FROM ir_attachment", &[]).await.unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    for _ in 0..3 {
        gc_pass(&node_a).await;
    }
    assert_eq!(scalar(&f.h, "SELECT count(*) FROM ir_blob").await, 0);
    assert!(
        f.store.content_keys().is_empty(),
        "{:?}",
        f.store.content_keys()
    );
}

/// Garbage collection is a leased job: two runners on two nodes drain the same garbage and the
/// singleton key keeps one run at a time; nothing referenced is lost, no job is dead.
#[tokio::test]
async fn the_collection_job_runs_on_any_node_under_a_lease() {
    let f = fixture(external_limits()).await;
    admit_cleanup(f.h.orm()).await;
    let store: Arc<dyn AttachmentStore> = Arc::clone(&f.store) as _;
    let node_b = second_node(&f, store);
    let keep = upload_and_link(f.h.orm(), "keep.txt", b"referenced stays").await;
    let drop_id = upload_and_link(&node_b, "drop.txt", b"garbage goes away").await;
    let mut tx = f.h.begin().await.unwrap();
    tx.env()
        .records("ir.attachment")
        .unwrap()
        .browse([drop_id])
        .unwrap()
        .unlink(&mut tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    attachments::gc::ensure_schedules(f.h.orm()).await.unwrap();
    attachments::gc::ensure_schedules(&node_b).await.unwrap();
    let mut tx = f.h.begin().await.unwrap();
    assert_eq!(
        count(
            &mut tx,
            "SELECT count(*) FROM erp_job WHERE kind = 'base.attachment_gc'"
        )
        .await,
        1,
        "one schedule row however many nodes ensure it"
    );
    tx.batch("UPDATE erp_job SET run_after = now() WHERE kind LIKE 'base.attachment_%'")
        .await
        .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let runner_a = support::jobs::runner(&f.h);
    let runner_b = {
        use erp_orm::jobs::JobRunner;
        JobRunner::builder(node_b.clone(), support::jobs::PassResolver)
            .build()
            .unwrap()
    };
    // Three kinds share the two runners since the owner heartbeat is scheduled too: which one a tick takes is
    // not fixed, so the bound is generous; the loop ends as soon as the blob is collected. The heartbeat keeps
    // its own five-minute cadence after its first run: made due on every pass it would run beside the
    // collection on the other runner each time and hold the store `recovering` while the collection checks
    // it, so the collection would skip the store on every pass.
    for _ in 0..24 {
        let (a, b) = tokio::join!(runner_a.tick(), runner_b.tick());
        a.unwrap();
        b.unwrap();
        let mut tx = f.h.begin().await.unwrap();
        tx.batch(
            "UPDATE erp_job SET run_after = now() WHERE kind LIKE 'base.attachment_%' \
                  AND kind <> 'base.attachment_owner_heartbeat' AND state = 'ready'",
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        if scalar(&f.h, "SELECT count(*) FROM ir_blob").await == 1 {
            break;
        }
    }
    let mut tx = f.h.begin().await.unwrap();
    let diagnosis: String = tx
        .query(
            "SELECT coalesce((SELECT string_agg(kind || ' ' || state || ' attempts=' || attempts || ' ' \
             || coalesce(last_error_code, '-') || ' ' || coalesce(last_error, '-') || ' progress=' \
             || coalesce(progress::text, '-'), E'\\n' ORDER BY kind) FROM erp_job \
             WHERE kind LIKE 'base.attachment_%'), '') || E'\\nowner: ' || coalesce((SELECT string_agg( \
             mode || ' fresh=' || (last_success_at > clock_timestamp() - make_interval(secs => $1::float8))::text \
             || ' ready=' || coalesce((observation_not_before <= clock_timestamp())::text, 'none'), ', ') \
             FROM ir_blob_owner_state), '') || E'\\nblobs: ' || coalesce((SELECT string_agg(id || ':' \
             || state || ':' || coalesce(sweep_attempted_at::text, '-'), ', ') FROM ir_blob), '')",
            &[&(erp_addon_base::attachments::ownership::FRESH_SECS as f64)],
        )
        .await
        .unwrap()[0]
        .get(0);
    tx.rollback().await.unwrap();
    drop(tx);
    assert_eq!(
        scalar(&f.h, "SELECT count(*) FROM ir_blob").await,
        1,
        "{diagnosis}"
    );
    assert_eq!(f.store.content_keys().len(), 1);
    let mut tx = f.h.begin().await.unwrap();
    let handle = attachments::open(&mut tx, keep).await.unwrap();
    assert_eq!(handle.read_to_vec(100).await.unwrap(), b"referenced stays");
    assert_eq!(
        count(
            &mut tx,
            "SELECT count(*) FROM erp_job WHERE kind LIKE 'base.attachment_%' AND state = 'dead'"
        )
        .await,
        0
    );
}

async fn local_runtime(shared: bool, dir: &std::path::Path) -> Arc<AttachmentRuntime> {
    let text = format!(
        "[attachments]\nwrite_store = \"m\"\ninline_max_bytes = 0\n[attachments.stores.m]\nkind = \"local\"\npath = \"{}\"\nshared_filesystem = {shared}\n",
        dir.display()
    );
    Arc::new(
        AttachmentRuntime::open(AttachmentConfig::from_toml(&text).unwrap())
            .await
            .unwrap(),
    )
}

async fn register_live_nodes(h: &Harness, n: usize) {
    let mut tx = h.begin().await.unwrap();
    for i in 0..n {
        tx.execute(
            &format!(
                "INSERT INTO erp_node (id, name, host, pid, role, dev, digest, config_digest, \
                 build_digest, addons_digest, build_label, registered_at, state, heartbeat_at, \
                 expires_at, beat, interval_ms, lease_ms) \
                 VALUES (gen_random_uuid(), 'n{i}', 'h{i}', {}, 'app', false, \
                 '\\x{d}'::bytea, '\\x{d}'::bytea, '\\x{d}'::bytea, '\\x{d}'::bytea, 'test', now(), \
                 'ready', now(), now() + interval '1 hour', 1, 1000, 5000)",
                100 + i,
                d = "00".repeat(32)
            ),
            &[],
        )
        .await
        .unwrap();
    }
    tx.commit().await.unwrap();
}

/// A node-local store is valid for one node and refused for a second live node; a shared path
/// declared by the operator, or an s3 store, is accepted.
#[tokio::test]
async fn an_unshared_local_store_is_refused_for_a_second_live_node() {
    let dir = tempfile::tempdir().unwrap();
    for (shared, nodes, refused) in [(false, 1, false), (false, 2, true), (true, 2, false)] {
        let rt = local_runtime(shared, &dir.path().join(format!("s{shared}{nodes}"))).await;
        let h = Harness::builder([&base_users::ADDON, &erp_addon_base::ADDON])
            .attach(Arc::clone(&rt))
            .build()
            .await
            .unwrap();
        register_live_nodes(&h, nodes).await;
        let mut tx = h.begin().await.unwrap();
        let result = verify_topology(&mut tx).await;
        tx.rollback().await.unwrap();
        drop(tx);
        match (refused, result) {
            (false, Ok(())) => {}
            (true, Err(error)) => {
                assert_eq!(error.kind(), ErrorKind::Programming);
                assert_eq!(
                    error.message(),
                    "The attachment store m is a node-local directory but 2 nodes are live in this \
                     database. Every node must reach the same bytes: set \
                     attachments.stores.<id>.shared_filesystem = true on a path every node mounts, \
                     or use an s3 store."
                );
                // an upload is refused too, before any byte is written
                let env = env_of(h.orm()).await;
                let mut body: &[u8] = b"x";
                let err = Uploader::new(h.orm())
                    .stage(&env, &mut body, StageRequest::named("x.txt"))
                    .await
                    .unwrap_err();
                assert_eq!(err.kind(), ErrorKind::Programming);
            }
            (expected_refused, other) => panic!("refused={expected_refused}: {other:?}"),
        }
        h.cleanup().await.unwrap();
    }
}

/// A node that mounts another directory than the one the database registered cannot read or write.
#[tokio::test]
async fn a_node_with_another_directory_fails_its_fingerprint_check() {
    let dir = tempfile::tempdir().unwrap();
    let first = local_runtime(true, &dir.path().join("one")).await;
    let h = Harness::builder([&base_users::ADDON, &erp_addon_base::ADDON])
        .attach(Arc::clone(&first))
        .build()
        .await
        .unwrap();
    let env = env_of(h.orm()).await;
    // the first upload registers the store row (and the directory's id) through the first node
    let mut body: &[u8] = b"registered through the first node";
    Uploader::new(h.orm())
        .stage(&env, &mut body, StageRequest::named("b.txt"))
        .await
        .unwrap();
    // a second node whose runtime points at an empty directory of its own
    let other = local_runtime(true, &dir.path().join("two")).await;
    let node_b = h
        .orm()
        .rebuild(Arc::clone(h.registry()))
        .generation_attachment(erp_orm::GenerationAttachment::new(
            Arc::clone(h.registry()),
            other,
        ))
        .build()
        .unwrap();
    let env_b = env_of(&node_b).await;
    let mut body: &[u8] = b"must not be written to an unregistered directory";
    let err = Uploader::new(&node_b)
        .stage(&env_b, &mut body, StageRequest::named("c.txt"))
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Internal);
    assert!(
        err.message()
            .contains("the store directory is not the one this database registered"),
        "{}",
        err.message()
    );
    h.cleanup().await.unwrap();
}
