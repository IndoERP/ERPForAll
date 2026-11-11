#![no_main]

//! Text input to `erp_html::sanitize` under `Limits::ceiling()` and `Limits::recommended()`; the
//! invariants are in `html_invariants.rs`.

mod html_invariants;

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // Only text reaches this target; the sibling target feeds raw bytes.
    if let Ok(input) = std::str::from_utf8(data) {
        html_invariants::check_str(input);
    }
});
