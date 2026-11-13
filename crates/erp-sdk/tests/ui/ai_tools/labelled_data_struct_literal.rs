// SPDX-License-Identifier: MIT OR Apache-2.0
//! `LabelledData` cannot be built with a struct literal.
use erp_sdk::ai::{DataSource, LabelledData};

fn build(source: DataSource, value: serde_json::Value) -> LabelledData {
    LabelledData { source, value }
}

fn main() {
    let _build = build;
}
