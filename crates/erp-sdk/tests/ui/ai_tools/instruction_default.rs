// SPDX-License-Identifier: MIT OR Apache-2.0
//! An `Instruction` has no `Default`.
use erp_sdk::ai::Instruction;

fn main() {
    let _instruction = Instruction::default();
}
