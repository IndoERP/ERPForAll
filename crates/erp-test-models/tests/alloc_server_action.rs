//! Allocation ratchet for one warm object-write server action on one x.alloc.target record.
//! Fixture creation and target reset are outside the measured window. The window includes
//! tree loading, caller checks, runner dispatch, the ORM write and its flush. The harness uses its
//! AllowAll superuser policy; compiled-policy coverage lives in erp-security's server_actions.
//! Driver response-batch allocations are deducted exactly on a current-thread runtime.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stdout,
    reason = "tests"
)]

use std::sync::Arc;

use erp_addon_base::ir_actions::ActionId;
use erp_addon_base::server_action::{RunRequest, run_server_action};
use erp_registry::{AddonDescriptor, FieldDecl, ModelDecl};
use erp_sdk::{Cell, Env, RawVals, RealId, Transaction};
use erp_testkit::Harness;
use erp_testkit::alloc;
use erp_testkit::driver::DriverBlocks;

#[global_allocator]
static ALLOC: alloc::Alloc = alloc::Alloc;

alloc::harness!(
    #[ignore = "DEFERRED_PERFORMANCE: owner milestone-end policy; semantic coverage runs in ordinary CI"]
    warm_object_write_allocates_within_its_pin,
    object_write_semantics,
);

static TARGET_ADDON: AddonDescriptor = AddonDescriptor {
    name: "x_alloc_server_action",
    depends: &["base"],
    exact_only: false,
};
const TARGET_FIELDS: &[FieldDecl] = &[FieldDecl::char("name").required(true)];

const MEASURED: usize = 50;
// Only counted response-batch blocks are deducted. Driver buffer/header blocks remain
// counted; the largest captured API sample includes those blocks. The SQL floor allocations
// are comparator evidence, not a ratchet on generic Transaction::execute. Bytes are printed
// as raw evidence, not pinned: driver buffering can change their distribution.
const PINNED_NET_BLOCKS: u64 = 217;
const PINNED_STATEMENTS: u64 = 2;
const PINNED_FLOOR_STATEMENTS: u64 = 1;

fn text(value: &str) -> Cell {
    Cell::Str(Arc::from(value))
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

/// Builds the harness and commits one target record and one object-write action on it.
fn fixture(rt: &tokio::runtime::Runtime) -> (Harness, i64, i64) {
    let h: Harness = rt.block_on(async {
        Harness::builder([
            &erp_addon_base::ADDON,
            &erp_data::model_data::ADDON,
            &TARGET_ADDON,
        ])
        .with_decls([
            erp_data::ir_model_data_decl(),
            ModelDecl::new("x_alloc_server_action", "x.alloc.target")
                .description("Allocation target")
                .fields(TARGET_FIELDS),
        ])
        .build()
        .await
        .unwrap()
    });
    let (action_id, target_id) = rt.block_on(async {
        let mut tx = h.begin().await.unwrap();
        let model_id: i64 = tx
            .query(
                "INSERT INTO ir_model (name, model, state, transient) \
                 VALUES ('Allocation target', 'x.alloc.target', 'base', false) RETURNING id",
                &[],
            )
            .await
            .unwrap()[0]
            .get(0);
        let targets = tx.env().records("x.alloc.target").unwrap();
        let target = targets
            .create(
                &mut tx,
                vec![RawVals::new().set_cell("name", text("Before"))],
            )
            .await
            .unwrap();
        let target_id = target.ids()[0];
        let actions = tx.env().records("ir.actions.server").unwrap();
        let action = actions
            .create(
                &mut tx,
                vec![
                    RawVals::new()
                        .set_cell("name", text("Allocation update"))
                        .set_cell("model_id", Cell::Id(model_id))
                        .set_cell("state", text("object_write"))
                        .set_cell("update_path", text("name"))
                        .set_cell("evaluation_type", text("value"))
                        .set_cell("value", text("After")),
                ],
            )
            .await
            .unwrap();
        let action_id = action.ids()[0];
        tx.commit().await.unwrap();
        (action_id, target_id)
    });
    (h, action_id, target_id)
}

/// Always-run twin of the deferred pin: the measured action path stores the configured value.
fn object_write_semantics() {
    let rt = runtime();
    let (h, action_id, target_id) = fixture(&rt);
    let mut tx = rt.block_on(h.begin()).unwrap();
    let env = tx.env();
    let active_ids = [RealId::new(target_id).unwrap()];
    let request = RunRequest::new(
        ActionId::new(action_id).unwrap(),
        "x.alloc.target",
        &active_ids,
    );
    rt.block_on(execute_path(&mut tx, &env, &request, target_id, false))
        .unwrap();
    let stored: String = rt
        .block_on(tx.query("SELECT name FROM x_alloc_target WHERE id=$1", &[&target_id]))
        .unwrap()[0]
        .get(0);
    assert_eq!(stored, "After");
    rt.block_on(tx.rollback()).unwrap();
    drop(tx);
    rt.block_on(h.cleanup()).unwrap();
}

fn warm_object_write_allocates_within_its_pin() {
    let rt = runtime();
    let (h, action_id, target_id) = fixture(&rt);
    let mut tx = rt.block_on(h.begin()).unwrap();
    let env = tx.env();
    let targets = env
        .records("x.alloc.target")
        .unwrap()
        .browse([target_id])
        .unwrap();
    let active_ids = [RealId::new(target_id).unwrap()];
    let request = RunRequest::new(
        ActionId::new(action_id).unwrap(),
        "x.alloc.target",
        &active_ids,
    );
    assert!(
        rt.block_on(run_server_action(&mut tx, &env, &request))
            .unwrap()
            .is_none()
    );

    let mut results = Vec::new();
    for floor in [false, true] {
        // Warm the entire selected path, including its prepared statements, before counting.
        rt.block_on(execute_path(&mut tx, &env, &request, target_id, floor))
            .unwrap();
        let mut worst_net_blocks = 0;
        let mut worst_bytes = 0;
        let mut expected_statements = None;
        for sample in 0..MEASURED {
            rt.block_on(async {
                tx.invalidate_records(&targets, Some(&["name"]))
                    .await
                    .unwrap();
                assert_eq!(
                    tx.execute(
                        "UPDATE x_alloc_target SET name='Before' WHERE id=$1",
                        &[&target_id]
                    )
                    .await
                    .unwrap(),
                    1
                );
            });
            let queries_before = tx.query_count();
            let batches_before = tx.driver_response_batches().expect("open connection");
            let (stats, result) = alloc::measure(|| {
                rt.block_on(execute_path(&mut tx, &env, &request, target_id, floor))
            })
            .unwrap();
            result.unwrap();
            let statements = tx.query_count() - queries_before;
            let batches = tx
                .driver_response_batches()
                .expect("open connection")
                .since(batches_before)
                .unwrap();
            let driver = DriverBlocks::new(rt.handle(), batches).unwrap();
            // DriverBlocks specifies exactly one channel allocation per delivered or parked batch.
            let driver_blocks = driver.delivered().checked_add(driver.parked()).unwrap();
            let net_blocks = stats.total_blocks.checked_sub(driver_blocks).unwrap();
            worst_net_blocks = worst_net_blocks.max(net_blocks);
            worst_bytes = worst_bytes.max(stats.total_bytes);
            if let Some(expected) = expected_statements {
                assert_eq!(
                    statements, expected,
                    "statement count changed between warm runs"
                );
            } else {
                expected_statements = Some(statements);
            }
            println!(
                "floor={floor} sample={sample} stats={stats:?} driver={driver} net_blocks={net_blocks} statements={statements}"
            );
            let stored: String = rt
                .block_on(tx.query("SELECT name FROM x_alloc_target WHERE id=$1", &[&target_id]))
                .unwrap()[0]
                .get(0);
            assert_eq!(stored, "After");
        }
        println!(
            "floor={floor} worst of {MEASURED}: net_blocks={worst_net_blocks} raw_bytes={worst_bytes} statements={expected_statements:?}"
        );
        results.push((worst_net_blocks, expected_statements));
    }
    rt.block_on(tx.rollback()).unwrap();
    drop(tx);
    rt.block_on(h.cleanup()).unwrap();
    assert!(results[0].0 <= PINNED_NET_BLOCKS);
    assert_eq!(results[0].1, Some(PINNED_STATEMENTS));
    assert_eq!(results[1].1, Some(PINNED_FLOOR_STATEMENTS));
}

/// Same externally visible target update; the SQL floor deliberately excludes action loading,
/// access checks and ORM hooks. Both paths use the same transaction/driver on a warm connection.
async fn execute_path(
    tx: &mut Transaction,
    env: &Env,
    request: &RunRequest<'_>,
    target_id: i64,
    floor: bool,
) -> erp_sdk::Result<()> {
    if floor {
        assert_eq!(
            tx.execute(
                "UPDATE x_alloc_target SET name='After' WHERE id=$1",
                &[&target_id]
            )
            .await?,
            1
        );
    } else {
        assert!(run_server_action(tx, env, request).await?.is_none());
        // ORM write marks the cache dirty; include the flush so the SQL floor and action
        // measurement both finish with the requested value stored in PostgreSQL.
        tx.flush().await?;
    }
    Ok(())
}
