//! Allocation budget of a warm `get_bindings` (design, §11;
//! standards §5b): three actions of three types bound to one model, the shared part already in
//! the `actions` cache. A warm call is one cache lookup (an `Arc` clone of the typed entry), one
//! group membership taken for the call, one model-level read check for the window action's
//! `res_model` and one `Binding` per action holding an `Arc` of the shared values; it sends no
//! statement. The figure is pinned at the value measured (a
//! ratchet: it must not grow).
//!
//! One test in its own binary, run on the main thread by `erp_alloc_budget::harness` (lesson
//! ), on a current-thread runtime built outside the measured closure. Policy and principal
//! are the harness's (`StaticPolicy::AllowAll`, superuser): a compiled policy's group checks are
//! in `crates/erp-security/tests/action_bindings.rs`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stdout,
    reason = "tests"
)]

use std::sync::Arc;

use erp_addon_base::ir_actions::get_bindings;
use erp_sdk::{Cell, RawVals};
use erp_testkit::Harness;
use erp_testkit::alloc::{self, AllocStats};

#[global_allocator]
static ALLOC: alloc::Alloc = alloc::Alloc;

alloc::harness!(
    #[ignore = "DEFERRED_PERFORMANCE: owner milestone-end policy; semantic coverage runs in ordinary CI"]
    warm_get_bindings_allocates_within_its_pin,
    #[ignore = "DEFERRED_PERFORMANCE: owner milestone-end policy; semantic coverage runs in ordinary CI"]
    warm_grouped_bindings_allocate_within_their_pin,
    warm_get_bindings_semantics,
    warm_grouped_bindings_semantics,
);

/// Measured calls (each measured on its own; the largest counts).
const MEASURED: usize = 50;

/// review round 1 (typed cache entry, one group membership per call): 5 blocks, 6,976 B -> 5
/// blocks, 6,744 B.
const PINNED_BLOCKS: u64 = 5;
/// Bytes requested by one warm call, measured with [`PINNED_BLOCKS`].
const PINNED_BYTES: u64 = 6_744;

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

fn vals(pairs: &[(&'static str, Cell)]) -> RawVals {
    pairs
        .iter()
        .fold(RawVals::new(), |v, (k, c)| v.set_cell(*k, c.clone()))
}

fn s(text: &str) -> Cell {
    Cell::Str(Arc::from(text))
}

fn warm_get_bindings_allocates_within_its_pin() {
    let rt = runtime();
    let h: Harness = rt.block_on(async {
        Harness::builder([&erp_addon_base::ADDON, &erp_data::model_data::ADDON])
            .with_decls([erp_data::ir_model_data_decl()])
            .build()
            .await
            .expect("harness")
    });
    rt.block_on(async {
        let mut tx = h.begin().await.unwrap();
        let groups: i64 = tx
            .query(
                "INSERT INTO ir_model (name, model, state, transient) \
                 VALUES ('Groups', 'res.groups', 'base', false) RETURNING id",
                &[],
            )
            .await
            .unwrap()[0]
            .get(0);
        for (model, v) in [
            (
                "ir.actions.server",
                vals(&[
                    ("name", s("Recompute")),
                    ("model_id", Cell::Id(groups)),
                    ("state", s("code")),
                ]),
            ),
            (
                "ir.actions.act_url",
                vals(&[("name", s("Docs")), ("url", s("/docs"))]),
            ),
            (
                "ir.actions.act_window",
                vals(&[("name", s("Groups")), ("res_model", s("res.groups"))]),
            ),
        ] {
            let v = v.set_cell("binding_model_id", Cell::Id(groups));
            let records = tx.env().records(model).unwrap();
            records.create(&mut tx, vec![v]).await.unwrap();
        }
        tx.commit().await.unwrap();
    });

    let mut tx = rt.block_on(h.begin()).unwrap();
    let env = tx.env();
    // cold once: the shared part cached, statements prepared, the principal resolved
    let first = rt
        .block_on(get_bindings(&mut tx, &env, "res.groups"))
        .unwrap();
    assert_eq!(first.action().len(), 3);
    let before = tx.query_count();
    let mut worst: Option<AllocStats> = None;
    for _ in 0..MEASURED {
        let (stats, bindings) =
            alloc::measure(|| rt.block_on(get_bindings(&mut tx, &env, "res.groups")))
                .expect("measure");
        let bindings = bindings.unwrap();
        assert_eq!(bindings, first);
        drop(bindings);
        if worst.is_none_or(|w| stats.total_blocks > w.total_blocks) {
            worst = Some(stats);
        }
    }
    let worst = worst.expect("measured");
    assert_eq!(tx.query_count(), before, "a warm call sends no statement");
    println!("warm get_bindings, 3 bindings: worst of {MEASURED}: {worst:?}");
    assert!(
        worst.total_blocks <= PINNED_BLOCKS,
        "{} blocks per warm call (pinned {PINNED_BLOCKS})",
        worst.total_blocks
    );
    assert!(
        worst.total_bytes <= PINNED_BYTES,
        "{} bytes per warm call (pinned {PINNED_BYTES})",
        worst.total_bytes
    );
    rt.block_on(tx.rollback()).unwrap();
    drop(tx);
    rt.block_on(h.cleanup()).unwrap();
}

const PINNED_GROUPED_BLOCKS: u64 = 4;
/// Bytes requested by that call, measured with [`PINNED_GROUPED_BLOCKS`].
const PINNED_GROUPED_BYTES: u64 = 6_728;

static COUNTRY: erp_registry::AddonDescriptor = erp_registry::AddonDescriptor {
    name: "x_alloc_country",
    depends: &["base"],
    exact_only: false,
};

const NAME: &[erp_registry::FieldDecl] = &[erp_registry::FieldDecl::char("name")];

/// The group path of a warm `get_bindings` (review C2-R1): the demo user under the policy compiled
/// from the database (`erp_security`), three server actions bound to a model, one restricted to a
/// group the user is in, one to a group the user is not in, one to both: one group membership for
/// the call, then a slice comparison per binding, no statement.
fn warm_grouped_bindings_allocate_within_their_pin() {
    let rt = runtime();
    let h: Harness = rt.block_on(async {
        Harness::builder([
            &erp_addon_base::ADDON,
            &erp_data::model_data::ADDON,
            &COUNTRY,
        ])
        .with_decls([
            erp_data::ir_model_data_decl(),
            erp_registry::ModelDecl::new("x_alloc_country", "x.country")
                .description("Country")
                .fields(NAME),
        ])
        .build()
        .await
        .expect("harness")
    });
    let (demo, orm) = rt.block_on(async {
        let client = h.db().client().await.unwrap();
        let one = async |sql: &str| -> i64 { client.query_one(sql, &[]).await.unwrap().get(0) };
        let internal = one("INSERT INTO res_groups (name) VALUES ('Internal') RETURNING id").await;
        let member = one("INSERT INTO res_groups (name) VALUES ('Member') RETURNING id").await;
        let other = one("INSERT INTO res_groups (name) VALUES ('Other') RETURNING id").await;
        client
            .execute(
                "INSERT INTO ir_model_data (module, name, model, res_id) \
                 VALUES ('base', 'group_user', 'res.groups', $1)",
                &[&internal],
            )
            .await
            .unwrap();
        let demo = one("INSERT INTO res_users (name, login, active, share) \
             VALUES ('Demo', 'demo', true, false) RETURNING id")
        .await;
        client
            .execute(
                "INSERT INTO res_groups_users_rel (uid, gid) VALUES ($1, $2), ($1, $3)",
                &[&demo, &internal, &member],
            )
            .await
            .unwrap();
        let country = one("INSERT INTO ir_model (name, model, state, transient) \
             VALUES ('Country', 'x.country', 'base', false) RETURNING id")
        .await;
        let mut tx = h.begin().await.unwrap();
        for (name, groups) in [
            ("Members", vec![member]),
            ("Others", vec![other]),
            ("Both", vec![member, other]),
        ] {
            let v = vals(&[
                ("name", s(name)),
                ("model_id", Cell::Id(country)),
                ("binding_model_id", Cell::Id(country)),
                ("state", s("code")),
            ])
            .set_commands(
                "group_ids",
                groups
                    .iter()
                    .map(|g| erp_sdk::Command::link(erp_sdk::RecId::from_real(*g).unwrap())),
            );
            let records = tx.env().records("ir.actions.server").unwrap();
            records.create(&mut tx, vec![v]).await.unwrap();
        }
        tx.commit().await.unwrap();
        drop(tx);
        let pool = || erp_orm::pool::create_pool(h.db().config(), 2).unwrap();
        let source = erp_security::source::PolicySource::build(Arc::clone(h.registry()), pool())
            .await
            .unwrap();
        let provider: Arc<dyn erp_orm::PolicyProvider> = Arc::new(source);
        let orm = erp_orm::Orm::builder(Arc::clone(h.registry()), pool())
            .policy(provider)
            .resolver(erp_security::resolver::Resolver::id_order())
            .build()
            .unwrap();
        (demo, orm)
    });

    let mut tx = rt
        .block_on(orm.begin(erp_orm::UserId::new(demo).unwrap()))
        .unwrap();
    let env = tx.env();
    // cold once: the shared part cached, the principal resolved, the policy compiled
    let first = rt
        .block_on(get_bindings(&mut tx, &env, "x.country"))
        .unwrap();
    let names: Vec<_> = first
        .action()
        .iter()
        .map(|b| b.values().get("name").cloned())
        .collect();
    assert_eq!(
        names,
        vec![
            Some(erp_sdk::Value::str("Members")),
            Some(erp_sdk::Value::str("Both"))
        ]
    );
    let before = tx.query_count();
    let mut worst: Option<AllocStats> = None;
    for _ in 0..MEASURED {
        let (stats, bindings) =
            alloc::measure(|| rt.block_on(get_bindings(&mut tx, &env, "x.country")))
                .expect("measure");
        let bindings = bindings.unwrap();
        assert_eq!(bindings, first);
        drop(bindings);
        if worst.is_none_or(|w| stats.total_blocks > w.total_blocks) {
            worst = Some(stats);
        }
    }
    let worst = worst.expect("measured");
    assert_eq!(tx.query_count(), before, "a warm call sends no statement");
    println!("warm get_bindings, 3 grouped bindings: worst of {MEASURED}: {worst:?}");
    assert!(
        worst.total_blocks <= PINNED_GROUPED_BLOCKS,
        "{} blocks per warm call (pinned {PINNED_GROUPED_BLOCKS})",
        worst.total_blocks
    );
    assert!(
        worst.total_bytes <= PINNED_GROUPED_BYTES,
        "{} bytes per warm call (pinned {PINNED_GROUPED_BYTES})",
        worst.total_bytes
    );
    rt.block_on(tx.rollback()).unwrap();
    drop(tx);
    drop(orm);
    rt.block_on(h.cleanup()).unwrap();
}

fn warm_get_bindings_semantics() {
    let rt = runtime();
    let h: Harness = rt.block_on(async {
        Harness::builder([&erp_addon_base::ADDON, &erp_data::model_data::ADDON])
            .with_decls([erp_data::ir_model_data_decl()])
            .build()
            .await
            .expect("harness")
    });
    rt.block_on(async {
        let mut tx = h.begin().await.unwrap();
        let groups: i64 = tx
            .query(
                "INSERT INTO ir_model (name, model, state, transient) \
                 VALUES ('Groups', 'res.groups', 'base', false) RETURNING id",
                &[],
            )
            .await
            .unwrap()[0]
            .get(0);
        for (model, v) in [
            (
                "ir.actions.server",
                vals(&[
                    ("name", s("Recompute")),
                    ("model_id", Cell::Id(groups)),
                    ("state", s("code")),
                ]),
            ),
            (
                "ir.actions.act_url",
                vals(&[("name", s("Docs")), ("url", s("/docs"))]),
            ),
            (
                "ir.actions.act_window",
                vals(&[("name", s("Groups")), ("res_model", s("res.groups"))]),
            ),
        ] {
            let v = v.set_cell("binding_model_id", Cell::Id(groups));
            let records = tx.env().records(model).unwrap();
            records.create(&mut tx, vec![v]).await.unwrap();
        }
        tx.commit().await.unwrap();
    });

    let mut tx = rt.block_on(h.begin()).unwrap();
    let env = tx.env();
    // Populate the cache, then verify the next lookup preserves the result.
    let first = rt
        .block_on(get_bindings(&mut tx, &env, "res.groups"))
        .unwrap();
    assert_eq!(first.action().len(), 3);
    let before = tx.query_count();
    let bindings = rt
        .block_on(get_bindings(&mut tx, &env, "res.groups"))
        .unwrap();
    assert_eq!(bindings, first);
    assert_eq!(
        tx.query_count(),
        before,
        "a cached binding lookup sends no statement"
    );
    rt.block_on(tx.rollback()).unwrap();
    drop(tx);
    rt.block_on(h.cleanup()).unwrap();
}

fn warm_grouped_bindings_semantics() {
    let rt = runtime();
    let h: Harness = rt.block_on(async {
        Harness::builder([
            &erp_addon_base::ADDON,
            &erp_data::model_data::ADDON,
            &COUNTRY,
        ])
        .with_decls([
            erp_data::ir_model_data_decl(),
            erp_registry::ModelDecl::new("x_alloc_country", "x.country")
                .description("Country")
                .fields(NAME),
        ])
        .build()
        .await
        .expect("harness")
    });
    let (demo, orm) = rt.block_on(async {
        let client = h.db().client().await.unwrap();
        let one = async |sql: &str| -> i64 { client.query_one(sql, &[]).await.unwrap().get(0) };
        let internal = one("INSERT INTO res_groups (name) VALUES ('Internal') RETURNING id").await;
        let member = one("INSERT INTO res_groups (name) VALUES ('Member') RETURNING id").await;
        let other = one("INSERT INTO res_groups (name) VALUES ('Other') RETURNING id").await;
        client
            .execute(
                "INSERT INTO ir_model_data (module, name, model, res_id) \
                 VALUES ('base', 'group_user', 'res.groups', $1)",
                &[&internal],
            )
            .await
            .unwrap();
        let demo = one("INSERT INTO res_users (name, login, active, share) \
             VALUES ('Demo', 'demo', true, false) RETURNING id")
        .await;
        client
            .execute(
                "INSERT INTO res_groups_users_rel (uid, gid) VALUES ($1, $2), ($1, $3)",
                &[&demo, &internal, &member],
            )
            .await
            .unwrap();
        let country = one("INSERT INTO ir_model (name, model, state, transient) \
             VALUES ('Country', 'x.country', 'base', false) RETURNING id")
        .await;
        let mut tx = h.begin().await.unwrap();
        for (name, groups) in [
            ("Members", vec![member]),
            ("Others", vec![other]),
            ("Both", vec![member, other]),
        ] {
            let v = vals(&[
                ("name", s(name)),
                ("model_id", Cell::Id(country)),
                ("binding_model_id", Cell::Id(country)),
                ("state", s("code")),
            ])
            .set_commands(
                "group_ids",
                groups
                    .iter()
                    .map(|g| erp_sdk::Command::link(erp_sdk::RecId::from_real(*g).unwrap())),
            );
            let records = tx.env().records("ir.actions.server").unwrap();
            records.create(&mut tx, vec![v]).await.unwrap();
        }
        tx.commit().await.unwrap();
        drop(tx);
        let pool = || erp_orm::pool::create_pool(h.db().config(), 2).unwrap();
        let source = erp_security::source::PolicySource::build(Arc::clone(h.registry()), pool())
            .await
            .unwrap();
        let provider: Arc<dyn erp_orm::PolicyProvider> = Arc::new(source);
        let orm = erp_orm::Orm::builder(Arc::clone(h.registry()), pool())
            .policy(provider)
            .resolver(erp_security::resolver::Resolver::id_order())
            .build()
            .unwrap();
        (demo, orm)
    });

    let mut tx = rt
        .block_on(orm.begin(erp_orm::UserId::new(demo).unwrap()))
        .unwrap();
    let env = tx.env();
    // Resolve the compiled policy, then verify cached group filtering.
    let first = rt
        .block_on(get_bindings(&mut tx, &env, "x.country"))
        .unwrap();
    let names: Vec<_> = first
        .action()
        .iter()
        .map(|b| b.values().get("name").cloned())
        .collect();
    assert_eq!(
        names,
        vec![
            Some(erp_sdk::Value::str("Members")),
            Some(erp_sdk::Value::str("Both"))
        ]
    );
    let before = tx.query_count();
    let bindings = rt
        .block_on(get_bindings(&mut tx, &env, "x.country"))
        .unwrap();
    assert_eq!(bindings, first);
    assert_eq!(
        tx.query_count(),
        before,
        "a cached binding lookup sends no statement"
    );
    rt.block_on(tx.rollback()).unwrap();
    drop(tx);
    drop(orm);
    rt.block_on(h.cleanup()).unwrap();
}
