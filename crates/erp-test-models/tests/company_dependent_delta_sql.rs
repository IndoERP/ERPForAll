// SPDX-License-Identifier: MIT OR Apache-2.0
//! Execute company-map patches against PostgreSQL, independently of the ORM projection.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]
use erp_fields::ColumnType;
use erp_sql::version::{VersionedUpdate, update_from_arrays};
use erp_testkit::Harness;
use serde_json::{Value, json};

#[tokio::test]
async fn company_delta_keeps_nulls_and_unmodified_companies() {
    let h = Harness::builder([&erp_test_models::base_users::ADDON])
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    tx.execute(
        "CREATE TEMP TABLE company_delta (id int8 PRIMARY KEY, value jsonb)",
        &[],
    )
    .await
    .unwrap();
    tx.execute(
        "INSERT INTO company_delta VALUES (1, $1), (2, $2), (3, NULL)",
        &[
            &json!({"1": "old", "2": "keep", "3": "remove"}),
            &json!({"1": 42}),
        ],
    )
    .await
    .unwrap();
    let sql = update_from_arrays(
        "company_delta",
        &[("value", ColumnType::Jsonb)],
        &[],
        &VersionedUpdate::new().merge_company(&["value"]),
    );
    let patches = vec![
        json!({"1": [null], "3": []}),
        json!({"1": []}),
        json!({"2": ["9007199254740993.125"]}),
    ];
    tx.execute(&sql, &[&vec![1_i64, 2, 3], &patches])
        .await
        .unwrap();
    let rows = tx
        .query("SELECT value FROM company_delta ORDER BY id", &[])
        .await
        .unwrap();
    assert_eq!(
        rows[0].get::<_, Option<Value>>(0),
        Some(json!({"1": null, "2": "keep"}))
    );
    assert_eq!(rows[1].get::<_, Option<Value>>(0), None);
    assert_eq!(
        rows[2].get::<_, Option<Value>>(0),
        Some(json!({"2": "9007199254740993.125"}))
    );
    // A later patch never resends or overwrites the previous company's map.
    tx.execute(&sql, &[&vec![1_i64], &vec![json!({"4": [false]})]])
        .await
        .unwrap();
    let row = tx
        .query("SELECT value FROM company_delta WHERE id = 1", &[])
        .await
        .unwrap();
    assert_eq!(
        row[0].get::<_, Value>(0),
        json!({"1": null, "2": "keep", "4": false})
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn company_delta_honors_versions_returns_actual_maps_and_preserves_translation_semantics() {
    let h = Harness::builder([&erp_test_models::base_users::ADDON])
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    tx.execute("CREATE TEMP TABLE company_delta_versions (id int8 PRIMARY KEY, value jsonb, label jsonb, row_version int8)", &[]).await.unwrap();
    tx.execute(
        "INSERT INTO company_delta_versions VALUES (1, $1, $2, 7)",
        &[
            &json!({"1":"old", "2":"keep"}),
            &json!({"en_US":"Hello", "fr_FR":"Salut"}),
        ],
    )
    .await
    .unwrap();
    let sql = update_from_arrays(
        "company_delta_versions",
        &[("value", ColumnType::Jsonb), ("label", ColumnType::Jsonb)],
        &[],
        &VersionedUpdate::new()
            .merge_company(&["value"])
            .merge_json(&["label"])
            .expected(true)
            .bump(true)
            .returning_old(&["value"])
            .returning_changed(&["value"]),
    );
    let rows = tx
        .query(
            &sql,
            &[
                &vec![1_i64],
                &vec![json!({"1":[null]})],
                &vec![json!({"fr_FR":null})],
                &vec![6_i64],
            ],
        )
        .await
        .unwrap();
    assert!(rows.is_empty(), "stale expected version must not patch");
    let rows = tx
        .query(
            &sql,
            &[
                &vec![1_i64],
                &vec![json!({"1":[null]})],
                &vec![json!({"fr_FR":null})],
                &vec![7_i64],
            ],
        )
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get::<_, Value>(1), json!({"1":"old", "2":"keep"}));
    assert_eq!(rows[0].get::<_, Value>(2), json!({"1":null, "2":"keep"}));
    assert!(rows[0].get::<_, bool>(3));
    let rows = tx
        .query(
            &sql,
            &[
                &vec![1_i64],
                &vec![None::<Value>],
                &vec![json!({})],
                &vec![8_i64],
            ],
        )
        .await
        .unwrap();
    assert!(
        !rows[0].get::<_, bool>(3),
        "SQL NULL company patch is a no-op"
    );
    let rows = tx
        .query(
            "SELECT value,label,row_version FROM company_delta_versions",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(rows[0].get::<_, Value>(0), json!({"1":null, "2":"keep"}));
    assert_eq!(rows[0].get::<_, Value>(1), json!({"en_US":"Hello"}));
    assert_eq!(rows[0].get::<_, i64>(2), 9);
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn company_delta_refuses_multi_value_patch_instead_of_deleting_key() {
    let h = Harness::builder([&erp_test_models::base_users::ADDON])
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    tx.execute(
        "CREATE TEMP TABLE company_bad_delta (id int8 PRIMARY KEY, value jsonb)",
        &[],
    )
    .await
    .unwrap();
    tx.execute(
        "INSERT INTO company_bad_delta VALUES (1, $1)",
        &[&json!({"1":"keep"})],
    )
    .await
    .unwrap();
    let sql = update_from_arrays(
        "company_bad_delta",
        &[("value", ColumnType::Jsonb)],
        &[],
        &VersionedUpdate::new().merge_company(&["value"]),
    );
    let result = tx
        .execute(&sql, &[&vec![1_i64], &vec![json!({"1":[1,2]})]])
        .await;
    assert!(
        result.is_err(),
        "a patch with multiple values must refuse, not silently remove the company"
    );
    let err = result.unwrap_err();
    assert_eq!(err.kind(), erp_fields::ErrorKind::Database);
    assert!(
        err.message().contains("more than one row"),
        "{}",
        err.message()
    );
    drop(tx);
    h.cleanup().await.unwrap();
}
