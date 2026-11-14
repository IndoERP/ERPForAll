// SPDX-License-Identifier: MIT OR Apache-2.0
//! Exercise the shipped SQL in a non-UTC session, independently of ORM connection defaults.

use super::*;

const POLICY_MIGRATION: &str =
    include_str!("../../../../addons/stock/migrations/1.2/post-policy-rows.sql");

#[test]
fn stock_policy_migration_stamps_utc_in_a_non_utc_session() {
    run(async {
        let db = TestDb::create().await.unwrap();
        let installed = install(&db).await;
        let client = db.client().await.unwrap();
        client.batch_execute("BEGIN;
            SET LOCAL TIME ZONE 'Asia/Jakarta';
            DROP TRIGGER stock_location_policy_insert ON stock_location;
            UPDATE stock_location_policy SET forbid_negative=true,
                create_date=TIMESTAMP '2001-02-03 04:05:06',write_date=TIMESTAMP '2002-03-04 05:06:07';").await.unwrap();
        let kept: String = client
            .query_one(
                "SELECT json_agg(p ORDER BY id)::text FROM stock_location_policy p",
                &[],
            )
            .await
            .unwrap()
            .get(0);
        let gap: i64 = client.query_one("INSERT INTO stock_location(name,usage,active) VALUES ('Non-UTC legacy gap','view',false) RETURNING id", &[]).await.unwrap().get(0);
        assert_eq!(
            client
                .query_one(
                    "SELECT count(*) FROM stock_location_policy WHERE location_id=$1",
                    &[&gap]
                )
                .await
                .unwrap()
                .get::<_, i64>(0),
            0
        );
        client.batch_execute(POLICY_MIGRATION).await.unwrap();
        let row = client
            .query_one(
                "SELECT forbid_negative,create_uid,write_uid,
            create_date=(now() AT TIME ZONE 'UTC') AND write_date=(now() AT TIME ZONE 'UTC'),
            create_date<>now()::timestamp AND write_date<>now()::timestamp
            FROM stock_location_policy WHERE location_id=$1",
                &[&gap],
            )
            .await
            .unwrap();
        assert!(!row.get::<_, bool>(0));
        assert_eq!(row.get::<_, Option<i64>>(1), None);
        assert_eq!(row.get::<_, Option<i64>>(2), None);
        assert_eq!(
            row.get::<_, Option<bool>>(3),
            Some(true),
            "audit stamps equal transaction UTC"
        );
        assert_eq!(
            row.get::<_, Option<bool>>(4),
            Some(true),
            "audit stamps differ from local wall time"
        );
        assert_eq!(client.query_one("SELECT json_agg(p ORDER BY id)::text FROM stock_location_policy p WHERE location_id<>$1", &[&gap]).await.unwrap().get::<_, String>(0), kept);
        let once: String = client
            .query_one(
                "SELECT json_agg(p ORDER BY id)::text FROM stock_location_policy p",
                &[],
            )
            .await
            .unwrap()
            .get(0);
        client.batch_execute(POLICY_MIGRATION).await.unwrap();
        assert_eq!(
            client
                .query_one(
                    "SELECT json_agg(p ORDER BY id)::text FROM stock_location_policy p",
                    &[]
                )
                .await
                .unwrap()
                .get::<_, String>(0),
            once
        );
        client.batch_execute("ROLLBACK").await.unwrap();
        drop(client);
        drop(installed);
        db.cleanup().await.unwrap();
    });
}
