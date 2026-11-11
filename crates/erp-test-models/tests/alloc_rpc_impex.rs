//! Direct JSON-2 impex binder allocation measurement, not the whole RpcService path.
//! Request parsing, transaction opening/commit and response serialization are outside the
//! window. The binder, ORM operation and construction of its Value result are inside.
//! Driver batch blocks are counted exactly; bytes remain raw (no driver-byte estimate).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stdout,
    clippy::cast_precision_loss,
    reason = "allocation measurement tests"
)]

#[path = "support/currency_fixture.rs"]
mod currency_fixture;

use erp_fields::{Value, ValueMap};
use erp_orm::rpc::{BindError, Exposure, RpcArgs};
use erp_test_models::{base_users, test_impex};
use erp_testkit::Harness;
use erp_testkit::alloc;
use erp_testkit::driver::DriverBlocks;

#[global_allocator]
static ALLOC: alloc::Alloc = alloc::Alloc;

alloc::harness!(
    #[ignore = "DEFERRED_PERFORMANCE: owner milestone-end policy; semantic twin runs in ordinary CI"]
    impex_binders_allocation_measurement,
    impex_binders_semantics
);

const WARM: usize = 10;
const MEASURED: usize = 10;

#[derive(Clone, Copy, Debug)]
enum Kind {
    Load,
    Export,
    Templates,
    MalformedLoad,
}

impl Kind {
    fn method(self) -> &'static str {
        match self {
            Self::Load | Self::MalformedLoad => "load",
            Self::Export => "export_data",
            Self::Templates => "get_import_templates",
        }
    }

    // Initial ratchet from revision 504abd2b, exact driver batch blocks removed. No slack.
    fn pinned_blocks(self) -> u64 {
        match self {
            Self::Load => 251,
            Self::Export => 66,
            Self::Templates => 2,
            Self::MalformedLoad => 5,
        }
    }

    fn args(self) -> RpcArgs {
        let mut args = ValueMap::new();
        match self {
            Self::Load | Self::MalformedLoad => {
                args.insert("fields".into(), Value::list([Value::str("value")]));
                let row = match self {
                    Self::MalformedLoad => Value::Int(7),
                    _ => Value::list([Value::str("42")]),
                };
                args.insert("data".into(), Value::list([row]));
            }
            Self::Export => {
                args.insert(
                    "fields_to_export".into(),
                    Value::list([Value::str("value")]),
                );
            }
            Self::Templates => {}
        }
        RpcArgs::new(args)
    }

    fn check(self, result: Result<Value, BindError>) {
        match self {
            Self::MalformedLoad => {
                let error = result.unwrap_err();
                let BindError::Call(error) = error else {
                    panic!("expected call validation error")
                };
                assert_eq!(error.kind(), erp_fields::ErrorKind::Validation);
                assert_eq!(error.message(), "load row 0 must be an array");
            }
            Self::Templates => assert_eq!(result.unwrap(), Value::list([])),
            Self::Export => {
                let Value::Map(map) = result.unwrap() else {
                    panic!("export result map")
                };
                assert_eq!(map.len(), 1);
                assert_eq!(map["datas"], Value::list([Value::list([Value::Int(42)])]));
            }
            Self::Load => {
                let Value::Map(map) = result.unwrap() else {
                    panic!("load result map")
                };
                assert_eq!(map.len(), 3);
                assert_eq!(map["messages"], Value::list([]));
                assert_eq!(map["nextrow"], Value::Int(0));
                let Value::List(ids) = &map["ids"] else {
                    panic!("load ids list")
                };
                assert_eq!(ids.len(), 1);
                assert!(matches!(ids[0], Value::Int(id) if id > 0));
            }
        }
    }
}

fn impex_binders_allocation_measurement() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let h = rt.block_on(async {
        Harness::builder([
            &base_users::ADDON,
            &test_impex::ADDON,
            &erp_data::model_data::ADDON,
        ])
        .with_static_decls(crate::currency_fixture::decls())
        .with_decls([erp_data::ir_model_data_decl()])
        .build()
        .await
        .unwrap()
    });
    let model = h.registry().model_id("export.integer").unwrap();
    for kind in [
        Kind::Load,
        Kind::Export,
        Kind::Templates,
        Kind::MalformedLoad,
    ] {
        let exposed = match h.orm().exposure().get(model, kind.method()).unwrap() {
            Exposure::Exposed(exposed) => exposed.clone(),
            other => panic!("expected exposed method: {other:?}"),
        };
        let mut tx = rt.block_on(h.begin()).unwrap();
        rt.block_on(tx.open()).unwrap();
        let id = rt.block_on(async {
            tx.query(
                "INSERT INTO export_integer (value, const) VALUES (42, 4) RETURNING id",
                &[],
            )
            .await
            .unwrap()[0]
                .get::<_, i64>(0)
        });
        let mut worst_blocks = 0.0_f64;
        let mut worst_bytes = 0_u64;
        for iteration in 0..WARM + MEASURED {
            rt.block_on(tx.invalidate_all()).unwrap();
            let records = tx.env().records("export.integer").unwrap();
            let records = if matches!(kind, Kind::Export) {
                records.browse([id]).unwrap()
            } else {
                records
            };
            let args = kind.args();
            if iteration < WARM {
                kind.check(rt.block_on(exposed.call(&mut tx, records, args)));
                continue;
            }
            let before = tx.driver_response_batches().expect("opened connection");
            let statements_before = tx.query_count();
            let (stats, result) =
                alloc::measure(|| rt.block_on(exposed.call(&mut tx, records, args)))
                    .expect("measurement");
            let batches = tx
                .driver_response_batches()
                .expect("same connection")
                .since(before)
                .expect("batch counts grow");
            let driver =
                DriverBlocks::new(rt.handle(), batches).expect("current-thread driver counts");
            let normalized = stats.total_blocks as f64 - driver.blocks();
            assert!(
                normalized >= 0.0,
                "driver blocks exceed measured blocks: {stats:?}; {driver}"
            );
            assert!(
                normalized <= kind.pinned_blocks() as f64,
                "{kind:?} used {normalized} blocks, pin {}: {stats:?}; {driver}",
                kind.pinned_blocks()
            );
            // These paths perform no I/O, so their byte counts have no driver batching term.
            match kind {
                Kind::Templates => assert!(stats.total_bytes <= 17, "{stats:?}"),
                Kind::MalformedLoad => assert!(stats.total_bytes <= 10_356, "{stats:?}"),
                Kind::Load | Kind::Export => {}
            }
            worst_blocks = worst_blocks.max(normalized);
            worst_bytes = worst_bytes.max(stats.total_bytes);
            kind.check(result);
            println!(
                "{kind:?} sample {}: {stats:?}; {driver}; normalized blocks {normalized}; statements {}",
                iteration - WARM,
                tx.query_count() - statements_before
            );
        }
        println!(
            "{kind:?}: worst normalized blocks {worst_blocks}; worst raw requested bytes {worst_bytes}; n {MEASURED}"
        );
        rt.block_on(tx.rollback()).unwrap();
    }
    rt.block_on(h.cleanup()).unwrap();
}

// Retain every binder result/error assertion without allocation sampling or warmups.
fn impex_binders_semantics() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let h = Harness::builder([
            &base_users::ADDON,
            &test_impex::ADDON,
            &erp_data::model_data::ADDON,
        ])
        .with_static_decls(crate::currency_fixture::decls())
        .with_decls([erp_data::ir_model_data_decl()])
        .build()
        .await
        .unwrap();
        let model = h.registry().model_id("export.integer").unwrap();
        for kind in [
            Kind::Load,
            Kind::Export,
            Kind::Templates,
            Kind::MalformedLoad,
        ] {
            let exposed = match h.orm().exposure().get(model, kind.method()).unwrap() {
                Exposure::Exposed(exposed) => exposed.clone(),
                other => panic!("expected exposed method: {other:?}"),
            };
            let mut tx = h.begin().await.unwrap();
            tx.open().await.unwrap();
            let id = tx
                .query(
                    "INSERT INTO export_integer (value, const) VALUES (42, 4) RETURNING id",
                    &[],
                )
                .await
                .unwrap()[0]
                .get::<_, i64>(0);
            tx.invalidate_all().await.unwrap();
            let records = tx.env().records("export.integer").unwrap();
            let records = if matches!(kind, Kind::Export) {
                records.browse([id]).unwrap()
            } else {
                records
            };
            kind.check(exposed.call(&mut tx, records, kind.args()).await);
            tx.rollback().await.unwrap();
        }
        h.cleanup().await.unwrap();
    });
}
