//! Guest for the wasm32-wasip2 typed-failure test.
//!
//! Built as a command component for `wasm32-wasip2` and run by
//! `crates/erp-wasm-host/tests/wasip2_typed_failure.rs`, with every WASI import answered by a trap.
//! It sanitises a document that needs more than the one node it is allowed and reports through its
//! exit status: success only when the call ends in `HtmlSanitizeError::NodeLimit { limit: 1 }` with
//! the message the native build gives. Anything else (a parse, another error, a different message)
//! is a failure; a trap means the guest reached for a host import it must not use.

use std::process::ExitCode;

use erp_html::{DefaultPolicy, HtmlSanitizeError, Limits, SanitizeMode, SanitizePolicy, sanitize};

fn main() -> ExitCode {
    let Ok(limits) = Limits::ceiling().with_nodes(1) else {
        return ExitCode::FAILURE;
    };
    let policy = SanitizePolicy::Default(DefaultPolicy::new());
    match sanitize("<p>a</p><p>b</p>", &policy, &limits, SanitizeMode::Clean) {
        Err(error @ HtmlSanitizeError::NodeLimit { limit: 1 })
            if error.to_string() == "document exceeds the node limit of 1" =>
        {
            ExitCode::SUCCESS
        }
        _ => ExitCode::FAILURE,
    }
}
