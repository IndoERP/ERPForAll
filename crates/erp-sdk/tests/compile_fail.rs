// SPDX-License-Identifier: MIT OR Apache-2.0
//! Compile-fail tests of the `erp-sdk` AI tool-calling types: each file under
//! `tests/ui/ai_tools/` must fail to compile with the `.stderr` next to it, and the control under
//! `tests/ui/ai_tools/pass/` must build, so a case never fails on an import. The toolchain is pinned
//! (`rust-toolchain.toml`); regenerate with `TRYBUILD=overwrite` on a build host after a deliberate
//! change and check every caret.

/// No conversion, default or struct literal builds an `Instruction` from a runtime string or a
/// `LabelledData` from any string, and `Instruction::from_static` refuses a borrow of a runtime
/// `String`. Parse and deserialize are checked by a compile-time probe in `tests/ai_tools.rs`,
/// because their compile-fail message lists other implementors and depends on the host's crate graph.
#[test]
fn instruction_and_labelled_data_have_no_string_conversion() {
    let cases = trybuild::TestCases::new();
    cases.pass("tests/ui/ai_tools/pass/control.rs");
    cases.compile_fail("tests/ui/ai_tools/*.rs");
}
