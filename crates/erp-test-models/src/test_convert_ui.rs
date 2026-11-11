//! Probe models for the data loader tests: the language a file loads in and a JSON
//! field for list and tuple values. The UI models the `<menuitem>` / `<template>`
//! tests used to stand in for are the real ones of addon `base`.

use erp_sdk::prelude::*;

erp_sdk::addon!(name = "test_convert_ui");

/// Remembers the `lang` of the environment it was created in, so a test can see which language a
/// data file loads in: `lang_seen` is `-` when the context has none.
#[model(
    name = "test_convert_ui.lang_probe",
    description = "Test Convert UI Language Probe",
    addon = "test_convert_ui",
    rec_name = "name"
)]
pub struct LangProbe {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(char, compute = "_compute_lang_seen", store)]
    pub lang_seen: Option<Arc<str>>,
}

#[methods(LangProbe, addon = "test_convert_ui")]
impl LangProbe {
    #[compute(depends = ["name"])]
    fn _compute_lang_seen(batch: &mut Batch<'_, LangProbe>) -> Result<()> {
        let seen: Arc<str> = match batch.env().context().get("lang") {
            Some(Value::Str(lang)) => lang,
            _ => Arc::from("-"),
        };
        for i in batch.indices() {
            batch.set(i, LangProbe::LANG_SEEN, Some(Arc::clone(&seen)))?;
        }
        Ok(())
    }
}

/// A record with a JSON field, for list and tuple values.
#[model(
    name = "test_convert_ui.listed",
    description = "Test Convert UI Listed Values",
    addon = "test_convert_ui",
    rec_name = "name"
)]
pub struct Listed {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(json)]
    pub data: Option<erp_sdk::serde_json::Value>,
}

/// A record with a text field for `type="xml"` values; the loader handles the
/// field type, so the test needs no view model.
#[model(
    name = "test_convert_ui.carrier",
    description = "Test Convert UI Xml Carrier",
    addon = "test_convert_ui",
    rec_name = "name"
)]
pub struct Carrier {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(text)]
    pub arch: Option<Arc<str>>,
}
