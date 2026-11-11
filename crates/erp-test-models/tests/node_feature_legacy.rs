//! Exact historical ERPForAll statements under an actual heartbeat LOGIN role.
//! The frozen constants retain their source revision. This is SQL emulation, not an old binary.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]
#[path = "auth_rollout_support/legacy_node_sql.rs"]
mod old;
use erp_testkit::Harness;
use tokio_postgres::{Client, Row, types::ToSql};

async fn register(client: &Client, id: &str) -> Result<Vec<Row>, tokio_postgres::Error> {
    let digest = &[1_u8; 32][..];
    let kinds: Vec<String> = vec![];
    let payloads: Vec<Vec<u8>> = vec![];
    let lease = 40000_i64;
    let interval = 10000_i64;
    let values: [&(dyn ToSql + Sync); 15] = [
        &id, &"legacy", &"host", &1_i32, &"app", &false, &digest, &digest, &digest, &digest,
        &"legacy", &lease, &kinds, &payloads, &interval,
    ];
    let params: Vec<_> = values.into_iter().zip(old::REGISTER_TYPES).collect();
    client.query_typed(old::REGISTER_SQL, &params).await
}
async fn renew(
    client: &Client,
    id: &str,
    registered: i64,
) -> Result<Vec<Row>, tokio_postgres::Error> {
    let digest = &[1_u8; 32][..];
    let lease = 40000_i64;
    let interval = 10000_i64;
    let reason: Option<String> = None;
    let beat = 1_i64;
    let values: [&(dyn ToSql + Sync); 17] = [
        &id,
        &"legacy",
        &"host",
        &1_i32,
        &"app",
        &false,
        &digest,
        &digest,
        &digest,
        &digest,
        &"legacy",
        &registered,
        &"ready",
        &reason,
        &lease,
        &beat,
        &interval,
    ];
    let params: Vec<_> = values.into_iter().zip(old::RENEW_TYPES).collect();
    client.query_typed(old::RENEW_SQL, &params).await
}
#[tokio::test]
async fn historical_statements_and_departures_obey_the_database_gate() {
    let h = Harness::builder([&erp_test_models::base_users::ADDON])
        .build()
        .await
        .unwrap();
    let admin = h.db().client().await.unwrap();
    let m = format!("lm_{}", h.db().name());
    let r = format!("lr_{}", h.db().name());
    let b = format!("lb_{}", h.db().name());
    let id: String = admin
        .query_one("SELECT uuidv7()::text", &[])
        .await
        .unwrap()
        .get(0);
    let registered: i64 = register(&admin, &id).await.unwrap()[0].get(0);
    let mut tx = h.begin().await.unwrap();
    tx.batch(&format!("CREATE ROLE {} NOLOGIN;CREATE ROLE {} NOLOGIN;CREATE ROLE {} LOGIN PASSWORD 'fixture-legacy-heartbeat-password' NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION",erp_sql::quote(&m),erp_sql::quote(&r),erp_sql::quote(&b))).await.unwrap();
    tx.grant_node_role(&b).await.unwrap();
    tx.grant_node_feature_roles(&m, &r).await.unwrap();
    // Trusted fixture state intentionally tests an already-present unsupported row; no real cutover is claimed.
    tx.batch("INSERT INTO erp_node_feature(key,version,enabled,generation,enrollment_confirmed,enrollment_actor,enrollment_at) VALUES('auth.login_principal',1,true,1,true,current_user,clock_timestamp())").await.unwrap();
    tx.commit().await.unwrap();
    let before: (i64, String) = {
        let row = admin
            .query_one(
                "SELECT beat,expires_at::text FROM erp_node WHERE id=$1::text::uuid",
                &[&id],
            )
            .await
            .unwrap();
        (row.get(0), row.get(1))
    };
    let mut config = h.db().config();
    config
        .user(&b)
        .password("fixture-legacy-heartbeat-password");
    let pool =
        erp_orm::node::heartbeat_pool(config, &erp_orm::node::NodeOptions::default()).unwrap();
    let client = pool.get().await.unwrap();
    let role = client
        .query_one("SELECT current_user::text,session_user::text", &[])
        .await
        .unwrap();
    assert_eq!(role.get::<_, String>(0), b);
    assert_eq!(role.get::<_, String>(1), b);
    let new_id: String = admin
        .query_one("SELECT uuidv7()::text", &[])
        .await
        .unwrap()
        .get(0);
    let missing: String = admin
        .query_one("SELECT uuidv7()::text", &[])
        .await
        .unwrap()
        .get(0);
    let mut outcomes = Vec::new();
    for operation in 0..4 {
        client
            .batch_execute("BEGIN ISOLATION LEVEL READ COMMITTED")
            .await
            .unwrap();
        let result = match operation {
            0 => register(&client, &new_id).await,
            1 => {
                client
                    .query_typed(
                        old::ADOPT_SQL,
                        &[
                            (&id as &(dyn ToSql + Sync), old::ID_LEASE_TYPES[0].clone()),
                            (&40000_i64, old::ID_LEASE_TYPES[1].clone()),
                        ],
                    )
                    .await
            }
            2 => renew(&client, &id, registered).await,
            3 => renew(&client, &missing, registered).await,
            _ => unreachable!(),
        };
        client.batch_execute("ROLLBACK").await.unwrap();
        outcomes.push(result);
    }
    let after = admin
        .query_one(
            "SELECT beat,expires_at::text FROM erp_node WHERE id=$1::text::uuid",
            &[&id],
        )
        .await
        .unwrap();
    assert_eq!((after.get::<_, i64>(0), after.get::<_, String>(1)), before);
    for state in ["stopping", "stopped"] {
        client
            .batch_execute("BEGIN ISOLATION LEVEL READ COMMITTED")
            .await
            .unwrap();
        let changed=client.execute("UPDATE erp_node SET state=$2,expires_at=least(expires_at,clock_timestamp()) WHERE id=$1::text::uuid",&[&id,&state]).await.unwrap();
        client.batch_execute("COMMIT").await.unwrap();
        assert_eq!(changed, 1);
    }
    client
        .batch_execute("BEGIN ISOLATION LEVEL READ COMMITTED")
        .await
        .unwrap();
    let restore = renew(&client, &id, registered).await;
    client.batch_execute("ROLLBACK").await.unwrap();
    outcomes.push(restore);
    drop(client);
    pool.close();
    let mut cleanup = h.begin().await.unwrap();
    cleanup
        .batch(&format!(
            "DROP OWNED BY {},{},{} CASCADE;DROP ROLE {},{},{}",
            erp_sql::quote(&m),
            erp_sql::quote(&r),
            erp_sql::quote(&b),
            erp_sql::quote(&m),
            erp_sql::quote(&r),
            erp_sql::quote(&b)
        ))
        .await
        .unwrap();
    cleanup.commit().await.unwrap();
    for result in outcomes {
        let e = result.unwrap_err();
        let e = e.as_db_error().unwrap();
        assert_eq!(
            (e.code().code(), e.message()),
            (
                "ERF01",
                "node feature auth.login_principal refuses unapproved digest"
            )
        );
    }
    assert_eq!(
        admin
            .query_one("SELECT count(*) FROM erp_node", &[])
            .await
            .unwrap()
            .get::<_, i64>(0),
        1
    );
}
