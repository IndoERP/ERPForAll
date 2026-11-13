//! RPC exposure and binding tests for `load`, `export_data`, and `get_import_templates`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use support::currency_fixture;

mod support;

use erp_orm::{
    AccessDomain, AccessPolicy, Capability, ExplainRequest, Explanation, FieldMask, GroupId,
    GroupSet, InternedGroups, ModelAllow, Operation, PolicyGeneration, PrincipalBinding,
    SnapshotRef,
};
use erp_registry::ModelSpec;
use erp_rpc::RpcConfig;
use erp_sdk::Result;
use erp_testkit::{Harness, RpcHarness};
use serde_json::json;
use std::sync::Arc;
use support::rpc::*;

/// Fixture model rules combined with actual named-group membership checks.
struct ImpexPolicy {
    rules: erp_testkit::RulesPolicy<FixturePolicy>,
    groups: erp_testkit::FieldPolicy,
}

impl AccessPolicy for ImpexPolicy {
    fn group_refs_are_stable(&self) -> bool {
        self.rules.group_refs_are_stable()
    }

    fn generation(&self) -> PolicyGeneration {
        self.rules.generation()
    }
    fn snapshot(&self) -> SnapshotRef {
        self.rules.snapshot()
    }
    fn needs_resolved_principals(&self) -> bool {
        self.rules.needs_resolved_principals()
    }
    fn intern_groups(&self, groups: &GroupSet) -> Result<InternedGroups> {
        self.rules.intern_groups(groups)
    }
    fn check_model(
        &self,
        b: &PrincipalBinding<'_>,
        model: &ModelSpec,
        op: Operation,
    ) -> Result<()> {
        self.rules.check_model(b, model, op)
    }
    fn access_domain(
        &self,
        b: &PrincipalBinding<'_>,
        model: &ModelSpec,
        op: Operation,
    ) -> Result<AccessDomain> {
        self.rules.access_domain(b, model, op)
    }
    fn field_access(&self, b: &PrincipalBinding<'_>, model: &ModelSpec) -> Result<Arc<FieldMask>> {
        self.rules.field_access(b, model)
    }
    fn groups(&self, direct: &[GroupId]) -> Result<GroupSet> {
        self.rules.groups(direct)
    }
    fn allowing_groups(&self, model: &ModelSpec, op: Operation) -> Result<Vec<GroupId>> {
        self.rules.allowing_groups(model, op)
    }
    fn has_capability(&self, b: &PrincipalBinding<'_>, cap: &Capability) -> Result<bool> {
        self.rules.has_capability(b, cap)
    }
    fn explain(&self, req: &ExplainRequest<'_>) -> Result<Explanation> {
        self.rules.explain(req)
    }
    fn has_groups(&self, b: &PrincipalBinding<'_>, spec: &str) -> Result<bool> {
        self.groups.has_groups(b, spec)
    }
}

/// Helper to build an RPC harness with test_impex and ir_model_data declared.
async fn impex_harness() -> RpcHarness {
    impex_harness_with_groups(true).await
}

async fn impex_harness_with_groups(define_groups: bool) -> RpcHarness {
    impex_harness_config(define_groups, RpcConfig::default()).await
}

async fn impex_harness_config(define_groups: bool, config: RpcConfig) -> RpcHarness {
    let addons = [
        &erp_test_models::base_users::ADDON,
        &erp_test_models::test_orm::ADDON,
        &erp_test_models::test_rpc::ADDON,
        &erp_test_models::test_impex::ADDON,
        &erp_data::model_data::ADDON,
    ];
    let hb = Harness::builder(addons)
        .with_static_decls(crate::currency_fixture::decls())
        .with_decls([erp_data::ir_model_data_decl()]);
    let groups = erp_testkit::FieldPolicy::new();
    let groups = if define_groups {
        groups
            .member(ALICE, ["base.group_allow_export"])
            .member(ADMIN, ["base.group_erp_manager"])
            .group_name("base.group_system", "Settings")
    } else {
        groups
    };
    let policy = ImpexPolicy {
        rules: erp_testkit::RulesPolicy::new(FixturePolicy).unwrap(),
        groups,
    };
    let h = RpcHarness::builder(hb, policy)
        .host(|_orm| FixtureHost)
        .config(config)
        .build()
        .await
        .expect("rpc impex harness");

    let mut tx = h.harness().begin().await.unwrap();
    tx.batch(
        "INSERT INTO res_users (id, name, login, active) VALUES (2, 'alice', 'alice', true), \
         (3, 'bob', 'bob', true), (4, 'admin', 'admin', true) ON CONFLICT (id) DO NOTHING; \
         SELECT setval(pg_get_serial_sequence('res_users', 'id'), 4)",
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    h
}

/// Helper for read-only API key.
fn reader_key() -> erp_rpc::Caller {
    alice_key(scope(10, "reader", ModelAllow::All, true))
}

#[tokio::test]
async fn test_rpc_load_success_and_shape() {
    let h = impex_harness().await;

    let body = json!({
        "fields": ["value"],
        "data": [["100"], ["200"]]
    });
    let r = call(
        &h,
        "/json/2/export.integer/load",
        &body.to_string(),
        alice(),
    )
    .await;
    let resp = ok(&r);

    let ids = resp["ids"].as_array().expect("ids should be an array");
    assert_eq!(ids.len(), 2);
    assert_eq!(resp["messages"], json!([]));
    assert_eq!(resp["nextrow"], json!(0));

    // Verify records exist in database
    let mut tx = h.harness().begin().await.unwrap();
    let records = tx
        .env()
        .records("export.integer")
        .unwrap()
        .browse([ids[0].as_i64().unwrap(), ids[1].as_i64().unwrap()])
        .unwrap();
    let vals = records.read(&mut tx, &["value"]).await.unwrap();
    assert_eq!(vals.len(), 2);
    assert_eq!(vals[0].get("value"), Some(&erp_sdk::Value::Int(100)));
    assert_eq!(vals[1].get("value"), Some(&erp_sdk::Value::Int(200)));

    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn test_rpc_export_data_success_and_shape() {
    let h = impex_harness().await;

    // Create a record to export
    let mut tx = h.harness().begin().await.unwrap();
    let created = tx
        .env()
        .records("export.integer")
        .unwrap()
        .create(
            &mut tx,
            vec![erp_orm::RawVals::new().set_cell("value", erp_fields::Cell::Int(42))],
        )
        .await
        .unwrap();
    let id = created.ids()[0];
    tx.commit().await.unwrap();

    let body = json!({
        "ids": [id],
        "fields_to_export": ["value"]
    });
    let r = call(
        &h,
        "/json/2/export.integer/export_data",
        &body.to_string(),
        alice(),
    )
    .await;
    let resp = ok(&r);

    assert_eq!(resp["datas"], json!([[42]]));

    let manager = call(
        &h,
        "/json/2/export.integer/export_data",
        &body.to_string(),
        user(ADMIN),
    )
    .await;
    assert_eq!(ok(&manager)["datas"], json!([[42]]));

    // Export without ids exports empty datas
    let empty_body = json!({
        "fields_to_export": ["value"]
    });
    let r_empty = call(
        &h,
        "/json/2/export.integer/export_data",
        &empty_body.to_string(),
        alice(),
    )
    .await;
    assert_eq!(ok(&r_empty)["datas"], json!([]));

    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn test_rpc_get_import_templates() {
    let h = impex_harness().await;

    let r = call(
        &h,
        "/json/2/export.integer/get_import_templates",
        "{}",
        alice(),
    )
    .await;
    assert_eq!(ok(&r), json!([]));

    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn test_rpc_impex_refused_under_restricting_policy() {
    let h = impex_harness().await;

    // 1. Read-only API key is refused on all three (they have effect = 'write')
    let r_load = call(
        &h,
        "/json/2/export.integer/load",
        r#"{"fields": ["value"], "data": [["1"]]}"#,
        reader_key(),
    )
    .await;
    assert_error(
        &r_load,
        403,
        "erp.exceptions.AccessError",
        "The API key 'reader' is read-only: it may not call 'export.integer/load' (effect 'write').",
    );

    let r_export = call(
        &h,
        "/json/2/export.integer/export_data",
        r#"{"fields_to_export": ["value"]}"#,
        reader_key(),
    )
    .await;
    assert_error(
        &r_export,
        403,
        "erp.exceptions.AccessError",
        "The API key 'reader' is read-only: it may not call 'export.integer/export_data' (effect 'write').",
    );

    let r_tmpl = call(
        &h,
        "/json/2/export.integer/get_import_templates",
        "{}",
        reader_key(),
    )
    .await;
    assert_error(
        &r_tmpl,
        403,
        "erp.exceptions.AccessError",
        "The API key 'reader' is read-only: it may not call 'export.integer/get_import_templates' (effect 'write').",
    );

    // 2. User BOB without export rights is refused on export_data
    let r_bob_export = call(
        &h,
        "/json/2/export.integer/export_data",
        r#"{"fields_to_export": ["value"]}"#,
        user(BOB),
    )
    .await;
    assert_error(
        &r_bob_export,
        422,
        "erp.exceptions.UserError",
        "You don't have the rights to export data. Please contact an Administrator.",
    );

    // 3. User BOB cannot create records on export.integer (load returns error in messages)
    let r_bob_load = call(
        &h,
        "/json/2/export.integer/load",
        r#"{"fields": ["value"], "data": [["1"]]}"#,
        user(BOB),
    )
    .await;
    let resp = ok(&r_bob_load);
    assert_eq!(resp["ids"], json!(false));
    let msgs = resp["messages"].as_array().expect("messages array");
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0]["type"], "error");
    assert_eq!(
        msgs[0]["message"],
        "You are not allowed to create 'Export: integer' (export.integer) records."
    );

    h.cleanup().await.unwrap();
}

/// Parameter validation and bad parameters (missing, wrong types, ids on model methods).
#[tokio::test]
async fn test_rpc_impex_bad_params() {
    let h = impex_harness().await;

    // load missing fields
    let r = call(
        &h,
        "/json/2/export.integer/load",
        r#"{"data": []}"#,
        alice(),
    )
    .await;
    assert_error(
        &r,
        422,
        "werkzeug.exceptions.UnprocessableEntity",
        "missing a required argument: 'fields'",
    );

    // load missing data
    let r = call(
        &h,
        "/json/2/export.integer/load",
        r#"{"fields": ["value"]}"#,
        alice(),
    )
    .await;
    assert_error(
        &r,
        422,
        "werkzeug.exceptions.UnprocessableEntity",
        "missing a required argument: 'data'",
    );

    // load with ids (load is @api.model, ids not allowed)
    let r = call(
        &h,
        "/json/2/export.integer/load",
        r#"{"ids": [1], "fields": ["value"], "data": []}"#,
        alice(),
    )
    .await;
    assert_error(
        &r,
        422,
        "werkzeug.exceptions.UnprocessableEntity",
        "cannot call export.integer.load with ids",
    );

    // load fields wrong type
    let r = call(
        &h,
        "/json/2/export.integer/load",
        r#"{"fields": 123, "data": []}"#,
        alice(),
    )
    .await;
    assert_error(
        &r,
        422,
        "werkzeug.exceptions.UnprocessableEntity",
        "argument 'fields' of export.integer.load: expected an array, got integer",
    );

    // export_data missing fields_to_export
    let r = call(&h, "/json/2/export.integer/export_data", "{}", alice()).await;
    assert_error(
        &r,
        422,
        "werkzeug.exceptions.UnprocessableEntity",
        "missing a required argument: 'fields_to_export'",
    );

    // export_data wrong type for fields_to_export
    let r = call(
        &h,
        "/json/2/export.integer/export_data",
        r#"{"fields_to_export": "not-an-array"}"#,
        alice(),
    )
    .await;
    assert_error(
        &r,
        422,
        "werkzeug.exceptions.UnprocessableEntity",
        "argument 'fields_to_export' of export.integer.export_data: expected an array, got string",
    );

    for (body, message) in [
        (
            r#"{"extra": 1}"#,
            "got an unexpected keyword argument 'extra'",
        ),
        (
            r#"{"ids": [1]}"#,
            "cannot call export.integer.get_import_templates with ids",
        ),
    ] {
        let r = call(
            &h,
            "/json/2/export.integer/get_import_templates",
            body,
            alice(),
        )
        .await;
        assert_error(&r, 422, "werkzeug.exceptions.UnprocessableEntity", message);
    }

    h.cleanup().await.unwrap();
}

/// Malformed rows fail loudly in load.
#[tokio::test]
async fn test_rpc_load_malformed_rows_fail_loudly() {
    let h = impex_harness().await;

    // Row is an integer instead of array
    let r = call(
        &h,
        "/json/2/export.integer/load",
        r#"{"fields": ["value"], "data": [42]}"#,
        alice(),
    )
    .await;
    assert_error(
        &r,
        422,
        "erp.exceptions.ValidationError",
        "load row 0 must be an array",
    );

    // Row is a string instead of array
    let r = call(
        &h,
        "/json/2/export.integer/load",
        r#"{"fields": ["value"], "data": ["not-a-row"]}"#,
        alice(),
    )
    .await;
    assert_error(
        &r,
        422,
        "erp.exceptions.ValidationError",
        "load row 0 must be an array",
    );

    // Cell in row is an integer instead of string
    let r = call(
        &h,
        "/json/2/export.integer/load",
        r#"{"fields": ["value"], "data": [[42]]}"#,
        alice(),
    )
    .await;
    assert_error(
        &r,
        422,
        "erp.exceptions.ValidationError",
        "load cell at row 0, column 0 must be a string",
    );

    // Row length mismatch: row has 2 columns, fields has 1 -> returns error message in messages
    let r = call(
        &h,
        "/json/2/export.integer/load",
        r#"{"fields": ["value"], "data": [["1", "2"]]}"#,
        alice(),
    )
    .await;
    let resp = ok(&r);
    assert_eq!(resp["ids"], json!(false));
    let msgs = resp["messages"].as_array().expect("messages array");
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0]["type"], "error");
    assert_eq!(
        msgs[0]["message"],
        "This row has 2 cells but there are 1 columns"
    );

    h.cleanup().await.unwrap();
}

/// Missing group definitions must never activate the old uid 2/4 export bypass.
#[tokio::test]
async fn test_rpc_export_propagates_missing_group_errors() {
    let h = impex_harness_with_groups(false).await;
    for uid in [ALICE, ADMIN] {
        let r = call(
            &h,
            "/json/2/export.integer/export_data",
            r#"{"fields_to_export": ["value"]}"#,
            user(uid),
        )
        .await;
        assert_error(
            &r,
            500,
            "erp.programming",
            "FieldPolicy does not know the group 'base.group_allow_export' in 'base.group_allow_export,base.group_erp_manager'",
        );
    }
    h.cleanup().await.unwrap();
}

/// Input column and export-field bounds reject one above the accepted count.
#[tokio::test]
async fn test_rpc_impex_column_count_limits() {
    let h = impex_harness().await;
    let fields = vec!["value"; 1001];
    for (method, body, message) in [
        (
            "load",
            json!({"fields": fields, "data": []}),
            "load received 1001 columns; at most 1000 are accepted per call",
        ),
        (
            "export_data",
            json!({"fields_to_export": fields}),
            "export_data received 1001 fields; at most 1000 are accepted per call",
        ),
    ] {
        let r = call(
            &h,
            &format!("/json/2/export.integer/{method}"),
            &body.to_string(),
            alice(),
        )
        .await;
        assert_error(&r, 422, "erp.exceptions.ValidationError", message);
    }
    h.cleanup().await.unwrap();
}

/// The configured request bound limits individual cells and aggregate import bytes.
#[tokio::test]
async fn test_rpc_load_configured_body_byte_limit() {
    let config = RpcConfig::builder().body_bytes(65_536).build().unwrap();
    let h = impex_harness_config(true, config).await;
    let prefix = r#"{"fields":["value"],"data":[[""#;
    let suffix = r#""]]}"#;
    let exact = format!(
        "{prefix}{}{suffix}",
        "1".repeat(65_536 - prefix.len() - suffix.len())
    );
    assert_eq!(exact.len(), 65_536);
    // At the bound the binder runs; the integer conversion failure is a row result.
    let accepted = call(&h, "/json/2/export.integer/load", &exact, alice()).await;
    let accepted = ok(&accepted);
    assert_eq!(accepted["ids"], json!(false));
    let messages = accepted["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0]["type"], "error");
    assert_eq!(messages[0]["message"], "integer out of range\n");

    let oversized = format!(
        "{prefix}{}{suffix}",
        "1".repeat(65_537 - prefix.len() - suffix.len())
    );
    assert_eq!(oversized.len(), 65_537);
    let aggregate =
        json!({"fields": ["value"], "data": [["1".repeat(33_000)], ["2".repeat(33_000)]]})
            .to_string();
    assert!(aggregate.len() > 65_536);
    for body in [oversized, aggregate] {
        let r = call(&h, "/json/2/export.integer/load", &body, alice()).await;
        assert_error(
            &r,
            413,
            "werkzeug.exceptions.RequestEntityTooLarge",
            "The request body is larger than the server limit of 65536 bytes (limits.rpc_body_bytes).",
        );
    }
    h.cleanup().await.unwrap();
}

/// Committed imports remain visible across independent service/ORM/pool instances.
#[tokio::test]
async fn test_rpc_import_export_shared_database_instances() {
    let h = impex_harness().await;
    let make_service = || {
        let pool = erp_orm::pool::create_pool(h.harness().db().config(), 4).unwrap();
        let policy = ImpexPolicy {
            rules: erp_testkit::RulesPolicy::new(FixturePolicy).unwrap(),
            groups: erp_testkit::FieldPolicy::new()
                .member(ALICE, ["base.group_allow_export"])
                .member(ADMIN, ["base.group_erp_manager"])
                .group_name("base.group_system", "Settings"),
        };
        let orm = erp_orm::Orm::builder(Arc::clone(h.orm().registry()), pool)
            .policy(policy)
            .build()
            .unwrap();
        erp_rpc::RpcService::new(orm, FixtureHost, RpcConfig::default())
    };
    let service_a = make_service();
    let service_b = make_service();
    let loaded = service_a
        .call(erp_rpc::RpcCall::json2(
            "/json/2/export.integer/load",
            br#"{"fields":["value"],"data":[["137"],["251"]]}"#.to_vec(),
            alice(),
        ))
        .await;
    let result = ok(&loaded);
    assert_eq!(result["messages"], json!([]));
    assert_eq!(result["nextrow"], json!(0));
    let ids = result["ids"].as_array().expect("imported record ids");
    assert_eq!(ids.len(), 2);
    let export_body = json!({"ids": ids, "fields_to_export": ["value"]}).to_string();
    let exported = service_b
        .call(erp_rpc::RpcCall::json2(
            "/json/2/export.integer/export_data",
            export_body.as_bytes().to_vec(),
            alice(),
        ))
        .await;
    assert_eq!(ok(&exported), json!({"datas": [[137], [251]]}));

    drop(service_a);
    let after_drop = service_b
        .call(erp_rpc::RpcCall::json2(
            "/json/2/export.integer/export_data",
            export_body.as_bytes().to_vec(),
            alice(),
        ))
        .await;
    assert_eq!(ok(&after_drop), json!({"datas": [[137], [251]]}));
    drop(service_b);
    h.cleanup().await.unwrap();
}
