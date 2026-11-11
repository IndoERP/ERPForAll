//! Installed catalog acceptance against immutable reference/project facts.

use erp_sdk::Decimal;
use serde_json::{Value, json};
use tokio_postgres::Client;

pub async fn language_catalog(client: &Client) -> Value {
    let rows = client.query("SELECT d.name,l.code,l.active,COALESCE(l.date_format,''),COALESCE(l.time_format,''),COALESCE(l.week_start,''),COALESCE(l.grouping,''),COALESCE(l.decimal_point,''),COALESCE(l.thousands_sep,'') FROM res_lang l JOIN ir_model_data d ON d.res_id=l.id AND d.model='res.lang' AND d.module='base' ORDER BY l.code COLLATE \"C\"", &[]).await.unwrap();
    let actual: Vec<Value> = rows.iter().map(|r| json!({
        "xmlid":r.get::<_,String>(0), "code":r.get::<_,String>(1), "active":r.get::<_,bool>(2),
        "date_format":r.get::<_,String>(3), "time_format":r.get::<_,String>(4),
        "week_start":r.get::<_,String>(5), "grouping":r.get::<_,String>(6),
        "decimal_point":r.get::<_,String>(7), "thousands_sep":r.get::<_,String>(8)
    })).collect();
    Value::Array(actual)
}

pub async fn assert_language_catalog(client: &Client) {
    let expected: Value =
        serde_json::from_str(include_str!("../fixtures/base-catalog/languages.json")).unwrap();
    let actual = language_catalog(client).await;
    let actual = actual.as_array().unwrap();
    let expected = expected.as_array().unwrap();
    assert_eq!(
        actual.len(),
        expected.len(),
        "installed language catalog size"
    );
    for (actual, expected) in actual.iter().zip(expected) {
        assert_eq!(actual, expected, "installed language {}", expected["code"]);
    }
}

pub async fn currency_catalog(client: &Client) -> Value {
    let rows = client.query("SELECT d.name,c.name,c.symbol,c.rounding,c.decimal_places,c.active,c.position,c.iso_numeric FROM res_currency c JOIN ir_model_data d ON d.res_id=c.id AND d.model='res.currency' AND d.module='base' ORDER BY c.name COLLATE \"C\"", &[]).await.unwrap();
    let actual: Vec<Value> = rows.iter().map(|r| json!({
        "xmlid":r.get::<_,String>(0), "name":r.get::<_,String>(1), "symbol":r.get::<_,String>(2),
        "rounding":r.get::<_,Decimal>(3).normalize().to_string(), "decimal_places":r.get::<_,i32>(4),
        "active":r.get::<_,bool>(5), "position":r.get::<_,String>(6), "iso_numeric":r.get::<_,Option<i32>>(7)
    })).collect();
    Value::Array(actual)
}

pub async fn assert_currency_catalog(client: &Client) {
    let expected: Value =
        serde_json::from_str(include_str!("../fixtures/base-catalog/currencies.json")).unwrap();
    let actual = currency_catalog(client).await;
    let actual = actual.as_array().unwrap();
    let expected = expected.as_array().unwrap();
    assert_eq!(
        actual.len(),
        expected.len(),
        "installed currency catalog size"
    );
    for (actual, expected) in actual.iter().zip(expected) {
        assert_eq!(actual, expected, "installed currency {}", expected["name"]);
    }
}
