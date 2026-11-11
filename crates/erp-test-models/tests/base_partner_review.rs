//! Focused regressions for the returned CORE review. No whole PARTNER case claimed.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use erp_sdk::ErrorKind;
use erp_sdk::prelude::*;
use erp_test_models::base_users;
use erp_testkit::Harness;

async fn harness() -> Harness {
    Harness::builder([&base_users::ADDON, &erp_addon_base::ADDON])
        .build()
        .await
        .unwrap()
}

#[tokio::test]
async fn language_uses_environment_and_catalog_can_be_empty() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let partners = tx.env().records("res.partner").unwrap();
    let catalog = partners
        .fields_get(&mut tx, Some(&["lang"]), Some(&["selection"]))
        .await
        .unwrap();
    assert_eq!(catalog["lang"]["selection"], Value::list([]));
    // Translated metadata validates the requested language against the installed
    // catalog. Keep the empty-catalog check above separate from the language fixture.
    tx.env()
        .records("res.lang")
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("French".into()))
                    .set_cell("code", Cell::Str("fr_FR".into()))
                    .set_cell("url_code", Cell::Str("fr".into()))
                    .set_cell("active", Cell::Bool(true))
                    .set_cell("direction", Cell::Str("ltr".into())),
            ],
        )
        .await
        .unwrap();
    let partners = partners.with_context([("lang", Value::str("fr_FR"))]);
    let p = partners
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("name", Cell::Str("French".into()))],
        )
        .await
        .unwrap();
    assert_eq!(p.value(&mut tx, "lang").await.unwrap(), Value::str("fr_FR"));
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn create_applies_context_defaults_before_contact_validation() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let partners = tx.env().records("res.partner").unwrap();
    let invoice = partners
        .with_context([("default_type", Value::str("invoice"))])
        .create(&mut tx, vec![RawVals::new()])
        .await
        .unwrap();
    assert_eq!(
        invoice.value(&mut tx, "type").await.unwrap(),
        Value::str("invoice")
    );
    let named = partners
        .with_context([("default_name", Value::str("Default name"))])
        .create(&mut tx, vec![RawVals::new()])
        .await
        .unwrap();
    assert_eq!(
        named.value(&mut tx, "name").await.unwrap(),
        Value::str("Default name")
    );
    let explicit = partners
        .with_context([("default_name", Value::str("Default name"))])
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("name", Cell::Str("Explicit".into()))],
        )
        .await
        .unwrap();
    assert_eq!(
        explicit.value(&mut tx, "name").await.unwrap(),
        Value::str("Explicit")
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn default_get_returns_only_requested_fields() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let partners = tx
        .env()
        .records("res.partner")
        .unwrap()
        .with_context([("tz", Value::str("Asia/Jakarta"))]);
    assert!(partners.default_get(&mut tx, &[]).await.unwrap().is_empty());
    let name_only = partners.default_get(&mut tx, &["name"]).await.unwrap();
    assert!(!name_only.contains_key("tz"));
    assert!(!name_only.contains_key("company_id"));
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn website_wrong_type_refuses_create_and_write_without_clearing() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let partners = tx.env().records("res.partner").unwrap();
    let invalid = RawVals::new()
        .set_cell("name", Cell::Str("Wrong type".into()))
        .set_cell("website", Cell::Int(42));
    let err = partners.create(&mut tx, vec![invalid]).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "res.partner.website: expected a string or null"
    );
    let p = partners
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Valid".into()))
                    .set_cell("website", Cell::Str("https://example.org".into())),
            ],
        )
        .await
        .unwrap();
    let err = p
        .write(&mut tx, RawVals::new().set_cell("website", Cell::Int(42)))
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "res.partner.website: expected a string or null"
    );
    assert_eq!(
        p.value(&mut tx, "website").await.unwrap(),
        Value::str("https://example.org")
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn sole_installed_language_is_default_without_context() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    tx.env()
        .records("res.lang")
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("French".into()))
                    .set_cell("code", Cell::Str("fr_FR".into()))
                    .set_cell("url_code", Cell::Str("fr".into()))
                    .set_cell("active", Cell::Bool(true)),
            ],
        )
        .await
        .unwrap();
    let p = tx
        .env()
        .records("res.partner")
        .unwrap()
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("name", Cell::Str("No language context".into()))],
        )
        .await
        .unwrap();
    assert_eq!(p.value(&mut tx, "lang").await.unwrap(), Value::str("fr_FR"));
    drop(tx);
    h.cleanup().await.unwrap();
}

mod changing_defaults {
    use erp_addon_base::ResPartner;
    use erp_sdk::prelude::*;
    pub static CALLS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    erp_sdk::addon!(name = "test_partner_changing_defaults", depends = ["base"]);
    #[extend(ResPartner, addon = "test_partner_changing_defaults")]
    pub struct PartnerDefaults;
    #[methods(ResPartner, addon = "test_partner_changing_defaults")]
    impl PartnerDefaults {
        #[method(override)]
        async fn default_get(
            this: &RecordSet<ResPartner>,
            tx: &mut Transaction,
            names: Vec<String>,
            sup: Super,
        ) -> Result<erp_sdk::ValueMap> {
            let call = CALLS.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
            let wants_name = names.iter().any(|n| n == "name");
            let mut values: erp_sdk::ValueMap = sup.call(tx, this.as_records(), (names,)).await?;
            if wants_name {
                values.insert("name".into(), Value::str(format!("Default call {call}")));
            }
            Ok(values)
        }
    }
}

#[tokio::test]
async fn create_resolves_the_actual_name_default_once() {
    let h = Harness::builder([
        &base_users::ADDON,
        &erp_addon_base::ADDON,
        &changing_defaults::ADDON,
    ])
    .build()
    .await
    .unwrap();
    let mut tx = h.begin().await.unwrap();
    changing_defaults::CALLS.store(0, std::sync::atomic::Ordering::SeqCst);
    let p = tx
        .env()
        .records("res.partner")
        .unwrap()
        .create(&mut tx, vec![RawVals::new()])
        .await
        .unwrap();
    assert_eq!(
        changing_defaults::CALLS.load(std::sync::atomic::Ordering::SeqCst),
        1
    );
    assert_eq!(
        p.value(&mut tx, "name").await.unwrap(),
        Value::str("Default call 1")
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn explicit_language_and_empty_create_do_not_require_language_read_access() {
    let h = Harness::builder([&base_users::ADDON, &erp_addon_base::ADDON])
        .policy(erp_testkit::FieldPolicy::new().deny_model("res.lang"))
        .build()
        .await
        .unwrap();
    let mut setup = h.begin().await.unwrap();
    setup
        .batch(
            "INSERT INTO res_users (id, name, login, active) VALUES (42, 'Clerk', 'clerk', true)",
        )
        .await
        .unwrap();
    setup.commit().await.unwrap();
    let mut tx = h.begin_as(42).await.unwrap();
    let partners = tx.env().records("res.partner").unwrap();
    assert!(partners.create(&mut tx, vec![]).await.unwrap().is_empty());
    let p = partners
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Explicit".into()))
                    .set_cell("lang", Cell::Str("fr_FR".into())),
            ],
        )
        .await
        .unwrap();
    assert_eq!(p.value(&mut tx, "lang").await.unwrap(), Value::str("fr_FR"));
    let child = partners
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Inherited".into()))
                    .set_cell("parent_id", Cell::Id(p.ids()[0])),
            ],
        )
        .await
        .unwrap();
    assert_eq!(
        child.value(&mut tx, "lang").await.unwrap(),
        Value::str("fr_FR")
    );
    let context_child = partners
        .with_context([("default_parent_id", Value::Int(p.ids()[0]))])
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("name", Cell::Str("Context child".into()))],
        )
        .await
        .unwrap();
    assert_eq!(
        context_child.value(&mut tx, "lang").await.unwrap(),
        Value::str("fr_FR")
    );
    drop(tx);
    h.cleanup().await.unwrap();
}
