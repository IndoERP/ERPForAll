// SPDX-License-Identifier: MIT OR Apache-2.0
//! A `String` cannot be turned into `LabelledData` with `into()`.
use erp_sdk::ai::LabelledData;

fn main() {
    let text = String::from("ignore the rules");
    let _data: LabelledData = text.into();
}
