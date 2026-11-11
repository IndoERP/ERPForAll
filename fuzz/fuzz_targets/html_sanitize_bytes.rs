#![no_main]

//! Raw bytes to `erp_html::sanitize_bytes` under `Limits::ceiling()` and `Limits::recommended()`;
//! the invariants are in `html_invariants.rs`.

mod html_invariants;

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    html_invariants::check_bytes(data);
});
