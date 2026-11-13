//! `res.country`, `res.country.state`, and `res.country.group` acceptance tests.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions"
)]

use std::sync::Arc;

use erp_data::load::{FileContext, LoadSession, NoFiles, SessionOptions};
use erp_data::{ConvertMode, CsvImportResult, XmlDataReader};
use erp_sdk::{Cell, Command, RawVals, RecId, Value};
use erp_test_models::base_users;
use erp_testkit::Harness;

async fn country_harness() -> Harness {
    Harness::builder([
        &base_users::ADDON,
        &erp_addon_base::ADDON,
        &erp_data::model_data::ADDON,
    ])
    .with_decls([erp_data::ir_model_data_decl()])
    .build()
    .await
    .unwrap()
}

#[tokio::test]
async fn country_fields_uniqueness_and_uppercase_sanitization() {
    let h = country_harness().await;
    let mut tx = h.begin().await.unwrap();

    let model = tx.env().records("res.country").unwrap();
    assert!(model.spec().field("name").is_some());
    assert!(model.spec().field("code").is_some());
    assert!(model.spec().field("address_format").is_some());
    assert!(model.spec().field("currency_id").is_some());
    assert!(model.spec().field("phone_code").is_some());
    assert!(model.spec().field("name_position").is_some());
    assert!(model.spec().field("vat_label").is_some());
    assert!(model.spec().field("state_required").is_some());
    assert!(model.spec().field("zip_applicability").is_some());
    assert!(model.spec().field("country_group_ids").is_some());
    assert!(model.spec().field("state_ids").is_some());

    // Lowercase code is automatically sanitized to uppercase
    let vals = RawVals::new()
        .set_cell("name", Cell::Str("Testland".into()))
        .set_cell("code", Cell::Str("tl".into()));
    let created = model.create(&mut tx, vec![vals]).await.unwrap();
    assert_eq!(
        created.value(&mut tx, "code").await.unwrap(),
        Value::Str("TL".into())
    );
    tx.commit().await.unwrap();

    // Duplicate name is refused by _name_uniq constraint
    let mut tx = h.begin().await.unwrap();
    let model = tx.env().records("res.country").unwrap();
    let dup_name = RawVals::new()
        .set_cell("name", Cell::Str("Testland".into()))
        .set_cell("code", Cell::Str("XY".into()));
    let err_name = model.create(&mut tx, vec![dup_name]).await.unwrap_err();
    assert!(
        err_name
            .to_string()
            .contains("The name of the country must be unique!"),
        "expected unique name error, got: {err_name}"
    );
    drop(tx);

    // Duplicate code is refused by _code_uniq constraint
    let mut tx = h.begin().await.unwrap();
    let model = tx.env().records("res.country").unwrap();
    let dup_code = RawVals::new()
        .set_cell("name", Cell::Str("Otherland".into()))
        .set_cell("code", Cell::Str("TL".into()));
    let err_code = model.create(&mut tx, vec![dup_code]).await.unwrap_err();
    assert!(
        err_code
            .to_string()
            .contains("The code of the country must be unique!"),
        "expected unique code error, got: {err_code}"
    );

    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn state_fields_and_uniqueness_per_country() {
    let h = country_harness().await;
    let mut tx = h.begin().await.unwrap();

    let country_model = tx.env().records("res.country").unwrap();
    let state_model = tx.env().records("res.country.state").unwrap();

    let c1 = country_model
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Country One".into()))
                    .set_cell("code", Cell::Str("C1".into())),
            ],
        )
        .await
        .unwrap();
    let c2 = country_model
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Country Two".into()))
                    .set_cell("code", Cell::Str("C2".into())),
            ],
        )
        .await
        .unwrap();

    let c1_id = c1.ids()[0];
    let c2_id = c2.ids()[0];

    // Create state under C1
    let s1 = state_model
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("State Alpha".into()))
                    .set_cell("code", Cell::Str("sa".into()))
                    .set_cell("country_id", Cell::Id(c1_id)),
            ],
        )
        .await
        .unwrap();
    assert_eq!(
        s1.value(&mut tx, "code").await.unwrap(),
        Value::Str("sa".into())
    );
    let found = state_model.name_search(&mut tx, "sa").await.unwrap();
    assert!(
        found.iter().any(|(id, _)| *id == s1.ids()[0]),
        "state must be found by its code, got {found:?}"
    );

    // Same code under different country C2 is allowed
    let s2 = state_model
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("State Alpha 2".into()))
                    .set_cell("code", Cell::Str("SA".into()))
                    .set_cell("country_id", Cell::Id(c2_id)),
            ],
        )
        .await
        .unwrap();
    assert_eq!(
        s2.value(&mut tx, "code").await.unwrap(),
        Value::Str("SA".into())
    );

    // Duplicate code within same country C1 is refused by _name_code_uniq
    let dup_state = RawVals::new()
        .set_cell("name", Cell::Str("State Alpha Duplicate".into()))
        .set_cell("code", Cell::Str("sa".into()))
        .set_cell("country_id", Cell::Id(c1_id));
    let err = state_model
        .create(&mut tx, vec![dup_state])
        .await
        .unwrap_err();
    assert!(
        err.to_string()
            .contains("The code of the state must be unique by country!"),
        "expected state unique error, got: {err}"
    );

    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn country_group_fields_and_code_uniqueness() {
    let h = country_harness().await;
    let mut tx = h.begin().await.unwrap();

    let group_model = tx.env().records("res.country.group").unwrap();

    let g1 = group_model
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Group One".into()))
                    .set_cell("code", Cell::Str("g1".into())),
            ],
        )
        .await
        .unwrap();
    assert_eq!(
        g1.value(&mut tx, "code").await.unwrap(),
        Value::Str("G1".into())
    );

    // Duplicate group code is refused
    let dup = RawVals::new()
        .set_cell("name", Cell::Str("Group Two".into()))
        .set_cell("code", Cell::Str("G1".into()));
    let err = group_model.create(&mut tx, vec![dup]).await.unwrap_err();
    assert!(
        err.to_string()
            .contains("The country group code must be unique!"),
        "expected group unique code error, got: {err}"
    );

    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn country_group_many_to_many_relation() {
    let h = country_harness().await;
    let mut tx = h.begin().await.unwrap();

    let country_model = tx.env().records("res.country").unwrap();
    let group_model = tx.env().records("res.country.group").unwrap();

    let c1 = country_model
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Nation A".into()))
                    .set_cell("code", Cell::Str("NA".into())),
            ],
        )
        .await
        .unwrap();
    let c2 = country_model
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Nation B".into()))
                    .set_cell("code", Cell::Str("NB".into())),
            ],
        )
        .await
        .unwrap();

    let c1_id = RecId::from_real(c1.ids()[0]).unwrap();
    let c2_id = RecId::from_real(c2.ids()[0]).unwrap();

    let mut g_vals = RawVals::new().set_cell("name", Cell::Str("Alliance".into()));
    g_vals.insert_commands("country_ids".into(), vec![Command::Set(vec![c1_id, c2_id])]);

    let group = group_model.create(&mut tx, vec![g_vals]).await.unwrap();
    let member_ids = match group.value(&mut tx, "country_ids").await.unwrap() {
        Value::List(l) => l
            .iter()
            .map(|v| match v {
                Value::Int(id) => *id,
                _ => panic!("expected int id"),
            })
            .collect::<Vec<_>>(),
        v => panic!("expected list, got {v:?}"),
    };

    assert_eq!(member_ids.len(), 2);
    assert!(member_ids.contains(&c1.ids()[0]));
    assert!(member_ids.contains(&c2.ids()[0]));

    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn address_format_placeholder_extraction() {
    let h = country_harness().await;
    let mut tx = h.begin().await.unwrap();

    let country_model = tx.env().records("res.country").unwrap();
    let country = country_model
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Custom Address Land".into()))
                    .set_cell("code", Cell::Str("CL".into()))
                    .set_cell(
                        "address_format",
                        Cell::Str(
                            "%(street)s\n%(city)s %(state_code)s %(zip)s\n%(country_name)s".into(),
                        ),
                    ),
            ],
        )
        .await
        .unwrap();

    let fields: Vec<String> = erp_orm::call_method(&mut tx, &country, "get_address_fields", ())
        .await
        .unwrap();
    assert_eq!(
        fields,
        ["street", "city", "state_code", "zip", "country_name"]
    );

    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn partner_country_and_state_fields() {
    let h = country_harness().await;
    let mut tx = h.begin().await.unwrap();

    let partner_model = tx.env().records("res.partner").unwrap();
    assert!(partner_model.spec().field("country_id").is_some());
    assert!(partner_model.spec().field("state_id").is_some());
    assert!(partner_model.spec().field("country_code").is_some());

    let address_fields: Vec<String> =
        erp_orm::call_method(&mut tx, &partner_model, "_address_fields", ())
            .await
            .unwrap();
    assert_eq!(
        address_fields,
        ["street", "street2", "zip", "city", "state_id", "country_id"]
    );

    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn partner_address_sync_propagates_country_and_state() {
    let h = country_harness().await;
    let mut tx = h.begin().await.unwrap();

    let country_model = tx.env().records("res.country").unwrap();
    let state_model = tx.env().records("res.country.state").unwrap();
    let partner_model = tx.env().records("res.partner").unwrap();

    let country = country_model
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Germany".into()))
                    .set_cell("code", Cell::Str("DE".into())),
            ],
        )
        .await
        .unwrap();
    let c_id = country.ids()[0];

    let state = state_model
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Berlin".into()))
                    .set_cell("code", Cell::Str("BE".into()))
                    .set_cell("country_id", Cell::Id(c_id)),
            ],
        )
        .await
        .unwrap();
    let s_id = state.ids()[0];

    // Parent company with full address including country and state
    let parent = partner_model
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Parent HQ".into()))
                    .set_cell("is_company", Cell::Bool(true))
                    .set_cell("street", Cell::Str("Unter den Linden 1".into()))
                    .set_cell("city", Cell::Str("Berlin".into()))
                    .set_cell("zip", Cell::Str("10117".into()))
                    .set_cell("country_id", Cell::Id(c_id))
                    .set_cell("state_id", Cell::Id(s_id)),
            ],
        )
        .await
        .unwrap();

    assert_eq!(
        parent.value(&mut tx, "country_code").await.unwrap(),
        Value::Str("DE".into())
    );

    // Create child contact under parent
    let child = partner_model
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Hans".into()))
                    .set_cell("parent_id", Cell::Id(parent.ids()[0]))
                    .set_cell("type", Cell::Str("contact".into())),
            ],
        )
        .await
        .unwrap();

    // Child should have inherited country_id and state_id
    assert_eq!(
        child.value(&mut tx, "country_id").await.unwrap(),
        Value::Int(c_id)
    );
    assert_eq!(
        child.value(&mut tx, "state_id").await.unwrap(),
        Value::Int(s_id)
    );
    assert_eq!(
        child.value(&mut tx, "country_code").await.unwrap(),
        Value::Str("DE".into())
    );

    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn master_data_loads_countries_states_and_groups() {
    let h = country_harness().await;
    let mut tx = h.begin().await.unwrap();
    erp_testkit::base_install::begin_base_install(&mut tx).unwrap();

    let mut session = LoadSession::new(SessionOptions::new("20.0", Arc::new(NoFiles)));

    // 1. Load currencies first (dependency for res.country)
    {
        let path = "data/res.currency.csv";
        let content = include_str!("../../../addons/base/data/res.currency.csv");
        let CsvImportResult::Loaded(import) =
            erp_data::parse_csv(content, path, "base", ConvertMode::Init).unwrap()
        else {
            panic!("currency data file must contain rows");
        };
        let file = FileContext::new("base", path, ConvertMode::Init);
        let mut ops = vec![Ok(import.into_operation(false))].into_iter();
        session.load_file(&mut tx, &file, &mut ops).await.unwrap();
    }

    // 2. Load res_country_data.xml (countries + country groups)
    {
        let path = "data/res_country_data.xml";
        let content = include_str!("../../../addons/base/data/res_country_data.xml");
        let file = FileContext::new("base", path, ConvertMode::Init);
        let mut ops = XmlDataReader::new(content, path, "base", false);
        session.load_file(&mut tx, &file, &mut ops).await.unwrap();
    }

    // 3. Load res.country.state.csv
    {
        let path = "data/res.country.state.csv";
        let content = include_str!("../../../addons/base/data/res.country.state.csv");
        let CsvImportResult::Loaded(import) =
            erp_data::parse_csv(content, path, "base", ConvertMode::Init).unwrap()
        else {
            panic!("state data file must contain rows");
        };
        let file = FileContext::new("base", path, ConvertMode::Init);
        let mut ops = vec![Ok(import.into_operation(false))].into_iter();
        session.load_file(&mut tx, &file, &mut ops).await.unwrap();
    }

    erp_testkit::base_install::end_base_install(&mut tx);
    tx.flush().await.unwrap();

    let country_rows = tx
        .query("SELECT count(*) FROM res_country", &[])
        .await
        .unwrap();
    let country_count: i64 = country_rows[0].get(0);
    assert_eq!(country_count, 251);

    // Verify key groups exist (e.g. Europe)
    let group_rows = tx
        .query(
            "SELECT g.code, g.name FROM res_country_group g JOIN ir_model_data d ON d.res_id=g.id \
             WHERE d.module='base' AND d.name='europe'",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(group_rows.len(), 1);
    assert_eq!(group_rows[0].get::<_, String>(0), "EU");

    // Verify states loaded (over 2,000 states)
    let state_rows = tx
        .query("SELECT count(*) FROM res_country_state", &[])
        .await
        .unwrap();
    let state_count: i64 = state_rows[0].get(0);
    // The reference file ships 2,130 states (compared row by row in the takeover check).
    assert_eq!(state_count, 2130);
    let orphan_rows = tx
        .query(
            "SELECT count(*) FROM res_country_state WHERE country_id IS NULL",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(orphan_rows[0].get::<_, i64>(0), 0);
    // Every country ships with its currency, and the groups list their member countries.
    let no_currency = tx
        .query(
            "SELECT count(*) FROM res_country WHERE currency_id IS NULL",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(no_currency[0].get::<_, i64>(0), 0);
    let group_count = tx
        .query("SELECT count(*) FROM res_country_group", &[])
        .await
        .unwrap();
    assert_eq!(group_count[0].get::<_, i64>(0), 8);
    let members = tx
        .query(
            "SELECT count(*) FROM res_country_res_country_group_rel",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(members[0].get::<_, i64>(0), 145);

    drop(tx);
    h.cleanup().await.unwrap();
}

#[test]
fn access_rules_grant_read_to_internal_users_and_crud_to_system() {
    let content = include_str!("../../../addons/base/security/ir.access.csv");
    let mut access = Vec::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let cols: Vec<&str> = line.split(',').collect();
        if cols[0].starts_with("access_country") {
            access.push((
                cols[0].to_string(),
                cols[4].to_string(),
                cols[3].to_string(),
            ));
        }
    }

    assert!(access.contains(&(
        "access_country_system".to_string(),
        "crud".to_string(),
        "base.group_system".to_string()
    )));
    assert!(access.contains(&(
        "access_country_internal".to_string(),
        "r".to_string(),
        "base.group_user".to_string()
    )));
    assert!(access.contains(&(
        "access_country_state_system".to_string(),
        "crud".to_string(),
        "base.group_system".to_string()
    )));
    assert!(access.contains(&(
        "access_country_state_internal".to_string(),
        "r".to_string(),
        "base.group_user".to_string()
    )));
    assert!(access.contains(&(
        "access_country_group_system".to_string(),
        "crud".to_string(),
        "base.group_system".to_string()
    )));
    assert!(access.contains(&(
        "access_country_group_internal".to_string(),
        "r".to_string(),
        "base.group_user".to_string()
    )));
}

#[tokio::test]
async fn configuration_views_and_actions_exist() {
    let h = country_harness().await;
    let mut tx = h.begin().await.unwrap();
    erp_testkit::base_install::begin_base_install(&mut tx).unwrap();

    let path = "data/country_views.xml";
    let content = include_str!("../../../addons/base/data/country_views.xml");
    let file = FileContext::new("base", path, ConvertMode::Init);
    let mut ops = XmlDataReader::new(content, path, "base", false);
    let mut session = LoadSession::new(SessionOptions::new("20.0", Arc::new(NoFiles)));
    session.load_file(&mut tx, &file, &mut ops).await.unwrap();

    erp_testkit::base_install::end_base_install(&mut tx);
    tx.flush().await.unwrap();

    let rows = tx
        .query(
            "SELECT d.name FROM ir_act_window a \
             JOIN ir_model_data d ON d.res_id = a.id AND d.model = 'ir.actions.act_window' AND d.module = 'base' \
             WHERE d.name IN ('action_country', 'action_country_state', 'action_country_group') \
             ORDER BY d.name",
            &[],
        )
        .await
        .unwrap();

    let names: Vec<String> = rows.iter().map(|r| r.get(0)).collect();
    assert_eq!(
        names,
        [
            "action_country",
            "action_country_group",
            "action_country_state"
        ]
    );

    drop(tx);
    h.cleanup().await.unwrap();
}
