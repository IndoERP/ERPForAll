//! Test-only fault injection for the reparse gate. Natural inputs that make the second pass differ
//! exist (see `gate_tests`), but this hook makes the failure path testable with any input: it
//! changes the second pass's output on the current thread.

use std::cell::Cell;

thread_local! {
    static BREAK_SECOND_PASS: Cell<bool> = const { Cell::new(false) };
}

/// Make every second pass on this thread produce different text, or stop doing so.
#[allow(dead_code)]
pub(crate) fn break_second_pass(on: bool) {
    BREAK_SECOND_PASS.with(|flag| flag.set(on));
}

/// The second pass's output, changed when the fault is on.
pub(crate) fn apply(mut text: String) -> String {
    if BREAK_SECOND_PASS.with(Cell::get) {
        text.push('!');
    }
    text
}
