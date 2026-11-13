//! Whole-process HTTP + RpcService allocations, with an identical-envelope SQL fixture floor.
//! No session/auth or general relation-copy performance is inferred from this scalar fixture.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stdout,
    reason = "tests"
)]

#[path = "support/copy_serve.rs"]
mod copy_serve;
use copy_serve::{Mode, OPERATIONS, Operation};
use erp_testkit::alloc;
use serde_json::Value;

#[global_allocator]
static ALLOC: alloc::Alloc = alloc::Alloc;
alloc::harness!(
    #[ignore = "DEFERRED_PERFORMANCE: owner milestone-end policy; semantic coverage runs in ordinary CI"]
    served_copy_allocations_and_floor,
    served_copy_semantics,
);

// Exact singleton-pool driver-normalized maxima: served-normalized-c5472b22.log.
// Earlier raw-pool calibration is retained but is not this fixture's ratchet.
// Linux maxima: served-linux-7db5148b.log; unsupported platforms remain unmeasured.
const CEILINGS: Option<[[u64; 5]; 2]> = if cfg!(target_os = "macos") {
    Some([[296, 156, 232, 236, 236], [20_598, 7_404, 0, 2_550, 2_550]])
} else if cfg!(target_os = "linux") {
    Some([[292, 152, 229, 232, 232], [20_592, 7_400, 0, 2_545, 2_545]])
} else {
    None
};

fn served_copy_allocations_and_floor() {
    let mut maxima = [[0_u64; 5]; 2];
    for (size_index, rows) in [1, 1000].into_iter().enumerate() {
        for (op_index, operation) in OPERATIONS.into_iter().enumerate() {
            if operation == Operation::Name && rows != 1 {
                continue;
            }
            let mut erp_state = None;
            for mode in [Mode::Erp, Mode::Floor] {
                let server = copy_serve::start(mode, operation, rows).unwrap();
                let mut client = server.client().unwrap();
                for sample in 0..12 {
                    server.reset(&mut client).unwrap();
                    let response = client.send().unwrap();
                    assert_eq!(response.status(), 200);
                    let body: Value =
                        serde_json::from_slice(response.content().expect("JSON response")).unwrap();
                    assert_eq!(body, operation.expected(rows));
                    let statements = std::str::from_utf8(
                        response
                            .header("x-copy-statements")
                            .expect("statement count"),
                    )
                    .unwrap()
                    .parse::<u64>()
                    .unwrap();
                    let query_ceiling = if matches!(mode, Mode::Erp) {
                        [2, 1, 1, 2, 2][op_index]
                    } else {
                        1
                    };
                    assert!(statements <= query_ceiling, "statement count regressed");
                    let state = server.state(&mut client).unwrap();
                    if matches!(mode, Mode::Erp) {
                        erp_state = Some(state);
                    } else {
                        assert_eq!(Some(&state), erp_state.as_ref(), "database effects differ");
                    }
                    if sample < 4 {
                        continue;
                    }
                    server.reset(&mut client).unwrap();
                    let before = server.counters(&mut client).unwrap();
                    let stats = server.probe.measure_window(&mut client, 1).unwrap();
                    let after = server.counters(&mut client).unwrap();
                    assert_eq!(
                        before["connection"], after["connection"],
                        "physical connection changed"
                    );
                    let delivered = after["delivered"]
                        .as_u64()
                        .unwrap()
                        .checked_sub(before["delivered"].as_u64().unwrap())
                        .unwrap();
                    let parked = after["parked"]
                        .as_u64()
                        .unwrap()
                        .checked_sub(before["parked"].as_u64().unwrap())
                        .unwrap();
                    assert!(parked <= delivered);
                    let driver = delivered * erp_testkit::driver::DRIVER_MIN_BLOCKS_PER_BATCH
                        + parked
                            * (erp_testkit::driver::DRIVER_MAX_BLOCKS_PER_BATCH
                                - erp_testkit::driver::DRIVER_MIN_BLOCKS_PER_BATCH);
                    let net_blocks = stats.total_blocks.checked_sub(driver).unwrap();
                    // The measured mutating request must have the same durable effect too.
                    assert_eq!(
                        Some(&server.state(&mut client).unwrap()),
                        erp_state.as_ref()
                    );
                    if matches!(mode, Mode::Erp) {
                        maxima[size_index][op_index] = maxima[size_index][op_index].max(net_blocks);
                        if let Some(ceilings) = CEILINGS {
                            assert!(
                                net_blocks <= ceilings[size_index][op_index],
                                "{operation:?} rows={rows}: {stats:?}"
                            );
                        }
                    }
                    println!(
                        "COPY-SERVED mode={mode:?} operation={operation:?} rows={rows} sample={sample} statements={statements} delivered={delivered} parked={parked} net_blocks={net_blocks} stats={stats:?}"
                    );
                }
                drop(client);
                server.stop().unwrap();
            }
        }
    }
    println!("COPY-SERVED maxima={maxima:?} os={}", std::env::consts::OS);
    assert!(
        CEILINGS.is_some(),
        "allocation ceilings are TODO-UNMEASURED; retain calibration then pin"
    );
}

fn served_copy_semantics() {
    for rows in [1, 1000] {
        for (index, operation) in OPERATIONS.into_iter().enumerate() {
            if operation == Operation::Name && rows != 1 {
                continue;
            }
            let server = copy_serve::start(Mode::Erp, operation, rows).unwrap();
            let mut client = server.client().unwrap();
            server.reset(&mut client).unwrap();
            let before = server.counters(&mut client).unwrap();
            let response = client.send().unwrap();
            assert_eq!(response.status(), 200);
            let body: Value =
                serde_json::from_slice(response.content().expect("JSON response")).unwrap();
            assert_eq!(body, operation.expected(rows));
            let statements = std::str::from_utf8(response.header("x-copy-statements").unwrap())
                .unwrap()
                .parse::<u64>()
                .unwrap();
            // ceilings per operation; the four writes include the audit INSERT of the commit preamble,
            // `copy_data` is a write too
            let ceiling = [3, 2, 2, 3, 3][index];
            assert!(
                statements <= ceiling,
                "{operation:?} with {rows} rows: {statements} statements, ceiling {ceiling}"
            );
            let after = server.counters(&mut client).unwrap();
            assert_eq!(before["connection"], after["connection"]);
            let delivered = after["delivered"]
                .as_u64()
                .unwrap()
                .checked_sub(before["delivered"].as_u64().unwrap())
                .unwrap();
            let parked = after["parked"]
                .as_u64()
                .unwrap()
                .checked_sub(before["parked"].as_u64().unwrap())
                .unwrap();
            assert!(parked <= delivered);
            let active = operation != Operation::Archive;
            let mut expected: Vec<Value> = (1..=rows)
                .map(|id| serde_json::json!([id, format!("Name {id}"), active]))
                .collect();
            match operation {
                Operation::Copy => expected.extend(
                    (1..=rows).map(|id| serde_json::json!([rows + id, format!("Name {id}"), true])),
                ),
                Operation::Name => expected.push(serde_json::json!([rows + 1, "Created", true])),
                Operation::Data | Operation::Archive | Operation::Unarchive => {}
            }
            assert_eq!(
                server.state(&mut client).unwrap(),
                serde_json::json!(expected)
            );
            drop(client);
            server.stop().unwrap();
        }
    }
}
