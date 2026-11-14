// SPDX-License-Identifier: MIT OR Apache-2.0
//! Real declared-condition failures through RPC, including terminal classification and rollback.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]
mod support;

use erp_sdk::prelude::*;
use erp_sdk::sql_objects::{SqlConditionDecl, SqlConditionKind, SqlObjectDecl, SqlObjectManifest};
use erp_testkit::{Harness, RpcHarness, RulesPolicy, SchemaSyncOptions};
use support::rpc::{FixtureHost, FixturePolicy, alice, call, json, sql_batch, user};

erp_sdk::addon!(name = "test_sqcond_rpc", depends = ["base_users"]);

#[model(name = "test_sqcond_rpc.probe", addon = "test_sqcond_rpc")]
pub struct Probe {
    #[field(char)]
    pub name: Option<Arc<str>>,
}

#[methods(Probe, addon = "test_sqcond_rpc")]
impl Probe {
    #[method]
    #[rpc(effect = write, model)]
    async fn raise_condition(_this: &RecordSet<Probe>, tx: &mut Transaction) -> Result<()> {
        tx.execute(
            "INSERT INTO test_sqcond_rpc_probe(name) VALUES ('rolled back')",
            &[],
        )
        .await?;
        tx.query("SELECT public.sqcond_rpc_raise()", &[]).await?;
        Err(Error::internal("the fixture condition did not raise"))
    }
}

#[erp_sdk::sql_objects::linkme::distributed_slice(erp_sdk::sql_objects::SQL_OBJECT_DECLS)]
#[linkme(crate = erp_sdk::sql_objects::linkme)]
static CONDITION: SqlObjectDecl = SqlObjectDecl::condition(
    "test_sqcond_rpc",
    "immutable",
    SqlConditionDecl::new("ERP10", "sqcond_rpc_immutable", "private declaration text")
        .kind(SqlConditionKind::Programming),
);
static MANIFEST: SqlObjectManifest = SqlObjectManifest::new("test_sqcond_rpc", &["immutable"]);

#[tokio::test]
async fn declared_programming_condition_is_terminal_and_safe_over_rpc() {
    let h = RpcHarness::builder(
        Harness::builder([&erp_test_models::base_users::ADDON, &ADDON])
            .require_sql_objects([&MANIFEST])
            .schema_sync_options(SchemaSyncOptions::inline().lifecycle(true)),
        RulesPolicy::new(FixturePolicy).unwrap(),
    )
    .host(|_| FixtureHost)
    .config(
        erp_rpc::RpcConfig::builder()
            .error_debug(true)
            .build()
            .unwrap(),
    )
    .build()
    .await
    .unwrap();
    sql_batch(&h, "INSERT INTO res_users(id,name,login,active) VALUES (2,'alice','alice',true),(4,'admin','admin',true);
        CREATE FUNCTION public.sqcond_rpc_raise() RETURNS void LANGUAGE plpgsql AS $$
        BEGIN RAISE EXCEPTION 'private row marker' USING ERRCODE='ERP10',
        CONSTRAINT='sqcond_rpc_immutable', DETAIL='private detail marker', HINT='private hint marker'; END $$;").await;
    for caller in [alice(), user(4)] {
        let response = call(
            &h,
            "/json/2/test_sqcond_rpc.probe/raise_condition",
            "{}",
            caller,
        )
        .await;
        assert_eq!(response.status(), 500);
        assert_eq!(response.attempts(), 1);
        assert_eq!(response.retry_after(), None);
        let body = json(&response);
        assert_eq!(body["name"], "erp.programming");
        assert_eq!(body["message"], "Internal server error");
        assert_eq!(
            body["arguments"],
            serde_json::json!(["Internal server error"])
        );
        assert_eq!(body["context"], serde_json::json!({}));
        let text = std::str::from_utf8(response.body()).unwrap();
        for hidden in [
            "private row marker",
            "private detail marker",
            "private hint marker",
            "private declaration text",
        ] {
            assert!(!text.contains(hidden), "leaked {hidden}: {text}");
        }
        let mut tx = h.harness().begin().await.unwrap();
        assert_eq!(
            tx.query("SELECT count(*) FROM test_sqcond_rpc_probe", &[])
                .await
                .unwrap()[0]
                .get::<_, i64>(0),
            0
        );
        tx.rollback().await.unwrap();
    }
    h.cleanup().await.unwrap();
}
