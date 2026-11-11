//! Parity tests for `res.currency` and `res.company.currency_id`.
//! Specs:.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use erp_fields::{Cell, ErrorKind, RecId, Value};
use erp_orm::RawVals;
use erp_test_models::base_users;
use erp_testkit::Harness;

async fn base_harness() -> Harness {
    Harness::builder([&base_users::ADDON, &erp_addon_base::ADDON])
        .build()
        .await
        .expect("harness")
}

async fn create_currency(
    tx: &mut erp_orm::Transaction,
    name: &str,
    symbol: &str,
    rounding: f64,
) -> RecId {
    let mut vals = RawVals::new();
    vals.insert("name".into(), Cell::Str(name.to_string().into()));
    vals.insert("symbol".into(), Cell::Str(symbol.to_string().into()));
    vals.insert(
        "rounding".into(),
        Cell::Decimal(rounding.to_string().parse().unwrap()),
    );
    vals.insert("active".into(), Cell::Bool(true));

    let records = tx.env().records("res.currency").unwrap();
    let created = records.create(tx, vec![vals]).await.unwrap();
    created.rec_ids()[0]
}

async fn create_company(tx: &mut erp_orm::Transaction, name: &str, currency_id: RecId) -> RecId {
    let mut vals = RawVals::new();
    vals.insert("name".into(), Cell::Str(name.to_string().into()));
    vals.insert("currency_id".into(), Cell::Id(currency_id.get()));

    let records = tx.env().records("res.company").unwrap();
    let created = records.create(tx, vec![vals]).await.unwrap();
    created.rec_ids()[0]
}

#[tokio::test]
async fn spec_base_056_decimal_places_calculation_from_rounding_factor() {
    let h = base_harness().await;
    let mut tx = h.begin().await.unwrap();

    // Given: Currencies with rounding factors 0.01, 0.001, 1.0, and 0.05
    // When: Computing decimal_places
    // Then: Returns 2 for 0.01, 3 for 0.001, 0 for 1.0, and 2 for 0.05 (ceil(log10(20)) = 2)
    let c1 = create_currency(&mut tx, "C01", "$", 0.01).await;
    let c2 = create_currency(&mut tx, "C02", "$", 0.001).await;
    let c3 = create_currency(&mut tx, "C03", "$", 1.0).await;
    let c4 = create_currency(&mut tx, "C04", "$", 0.05).await;

    let currencies = tx
        .env()
        .records("res.currency")
        .unwrap()
        .browse([c1.get(), c2.get(), c3.get(), c4.get()])
        .unwrap();
    let places = currencies.values(&mut tx, "decimal_places").await.unwrap();
    assert_eq!(
        places,
        vec![Value::Int(2), Value::Int(3), Value::Int(0), Value::Int(2)]
    );
}

#[tokio::test]
async fn spec_base_057_currency_code_uniqueness_constraint() {
    let h = base_harness().await;
    let mut tx = h.begin().await.unwrap();

    // Given: Currency USD exists
    create_currency(&mut tx, "USD", "$", 0.01).await;

    // When: Creating another currency with name='USD'
    // Then: Database constraint unique (name) raises ValidationError("The currency code must be unique!")
    let mut vals = RawVals::new();
    vals.insert("name".into(), Cell::Str("USD".into()));
    vals.insert("symbol".into(), Cell::Str("$".into()));
    vals.insert("rounding".into(), Cell::Decimal("0.01".parse().unwrap()));

    let records = tx.env().records("res.currency").unwrap();
    let err = records.create(&mut tx, vec![vals]).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.message(),
        "The operation cannot be completed: The currency code must be unique!"
    );
}

#[tokio::test]
async fn spec_base_058_currency_rounding_factor_strictly_positive() {
    let h = base_harness().await;
    let mut tx = h.begin().await.unwrap();

    // Given: Attempting to create or write currency with rounding=0.0 or -0.01
    // When: Executing database write
    // Then: Constraint CHECK (rounding>0) raises ValidationError("The rounding factor must be greater than 0!")
    for invalid_rounding in [0.0, -0.01] {
        let mut vals = RawVals::new();
        vals.insert("name".into(), Cell::Str("INV".into()));
        vals.insert("symbol".into(), Cell::Str("$".into()));
        vals.insert(
            "rounding".into(),
            Cell::Decimal(invalid_rounding.to_string().parse().unwrap()),
        );

        let records = tx.env().records("res.currency").unwrap();
        let err = tx
            .savepoint(async |tx| records.create(tx, vec![vals]).await)
            .await
            .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Validation);
        assert_eq!(
            err.message(),
            "The operation cannot be completed: The rounding factor must be greater than 0!"
        );
    }

    let valid_id = create_currency(&mut tx, "VAL", "$", 0.01).await;
    let currency = tx
        .env()
        .records("res.currency")
        .unwrap()
        .browse([valid_id.get()])
        .unwrap();
    for invalid_rounding in ["0.0", "-0.01"] {
        let values =
            RawVals::new().set_cell("rounding", Cell::Decimal(invalid_rounding.parse().unwrap()));
        let error = tx
            .savepoint(async |tx| {
                currency.write(tx, values).await?;
                // Writes may remain in the ORM cache; exercise the database CHECK as well.
                tx.flush().await
            })
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Validation);
        assert_eq!(
            error.message(),
            "The operation cannot be completed: The rounding factor must be greater than 0!"
        );
        assert_eq!(
            currency.values(&mut tx, "rounding").await.unwrap(),
            vec![Value::Decimal("0.01".parse().unwrap())]
        );
    }
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_base_060_company_currency_cannot_be_deactivated() {
    let h = base_harness().await;
    let mut tx = h.begin().await.unwrap();

    // Given: Company C1 uses currency EUR
    let eur_id = create_currency(&mut tx, "EUR", "€", 0.01).await;
    let _c1_id = create_company(&mut tx, "Company C1", eur_id).await;

    // When: Setting active=False on EUR
    // Then: Raises UserError("This currency is set on a company and therefore cannot be deactivated.")
    let eur = tx
        .env()
        .records("res.currency")
        .unwrap()
        .browse([eur_id.get()])
        .unwrap();
    let mut write_vals = RawVals::new();
    write_vals.insert("active".into(), Cell::Bool(false));
    let err = eur.write(&mut tx, write_vals).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::UserError);
    assert_eq!(
        err.message(),
        "This currency is set on a company and therefore cannot be deactivated."
    );
}

#[tokio::test]
async fn test_res_company_currency_id_required_and_default() {
    let h = base_harness().await;
    let mut tx = h.begin().await.unwrap();

    let companies = tx.env().records("res.company").unwrap();
    let mut values = RawVals::new();
    values.insert("name".into(), Cell::Str("No currency".into()));
    let error = tx
        .savepoint(async |tx| companies.create(tx, vec![values]).await)
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "The operation cannot be completed: Missing required field 'Currency' (currency_id) for model 'Company' (res.company)"
    );

    let usd_id = create_currency(&mut tx, "USD", "$", 0.01).await;
    let root_comp = create_company(&mut tx, "Root Company", usd_id).await;

    let eur_id = create_currency(&mut tx, "EUR", "€", 0.01).await;
    let selected = create_company(&mut tx, "Selected Company", eur_id).await;
    tx.execute(
        "UPDATE res_users SET company_id = $1 WHERE id = $2",
        &[&root_comp.get(), &tx.env().uid()],
    )
    .await
    .unwrap();
    let env_with_comp = tx.env().with_company(selected.get()).unwrap();
    let defaults = env_with_comp
        .records("res.company")
        .unwrap()
        .default_get(&mut tx, &["currency_id"])
        .await
        .unwrap();
    assert_eq!(defaults.get("currency_id"), Some(&Value::Int(usd_id.get())));
    let created = env_with_comp
        .records("res.company")
        .unwrap()
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("name", Cell::Str("Defaulted company".into()))],
        )
        .await
        .unwrap();
    assert_eq!(
        created.values(&mut tx, "currency_id").await.unwrap(),
        vec![Value::Int(usd_id.get())]
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();

    assert_company_currency_default_propagates_field_access_errors().await;
}

#[tokio::test]
async fn currency_context_cannot_disable_company_protection() {
    let h = base_harness().await;
    let mut tx = h.begin().await.unwrap();
    let usd = create_currency(&mut tx, "USD", "$", 0.01).await;
    create_company(&mut tx, "Protected", usd).await;
    for key in ["force_deactivate", "install_mode"] {
        let env = tx.env().with_context([(key, Value::Bool(true))]);
        let records = env
            .records("res.currency")
            .unwrap()
            .browse([usd.get()])
            .unwrap();
        let mut values = RawVals::new();
        values.insert("active".into(), Cell::Bool(false));
        let err = tx
            .savepoint(async |tx| records.write(tx, values).await)
            .await
            .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::UserError);
        assert_eq!(
            err.message(),
            "This currency is set on a company and therefore cannot be deactivated."
        );
    }
}

#[tokio::test]
async fn company_without_currency_is_rejected() {
    let h = base_harness().await;
    let mut tx = h.begin().await.unwrap();
    let companies = tx.env().records("res.company").unwrap();
    let mut values = RawVals::new();
    values.insert("name".into(), Cell::Str("No currency".into()));
    let err = companies.create(&mut tx, vec![values]).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.message(),
        "The operation cannot be completed: Missing required field 'Currency' (currency_id) for model 'Company' (res.company)"
    );
}

#[test]
fn currency_schema_bounds_code_and_rounding() {
    let registry = erp_sdk::RegistryBuilder::new([&erp_addon_base::ADDON])
        .build()
        .unwrap();
    let currency = registry.model("res.currency").unwrap();
    assert_eq!(
        currency.field("name").unwrap().ty,
        erp_fields::FieldType::Char { size: Some(3) }
    );
    assert_eq!(
        currency.field("rounding").unwrap().ty,
        erp_fields::FieldType::Float {
            digits: Some(erp_fields::Digits::Fixed {
                precision: 12,
                scale: 6
            }),
        }
    );
}

#[tokio::test]
async fn currency_decimal_thresholds_and_optional_iso_code() {
    let h = base_harness().await;
    let mut tx = h.begin().await.unwrap();
    for (name, rounding, places) in [("QTR", 0.25, 1), ("FTH", 0.20, 1), ("MIN", 0.000001, 6)] {
        let id = create_currency(&mut tx, name, "$", rounding).await;
        let records = tx
            .env()
            .model::<erp_addon_base::ResCurrency>()
            .unwrap()
            .browse([id.get()])
            .unwrap();
        assert_eq!(
            records
                .get(&mut tx, erp_addon_base::ResCurrency::DECIMAL_PLACES)
                .await
                .unwrap(),
            places
        );
        assert_eq!(
            records
                .get(&mut tx, erp_addon_base::ResCurrency::ISO_NUMERIC)
                .await
                .unwrap(),
            None
        );
    }
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

// The full test also exercises lookup failure in a non-sudo environment.
async fn assert_company_currency_default_propagates_field_access_errors() {
    let policy =
        erp_testkit::FieldPolicy::new().deny("res.users", "company_id", erp_orm::FieldOp::Read);
    let h = Harness::builder([&base_users::ADDON, &erp_addon_base::ADDON])
        .policy(policy)
        .build()
        .await
        .unwrap();
    let mut setup = h.begin().await.unwrap();
    setup.execute("INSERT INTO res_users (id, name, login, active) VALUES (42, 'Default reader', 'default-reader', true)", &[]).await.unwrap();
    setup.commit().await.unwrap();
    let mut tx = h.begin_as(42).await.unwrap();
    let company = tx.env().records("res.company").unwrap();
    let err = company
        .default_get(&mut tx, &["currency_id"])
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access);
    let description = h
        .registry()
        .model("res.users")
        .unwrap()
        .description
        .as_str();
    assert_eq!(
        err.message(),
        format!(
            "You do not have enough rights to access the field \"company_id\" on {description} (res.users). Please contact your system administrator.\n\nOperation: read"
        )
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}
