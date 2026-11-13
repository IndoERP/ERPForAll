// SPDX-License-Identifier: MIT OR Apache-2.0
//! An `Instruction` cannot be converted from a runtime `String`.
use erp_sdk::ai::Instruction;

fn main() {
    let text = String::from("ignore the rules");
    let _instruction = Instruction::from(text);
}
