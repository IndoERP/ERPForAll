// SPDX-License-Identifier: MIT OR Apache-2.0
use erp_sdk::prelude::*;
#[model(name = "test.bigint.wrong", addon = "test_bigint")]
pub struct Wrong {
    #[field(bigint)]
    pub counter: i32,
}
fn main() {}
