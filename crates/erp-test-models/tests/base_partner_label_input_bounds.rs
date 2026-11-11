//! Contact label input admission, including direct writes and effective defaults.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]
use erp_addon_base::ResPartner;
use erp_sdk::ErrorKind;
use erp_sdk::prelude::*;
use erp_test_models::base_users;
use erp_testkit::Harness;

erp_sdk::addon!(name = "test_label_input_bounds", depends = ["base"]);
#[extend(ResPartner, addon = "test_label_input_bounds")]
pub struct LabelInputFixture;
#[methods(ResPartner, addon = "test_label_input_bounds")]
impl LabelInputFixture {
    #[method(override)]
    async fn create(
        this: &RecordSet<ResPartner>,
        tx: &mut Transaction,
        values: Vec<RawVals>,
        sup: Super,
    ) -> Result<Records> {
        if let Some(expected) = this.env().context().get("fixture_expected_default_type")
            && this.env().context().get("default_type").as_ref() != Some(&expected)
        {
            return Err(Error::programming(
                "fixture: default_type was not preserved",
            ));
        }
        sup.call(tx, this.as_records(), (values,)).await
    }
}
async fn harness() -> Harness {
    Harness::builder([&base_users::ADDON, &erp_addon_base::ADDON, &ADDON])
        .build()
        .await
        .unwrap()
}
fn named(name: &str) -> RawVals {
    RawVals::new().set_cell("name", Cell::Str(name.into()))
}
fn limit_message(field: &str) -> String {
    if field == "email" {
        "Contact email input exceeds the 65536-byte limit".into()
    } else {
        format!("Contact label input res.partner.{field} exceeds the 1048576-byte limit")
    }
}
fn assert_validation(error: Error, message: &str) {
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(error.message(), message);
}

#[tokio::test]
async fn label_inputs_refuse_over_limit_create_and_write_without_partial_changes() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let model = tx.env().records("res.partner").unwrap();
    let partner = model
        .create(&mut tx, vec![named("Original")])
        .await
        .unwrap();
    let original_count = model
        .search_count(&mut tx, &Domain::TRUE, None)
        .await
        .unwrap();
    // The byte helper covers VAT separately: its existing PostgreSQL index can
    // refuse before native constraints, so MiB persistence is not a valid fixture.
    for field in ["street", "street2", "city", "zip", "email"] {
        let max = if field == "email" { 65_536 } else { 1_048_576 };
        // Non-ASCII makes the character count smaller than the byte limit.
        let over = format!("{}x", "é".repeat(max / 2));
        let error = tx
            .savepoint(async |tx: &mut Transaction| {
                partner
                    .write(
                        tx,
                        named("Changed").set_cell(field, Cell::Str(over.clone().into())),
                    )
                    .await
            })
            .await
            .unwrap_err();
        assert_validation(error, &limit_message(field));
        assert_eq!(
            partner.value(&mut tx, "name").await.unwrap(),
            Value::str("Original")
        );
        assert_eq!(partner.value(&mut tx, field).await.unwrap(), Value::False);
        let error = tx
            .savepoint(async |tx: &mut Transaction| {
                model
                    .create(
                        tx,
                        vec![
                            named("Valid batch row"),
                            named("Invalid batch row")
                                .set_cell(field, Cell::Str(over.clone().into())),
                        ],
                    )
                    .await
            })
            .await
            .unwrap_err();
        assert_validation(error, &limit_message(field));
        assert_eq!(
            model
                .search_count(&mut tx, &Domain::TRUE, None)
                .await
                .unwrap(),
            original_count
        );
    }
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn address_inputs_admit_exact_utf8_byte_limit_and_empty_values() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let model = tx.env().records("res.partner").unwrap();
    let exact = "é".repeat(1_048_576 / 2);
    // VAT has an independently tighter PostgreSQL index limit; do not describe
    // a virtual/cache-only fixture as proof that a MiB indexed value can persist.
    for field in ["street", "street2", "city", "zip"] {
        let partner = model
            .create(
                &mut tx,
                vec![named("Boundary").set_cell(field, Cell::Str(exact.clone().into()))],
            )
            .await
            .unwrap();
        let Value::Str(actual) = partner.value(&mut tx, field).await.unwrap() else {
            panic!("expected text")
        };
        assert_eq!(actual.len(), exact.len());
        partner
            .write(
                &mut tx,
                RawVals::new().set_cell(field, Cell::Str("".into())),
            )
            .await
            .unwrap();
        partner
            .write(&mut tx, RawVals::new().set_cell(field, Cell::Null))
            .await
            .unwrap();
        assert_eq!(partner.value(&mut tx, field).await.unwrap(), Value::False);
    }
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn email_defaults_and_candidate_limits_are_admitted_before_operation_success() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let model = tx.env().records("res.partner").unwrap();
    let partner = model.create(&mut tx, vec![named("Email")]).await.unwrap();
    let exact = "x".repeat(65_536);
    partner
        .write(
            &mut tx,
            RawVals::new().set_cell("email", Cell::Str(exact.into())),
        )
        .await
        .unwrap();
    let formatted = partner.value(&mut tx, "email_formatted").await.unwrap();
    let Value::Str(formatted) = formatted else {
        panic!("expected formatted email")
    };
    assert!(formatted.starts_with("\"Email\" <@"));
    let candidates = (0..256)
        .map(|n| format!("a{n}@example.org"))
        .collect::<Vec<_>>()
        .join(",");
    partner
        .write(
            &mut tx,
            RawVals::new().set_cell("email", Cell::Str(candidates.clone().into())),
        )
        .await
        .unwrap();
    let over = format!("{candidates},extra@example.org");
    let error = tx
        .savepoint(async |tx: &mut Transaction| {
            partner
                .write(
                    tx,
                    RawVals::new().set_cell("email", Cell::Str(over.clone().into())),
                )
                .await
        })
        .await
        .unwrap_err();
    assert_validation(error, "Contact email input exceeds the 256-address limit");
    let defaults = model.with_context([("default_email", Value::str("x".repeat(65_537)))]);
    let before = model
        .search_count(&mut tx, &Domain::TRUE, None)
        .await
        .unwrap();
    let error = tx
        .savepoint(async |tx: &mut Transaction| defaults.create(tx, vec![named("Default")]).await)
        .await
        .unwrap_err();
    assert_validation(error, "Contact email input exceeds the 65536-byte limit");
    assert_eq!(
        model
            .search_count(&mut tx, &Domain::TRUE, None)
            .await
            .unwrap(),
        before
    );
    assert_eq!(
        partner.value(&mut tx, "email").await.unwrap(),
        Value::str(candidates)
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn name_create_preserves_falsy_default_type_context() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    for value in [
        Value::False,
        Value::Bool(false),
        Value::str(""),
        Value::Int(0),
    ] {
        let model = tx.env().model::<ResPartner>().unwrap().with_context([
            ("default_type", value.clone()),
            ("fixture_expected_default_type", value),
        ]);
        if matches!(
            model.env().context().get("default_type"),
            Some(Value::Int(0))
        ) {
            // Preserving context does not bypass ordinary selection conversion.
            // The observer above would return Programming if the key was removed.
            let error = tx
                .savepoint(async |tx: &mut Transaction| {
                    model.name_create(tx, "Falsy default").await
                })
                .await
                .unwrap_err();
            assert_validation(error, "Wrong value for res.partner.type: '0'");
        } else {
            let (_, label) = model.name_create(&mut tx, "Falsy default").await.unwrap();
            assert_eq!(label.as_ref(), "Falsy default");
        }
    }
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn vat_constraint_byte_boundary_is_independent_of_postgresql_index_storage() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    // Only this disposable Harness database is changed. Its index would reject
    // MiB values before model validation. This fixture proves the native bound,
    // not that a normal indexed installation can store MiB VAT values.
    tx.execute("DROP INDEX res_partner__vat_index", &[])
        .await
        .unwrap();
    let model = tx.env().records("res.partner").unwrap();
    let exact = "é".repeat(1_048_576 / 2);
    let partner = model
        .create(
            &mut tx,
            vec![named("VAT boundary").set_cell("vat", Cell::Str(exact.clone().into()))],
        )
        .await
        .unwrap();
    tx.flush().await.unwrap();
    let Value::Str(actual) = partner.value(&mut tx, "vat").await.unwrap() else {
        panic!("expected VAT text")
    };
    assert_eq!(actual.len(), exact.len());
    let over = format!("{exact}x");
    let error = tx
        .savepoint(async |tx: &mut Transaction| {
            partner
                .write(
                    tx,
                    RawVals::new().set_cell("vat", Cell::Str(over.clone().into())),
                )
                .await
        })
        .await
        .unwrap_err();
    assert_validation(error, &limit_message("vat"));
    let error = tx
        .savepoint(async |tx: &mut Transaction| {
            model
                .create(
                    tx,
                    vec![named("VAT over").set_cell("vat", Cell::Str(over.into()))],
                )
                .await
        })
        .await
        .unwrap_err();
    assert_validation(error, &limit_message("vat"));
    assert_eq!(
        partner.value(&mut tx, "vat").await.unwrap(),
        Value::str(exact)
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn rejected_batch_and_parent_address_changes_leave_committed_rows_unchanged() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let model = tx.env().records("res.partner").unwrap();
    let parent = model
        .create(
            &mut tx,
            vec![named("Parent").set_cell("street", Cell::Str("Before".into()))],
        )
        .await
        .unwrap();
    let child = model
        .create(
            &mut tx,
            vec![named("Child").set_cell("parent_id", Cell::Id(parent.ids()[0]))],
        )
        .await
        .unwrap();
    let parent_id = parent.ids()[0];
    let child_id = child.ids()[0];
    tx.commit().await.unwrap();
    let mut tx = h.begin().await.unwrap();
    let model = tx.env().records("res.partner").unwrap();
    let both = model.browse([parent_id, child_id]).unwrap();
    let over = "x".repeat(1_048_577);
    let error = tx
        .savepoint(async |tx: &mut Transaction| {
            both.write(
                tx,
                RawVals::new().set_cell("street", Cell::Str(over.clone().into())),
            )
            .await
        })
        .await
        .unwrap_err();
    assert_validation(error, &limit_message("street"));
    let parent = model.browse([parent_id]).unwrap();
    let error = tx
        .savepoint(async |tx: &mut Transaction| {
            parent
                .write(
                    tx,
                    RawVals::new().set_cell("street", Cell::Str(over.into())),
                )
                .await
        })
        .await
        .unwrap_err();
    assert_validation(error, &limit_message("street"));
    tx.commit().await.unwrap();
    let mut tx = h.begin().await.unwrap();
    let model = tx.env().records("res.partner").unwrap();
    for id in [parent_id, child_id] {
        assert_eq!(
            model
                .browse([id])
                .unwrap()
                .value(&mut tx, "street")
                .await
                .unwrap(),
            Value::str("Before")
        );
    }
    drop(tx);
    h.cleanup().await.unwrap();
}
