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
        assert_eq!(submitted["policy_name"], "Review preference updates");
        assert_eq!(submitted["requester_name"], requester.login);
        assert!(submitted["expires_at"].as_str().unwrap().ends_with('Z'));
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
        for key in ["requester_name", "policy_name", "expires_at"] {
            assert_eq!(approved[key], submitted[key]);
        }
        // Upgraded single-stage receipts keep decider_id without a historical stage row.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime
            .block_on(fixture.native.client.execute(
                "DELETE FROM ir_approval_decision WHERE request_id=$1",
                &[&id],
            ))
            .unwrap();
        let legacy =
            success(&fixture.rpc(Subject::A, MODEL, "erp_status", json!({"request_id":id})));
        assert_eq!(legacy["steps"][0]["decider_name"], approver.login);
        assert_eq!(legacy["steps"][0]["state"], "approved");
        assert!(legacy["steps"][0].get("decided_at").is_none());
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
fn approval_queues_page_only_requests_visible_to_the_current_user() {
    run(|fixture| {
        let policy = success(&fixture.rpc(
            Subject::Admin,
            "ir.approval.policy",
            "create",
            json!({"vals_list":[{
                "name":"Review preferences", "principal_kind":"user", "effect":"write",
                "model":"res.users", "method":"write", "outcome":"approve",
                "approver_group_ids":[[6,0,[fixture.group("group_user")]]]
            }]}),
        ))[0]
            .as_i64()
            .unwrap();
        let queue = |subject, scope, offset, limit| {
            success(&fixture.rpc(
                subject,
                MODEL,
                "erp_queue",
                json!({"scope":scope,"offset":offset,"limit":limit}),
            ))
        };
        let submit = |subject, key| {
            let user = fixture.user(subject);
            let reply = user.keyed_rpc(
                fixture.native.port,
                "res.users",
                "write",
                key,
                &json!({"ids":[user.id],"vals":{"tz":"Europe/London"}}),
            );
            assert_eq!(reply.status, 409);
            serde_json::from_str::<Value>(&reply.body).unwrap()["context"]["approval_request_id"]
                .as_i64()
                .unwrap()
        };
        let first = submit(Subject::A, KEY);
        let second = submit(Subject::A, "019a0000-0000-7000-8000-000000000012");
        let third = submit(Subject::B, "019a0000-0000-7000-8000-000000000013");
        const VIEW: &str = "approval.request.view";
        const STAGES: &str = "approval.request.stage";
        let native = fixture.native;
        let install = native_http::json_post(
            native.port,
            &native.cookies,
            "/erp/module/install",
            r#"{"modules":["approvals"]}"#,
        );
        if install.status != 200 {
            let err: Value = serde_json::from_str(&install.body).unwrap_or(Value::Null);
            panic!(
                "Installing approvals returned {} {}",
                install.status, err["message"]
            );
        }
        let orm = |who, model, method, args| {
            let response = fixture.rpc(who, model, method, args);
            if response.status != 200 {
                let err: Value = serde_json::from_str(&response.body).unwrap_or(Value::Null);
                panic!(
                    "{model}.{method}: HTTP {} {}",
                    response.status, err["message"]
                );
            }
            success(&response)
        };
        let bookmark = |who, id| {
            fixture.rpc(
                who,
                "ir.actions.act_window",
                "erp_load_web_action_views",
                json!({"ids":[id],"wire_major":1,"bookmark":true}),
            )
        };
        let opened = |who, id| {
            let response = bookmark(who, id);
            if response.status != 200 {
                let err: Value = serde_json::from_str(&response.body).unwrap_or(Value::Null);
                panic!(
                    "Opening bookmark {id} returned HTTP {} {}",
                    response.status, err["message"]
                );
            }
            success(&response)
        };
        let request_action = fixture.xmlid("approvals", "requests_action");
        let config_action = fixture.xmlid("approvals", "configuration_action");
        let window = fixture.xmlid("approvals", "requests_window");
        assert_eq!(
            opened(Subject::A, request_action)["result"]["tag"],
            "approvals.requests"
        );
        assert_eq!(
            opened(Subject::Admin, config_action)["result"]["tag"],
            "approvals.configuration"
        );
        assert_eq!(opened(Subject::A, window)["action"]["model"], VIEW);
        let refused = bookmark(Subject::A, config_action);
        let missing = bookmark(Subject::A, i64::from(i32::MAX));
        assert_eq!(refused.status, 403);
        assert_eq!(missing.status, 403);
        let message = |body: &str| serde_json::from_str::<Value>(body).unwrap()["message"].clone();
        assert_eq!(message(&refused.body), message(&missing.body));
        for (who, expected) in [
            (Subject::A, vec![first, second]),
            (Subject::B, vec![third]),
            (Subject::Admin, vec![first, second, third]),
        ] {
            assert_eq!(
                orm(who, VIEW, "search", json!({"domain":[],"order":"id"})),
                json!(expected)
            );
            assert_eq!(
                orm(who, VIEW, "search_count", json!({"domain":[]})),
                expected.len()
            );
            assert_eq!(
                orm(
                    who,
                    VIEW,
                    "read_group",
                    json!({"domain":[],"groupby":[],"aggregates":["__count"]})
                ),
                json!([[expected.len()]])
            );
            let page = orm(
                who,
                VIEW,
                "web_search_read",
                json!({"domain":[],"order":"id","limit":1,"offset":0,
                "specification":{"id":{},"state":{},"current_step_name":{},"can_decide":{},"company_names":{}}}),
            );
            assert_eq!(page["length"], expected.len());
            assert_eq!(page["records"][0]["id"], expected[0]);
            let form = orm(
                who,
                VIEW,
                "web_read",
                json!({"ids":[expected[0]],"specification":{"id":{},
                "stage_ids":{"fields":{"id":{},"name":{},"state":{}}}}}),
            );
            assert_eq!(form[0]["stage_ids"][0]["state"], "pending");
            assert_eq!(
                orm(who, STAGES, "search_count", json!({"domain":[]})),
                expected.len()
            );
            assert_eq!(
                orm(
                    who,
                    "approval.request.scope",
                    "search_count",
                    json!({"domain":[]})
                ),
                expected.len()
            );
        }
        assert_eq!(
            orm(
                Subject::Admin,
                VIEW,
                "search_count",
                json!({"domain":[["to_review","=",true]]})
            ),
            3
        );
        assert_eq!(
            orm(
                Subject::A,
                VIEW,
                "search_count",
                json!({"domain":[["is_mine","=",true]]})
            ),
            2
        );
        assert_eq!(
            orm(
                Subject::B,
                VIEW,
                "search_count",
                json!({"domain":[["is_reviewed","=",true]]})
            ),
            0
        );
        assert_eq!(
            fixture
                .rpc(
                    Subject::A,
                    VIEW,
                    "read",
                    json!({"ids":[third],"fields":["payload_sha256"]})
                )
                .status,
            403
        );
        let foreign_grants = orm(
            Subject::A,
            "approval.request.scope",
            "search_count",
            json!({"domain":[["viewer_id","=",fixture.user(Subject::Admin).id]]}),
        );
        assert_eq!(foreign_grants, 0);
        let scopes = orm(
            Subject::A,
            VIEW,
            "web_read",
            json!({"ids":[first],
            "specification":{"scope_ids":{"fields":{"id":{},"is_mine":{}}}}}),
        );
        assert_eq!(scopes[0]["scope_ids"].as_array().unwrap().len(), 1);
        assert_eq!(scopes[0]["scope_ids"][0]["is_mine"], true);
        let private_stage = orm(
            Subject::B,
            STAGES,
            "search",
            json!({"domain":[["request_id","=",third]]}),
        )[0]
        .clone();
        let refused_stage = fixture.rpc(
            Subject::A,
            STAGES,
            "read",
            json!({"ids":[private_stage],"fields":["name"]}),
        );
        assert_eq!(refused_stage.status, 403);
        assert!(
            !refused_stage.body.contains("Review preferences"),
            "an inaccessible stage name leaked in the denial"
        );
        for method in ["action_approve", "action_deny"] {
            assert_eq!(
                fixture
                    .rpc(Subject::Admin, VIEW, method, json!({"ids":[first]}))
                    .status,
                422
            );
            assert_eq!(fixture.rpc(Subject::Admin,VIEW,method,json!({"ids":[first],"context":{"approval_step":1,"approval_digest":"stale"}})).status,422);
        }
        // A device key for the same internal user can read ordinary ORM records, but
        // does not inherit the interactive session's approval receipts or stage history.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(native.client.batch_execute(
            "INSERT INTO ir_config_parameter (key,value) VALUES ('base.enable_programmatic_api_keys','True')
             ON CONFLICT (key) DO UPDATE SET value=EXCLUDED.value;
             UPDATE res_groups SET api_key_duration=7 WHERE id IN
             (SELECT res_id FROM ir_model_data WHERE module='base' AND name='group_user')"
        )).unwrap();
        let owner = fixture.user(Subject::A);
        let device = native_http::request(
            native.port,
            "POST",
            "/erp/session/api-key",
            &[("content-type", "application/json")],
            &json!({"db":native.db.name(),"login":owner.login,
                    "password":format!("{} & = fixture £ password",native.db.name()),
                    "device_name":"approval-read-test"})
            .to_string(),
        );
        assert_eq!(device.status, 200, "device key issue failed");
        let issued: Value = serde_json::from_str(&device.body).unwrap();
        let auth = format!("Bearer {}", issued["api_key"].as_str().unwrap());
        let key_rpc = |model, method, args: Value| {
            native_http::request(
                native.port,
                "POST",
                &format!("/erp/json/2/{model}/{method}"),
                &[
                    ("content-type", "application/json"),
                    ("authorization", &auth),
                ],
                &args.to_string(),
            )
        };
        assert_eq!(
            key_rpc(
                "res.users",
                "read",
                json!({"ids":[owner.id],"fields":["id"]})
            )
            .status,
            200
        );
        for projection in [VIEW, STAGES, "approval.request.scope"] {
            assert_eq!(
                success(&key_rpc(projection, "search_count", json!({"domain":[]}))),
                0
            );
        }
        assert_eq!(
            key_rpc(VIEW, "read", json!({"ids":[first],"fields":["state"]})).status,
            403
        );

        let empty = json!({"total":0,"items":[]});

        // Admin implies the policy's named group. A and B each lack the other's company
        // and neither can decide their own requests despite direct membership of that group.
        assert_eq!(
            queue(Subject::Admin, "review", 0, 0),
            json!({"total":3,"items":[]})
        );
        assert_eq!(queue(Subject::A, "review", 0, 25), empty);
        assert_eq!(queue(Subject::B, "review", 0, 25), empty);
        assert_eq!(queue(Subject::Admin, "mine", 0, 25), empty);
        let page = queue(Subject::Admin, "review", 1, 1);
        assert_eq!(page["total"], 3);
        assert_eq!(page["items"].as_array().unwrap().len(), 1);
        let item = &page["items"][0];
        assert_eq!(item["id"], second);
        assert_eq!(item["can_decide"], true);
        assert_eq!(item["requester_name"], fixture.user(Subject::A).login);
        assert_eq!(item["policy_name"], "Review preferences");
        assert_eq!(item["model"], "res.users");
        assert_eq!(item["method"], "write");
        assert_eq!(item["state"], "pending");
        for field in [
            "binding",
            "excluded",
            "payload",
            "payload_sha256",
            "request_key",
            "params",
        ] {
            assert!(item.get(field).is_none());
        }
        assert_eq!(
            queue(Subject::Admin, "review", 4, 10),
            json!({"total":3,"items":[]})
        );
        let mine = success(&fixture.rpc(Subject::A, MODEL, "erp_queue", json!({"scope":"mine"})));
        assert_eq!(mine["total"], 2);
        assert_eq!(mine["items"][0]["id"], second);
        assert_eq!(mine["items"][0]["can_decide"], false);
        assert_eq!(mine["items"][1]["id"], first);
        assert_eq!(queue(Subject::B, "mine", 0, 25)["items"][0]["id"], third);
        for invalid in [
            json!({"scope":"all"}),
            json!({"scope":"review","offset":-1}),
            json!({"scope":"mine","limit":101}),
            json!({"scope":"mine","limit":-1}),
        ] {
            assert_eq!(
                fixture.rpc(Subject::A, MODEL, "erp_queue", invalid).status,
                422
            );
        }
        assert_eq!(
            fixture
                .rpc(
                    Subject::A,
                    MODEL,
                    "read",
                    json!({"ids":[third],"fields":["state"]})
                )
                .status,
            403
        );
        assert_eq!(
            fixture
                .rpc(Subject::A, MODEL, "erp_status", json!({"request_id":third}))
                .status,
            403
        );

        success(&fixture.rpc(
            Subject::Admin,
            MODEL,
            "erp_decide",
            json!({"request_id":first,"approve":false}),
        ));
        assert_eq!(
            orm(
                Subject::Admin,
                VIEW,
                "search_count",
                json!({"domain":[["is_reviewed","=",true]]})
            ),
            1
        );
        // A pre-multistage signature lives only in the historical request's decider.
        runtime
            .block_on(native.client.execute(
                "DELETE FROM ir_approval_decision WHERE request_id=$1",
                &[&first],
            ))
            .unwrap();
        let legacy = orm(
            Subject::A,
            STAGES,
            "search_read",
            json!({
            "domain":[["request_id","=",first]],"fields":["decider_name","decided_at","state"]}),
        );
        assert_eq!(legacy[0]["state"], "denied");
        assert_eq!(
            legacy[0]["decider_name"],
            fixture.user(Subject::Admin).login
        );
        assert!(legacy[0]["decided_at"].is_null() || legacy[0]["decided_at"] == false);
        let reviewed = queue(Subject::Admin, "reviewed", 0, 25);
        assert_eq!(reviewed["total"], 1);
        assert_eq!(reviewed["items"][0]["id"], first);
        assert_eq!(reviewed["items"][0]["state"], "denied");
        assert_eq!(reviewed["items"][0]["can_decide"], false);
        assert_eq!(queue(Subject::A, "reviewed", 0, 25), empty);
        assert_eq!(queue(Subject::Admin, "review", 0, 0)["total"], 2);

        // Advance the stored deadline without waiting a day.
        runtime.block_on(fixture.native.client.execute(
            "UPDATE ir_approval_request SET expires_at = clock_timestamp() - interval '1 second' WHERE id = $1",
            &[&second],
        )).unwrap();
        assert_eq!(queue(Subject::Admin, "review", 0, 25)["total"], 1);
        assert_eq!(
            queue(Subject::A, "mine", 0, 25)["items"][0]["state"],
            "expired"
        );
        success(&fixture.rpc(
            Subject::Admin,
            "ir.approval.policy",
            "write",
            json!({"ids":[policy],"vals":{"active":false}}),
        ));
        assert_eq!(queue(Subject::Admin, "review", 0, 25), empty);
        assert_eq!(
            orm(Subject::Admin, VIEW, "search_count", json!({"domain":[]})),
            0
        );
        assert_eq!(
            orm(Subject::Admin, STAGES, "search_count", json!({"domain":[]})),
            0
        );
        assert_eq!(
            orm(
                Subject::A,
                STAGES,
                "search_read",
                json!({"domain":[["request_id","=",second]],"fields":["state"]})
            )[0]["state"],
            "expired"
        );
        assert_eq!(queue(Subject::Admin, "reviewed", 0, 25), empty);
        assert_eq!(queue(Subject::B, "mine", 0, 25)["total"], 1);
        assert_eq!(
            queue(Subject::B, "mine", 0, 25)["items"][0]["state"],
            "expired"
        );
        success(&fixture.rpc(
            Subject::Admin,
            "ir.approval.policy",
            "write",
            json!({"ids":[policy],"vals":{"active":true}}),
        ));
        assert_eq!(queue(Subject::Admin, "review", 0, 25), empty);
        let old =
            success(&fixture.rpc(Subject::B, MODEL, "erp_status", json!({"request_id":third})));
        assert_eq!(old["state"], "expired");
        // Simulate the previous addon generation and upgrade only Approvals. Its projection
        // must be rebuilt from the existing durable records, including historical signatures.
        runtime
            .block_on(
                native
                    .client
                    .batch_execute("DROP VIEW approval_request_stage"),
            )
            .unwrap();
        success(&native_http::json_post(
            native.port,
            &native.cookies,
            "/erp/module/upgrade",
            r#"{"modules":["approvals"]}"#,
        ));
        assert_eq!(
            orm(Subject::A, STAGES, "search_count", json!({"domain":[]})),
            2
        );
        assert_eq!(
            queue(Subject::Admin, "reviewed", 0, 25)["items"][0]["state"],
            "denied"
        );
    });
}

#[test]
fn sequential_review_stages_bind_decisions_and_recheck_every_signer() {
    use multiuser_fixture::{User, UserSpec};

    run(|fixture| {
        let port = fixture.native.port;
        let owner = fixture.user(Subject::A);
        let director = fixture.user(Subject::Admin);
        let group = success(&fixture.rpc(
            Subject::Admin,
            "res.groups",
            "create",
            json!({"vals_list":[{"name":"Approval supervisors"}]}),
        ))[0]
            .as_i64()
            .unwrap();
        let supervisor = fixture.provision(UserSpec {
            login: "stage-supervisor",
            company: owner.company,
            companies: vec![owner.company],
            groups: vec![fixture.group("group_user"), group],
        });
        let policy = |steps| {
            fixture.rpc(Subject::Admin,"ir.approval.policy","create",json!({"vals_list":[{
            "name":"Review preference stages","outcome":"approve","effect":"write","principal_kind":"user",
            "model":"res.users","method":"write","review_steps":steps
        }]}))
        };
        for steps in [
            json!([{"name":"", "user_ids":[director.id]}]),
            json!([{"name":"Nobody", "user_ids":[]}]),
            json!([{"name":"Repeated", "user_ids":[director.id,director.id]}]),
            json!([{"name":"Missing", "group_ids":[99999999]}]),
            json!([{"name":"Unsupported","user_ids":[director.id],"delegate":true}]),
        ] {
            assert_eq!(policy(steps).status, 422);
        }
        let id = success(&policy(json!([
            {"name":"Supervisor", "group_ids":[group],"user_ids":[owner.id]},
            {"name":"Director", "user_ids":[director.id]}
        ])))[0]
            .as_i64()
            .unwrap();
        assert_eq!(
            fixture
                .rpc(
                    Subject::Admin,
                    "ir.approval.policy",
                    "write",
                    json!({"ids":[id],"vals":{"review_steps":[]}})
                )
                .status,
            422
        );
        let write = |key| {
            owner.keyed_rpc(
                port,
                "res.users",
                "write",
                key,
                &json!({"ids":[owner.id],"vals":{"tz":"Europe/London"}}),
            )
        };
        let submit = |key| {
            let call = write(key);
            assert_eq!(call.status, 409);
            serde_json::from_str::<Value>(&call.body).unwrap()["context"]["approval_request_id"]
                .as_i64()
                .unwrap()
        };
        let status =
            |who: &User, id| who.rpc_on(port, MODEL, "erp_status", json!({"request_id":id}));
        let queue = |who: &User, scope| {
            success(&who.rpc_on(port, MODEL, "erp_queue", json!({"scope":scope})))
        };
        let decide = |who: &User, id, step, approve, digest: &Value| {
            who.rpc_on(
                port,
                MODEL,
                "erp_decide",
                json!({"request_id":id,"step":step,"approve":approve,"payload_sha256":digest}),
            )
        };
        let req = submit(KEY);
        let first = success(&status(&supervisor, req));
        assert_eq!(first["step_count"], 2);
        assert_eq!(first["current_step"], 1);
        assert_eq!(first["steps"][0]["state"], "pending");
        assert_eq!(first["steps"][1]["state"], "waiting");
        assert_eq!(first["steps"][0]["name"], "Supervisor");
        assert_eq!(queue(&supervisor, "review")["total"], 1);
        assert_eq!(queue(director, "review")["total"], 0);
        assert_eq!(status(director, req).status, 403);
        let digest = &first["payload_sha256"];
        assert_eq!(decide(owner, req, 1, true, digest).status, 403);
        assert_eq!(
            decide(&supervisor, req, 1, true, &json!("wrong")).status,
            422
        );
        assert_eq!(
            supervisor
                .rpc_on(
                    port,
                    MODEL,
                    "erp_decide",
                    json!({"request_id":req,"approve":true})
                )
                .status,
            422
        );
        // Two identical simultaneous decisions can advance the request only once.
        std::thread::scope(|s| {
            let other = s.spawn(|| decide(&supervisor, req, 1, true, digest));
            success(&decide(&supervisor, req, 1, true, digest));
            success(&other.join().unwrap());
        });
        let next = success(&status(director, req));
        assert_eq!(next["state"], "pending");
        assert_eq!(next["current_step"], 2);
        assert_eq!(next["steps"][0]["state"], "approved");
        assert_eq!(next["steps"][0]["decider_name"], supervisor.login);
        assert!(next["steps"][0]["decided_at"].is_string());
        assert_eq!(next["steps"][1]["state"], "pending");
        assert_eq!(next["can_decide"], true);
        assert_eq!(queue(&supervisor, "review")["total"], 0);
        assert_eq!(queue(&supervisor, "reviewed")["total"], 1);
        assert_eq!(
            queue(director, "review")["items"][0]["current_step_name"],
            "Director"
        );
        assert_eq!(decide(&supervisor, req, 1, false, digest).status, 409);
        assert_eq!(decide(&supervisor, req, 2, true, digest).status, 403);
        assert_eq!(write(KEY).status, 409);
        let approved = success(&decide(director, req, 2, true, digest));
        assert_eq!(approved["state"], "approved");
        assert_eq!(approved["steps"][1]["decider_name"], director.login);
        // Leave a different eligible supervisor in the group so admission can still resolve
        // the policy. The first signer's revoked membership alone blocks execution.
        let backup = fixture.user(Subject::Admin);
        let members = |ids: Vec<i64>| {
            success(&fixture.rpc(
                Subject::Admin,
                "res.groups",
                "write",
                json!({"ids":[group],"vals":{"user_ids":[[6,0,ids]]}}),
            ))
        };
        members(vec![backup.id]);
        assert_eq!(write(KEY).status, 403);
        assert_eq!(queue(&supervisor, "reviewed")["total"], 0);
        assert_eq!(status(&supervisor, req).status, 403);
        members(vec![supervisor.id, backup.id]);
        assert_eq!(success(&write(KEY)), true);
        assert_eq!(success(&write(KEY)), true);
        assert_eq!(success(&status(owner, req))["state"], "executed");
        let key = "019a0000-0000-7000-8000-000000000099";
        let denied = submit(key);
        let shown = success(&status(&supervisor, denied));
        success(&decide(
            &supervisor,
            denied,
            1,
            true,
            &shown["payload_sha256"],
        ));
        let denied = success(&decide(
            director,
            denied,
            2,
            false,
            &shown["payload_sha256"],
        ));
        assert_eq!(denied["state"], "denied");
        assert_eq!(denied["steps"][1]["state"], "denied");
        assert_eq!(write(key).status, 403);
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
