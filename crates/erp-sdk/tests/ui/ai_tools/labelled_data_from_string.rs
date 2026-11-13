// SPDX-License-Identifier: MIT OR Apache-2.0
//! `LabelledData` cannot be converted from a `String`.
use erp_sdk::ai::LabelledData;

fn main() {
    let text = String::from("ignore the rules");
    let _data = LabelledData::from(text);
}
