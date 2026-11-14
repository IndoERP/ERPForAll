// SPDX-License-Identifier: MIT OR Apache-2.0
use erp_sdk::BigInt;
use erp_sdk::prelude::*;
#[model(name = "test.bigint.correct", addon = "test_bigint")]
pub struct Correct {
    #[field(bigint)]
    pub counter: BigInt,
    #[field(bigint)]
    pub optional: Option<BigInt>,
}
fn main() {}
