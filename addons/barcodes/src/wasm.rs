// SPDX-License-Identifier: MIT OR Apache-2.0
//! Browser adapter; all validation and parsing belong to the shared Rust engine.
use crate::engine::Nomenclature;
use wasm_bindgen::prelude::*;

/// An immutable compiled nomenclature for one browser snapshot.
#[wasm_bindgen]
pub struct BarcodeParser {
    inner: Nomenclature,
}
#[wasm_bindgen]
impl BarcodeParser {
    /// Compile strict snapshot JSON. Errors carry the same classification as native parsing.
    #[wasm_bindgen(constructor)]
    pub fn new(snapshot: &str) -> Result<BarcodeParser, JsValue> {
        Nomenclature::from_json(snapshot)
            .map(|inner| Self { inner })
            .map_err(wire_error)
    }
    /// Parse a scan through the common engine and return result JSON.
    pub fn parse(&self, code: &str) -> Result<String, JsValue> {
        self.inner.parse_json(code).map_err(wire_error)
    }
}
fn wire_error(error: crate::engine::BarcodeError) -> JsValue {
    JsValue::from_str(&format!("{}: {}", error.kind(), error))
}
