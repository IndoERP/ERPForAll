//! `<menuitem>` and `<template>` in an install without addon `base`: the loader names
//! what to add. In a file of its own because a test binary that links `erp_addon_base` composes
//! `base`'s declarations into every registry (see `load_schema_records_specs.rs`): never import
//! `erp_addon_base` here, or the "missing model" cases below stop being missing.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use std::sync::Arc;

use erp_data::load::{FileContext, LoadError, LoadErrorKind, LoadSession, NoFiles, SessionOptions};
use erp_data::{ConvertMode, XmlDataReader};
use erp_sdk::Transaction;
use erp_test_models::base_users;
use erp_testkit::Harness;

/// A registry without the UI models: what an install without `base` has.
async fn harness_without_ui_models() -> Harness {
    Harness::builder([&base_users::ADDON, &erp_data::model_data::ADDON])
        .with_decls([erp_data::ir_model_data_decl()])
        .unlink_hook(erp_data::model_data::UNLINK_HOOK)
        .build()
        .await
        .expect("harness")
}

fn session() -> LoadSession {
    LoadSession::new(SessionOptions::new("20.0", Arc::new(NoFiles)))
}

async fn init(session: &mut LoadSession, tx: &mut Transaction, xml: &str) -> Result<(), LoadError> {
    let file = FileContext::new("m", "data/f.xml", ConvertMode::Init);
    let mut ops = XmlDataReader::new(xml, "data/f.xml", "m", false);
    session.load_file(tx, &file, &mut ops).await
}

#[tokio::test]
async fn a_menuitem_without_the_menu_model_names_the_task_that_brings_it() {
    let h = harness_without_ui_models().await;
    let mut tx = h.begin().await.unwrap();
    let mut session = session();
    let err = init(
        &mut session,
        &mut tx,
        "<odoo><menuitem id=\"mn\" name=\"Menu\"/></odoo>",
    )
    .await
    .unwrap_err();
    assert!(matches!(
        err.kind(),
        LoadErrorKind::ModelArrivesLater { .. }
    ));
    assert_eq!(
        err.to_string(),
        "while parsing data/f.xml:1, somewhere inside\n<menuitem id=\"mn\">\n\
         <menuitem> needs model 'ir.ui.menu', which arrives with addon base: add 'base' to the \
         module's depends"
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn a_template_without_the_view_model_names_the_task_that_brings_it() {
    let h = harness_without_ui_models().await;
    let mut tx = h.begin().await.unwrap();
    let mut session = session();
    let err = init(
        &mut session,
        &mut tx,
        "<odoo><template id=\"t\"><p/></template></odoo>",
    )
    .await
    .unwrap_err();
    assert!(matches!(
        err.kind(),
        LoadErrorKind::ModelArrivesLater { .. }
    ));
    assert_eq!(
        err.to_string(),
        "while parsing data/f.xml:1, somewhere inside\n<template id=\"t\">\n\
         <template> needs model 'ir.ui.view', which arrives with addon base"
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}
