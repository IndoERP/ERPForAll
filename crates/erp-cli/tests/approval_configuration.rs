// SPDX-License-Identifier: MIT OR Apache-2.0
//! The installed approval workflow editor manages immutable policy revisions over JSON-2.
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    reason = "native test assertions"
)]

#[path = "support/multiuser_fixture.rs"]
mod multiuser_fixture;
#[path = "support/native_http.rs"]
mod native_http;

use multiuser_fixture::Subject;
use serde_json::{Value, json};

const POLICY: &str = "ir.approval.policy";
const WORKFLOW: &str = "approval.workflow";
const STEP: &str = "approval.workflow.step";

#[track_caller]
fn success(response: &native_http::Reply) -> Value {
    if response.status != 200 {
        let failure: Value = serde_json::from_str(&response.body).unwrap_or(Value::Null);
        panic!(
            "workflow request: HTTP {} {}: {}",
            response.status, failure["name"], failure["message"]
        );
    }
    multiuser_fixture::success(response)
}

#[test]
fn native_drafts_publish_once_and_legacy_policies_keep_visible_history() {
    multiuser_fixture::run(|fixture| {
        let admin = |model, method, args| fixture.rpc(Subject::Admin, model, method, args);
        let [first, second] = [fixture.user(Subject::A).id, fixture.user(Subject::B).id];
        let legacy = success(&admin(POLICY, "create", json!({"vals_list":[{
            "name":"Legacy procedure","principal_kind":"user","effect":"destructive","model":"res.partner",
            "method":"unlink","outcome":"approve","approver_user_ids":[[6,0,[first]]]
        }]})))[0].as_i64().unwrap();
        let native = fixture.native;
        success(&native_http::json_post(
            native.port,
            &native.cookies,
            "/erp/module/install",
            r#"{"modules":["approvals"]}"#,
        ));
        let projection = |policy| {
            success(&admin(
                WORKFLOW,
                "search_read",
                json!({
                    "domain":[["policy_id","=",policy]],"fields":["name","state","step_ids"]
                }),
            ))
        };
        let workflow =
            |id| {
                success(&admin(WORKFLOW, "read", json!({
            "ids":[id],"fields":["name","state","policy_id","replaces_policy_id","step_ids"]
        })))[0].clone()
            };
        let legacy_rows = projection(legacy);
        assert_eq!(legacy_rows.as_array().unwrap().len(), 1);
        assert_eq!(legacy_rows[0]["state"], "active");
        assert_eq!(legacy_rows[0]["step_ids"].as_array().unwrap().len(), 1);
        success(&admin(
            POLICY,
            "write",
            json!({"ids":[legacy],"vals":{"active":false}}),
        ));
        assert_eq!(projection(legacy)[0]["state"], "inactive");

        let model = success(&admin(
            "ir.model",
            "search",
            json!({"domain":[["model","=","res.partner"]]}),
        ))[0]
            .as_i64()
            .unwrap();
        let saved = success(&admin(
            WORKFLOW,
            "create",
            json!({"vals_list":[{
                "name":"Saved reviewers","model_id":model,"method":"write",
                "step_ids":[[0,0,{"name":"Team","user_ids":[[6,0,[first,second]]]}]]
            }]}),
        ))[0]
            .as_i64()
            .unwrap();
        let limited = admin(
            WORKFLOW,
            "action_publish",
            json!({
                "ids":[saved], "context":{"allowed_company_ids":[fixture.user(Subject::A).company]}
            }),
        );
        assert_eq!(limited.status, 403);
        assert_eq!(workflow(saved)["state"], "draft");
        let title = "Policy with unavailable reviewers";
        let restricted_policy = admin(
            POLICY,
            "create",
            json!({
                "context":{"allowed_company_ids":[fixture.user(Subject::A).company]},
                "vals_list":[{
                    "name":title,"principal_kind":"user","effect":"write","model":"res.partner",
                    "method":"create","outcome":"approve","review_steps":[{"name":"Team","user_ids":[first,second]}]
                }]
            }),
        );
        assert_eq!(restricted_policy.status, 403);
        let error: Value = serde_json::from_str(&restricted_policy.body).unwrap();
        assert_eq!(
            error["message"],
            "All reviewers must be visible when creating a workflow revision."
        );
        assert_eq!(
            success(&admin(
                POLICY,
                "search_count",
                json!({"domain":[["name","=",title]]})
            )),
            0
        );
        success(&admin(
            "res.users",
            "write",
            json!({"ids":[second],"vals":{"active":false}}),
        ));
        assert_eq!(
            admin(WORKFLOW, "action_publish", json!({"ids":[saved]})).status,
            422
        );
        assert_eq!(workflow(saved)["state"], "draft");
        let step = workflow(saved)["step_ids"][0].as_i64().unwrap();
        success(&admin(
            STEP,
            "write",
            json!({"ids":[step],"vals":{"user_ids":[[6,0,[first]]]}}),
        ));
        success(&admin(WORKFLOW, "action_publish", json!({"ids":[saved]})));
        success(&admin(
            "res.users",
            "write",
            json!({"ids":[second],"vals":{"active":true}}),
        ));
        let draft_values =
            json!({"vals_list":[{"name":"Native review","model_id":model,"method":"web_save"}]});
        assert_eq!(
            fixture
                .rpc(Subject::A, WORKFLOW, "create", draft_values.clone())
                .status,
            403
        );
        let draft = success(&admin(
            WORKFLOW,
            "web_save",
            json!({
                "ids":[],"vals":draft_values["vals_list"][0],
                "specification":{"name":{},"state":{},"company_id":{},"model_id":{},"method":{},
                    "replaces_policy_id":{},"step_ids":{}}
            }),
        ))[0]["id"]
            .as_i64()
            .unwrap();
        assert_eq!(workflow(draft)["state"], "draft");
        assert_eq!(
            admin(WORKFLOW, "action_publish", json!({"ids":[draft]})).status,
            422
        );
        assert_eq!(workflow(draft)["state"], "draft");
        assert_eq!(
            admin(
                WORKFLOW,
                "write",
                json!({"ids":[draft],"vals":{"state":"active"}})
            )
            .status,
            422
        );

        success(&admin(
            WORKFLOW,
            "write",
            json!({"ids":[draft],"vals":{"step_ids":[
                [0,0,{"sequence":20,"name":"Final authorization","user_ids":[[6,0,[second]]]}],
                [0,0,{"sequence":10,"name":"Operations review","user_ids":[[6,0,[first]]]}]
            ]}}),
        ));
        success(&admin(WORKFLOW, "action_publish", json!({"ids":[draft]})));
        let published = workflow(draft);
        let restricted_revision = admin(
            WORKFLOW,
            "action_create_revision",
            json!({
                "ids":[draft],"context":{"allowed_company_ids":[fixture.user(Subject::A).company]}
            }),
        );
        assert_eq!(restricted_revision.status, 403);
        let policy = published["policy_id"]
            .as_i64()
            .or_else(|| published["policy_id"][0].as_i64())
            .unwrap();
        assert_eq!(published["state"], "active");
        assert_eq!(projection(policy)[0]["id"], draft);
        let review = success(&admin(
            POLICY,
            "read",
            json!({"ids":[policy],"fields":["review_steps"]}),
        ));
        assert_eq!(
            review[0]["review_steps"],
            json!([
                {"name":"Operations review","user_ids":[first],"group_ids":[]},
                {"name":"Final authorization","user_ids":[second],"group_ids":[]},
            ])
        );
        assert_eq!(
            admin(
                WORKFLOW,
                "write",
                json!({"ids":[draft],"vals":{"name":"Changed"}})
            )
            .status,
            422
        );
        assert_eq!(
            admin(
                STEP,
                "create",
                json!({"vals_list":[{"name":"Direct addition","workflow_id":draft}]})
            )
            .status,
            422
        );
        let step = published["step_ids"][0].as_i64().unwrap();
        assert_eq!(
            admin(
                STEP,
                "write",
                json!({"ids":[step],"vals":{"name":"Changed stage"}})
            )
            .status,
            422
        );
        assert_eq!(admin(STEP, "unlink", json!({"ids":[step]})).status, 422);

        assert_eq!(
            fixture
                .rpc(
                    Subject::A,
                    WORKFLOW,
                    "action_archive_workflow",
                    json!({"ids":[draft]})
                )
                .status,
            403
        );
        success(&admin(
            WORKFLOW,
            "action_archive_workflow",
            json!({"ids":[draft]}),
        ));
        assert_eq!(workflow(draft)["state"], "inactive");
        let copy = success(&admin(
            WORKFLOW,
            "action_create_revision",
            json!({"ids":[draft]}),
        ))["res_id"]
            .as_i64()
            .unwrap();
        let inactive_copy = workflow(copy);
        assert_eq!(inactive_copy["state"], "draft");
        assert_eq!(inactive_copy["replaces_policy_id"], false);
        assert_eq!(inactive_copy["step_ids"].as_array().unwrap().len(), 2);
        success(&admin(WORKFLOW, "action_activate", json!({"ids":[draft]})));
        assert_eq!(workflow(draft)["state"], "active");
        assert_eq!(
            admin(WORKFLOW, "action_publish", json!({"ids":[draft]})).status,
            422
        );
        let replacement = success(&admin(
            WORKFLOW,
            "action_create_revision",
            json!({"ids":[draft]}),
        ))["res_id"]
            .as_i64()
            .unwrap();
        let revising = workflow(replacement);
        assert_eq!(revising["state"], "draft");
        let source = &revising["replaces_policy_id"];
        assert_eq!(source.as_i64().or_else(|| source[0].as_i64()), Some(policy));
        success(&admin(
            WORKFLOW,
            "action_publish",
            json!({"ids":[replacement]}),
        ));
        assert_eq!(workflow(draft)["state"], "inactive");
        assert_eq!(workflow(replacement)["state"], "active");
    });
}

#[test]
fn workflow_replacements_are_administrator_only_and_atomic() {
    multiuser_fixture::run(|fixture| {
        let native = fixture.native;
        let response = native_http::json_post(
            native.port,
            &native.cookies,
            "/erp/module/install",
            r#"{"modules":["approvals"]}"#,
        );
        success(&response);
        let reviewers = [fixture.user(Subject::A).id, fixture.user(Subject::B).id];
        let steps = json!([
            {"name":"Operations review","user_ids":[reviewers[0]],"group_ids":[]},
            {"name":"Final authorization","user_ids":[reviewers[1]],"group_ids":[]},
        ]);
        let workflow = |name: &str, method: &str, active: bool| {
            json!({
                "name":name, "principal_kind":"user", "effect":"write", "model":"res.partner",
                "method":method,"active":active, "review_steps":steps,
            })
        };
        let create = |actor, draft: Value, replaces: Option<i64>| {
            let mut issued = json!({"workflow":draft});
            if let Some(id) = replaces {
                issued["replaces_id"] = json!(id);
            }
            fixture.rpc(actor, POLICY, "erp_publish_workflow", issued)
        };
        let read = |id| {
            success(&fixture.rpc(
                    Subject::Admin,
                    POLICY,
                    "read",
                    json!({"ids":[id],"fields":["active","name","review_steps"],"context":{"active_test":false}}),
                ))[0].clone()
        };
        let methods = |actor| {
            fixture.rpc(
                actor,
                POLICY,
                "erp_workflow_methods",
                json!({"model":"res.partner","effect":"write","query":"write"}),
            )
        };
        assert_eq!(methods(Subject::A).status, 403);
        assert!(
            success(&methods(Subject::Admin))
                .as_array()
                .unwrap()
                .contains(&json!("write"))
        );
        assert_eq!(
            create(Subject::A, workflow("Forbidden", "write", true), None).status,
            403
        );
        assert_eq!(
            create(
                Subject::Anonymous,
                workflow("Anonymous", "write", true),
                None
            )
            .status,
            401
        );

        let first = success(&create(
            Subject::Admin,
            workflow("Initial workflow", "write", true),
            None,
        ))
        .as_i64()
        .unwrap();
        let competing = success(&create(
            Subject::Admin,
            workflow("Competing workflow", "create", true),
            None,
        ))
        .as_i64()
        .unwrap();
        let invalid = create(
            Subject::Admin,
            workflow("Conflicting replacement", "create", true),
            Some(first),
        );
        assert_eq!(invalid.status, 422);
        assert_eq!(read(first)["active"], true);
        assert_eq!(read(competing)["active"], true);

        let second = success(&create(
            Subject::Admin,
            workflow("Second revision", "write", true),
            Some(first),
        ))
        .as_i64()
        .unwrap();
        assert_eq!(read(first)["active"], false);
        assert_eq!(read(second)["active"], true);
        assert_eq!(read(second)["review_steps"], steps);
        assert_eq!(
            create(
                Subject::Admin,
                workflow("Stale replacement", "web_save", true),
                Some(first)
            )
            .status,
            409
        );
        assert_eq!(read(second)["active"], true);

        let draft = success(&create(
            Subject::Admin,
            workflow("Inactive copy", "write", false),
            None,
        ))
        .as_i64()
        .unwrap();
        assert_eq!(read(draft)["active"], false);
        assert_eq!(read(draft)["review_steps"], steps);

        let competing_replacements: [u16; 2] = std::thread::scope(|scope| {
            let first = scope.spawn(|| {
                create(
                    Subject::Admin,
                    workflow("Web save revision", "web_save", true),
                    Some(second),
                )
                .status
            });
            let other = scope.spawn(|| {
                create(
                    Subject::Admin,
                    workflow("Name create revision", "name_create", true),
                    Some(second),
                )
                .status
            });
            [first.join().unwrap(), other.join().unwrap()]
        });
        assert_eq!(
            competing_replacements
                .iter()
                .filter(|status| **status == 200)
                .count(),
            1
        );
        assert!(
            competing_replacements
                .iter()
                .any(|status| matches!(status, 409 | 422))
        );
        assert_eq!(read(second)["active"], false);
    });
}
