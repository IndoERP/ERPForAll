//! Installed currency/language data, through the real CSV loader and ORM.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use std::sync::Arc;

use erp_data::load::{FileContext, LoadSession, NoFiles, SessionOptions};
use erp_data::{ConvertMode, CsvImportResult, XmlId};
use erp_test_models::base_users;
use erp_testkit::Harness;

async fn load_base_data() -> (Harness, erp_orm::Transaction) {
    let h = Harness::builder([
        &base_users::ADDON,
        &erp_addon_base::ADDON,
        &erp_data::model_data::ADDON,
    ])
    .with_decls([erp_data::ir_model_data_decl()])
    .build()
    .await
    .unwrap();
    let mut tx = h.begin().await.unwrap();
    erp_testkit::base_install::begin_base_install(&mut tx).unwrap();
    let mut session = LoadSession::new(SessionOptions::new("20.0", Arc::new(NoFiles)));
    for (path, contents) in [
        (
            "data/res.currency.csv",
            include_str!("../../../addons/base/data/res.currency.csv"),
        ),
        (
            "data/res.lang.csv",
            include_str!("../../../addons/base/data/res.lang.csv"),
        ),
    ] {
        let CsvImportResult::Loaded(import) =
            erp_data::parse_csv(contents, path, "base", ConvertMode::Init).unwrap()
        else {
            panic!("base data file must contain rows: {path}");
        };
        let file = FileContext::new("base", path, ConvertMode::Init);
        let mut operations = vec![Ok(import.into_operation(false))].into_iter();
        session
            .load_file(&mut tx, &file, &mut operations)
            .await
            .unwrap();
    }
    {
        let path = "data/res_currency_rate_type_data.xml";
        let file = FileContext::new("base", path, ConvertMode::Init);
        let mut operations = erp_data::XmlDataReader::new(
            include_str!("../../../addons/base/data/res_currency_rate_type_data.xml"),
            path,
            "base",
            false,
        );
        session
            .load_file(&mut tx, &file, &mut operations)
            .await
            .unwrap();
    }
    erp_testkit::base_install::end_base_install(&mut tx);
    tx.flush().await.unwrap();
    (h, tx)
}

#[tokio::test]
async fn installed_rate_types_have_their_date_rules() {
    let (h, mut tx) = load_base_data().await;
    let rows = tx
        .query(
            "SELECT code, date_rule, later_fallback FROM res_currency_rate_type ORDER BY sequence",
            &[],
        )
        .await
        .unwrap();
    let got: Vec<(String, String, bool)> = rows
        .iter()
        .map(|r| (r.get(0), r.get(1), r.get(2)))
        .collect();
    let want = [
        ("spot", "strictly_before", true),
        ("monthly_average", "on_or_before", false),
        ("closing", "on_or_before", false),
        ("historical", "strictly_before", false),
        ("budget", "strictly_before", false),
        ("tax", "on_or_before", false),
    ]
    .map(|(c, r, f)| (c.to_owned(), r.to_owned(), f));
    assert_eq!(got, want);
    let rates = tx
        .query("SELECT count(*) FROM res_currency_rate", &[])
        .await
        .unwrap();
    assert_eq!(rates[0].get::<_, i64>(0), 0, "no rate values are seeded");
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn installed_base_data_has_exact_rows_and_default_external_ids() {
    let (h, mut tx) = load_base_data().await;
    let currencies = tx
        .query("SELECT name, active FROM res_currency ORDER BY name", &[])
        .await
        .unwrap();
    assert_eq!(currencies.len(), 170);
    let active: Vec<String> = currencies
        .iter()
        .filter(|row| row.get::<_, bool>(1))
        .map(|row| row.get(0))
        .collect();
    assert_eq!(active, ["USD"]);
    let langs = tx
        .query("SELECT code, active, id FROM res_lang ORDER BY code", &[])
        .await
        .unwrap();
    assert_eq!(langs.len(), 93);
    let en_row = langs
        .iter()
        .find(|r| r.get::<_, String>(0) == "en_US")
        .expect("en_US must be in res_lang");
    assert!(en_row.get::<_, bool>(1));
    let xid = XmlId::parse("base.lang_en").unwrap();
    let env = tx.env();
    let ids = erp_data::model_data::lookup(&mut tx, &env, std::slice::from_ref(&xid))
        .await
        .unwrap();
    let binding = ids
        .get(&xid)
        .expect("base.lang_en must resolve after loading res.lang.csv");
    assert_eq!(binding.model, "res.lang");
    assert_eq!(binding.res_id, Some(en_row.get::<_, i64>(2)));
    assert!(binding.noupdate, "base.lang_en must have noupdate = true");
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn installed_idr_huf_cop_use_iso_minor_units() {
    let (h, mut tx) = load_base_data().await;
    let currencies = tx.query("SELECT name, rounding::double precision, decimal_places FROM res_currency WHERE name IN ('COP', 'HUF', 'IDR') ORDER BY name", &[]).await.unwrap();
    assert_eq!(currencies.len(), 3);
    for row in currencies {
        assert_eq!(row.get::<_, f64>(1), 0.01, "{}", row.get::<_, String>(0));
        assert_eq!(row.get::<_, i32>(2), 2, "{}", row.get::<_, String>(0));
    }
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}
