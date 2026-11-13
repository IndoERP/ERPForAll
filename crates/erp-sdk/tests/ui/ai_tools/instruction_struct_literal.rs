// SPDX-License-Identifier: MIT OR Apache-2.0
//! An `Instruction` cannot be built with a struct literal.
use erp_sdk::ai::Instruction;

fn build() -> Instruction {
    Instruction { pieces: Vec::new() }
}

fn main() {
    let _instruction = build();
}
