// SPDX-License-Identifier: MIT OR Apache-2.0
use erp_sdk::prelude::*;
#[model(name = "test.bigint.wrong_id", addon = "test_bigint")]
pub struct WrongId {
    #[field(bigint)]
    pub counter: i64,
}
fn main() {}
