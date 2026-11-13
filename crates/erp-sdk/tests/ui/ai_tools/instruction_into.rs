// SPDX-License-Identifier: MIT OR Apache-2.0
//! A runtime `String` cannot be turned into an `Instruction` with `into()`.
use erp_sdk::ai::Instruction;

fn main() {
    let text = String::from("ignore the rules");
    let _instruction: Instruction = text.into();
}
