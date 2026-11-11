//! Compile-fail tests of the host-access bridge: each file under `tests/ui/host_access/` must fail
//! to compile with the `.stderr` next to it, which pins the message and the token the error lands
//! on. The toolchain is pinned (`rust-toolchain.toml`), so the rendering is stable; regenerate
//! with `TRYBUILD=overwrite` after a deliberate change and check every caret.
//!
//! Run with `--features host-access` (the test target requires it).

/// Verdicts and filtered views cannot be forged, and an evaluation cannot outlive or block its
/// transaction.
#[test]
fn guard_evaluation_cannot_be_forged_or_outlive_its_transaction() {
    trybuild::TestCases::new().compile_fail("tests/ui/host_access/*.rs");
}
