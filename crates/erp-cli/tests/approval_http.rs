//! Approval decisions and execution through a native server and real browser sessions.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration assertions"
)]

#[path = "support/multiuser_fixture.rs"]
mod multiuser_fixture;
#[path = "support/native_http.rs"]
mod native_http;

use multiuser_fixture::{Subject, run, success};
use serde_json::{Value, json};

const MODEL: &str = "ir.approval.request";
const KEY: &str = "019a0000-0000-7000-8000-000000000011";

#[test]
fn approval_receipt_records_the_requesting_browser_user() {
    run(|fixture| {
        let requester = fixture.user(Subject::A);
        let approver = fixture.user(Subject::Admin);
        success(&fixture.rpc(
            Subject::Admin,
            "ir.approval.policy",
            "create",
            json!({"vals_list":[{
                "name":"Review preference updates",
                "principal_kind":"user",
                "effect":"write",
                "model":"res.users",
                "method":"write",
                "outcome":"approve",
                "approver_user_ids":[[6,0,[approver.id]]]
            }]}),
        ));
        let body = json!({"ids":[requester.id],"vals":{"tz":"Europe/London"}});
        let unkeyed = fixture.rpc(Subject::A, "res.users", "write", body.clone());
        assert_eq!(unkeyed.status, 422);
        let unkeyed: Value = serde_json::from_str(&unkeyed.body).unwrap();
        assert_eq!(unkeyed["context"]["approval_key_required"], true);
        let write = || requester.keyed_rpc(fixture.native.port, "res.users", "write", KEY, &body);
        let pending = write();
        assert_eq!(pending.status, 409);
        let pending: Value = serde_json::from_str(&pending.body).unwrap();
        let id = pending["context"]["approval_request_id"].as_i64().unwrap();
        let submitted =
            success(&fixture.rpc(Subject::A, MODEL, "erp_status", json!({"request_id":id})));
        assert_eq!(submitted["can_decide"], false);
        let review = success(&fixture.rpc(
            Subject::Admin,
            MODEL,
            "erp_status",
            json!({"request_id":id}),
        ));
        assert_eq!(review["can_decide"], true);
        assert_eq!(review["requester_id"], requester.id);
        assert_eq!(review["companies"], json!([requester.company]));
        let rejected = fixture.rpc(
            Subject::Admin,
            MODEL,
            "erp_decide",
            json!({"request_id":id,"approve":true,"payload_sha256":"changed"}),
        );
        assert_eq!(rejected.status, 422);
        let decision =
            json!({"request_id":id,"approve":true,"payload_sha256":review["payload_sha256"]});
        assert_eq!(
            fixture
                .rpc(Subject::B, MODEL, "erp_decide", decision.clone())
                .status,
            403
        );
        let approved = success(&fixture.rpc(Subject::Admin, MODEL, "erp_decide", decision));
        assert_eq!(approved["state"], "approved");
        assert_eq!(approved["can_decide"], false);
        assert_eq!(success(&write()), json!(true));
        assert_eq!(success(&write()), json!(true));

        let receipt =
            success(&fixture.rpc(Subject::A, MODEL, "erp_status", json!({"request_id":id})));
        assert_eq!(receipt["state"], "executed");
        assert!(receipt["operation"].is_string());
        let row = success(&fixture.rpc(
            Subject::Admin,
            "res.users",
            "read",
            json!({"ids":[requester.id],"fields":["tz","write_uid"]}),
        ));
        assert_eq!(row[0]["tz"], "Europe/London");
        let actor = &row[0]["write_uid"];
        assert_eq!(
            actor.as_i64().or_else(|| actor[0].as_i64()),
            Some(requester.id)
        );
    });
}

#[test]
fn managed_password_operations_respect_exact_and_wildcard_approval_policies() {
    run(|fixture| {
        let requester = fixture.user(Subject::A);
        let reviewer = fixture.user(Subject::Admin);
        let new_password = "synthetic-password-policy-fixture";
        let change = |subject| {
            fixture.rpc(
                subject,
                "res.users",
                "change_password",
                json!({
                    "old_passwd":format!("{} & = fixture £ password", fixture.native.db.name()),
                    "new_passwd":new_password
                }),
            )
        };
        let refuse = |reply: native_http::Reply| {
            assert_eq!(reply.status, 403);
            let error: Value = serde_json::from_str(&reply.body).unwrap();
            assert_eq!(
                error["message"],
                "An approval policy does not permit this password operation."
            );
            assert_eq!(error["context"], json!({}));
            assert!(!reply.body.contains(new_password));
        };

        let policy = "ir.approval.policy";
        let exact = success(&fixture.rpc(
            Subject::Admin,
            policy,
            "create",
            json!({"vals_list":[{
                "name":"Deny this password change",
                "principal_kind":"user",
                "effect":"write",
                "model":"res.users",
                "method":"change_password",
                "company_id":requester.company,
                "outcome":"deny"
            }]}),
        ))[0]
            .as_i64()
            .unwrap();
        refuse(change(Subject::A));
        success(&fixture.rpc(
            Subject::Admin,
            policy,
            "write",
            json!({"ids":[exact],"vals":{"active":false}}),
        ));
        success(&fixture.rpc(
            Subject::Admin,
            policy,
            "create",
            json!({"vals_list":[{
                "name":"Review writes in this company",
                "principal_kind":"user",
                "effect":"write",
                "company_id":requester.company,
                "outcome":"approve",
                "approver_user_ids":[[6,0,[reviewer.id]]]
            }]}),
        ));
        refuse(change(Subject::A));
        assert_eq!(success(&change(Subject::B)), true);

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let counts = runtime.block_on(fixture.native.client.query_one(
            "SELECT (SELECT count(*) FROM ir_approval_request),
                    (SELECT count(*) FROM ir_idempotency_key WHERE model='res.users' AND method='change_password')",
            &[],
        )).unwrap();
        assert_eq!((counts.get::<_, i64>(0), counts.get::<_, i64>(1)), (0, 0));
        assert!(
            !std::fs::read_to_string(&fixture.native.stderr_path)
                .unwrap()
                .contains(new_password)
        );
    });
}
