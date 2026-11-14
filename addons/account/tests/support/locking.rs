// SPDX-License-Identifier: MIT OR Apache-2.0
#![allow(
    dead_code,
    reason = "shared fixture helpers used by different test binaries"
)]
use erp_addon_account::fiscal::calendar::FiscalYearEnd;
use erp_addon_account::fiscal::ensure_periods_with;
use erp_addon_account::fiscal::settings::FiscalCalendar;
use erp_sdk::prelude::*;
use erp_sdk::{BeginOptions, OperationIdentity, SUPERUSER_ID};
use erp_testkit::Harness;
pub fn d(y: i32, m: u32, day: u32) -> Date {
    Date::from_ymd(y, m, day).unwrap()
}
pub async fn harness() -> Harness {
    Harness::builder([
        &erp_addon_base::ADDON,
        &erp_addon_uom::ADDON,
        &erp_addon_product::ADDON,
        &erp_addon_analytic::ADDON,
        &erp_addon_account::ADDON,
    ])
    .build()
    .await
    .unwrap()
}
pub async fn begin(h: &Harness) -> Transaction {
    h.orm()
        .begin_opts(
            BeginOptions::new(SUPERUSER_ID)
                .operation_identity(OperationIdentity::generate().unwrap()),
        )
        .await
        .unwrap()
}
pub async fn company(h: &Harness, parent: Option<i64>) -> i64 {
    let client = h.db().client().await.unwrap();
    let rows = client
        .query("SELECT id::int8 FROM res_currency ORDER BY id LIMIT 1", &[])
        .await
        .unwrap();
    let currency = match rows.first() {
        Some(row) => row.get(0),
        None => erp_testkit::fixtures::seed_currency(&client).await.unwrap(),
    };
    client.query_one("WITH n AS (SELECT nextval(pg_get_serial_sequence('res_company','id')) AS id) \
        INSERT INTO res_company(id,name,active,currency_id,parent_id,parent_path,account_price_include) \
        SELECT id,'Lock test',true,$1,$2::int8,COALESCE((SELECT parent_path FROM res_company \
        WHERE id=$2::int8),'') || id::text || '/','tax_excluded' FROM n RETURNING id::int8",
        &[&currency,&parent]).await.unwrap().get(0)
}
pub async fn periods(h: &Harness, company: i64) {
    ensure_periods_with(
        h.orm(),
        BeginOptions::new(SUPERUSER_ID),
        &FiscalCalendar::new(FiscalYearEnd::new(31, 12).unwrap(), 12).unwrap(),
        company,
        d(2024, 1, 1),
        d(2024, 3, 31),
    )
    .await
    .unwrap();
}
pub async fn date(h: &Harness, company: i64, field: &str) -> Option<Date> {
    // Test-selected identifiers only; values remain parameters.
    h.db()
        .client()
        .await
        .unwrap()
        .query_one(
            &format!("SELECT {field} FROM res_company WHERE id=$1"),
            &[&company],
        )
        .await
        .unwrap()
        .get(0)
}
