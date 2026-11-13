// SPDX-License-Identifier: MIT OR Apache-2.0
//! Base 1.5 rate-schema reconstruction followed by the registered 1.6 pre-migration and schema sync.
//!
//! The historical key is restored from ERPForAll commit
//! 46c69cd74cff24f443b2ed5875d0b9d3cfd9c719 (base manifest 1.5). Supporting tables come from
//! the current harness: this is not an old-binary or complete module-lifecycle upgrade rehearsal.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

#[path = "support/rate_type_fixture.rs"]
mod rate_type_fixture;

use std::sync::Arc;

use erp_data::load::{FileContext, LoadSession, NoFiles, SessionOptions};
use erp_data::migration::{MIGRATIONS, MigrationImpl, MigrationStage, SemverVersion};
use erp_data::{ConvertMode, XmlDataReader};
use erp_sdk::money::parse_decimal;
use erp_sdk::{Cell, Date, ErrorKind, RawVals, RecId, Transaction};
use erp_test_models::base_users;
use erp_testkit::Harness;

const RATE_KEY: &str = "res_currency_rate_unique_name_per_day";
const OLD_COLUMNS: &[&str] = &["currency_id", "company_id", "name"];
const TYPED_COLUMNS: &[&str] = &["currency_id", "company_id", "rate_type_id", "name"];

#[derive(Debug, PartialEq, Eq)]
struct RateSnapshot {
    id: i64,
    currency: i64,
    company: Option<i64>,
    day: String,
    value: String,
}

async fn rate_snapshot(tx: &mut Transaction) -> Vec<RateSnapshot> {
    tx.query(
        "SELECT id, currency_id, company_id, to_char(name, 'YYYY-MM-DD'), rate::text \
         FROM res_currency_rate ORDER BY id",
        &[],
    )
    .await
    .unwrap()
    .iter()
    .map(|row| RateSnapshot {
        id: row.get(0),
        currency: row.get(1),
        company: row.get(2),
        day: row.get(3),
        value: row.get(4),
    })
    .collect()
}

async fn assert_rate_key(tx: &mut Transaction, columns: &[&str]) {
    let rows = tx
        .query(
            "SELECT ARRAY(SELECT a.attname::text FROM unnest(c.conkey) WITH ORDINALITY k(attnum, ord) \
                JOIN pg_attribute a ON a.attrelid = c.conrelid AND a.attnum = k.attnum ORDER BY k.ord), \
                i.indnullsnotdistinct, obj_description(c.oid, 'pg_constraint') \
             FROM pg_constraint c JOIN pg_index i ON i.indexrelid = c.conindid \
             WHERE c.conrelid = 'res_currency_rate'::regclass AND c.conname = $1 AND c.contype = 'u'",
            &[&RATE_KEY],
        )
        .await
        .unwrap();
    assert_eq!(rows.len(), 1, "the declared rate key must exist");
    assert_eq!(rows[0].get::<_, Vec<String>>(0), columns);
    assert!(rows[0].get::<_, bool>(1), "NULL company is one bucket");
    assert_eq!(
        rows[0].get::<_, Option<String>>(2),
        Some(erp_sql::upsert::key_definition(columns, false)),
        "the constraint comment must record its actual definition"
    );
}

fn migration_1_6() -> &'static MigrationImpl {
    let steps: Vec<_> = MIGRATIONS
        .iter()
        .filter(|m| m.addon() == "base" && m.version() == "1.6" && m.stage() == MigrationStage::Pre)
        .collect();
    assert_eq!(steps.len(), 1, "base 1.6 must register one pre-migration");
    assert_eq!(steps[0].name(), "prepare_rate_types_base_1_6");
    assert!(erp_modules::migrations::should_run_migration(
        &SemverVersion::new(1, 6, 0),
        &SemverVersion::new(1, 5, 0),
        &SemverVersion::new(1, 6, 0),
    ));
    steps[0]
}

async fn load_rate_types_update(tx: &mut Transaction) {
    let path = "data/res_currency_rate_type_data.xml";
    let file = FileContext::new("base", path, ConvertMode::Update);
    let mut operations = XmlDataReader::new(
        include_str!("../../../addons/base/data/res_currency_rate_type_data.xml"),
        path,
        "base",
        false,
    );
    let mut session = LoadSession::new(SessionOptions::new("20.0", Arc::new(NoFiles)));
    erp_testkit::base_install::begin_base_install(tx).unwrap();
    session.load_file(tx, &file, &mut operations).await.unwrap();
    erp_testkit::base_install::end_base_install(tx);
    tx.flush().await.unwrap();
}

async fn type_bindings(tx: &mut Transaction) -> Vec<(String, i64, bool)> {
    tx.query(
        "SELECT d.name, d.res_id, d.noupdate FROM ir_model_data d \
         JOIN res_currency_rate_type t ON t.id = d.res_id \
         WHERE d.module = 'base' AND d.model = 'res.currency.rate.type' ORDER BY d.name",
        &[],
    )
    .await
    .unwrap()
    .iter()
    .map(|row| (row.get(0), row.get(1), row.get(2)))
    .collect()
}

async fn type_snapshot(tx: &mut Transaction) -> Vec<String> {
    tx.query(
        "SELECT row_to_json(t)::text FROM res_currency_rate_type t ORDER BY id",
        &[],
    )
    .await
    .unwrap()
    .iter()
    .map(|row| row.get(0))
    .collect()
}

fn rate_vals(currency: RecId, company: Option<RecId>, day: &str, value: &str) -> RawVals {
    RawVals::new()
        .set_cell("currency_id", Cell::Id(currency.get()))
        .set_cell(
            "company_id",
            company.map_or(Cell::Null, |id| Cell::Id(id.get())),
        )
        .set_cell("name", Cell::Date(Date::parse_odoo(day).unwrap()))
        .set_cell("rate", Cell::Decimal(parse_decimal(value).unwrap()))
}

#[tokio::test]
async fn existing_rates_become_spot_before_the_key_switches() {
    let h = Harness::builder([
        &base_users::ADDON,
        &erp_addon_base::ADDON,
        &erp_data::model_data::ADDON,
    ])
    .with_decls([erp_data::ir_model_data_decl()])
    .build()
    .await
    .unwrap();
    rate_type_fixture::seed_rate_types(&h).await;
    let mut tx = h.begin().await.unwrap();
    let currencies = tx.env().records("res.currency").unwrap();
    let mut ids = Vec::new();
    for code in ["USD", "EUR"] {
        let vals = RawVals::new()
            .set_cell("name", Cell::Str(code.into()))
            .set_cell("symbol", Cell::Str(code.into()))
            .set_cell("rounding", Cell::Decimal(parse_decimal("0.01").unwrap()))
            .set_cell("active", Cell::Bool(true));
        ids.push(
            currencies
                .create(&mut tx, vec![vals])
                .await
                .unwrap()
                .rec_ids()[0],
        );
    }
    let companies = tx.env().records("res.company").unwrap();
    let root = companies
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Legacy root".into()))
                    .set_cell("currency_id", Cell::Id(ids[0].get())),
            ],
        )
        .await
        .unwrap()
        .rec_ids()[0];
    let rates = tx.env().records("res.currency.rate").unwrap();
    for (company, day, value) in [
        (None, "2026-01-01", "1.123456789012345678"),
        (None, "2026-02-01", "1.2"),
        (Some(root), "2026-01-01", "2.345678901234567890"),
        (Some(root), "2026-02-01", "2.4"),
    ] {
        rates
            .create(&mut tx, vec![rate_vals(ids[1], company, day, value)])
            .await
            .unwrap();
    }
    tx.flush().await.unwrap();

    // Reconstruct the base 1.5 rate schema, including the constraint that schema sync must replace.
    tx.batch("ALTER TABLE res_currency_rate DROP COLUMN rate_type_id")
        .await
        .unwrap();
    tx.batch("DROP TABLE res_currency_rate_type CASCADE")
        .await
        .unwrap();
    tx.execute(
        "DELETE FROM ir_model_data WHERE module = 'base' AND model = 'res.currency.rate.type'",
        &[],
    )
    .await
    .unwrap();
    let definition = erp_sql::upsert::key_definition(OLD_COLUMNS, false);
    for sql in erp_sql::ddl::add_constraint("res_currency_rate", RATE_KEY, &definition) {
        tx.batch(&sql).await.unwrap();
    }
    tx.invalidate_all().await.unwrap();
    assert_rate_key(&mut tx, OLD_COLUMNS).await;
    let old_shape = tx.query(
        "SELECT to_regclass('res_currency_rate_type') IS NULL, \
         NOT EXISTS (SELECT 1 FROM information_schema.columns WHERE table_schema = current_schema() \
         AND table_name = 'res_currency_rate' AND column_name = 'rate_type_id')",
        &[],
    ).await.unwrap();
    assert!(old_shape[0].get::<_, bool>(0));
    assert!(old_shape[0].get::<_, bool>(1));
    let before = rate_snapshot(&mut tx).await;
    assert_eq!(before.len(), 4);
    assert!(before.iter().any(|row| row.company.is_none()));
    assert!(before.iter().any(|row| row.company == Some(root.get())));

    let step = migration_1_6();
    (step.run())(&mut tx, &SemverVersion::new(1, 5, 0))
        .await
        .unwrap();
    // Pre backfills the type; the old key must survive until schema sync itself replaces it.
    assert_rate_key(&mut tx, OLD_COLUMNS).await;
    let bindings = type_bindings(&mut tx).await;
    assert_eq!(bindings.len(), 1);
    assert_eq!(bindings[0].0, "rate_type_spot");
    assert!(bindings[0].2);
    let spot = bindings[0].1;
    let typed = tx
        .query(
            "SELECT rate_type_id FROM res_currency_rate ORDER BY id",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(
        typed
            .iter()
            .map(|row| row.get::<_, i64>(0))
            .collect::<Vec<_>>(),
        vec![spot; before.len()]
    );
    assert_eq!(rate_snapshot(&mut tx).await, before);

    tx.init_models(&[]).await.unwrap();
    assert_rate_key(&mut tx, TYPED_COLUMNS).await;
    let unique_indexes = tx.query(
        "SELECT ARRAY(SELECT a.attname::text FROM unnest(i.indkey::int2[]) WITH ORDINALITY k(attnum, ord) \
         JOIN pg_attribute a ON a.attrelid = i.indrelid AND a.attnum = k.attnum \
         WHERE k.ord <= i.indnkeyatts ORDER BY k.ord) \
         FROM pg_index i WHERE i.indrelid = 'res_currency_rate'::regclass AND i.indisunique",
        &[],
    ).await.unwrap();
    for index in unique_indexes {
        assert_ne!(
            index.get::<_, Vec<String>>(0),
            OLD_COLUMNS,
            "no obsolete three-column uniqueness may survive"
        );
    }
    let required_and_fk = tx.query(
        "SELECT a.attnotnull, EXISTS (SELECT 1 FROM pg_constraint c \
         WHERE c.conrelid = a.attrelid AND c.contype = 'f' AND c.conkey = ARRAY[a.attnum] \
         AND c.confrelid = 'res_currency_rate_type'::regclass AND c.convalidated) \
         FROM pg_attribute a WHERE a.attrelid = 'res_currency_rate'::regclass AND a.attname = 'rate_type_id'",
        &[],
    ).await.unwrap();
    assert_eq!(required_and_fk.len(), 1);
    assert!(required_and_fk[0].get::<_, bool>(0));
    assert!(required_and_fk[0].get::<_, bool>(1));
    // A tenant label distinguishes a skipped noupdate record from a rewrite of identical seed data.
    tx.execute(
        "UPDATE res_currency_rate_type SET name = $1 WHERE id = $2",
        &[&"Tenant spot label", &spot],
    )
    .await
    .unwrap();
    tx.invalidate_all().await.unwrap();
    load_rate_types_update(&mut tx).await;
    let spot_label = tx
        .query(
            "SELECT name FROM res_currency_rate_type WHERE id = $1",
            &[&spot],
        )
        .await
        .unwrap();
    assert_eq!(spot_label[0].get::<_, String>(0), "Tenant spot label");
    let seeded = type_bindings(&mut tx).await;
    assert_eq!(seeded.len(), rate_type_fixture::SEEDED.len());
    assert_eq!(
        seeded
            .iter()
            .filter(|b| b.0 == "rate_type_spot")
            .collect::<Vec<_>>(),
        vec![&bindings[0]]
    );
    let spot_count = tx
        .query(
            "SELECT count(*) FROM res_currency_rate_type WHERE code = 'spot'",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(spot_count[0].get::<_, i64>(0), 1);
    assert_eq!(rate_snapshot(&mut tx).await, before);

    let tax_rows = tx
        .query(
            "SELECT id FROM res_currency_rate_type WHERE code = 'tax'",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(tax_rows.len(), 1);
    let tax: i64 = tax_rows[0].get(0);
    for company in [None, Some(root)] {
        rates
            .create(
                &mut tx,
                vec![
                    rate_vals(ids[1], company, "2026-01-01", "9.8765")
                        .set_cell("rate_type_id", Cell::Id(tax)),
                ],
            )
            .await
            .unwrap();
        tx.flush().await.unwrap();
        let same_day = tx.query(
            "SELECT rate_type_id FROM res_currency_rate WHERE currency_id = $1 \
             AND company_id IS NOT DISTINCT FROM $2 AND name = '2026-01-01'::date ORDER BY rate_type_id",
            &[&ids[1].get(), &company.map(RecId::get)],
        ).await.unwrap();
        let mut expected = vec![spot, tax];
        expected.sort();
        assert_eq!(
            same_day
                .iter()
                .map(|row| row.get::<_, i64>(0))
                .collect::<Vec<_>>(),
            expected
        );
        for type_id in [spot, tax] {
            let error = tx
                .savepoint(async |tx| {
                    rates
                        .create(
                            tx,
                            vec![
                                rate_vals(ids[1], company, "2026-01-01", "7.77")
                                    .set_cell("rate_type_id", Cell::Id(type_id)),
                            ],
                        )
                        .await?;
                    tx.flush().await
                })
                .await
                .unwrap_err();
            assert_eq!(error.kind(), ErrorKind::Validation);
            assert_eq!(
                error.message(),
                "The operation cannot be completed: Only one currency rate per day allowed!"
            );
            assert_eq!(error.constraint(), Some(RATE_KEY));
        }
    }

    let rates_before_repeat = rate_snapshot(&mut tx).await;
    let types_before_repeat = type_snapshot(&mut tx).await;
    (step.run())(&mut tx, &SemverVersion::new(1, 5, 0))
        .await
        .unwrap();
    tx.init_models(&[]).await.unwrap();
    load_rate_types_update(&mut tx).await;
    assert_rate_key(&mut tx, TYPED_COLUMNS).await;
    assert_eq!(rate_snapshot(&mut tx).await, rates_before_repeat);
    assert_eq!(type_snapshot(&mut tx).await, types_before_repeat);
    assert_eq!(type_bindings(&mut tx).await, seeded);
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}
