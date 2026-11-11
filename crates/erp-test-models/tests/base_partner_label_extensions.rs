//! Independently authored native extension fixtures for partner label contracts.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use erp_addon_base::ResPartner;
use erp_sdk::prelude::*;
use erp_test_models::base_users;
use erp_testkit::Harness;

erp_sdk::addon!(name = "test_partner_label_extensions", depends = ["base"]);

#[extend(ResPartner, addon = "test_partner_label_extensions")]
pub struct PartnerLabelExtension;

#[methods(ResPartner, addon = "test_partner_label_extensions")]
impl PartnerLabelExtension {
    #[method(override)]
    async fn _complete_name_displayed_types(
        this: &RecordSet<ResPartner>,
        tx: &mut Transaction,
        sup: Super,
    ) -> Result<Vec<String>> {
        let mut types: Vec<String> = sup.call(tx, this.as_records(), ()).await?;
        match this.env().context().get("fixture_bad_types") {
            Some(Value::Str(mode)) if mode.as_ref() == "unknown" => types.push("bogus".to_owned()),
            Some(Value::Str(mode)) if mode.as_ref() == "duplicate" => {
                types.push("other".to_owned())
            }
            _ => {}
        }
        Ok(types)
    }

    #[method(override)]
    async fn _get_complete_name(
        this: &RecordSet<ResPartner>,
        tx: &mut Transaction,
        sup: Super,
    ) -> Result<Vec<Arc<str>>> {
        let mut labels: Vec<Arc<str>> = sup.call(tx, this.as_records(), ()).await?;
        if this
            .env()
            .context()
            .get("fixture_extra_label")
            .is_some_and(|value| value.is_truthy())
        {
            labels.push(Arc::from("extra"));
        }
        if this
            .env()
            .context()
            .get("fixture_phone_label")
            .is_some_and(|value| value.is_truthy())
        {
            let phones = this.mapped(tx, ResPartner::PHONE).await?;
            for (label, phone) in labels.iter_mut().zip(phones) {
                if let Some(phone) = phone {
                    *label = Arc::from(format!("{label} [{phone}]"));
                }
            }
        }
        Ok(labels)
    }

    // The added dependency and context keys must merge with the base compute's
    // declarations. Calling super leaves the base's actual output construction
    // in place, so the test cannot pass from a replacement compute alone.
    #[compute(
        override,
        depends = ["phone"],
        depends_context = ["fixture_phone_label", "fixture_bad_types", "fixture_extra_label"]
    )]
    async fn _compute_display_name(
        this: &RecordSet<ResPartner>,
        tx: &mut Transaction,
        out: &mut Assign<'_, ResPartner>,
        sup: ComputeSuper,
    ) -> Result<()> {
        sup.call(tx, this.as_records(), out).await
    }
}

async fn harness() -> Harness {
    Harness::builder([&base_users::ADDON, &erp_addon_base::ADDON, &ADDON])
        .build()
        .await
        .unwrap()
}

async fn create(tx: &mut Transaction, name: &str, parent: Option<i64>) -> RecordSet<ResPartner> {
    let model = tx.env().model::<ResPartner>().unwrap();
    let mut values = model.vals().set(ResPartner::NAME, name);
    if let Some(parent) = parent {
        values = values.set(
            ResPartner::PARENT_ID,
            Some(RecId::from_real(parent).unwrap()),
        );
    }
    model.create(tx, vec![values]).await.unwrap()
}

#[tokio::test]
async fn native_label_helpers_support_super_batch_order_and_empty_input() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let root = create(&mut tx, "Root", None).await;
    let child = create(&mut tx, "Child", Some(root.ids()[0])).await;
    let reordered = tx
        .env()
        .model::<ResPartner>()
        .unwrap()
        .browse([child.ids()[0], root.ids()[0]])
        .unwrap();
    let labels: Vec<Arc<str>> =
        erp_orm::call_method(&mut tx, reordered.as_records(), "_get_complete_name", ())
            .await
            .unwrap();
    assert_eq!(
        labels.iter().map(AsRef::as_ref).collect::<Vec<&str>>(),
        vec!["Root, Child", "Root"]
    );
    let empty = reordered.browse([]).unwrap();
    let labels: Vec<Arc<str>> =
        erp_orm::call_method(&mut tx, empty.as_records(), "_get_complete_name", ())
            .await
            .unwrap();
    assert!(labels.is_empty());
    let types: Vec<String> = erp_orm::call_method(
        &mut tx,
        root.as_records(),
        "_complete_name_displayed_types",
        (),
    )
    .await
    .unwrap();
    assert_eq!(types, vec!["invoice", "delivery", "other"]);
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn native_display_override_merges_data_and_context_dependencies() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let root = create(&mut tx, "Root", None).await;
    let child = create(&mut tx, "Child", Some(root.ids()[0])).await;
    child.set(&mut tx, ResPartner::PHONE, "111").await.unwrap();
    child
        .set(&mut tx, ResPartner::EMAIL, "child@example.com")
        .await
        .unwrap();
    let dependencies = tx.field_depends("res.partner", "display_name").unwrap();
    assert!(
        dependencies.contains(&"complete_name"),
        "base dependency retained: {dependencies:?}"
    );
    assert!(
        dependencies.contains(&"phone"),
        "override dependency added: {dependencies:?}"
    );
    let ordinary = child.with_context([("show_email", Value::Bool(true))]);
    let extended = ordinary.with_context([("fixture_phone_label", Value::Bool(true))]);
    assert_eq!(
        ordinary
            .get(&mut tx, ResPartner::DISPLAY_NAME)
            .await
            .unwrap()
            .as_deref(),
        Some("Root, Child <child@example.com>")
    );
    assert_eq!(
        extended
            .get(&mut tx, ResPartner::DISPLAY_NAME)
            .await
            .unwrap()
            .as_deref(),
        Some("Root, Child [111] <child@example.com>")
    );
    assert_eq!(
        ordinary
            .get(&mut tx, ResPartner::DISPLAY_NAME)
            .await
            .unwrap()
            .as_deref(),
        Some("Root, Child <child@example.com>")
    );
    child.set(&mut tx, ResPartner::PHONE, "222").await.unwrap();
    child
        .set(&mut tx, ResPartner::EMAIL, "changed@example.com")
        .await
        .unwrap();
    assert_eq!(
        extended
            .get(&mut tx, ResPartner::DISPLAY_NAME)
            .await
            .unwrap()
            .as_deref(),
        Some("Root, Child [222] <changed@example.com>")
    );
    let hidden = extended.with_context([("partner_display_name_hide_company", Value::Bool(true))]);
    assert_eq!(
        hidden
            .get(&mut tx, ResPartner::DISPLAY_NAME)
            .await
            .unwrap()
            .as_deref(),
        Some("Child [222] <changed@example.com>")
    );
    assert_eq!(
        extended
            .get(&mut tx, ResPartner::COMPLETE_NAME)
            .await
            .unwrap()
            .as_deref(),
        Some("Root, Child")
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn helper_output_unknown_and_duplicate_type_keys_fail_loudly() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let root = create(&mut tx, "Root", None).await;
    for (mode, message) in [
        (
            "unknown",
            "res.partner._complete_name_displayed_types: unknown type 'bogus'",
        ),
        (
            "duplicate",
            "res.partner._complete_name_displayed_types: duplicate type 'other'",
        ),
    ] {
        let record = root.with_context([("fixture_bad_types", Value::str(mode))]);
        let error = tx
            .savepoint(async |tx: &mut Transaction| record.get(tx, ResPartner::DISPLAY_NAME).await)
            .await
            .unwrap_err();
        assert_eq!(error.kind(), erp_sdk::ErrorKind::Programming);
        assert_eq!(error.message(), message);
    }
    assert_eq!(
        root.get(&mut tx, ResPartner::DISPLAY_NAME)
            .await
            .unwrap()
            .as_deref(),
        Some("Root")
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn helper_output_cardinality_is_checked_before_assigning_display_names() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let root = create(&mut tx, "Root", None).await;
    let extra = root.with_context([("fixture_extra_label", Value::Bool(true))]);
    let error = tx
        .savepoint(async |tx: &mut Transaction| extra.get(tx, ResPartner::DISPLAY_NAME).await)
        .await
        .unwrap_err();
    assert_eq!(error.kind(), erp_sdk::ErrorKind::Programming);
    assert_eq!(
        error.message(),
        "res.partner._get_complete_name: expected 1 labels, got 2"
    );
    assert_eq!(
        root.get(&mut tx, ResPartner::DISPLAY_NAME)
            .await
            .unwrap()
            .as_deref(),
        Some("Root")
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn formatted_display_skips_complete_name_helper_errors() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let root = create(&mut tx, "Root", None).await;
    let flagged = root.with_context([("fixture_extra_label", Value::Bool(true))]);
    let formatted = flagged.with_context([("formatted_display_name", Value::Bool(true))]);
    assert_eq!(
        formatted
            .get(&mut tx, ResPartner::DISPLAY_NAME)
            .await
            .unwrap()
            .as_deref(),
        Some("Root")
    );
    let error = tx
        .savepoint(async |tx: &mut Transaction| flagged.get(tx, ResPartner::DISPLAY_NAME).await)
        .await
        .unwrap_err();
    assert_eq!(error.kind(), erp_sdk::ErrorKind::Programming);
    assert_eq!(
        error.message(),
        "res.partner._get_complete_name: expected 1 labels, got 2"
    );
    drop(tx);
    h.cleanup().await.unwrap();
}
