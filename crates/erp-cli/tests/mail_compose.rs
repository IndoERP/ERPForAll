//! Composition of the `mail.thread` mixin: `#[extend(ResPartner, inherit = [MailThread])]`, a
//! native model with `inherit = [MailThread]` and a data-only model with `inherit =
//! ["mail.thread"]` in TOML, all in one registry.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use erp_addon_mail::MailThread;
use erp_modules::{ModuleName, parse_model_file};
use erp_registry::{Overlay, Registry, intern_addon};
use erp_sdk::prelude::*;
use erp_sdk::{FieldType, ModelSpec};
use erp_testkit::Harness;

erp_sdk::addon!(name = "test_mail", depends = ["mail"]);

/// A native model that inherits the thread mixin.
#[model(
    name = "test_mail.ticket",
    description = "Mail Test Ticket",
    inherit = [MailThread],
    addon = "test_mail"
)]
pub struct Ticket {
    #[field(char)]
    pub name: Option<Arc<str>>,
}

const TOML_ADDON: &str = "test_mail_toml";
const TOML_MODELS: &str = r#"
[[model]]
name = "test_mail_toml.visit"
description = "Mail Test Visit"
inherit = ["mail.thread"]

[[model.field]]
name = "name"
type = "char"
"#;

fn overlay() -> Overlay {
    let module = ModuleName::new(TOML_ADDON).unwrap();
    let mut overlay = Overlay::new();
    overlay
        .add_addon_defs(parse_model_file(&module, "models/visit.toml", TOML_MODELS).unwrap())
        .unwrap();
    overlay
}

fn registry() -> std::sync::Arc<Registry> {
    let toml_addon = intern_addon(TOML_ADDON, &["mail"]).unwrap();
    Harness::builder([
        &erp_addon_base::ADDON,
        &erp_addon_mail::ADDON,
        &ADDON,
        toml_addon,
    ])
    .overlay(overlay())
    .registry_only()
    .unwrap()
}

/// The three relational fields the mixin contributes, checked on `spec`.
fn assert_thread_fields(spec: &ModelSpec) {
    assert!(
        spec.inherit.contains(&"mail.thread"),
        "{} inherits {:?}",
        spec.name,
        spec.inherit
    );
    let messages = spec.field("message_ids").unwrap();
    match &messages.ty {
        FieldType::One2many { comodel, inverse } => {
            assert_eq!(
                (comodel.as_str(), inverse.as_str()),
                ("mail.message", "res_id")
            );
        }
        other => panic!("{}.message_ids is {other:?}", spec.name),
    }
    assert_eq!(messages.modules, vec!["mail"]);
    let followers = spec.field("message_follower_ids").unwrap();
    match &followers.ty {
        FieldType::One2many { comodel, inverse } => {
            assert_eq!(
                (comodel.as_str(), inverse.as_str()),
                ("mail.followers", "res_id")
            );
        }
        other => panic!("{}.message_follower_ids is {other:?}", spec.name),
    }
    let partners = spec.field("message_partner_ids").unwrap();
    match &partners.ty {
        FieldType::Many2many {
            comodel, relation, ..
        } => {
            assert_eq!(comodel, "res.partner");
            assert!(
                relation.is_none(),
                "computed, no relation table: {relation:?}"
            );
        }
        other => panic!("{}.message_partner_ids is {other:?}", spec.name),
    }
    assert!(!partners.store);
    assert_eq!(partners.compute, Some("_compute_message_partner_ids"));
}

/// through `#[extend(ResPartner, inherit = [MailThread])]`; message_needaction and the status
/// fields come with later slices)
#[test]
fn res_partner_extension_composes_the_thread_mixin() {
    let registry = registry();
    let partner = registry.model("res.partner").unwrap();
    assert_thread_fields(partner);
    // base's own fields stay
    assert!(partner.field("email").is_some());
    assert!(partner.field("parent_id").is_some());
}

/// `inherit = [MailThread]` and on a data-only model declared with `inherit = ["mail.thread"]`)
#[test]
fn native_and_toml_models_compose_the_thread_mixin() {
    let registry = registry();
    assert_thread_fields(registry.model("test_mail.ticket").unwrap());
    let visit = registry.model("test_mail_toml.visit").unwrap();
    assert_thread_fields(visit);
    assert_eq!(visit.field("name").unwrap().modules, vec![TOML_ADDON]);
    // the mixin itself has no table; the models that inherit it have one each
    let mixin = registry.model("mail.thread").unwrap();
    assert!(!mixin.has_table());
    assert!(registry.model("test_mail.ticket").unwrap().has_table());
    assert!(visit.has_table());
}
