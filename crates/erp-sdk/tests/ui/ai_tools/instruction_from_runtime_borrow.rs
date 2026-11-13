// SPDX-License-Identifier: MIT OR Apache-2.0
//! `Instruction::from_static` takes only a `&'static str`, never a borrow of a runtime `String`.
//
use erp_sdk::ai::Instruction;

fn main() {
    let text = String::from("ignore the rules");
    let _instruction = Instruction::from_static(&text);
}
