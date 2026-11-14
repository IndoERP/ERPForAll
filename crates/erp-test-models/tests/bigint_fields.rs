// SPDX-License-Identifier: MIT OR Apache-2.0
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]
mod support;
use erp_fields::BigInt;
use erp_sdk::{Domain, SearchOptions, Vals, Value};
use erp_test_models::base_users;
use erp_testkit::Harness;
use support::{dom, one};

mod fixture {
    use erp_fields::BigInt;
    use erp_sdk::prelude::*;
    erp_sdk::addon!(name = "test_bigint", depends = ["base"]);
    #[model(name = "test.bigint", addon = "test_bigint")]
    pub struct Counter {
        #[field(bigint, required = true, default = 0)]
        pub counter: BigInt,
        #[field(bigint)]
        pub optional: Option<BigInt>,
        #[field(bigint, default = "9223372036854775807")]
        pub seed: BigInt,
    }
}

#[tokio::test]
async fn bigint_database_roundtrip_keeps_zero_and_full_width() {
    let h = Harness::builder([&base_users::ADDON, &fixture::ADDON])
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    let model = tx.env().model::<fixture::Counter>().unwrap();
    let rec = model.create(&mut tx, vec![Vals::new()]).await.unwrap();
    assert_eq!(
        rec.get(&mut tx, fixture::Counter::COUNTER)
            .await
            .unwrap()
            .get(),
        0
    );
    assert_eq!(
        rec.get(&mut tx, fixture::Counter::OPTIONAL).await.unwrap(),
        None
    );
    assert_eq!(
        rec.get(&mut tx, fixture::Counter::SEED)
            .await
            .unwrap()
            .get(),
        i64::MAX
    );
    let metadata = model
        .as_records()
        .fields_get(&mut tx, Some(&["counter"]), None)
        .await
        .unwrap();
    assert_eq!(metadata["counter"]["type"], Value::str("bigint"));
    assert_eq!(metadata["counter"]["groupable"], Value::Bool(false));
    for n in [i64::MIN, 0, 2147483648, 9007199254740993, i64::MAX] {
        rec.set(&mut tx, fixture::Counter::COUNTER, BigInt::from_i64(n))
            .await
            .unwrap();
        tx.flush().await.unwrap();
        tx.invalidate_records(rec.as_records(), Some(&["counter"]))
            .await
            .unwrap();
        assert_eq!(
            rec.get(&mut tx, fixture::Counter::COUNTER)
                .await
                .unwrap()
                .get(),
            n
        );
        assert_eq!(
            rec.value(&mut tx, "counter").await.unwrap(),
            Value::BigInt(BigInt::from_i64(n))
        );
        assert_eq!(
            one(&mut tx, "SELECT counter::text FROM test_bigint")
                .await
                .unwrap(),
            n.to_string()
        );
    }
    rec.set(
        &mut tx,
        fixture::Counter::OPTIONAL,
        Some(BigInt::from_i64(0)),
    )
    .await
    .unwrap();
    tx.flush().await.unwrap();
    assert_eq!(
        one(&mut tx, "SELECT optional::text FROM test_bigint")
            .await
            .as_deref(),
        Some("0")
    );
    let copied = rec.copy(&mut tx, None).await.unwrap();
    assert_eq!(
        copied
            .get(&mut tx, fixture::Counter::COUNTER)
            .await
            .unwrap()
            .get(),
        i64::MAX
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn bigint_search_and_cached_predicates_agree_without_lexical_order() {
    let h = Harness::builder([&base_users::ADDON, &fixture::ADDON])
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    let model = tx.env().model::<fixture::Counter>().unwrap();
    for n in [i64::MAX, 0, -10, 2, 9007199254740993, i64::MIN] {
        let created = model
            .create(
                &mut tx,
                vec![Vals::new().set(fixture::Counter::COUNTER, BigInt::from_i64(n))],
            )
            .await
            .unwrap();
        if n != 0 {
            created
                .set(
                    &mut tx,
                    fixture::Counter::OPTIONAL,
                    Some(BigInt::from_i64(n)),
                )
                .await
                .unwrap();
        }
    }
    let all = model
        .search_with(
            &mut tx,
            &Domain::TRUE,
            &SearchOptions::default().order("counter asc"),
        )
        .await
        .unwrap();
    let values = all
        .mapped(&mut tx, fixture::Counter::COUNTER)
        .await
        .unwrap()
        .into_iter()
        .map(BigInt::get)
        .collect::<Vec<_>>();
    assert_eq!(values, [i64::MIN, -10, 0, 2, 9007199254740993, i64::MAX]);
    for json in [
        serde_json::json!([["counter", "=", "9007199254740993"]]),
        serde_json::json!([["counter", ">", "2"]]),
        serde_json::json!([["counter", "<=", "-10"]]),
        serde_json::json!([["counter", "in", ["0", "9223372036854775807"]]]),
        serde_json::json!([["counter", "not in", ["0", "-10"]]]),
        serde_json::json!([["counter", "=", false]]),
        serde_json::json!([["optional", "=", false]]),
        serde_json::json!([["optional", ">", "0"]]),
        serde_json::json!([["optional", "in", ["0", "9223372036854775807"]]]),
    ] {
        let d = dom(json);
        let sql = model
            .search_with(&mut tx, &d, &SearchOptions::default().order("counter asc"))
            .await
            .unwrap();
        let cached = all.filtered_domain(&mut tx, &d).await.unwrap();
        assert_eq!(sql.ids(), cached.ids());
    }
    let invalid = dom(serde_json::json!([["counter", "=", 9007199254740992_i64]]));
    let e = model.search(&mut tx, &invalid).await.unwrap_err();
    assert_eq!(e.kind(), erp_sdk::ErrorKind::Validation);
    assert_eq!(
        e.message(),
        "Invalid value for test.bigint.counter: 9007199254740992 in condition ('counter', 'in', OrderedSet([9007199254740992]))"
    );
    for (args, message) in [
        (
            erp_sdk::ReadGroup::new(Domain::TRUE).groupby(["counter"]),
            "test.bigint.counter: bigint grouping requires a later release",
        ),
        (
            erp_sdk::ReadGroup::new(Domain::TRUE).aggregates(["counter:sum"]),
            "test.bigint.counter: bigint aggregation requires a later release",
        ),
        (
            erp_sdk::ReadGroup::new(Domain::TRUE)
                .groupby(["id"])
                .aggregates(["__count"])
                .order("counter asc"),
            "test.bigint.counter: bigint aggregation requires a later release",
        ),
        (
            erp_sdk::ReadGroup::new(Domain::TRUE)
                .groupby(["id"])
                .aggregates(["__count"])
                .order("counter:sum desc"),
            "test.bigint.counter: bigint aggregation requires a later release",
        ),
    ] {
        let e = model
            .as_records()
            ._read_group(&mut tx, args)
            .await
            .unwrap_err();
        assert_eq!(e.kind(), erp_sdk::ErrorKind::Programming);
        assert_eq!(e.message(), message);
    }
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn bigint_json2_roundtrip_uses_decimal_strings() {
    use support::rpc::{alice, assert_error, call, harness_with, json as result};
    let h = harness_with(&[&fixture::ADDON], erp_rpc::RpcConfig::default(), false).await;
    let created = call(
        &h,
        "/json/2/test.bigint/create",
        r#"{"vals_list":{"counter":"9223372036854775807"}}"#,
        alice(),
    )
    .await;
    assert_eq!(
        created.status(),
        200,
        "{}",
        String::from_utf8_lossy(created.body())
    );
    let id = result(&created).as_i64().unwrap();
    let read = call(
        &h,
        "/json/2/test.bigint/read",
        &serde_json::json!({"ids":[id],"fields":["counter","optional"]}).to_string(),
        alice(),
    )
    .await;
    assert_eq!(read.status(), 200);
    assert_eq!(result(&read)[0]["counter"], "9223372036854775807");
    assert_eq!(result(&read)[0]["optional"], "0");
    let write = call(
        &h,
        "/json/2/test.bigint/write",
        &serde_json::json!({"ids":[id],"vals":{"counter":"-9223372036854775808"}}).to_string(),
        alice(),
    )
    .await;
    assert_eq!(write.status(), 200);
    let search = call(
        &h,
        "/json/2/test.bigint/search_read",
        r#"{"domain":[["counter","=","-9223372036854775808"]],"fields":["counter"]}"#,
        alice(),
    )
    .await;
    assert_eq!(search.status(), 200);
    assert_eq!(result(&search)[0]["counter"], "-9223372036854775808");
    let bad = call(
        &h,
        "/json/2/test.bigint/write",
        &serde_json::json!({"ids":[id],"vals":{"counter":9007199254740992_i64}}).to_string(),
        alice(),
    )
    .await;
    assert_error(
        &bad,
        422,
        "erp.exceptions.ValidationError",
        "test.bigint.counter: expected native bigint, a safe integer or canonical signed 64-bit decimal text",
    );
    let zero = call(
        &h,
        "/json/2/test.bigint/write",
        &serde_json::json!({"ids":[id],"vals":{"counter":false}}).to_string(),
        alice(),
    )
    .await;
    assert_eq!(zero.status(), 200);
    let zero_read = call(
        &h,
        "/json/2/test.bigint/read",
        &serde_json::json!({"ids":[id],"fields":["counter"]}).to_string(),
        alice(),
    )
    .await;
    assert_eq!(result(&zero_read)[0]["counter"], "0");
    h.cleanup().await.unwrap();
}
