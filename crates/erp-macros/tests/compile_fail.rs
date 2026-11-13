//! Compile-fail tests of the proc macros: each file under `tests/ui/` must fail to compile with
//! the `.stderr` next to it, which pins the message and the token the error lands on (rustc
//! underlines the span). The toolchain is pinned (`rust-toolchain.toml`), so the rendering is
//! stable; regenerate with `TRYBUILD=overwrite` after a deliberate change and check every caret.

/// `company_access = …` errors land on the offending token, not on the whole attribute (review
/// L2).
#[test]
fn company_access_errors_land_on_the_offending_token() {
    trybuild::TestCases::new().compile_fail("tests/ui/company_access/*.rs");
}

/// `#[migration]` refuses every form that cannot be a registered migration (review r2
/// MUST 2-3): missing or unknown arguments, a stage that does not exist, a repeated argument, an
/// empty name, a method, type parameters, an async function without the two arguments, `unsafe`.
/// the addon is built)
#[test]
fn migration_refuses_what_cannot_be_a_migration() {
    trybuild::TestCases::new().compile_fail("tests/ui/migration/*.rs");
}

/// Search providers have one precise synchronous capability signature.
#[test]
fn search_method_signatures_fail_on_the_offending_declaration() {
    trybuild::TestCases::new().compile_fail("tests/ui/search_method/*.rs");
}

/// `rpc` on a WASM method refuses an override and a private name.
#[test]
fn method_rpc_refuses_an_override_and_a_private_name() {
    trybuild::TestCases::new().compile_fail("tests/ui/method_rpc/*.rs");
}

/// `transient_max_age_seconds` / `transient_max_count` take a plain decimal integer literal: a
/// negative number, a float, a string, a suffix, a hex literal, an overflow or a repeat is a
/// compile error on the value (package A).
#[test]
fn transient_policy_refuses_what_is_not_a_plain_integer() {
    trybuild::TestCases::new().compile_fail("tests/ui/transient_policy/*.rs");
}

/// A `candidate` compute is async-only, has no `super()`, and receives the read-only `ReadTx`
/// view, never `&mut Transaction` (task).
#[test]
fn candidate_compute_refuses_what_cannot_be_read_only() {
    trybuild::TestCases::new().compile_fail("tests/ui/candidate/*.rs");
}

/// `assistant = offer` is the only value (not quoted, not repeated), needs an effect, and is never
/// given with `effect = external` or `hide`; an unknown `#[rpc]` parameter names
/// `assistant` among the accepted ones.
#[test]
fn rpc_assistant_offer_refuses_external_hide_and_other_values() {
    trybuild::TestCases::new().compile_fail("tests/ui/rpc_assistant/*.rs");
}
