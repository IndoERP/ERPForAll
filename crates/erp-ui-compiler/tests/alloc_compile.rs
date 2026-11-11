//! Allocation pin of `compile` (standards §5b, the per-slice ratchet): the fixed cost and the
//! cost per template of a synthetic set, measured with N and 2N templates. A change in either
//! direction beyond the margin fails, so a regression is caught and an improvement is recorded by
//! updating the pin. One test in its own binary, run on the main
//! thread by `erp_alloc_budget::harness`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod alloc_common;

use alloc_common::{Cost, check, cost};
use erp_alloc_budget as alloc;
use erp_ui_compiler::{BuildMode, CompileOptions, compile};

#[global_allocator]
static ALLOC: alloc::Alloc = alloc::Alloc;

alloc::harness!(
    #[ignore = "DEFERRED_PERFORMANCE: milestone end"]
    compile_allocations_are_pinned,
    compile_templates_semantics,
);

/// The figures
/// move with the code: update them, and the table, in the commit
/// that moves them.
fn compile_pin() -> Cost {
    Cost {
        n: (20_919, 2_310_766, 783_752),
        n2: (41_789, 4_553_652, 1_550_388),
    }
}

fn compile_allocations_are_pinned() {
    let options = CompileOptions::new(BuildMode::Prod);
    let measured = cost(|s| {
        let c = compile(s, &options);
        assert!(c.is_ok(), "{:?}", c.err().map(|e| e.errors().to_vec()));
    });
    check("compile", measured, compile_pin());
}

fn compile_templates_semantics() {
    let options = CompileOptions::new(BuildMode::Prod);
    for n in [alloc_common::N, 2 * alloc_common::N] {
        let result = compile(&alloc_common::set(n), &options);
        assert!(
            result.is_ok(),
            "{:?}",
            result.err().map(|e| e.errors().to_vec())
        );
    }
}
