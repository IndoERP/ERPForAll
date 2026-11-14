// SPDX-License-Identifier: MIT OR Apache-2.0
//! Technical parameter mutations enforce the generic partition upkeep cadence.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod auth_support;
use auth_support::{builder, map, prepare, s};
use erp_sdk::{Error, ErrorKind, Value};

const KEY: &str = "base.partition_upkeep_interval_secs";
const INVALID: &str = "base.partition_upkeep_interval_secs must be an integer from 60 to 3600.";

fn refused(error: Error) {
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(error.message(), INVALID);
}

#[tokio::test]
async fn upkeep_cadence_rejects_invalid_technical_mutations() {
    let h = builder().build().await.unwrap();
    prepare(&h).await;
    let oversized = "9".repeat(128);
    for value in [
        "0",
        "-1",
        "59",
        "3601",
        "1.5",
        "no",
        "9223372036854775808",
        &oversized,
    ] {
        let mut tx = h.begin().await.unwrap();
        let params = tx.env().records("ir.config_parameter").unwrap();
        let error = params
            .create_values(&mut tx, &[map(&[("key", s(KEY)), ("value", s(value))])])
            .await
            .unwrap_err();
        refused(error);
        let rows = tx
            .query(
                "SELECT count(*)::int8 FROM ir_config_parameter WHERE key=$1",
                &[&KEY],
            )
            .await
            .unwrap();
        assert_eq!(rows[0].get::<_, i64>(0), 0);
        tx.rollback().await.unwrap();
        drop(tx);

        let mut tx = h.begin().await.unwrap();
        let params = tx.env().records("ir.config_parameter").unwrap();
        let row = params
            .create_values(&mut tx, &[map(&[("key", s(KEY)), ("value", s("60"))])])
            .await
            .unwrap();
        refused(
            row.write_values(&mut tx, &map(&[("value", s(value))]))
                .await
                .unwrap_err(),
        );
        let rows = tx
            .query(
                "SELECT value FROM ir_config_parameter WHERE key=$1",
                &[&KEY],
            )
            .await
            .unwrap();
        assert_eq!(rows[0].get::<_, String>(0), "60");
        tx.rollback().await.unwrap();
        drop(tx);

        let mut tx = h.begin().await.unwrap();
        let params = tx.env().records("ir.config_parameter").unwrap();
        let row = params
            .create_values(
                &mut tx,
                &[map(&[
                    ("key", s("test.upkeep_cadence")),
                    ("value", s(value)),
                ])],
            )
            .await
            .unwrap();
        refused(
            row.write_values(&mut tx, &map(&[("key", s(KEY))]))
                .await
                .unwrap_err(),
        );
        let rows = tx
            .query(
                "SELECT key FROM ir_config_parameter WHERE id=$1",
                &[&row.ids()[0]],
            )
            .await
            .unwrap();
        assert_eq!(rows[0].get::<_, String>(0), "test.upkeep_cadence");
        tx.rollback().await.unwrap();
    }
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn upkeep_cadence_accepts_boundaries_and_parameter_resets() {
    let h = builder().build().await.unwrap();
    prepare(&h).await;
    for value in [s("60"), s("3600"), s(""), Value::Bool(false)] {
        let mut tx = h.begin().await.unwrap();
        let params = tx.env().records("ir.config_parameter").unwrap();
        let row = params
            .create_values(
                &mut tx,
                &[map(&[("key", s(KEY)), ("value", value.clone())])],
            )
            .await
            .unwrap();
        for next in [s("3600"), s("60"), s(""), Value::Bool(false)] {
            row.write_values(&mut tx, &map(&[("value", next)]))
                .await
                .unwrap();
        }
        // a declared key is protected: clearing the value resets it, deleting is refused
        let refusal = row.unlink(&mut tx).await.unwrap_err();
        assert_eq!(refusal.kind(), ErrorKind::Validation);
        assert_eq!(
            refusal.message(),
            format!("You cannot delete the {KEY} record.")
        );
        let rows = tx
            .query(
                "SELECT count(*)::int8 FROM ir_config_parameter WHERE key=$1",
                &[&KEY],
            )
            .await
            .unwrap();
        assert_eq!(rows[0].get::<_, i64>(0), 1);
        tx.rollback().await.unwrap();
    }
    h.cleanup().await.unwrap();
}
