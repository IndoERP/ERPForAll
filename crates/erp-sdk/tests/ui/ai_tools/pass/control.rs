// SPDX-License-Identifier: MIT OR Apache-2.0
//! Control for the compile-fail cases next to this directory: the same imports build both
//! types through their public constructors, so a failing case never fails on an import.
use erp_sdk::ai::{DataSource, Instruction, LabelledData, ToolName};

fn main() {
    let instruction = Instruction::from_static("You answer questions about ")
        .then_integer(3)
        .then_static(" records; read only: ")
        .then_flag(true);
    assert_eq!(
        instruction.render(),
        "You answer questions about 3 records; read only: true"
    );

    let tool = ToolName::new("search_read").expect("valid tool name");
    let source =
        DataSource::new(tool, Some("res.partner".to_owned()), vec![7, 9]).expect("valid source");
    let data = LabelledData::new(source, serde_json::json!({"name": "Azure Interior"}));
    assert_eq!(data.source().ids(), &[7, 9]);
}
