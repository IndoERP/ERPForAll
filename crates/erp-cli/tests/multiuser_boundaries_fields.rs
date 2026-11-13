// SPDX-License-Identifier: MIT OR Apache-2.0
//! Real-session authorization boundary acceptance; no mocked RPC responses.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration assertions"
)]
#[path = "support/multiuser_boundaries.rs"]
mod boundaries;
#[path = "support/multiuser_fixture.rs"]
mod multiuser_fixture;
#[path = "support/native_http.rs"]
mod native_http;

use boundaries::*;
use multiuser_fixture::{Subject, error, run, success};
use serde_json::json;

#[test]
fn self_service_persists_and_field_metadata_omits_protected_owner() {
    run(|f| {
        let uid = f.user(Subject::A).id;
        success(&f.rpc(
            Subject::A,
            "res.users",
            "write",
            json!({"ids":[uid],"vals":{"phone":"boundary self service"}}),
        ));
        assert_eq!(
            admin_read(f, "res.users", uid, &["phone"])["phone"],
            "boundary self service"
        );
        let own = settings(f, Subject::A);
        assert_eq!(
            admin_read(f, "res.users.settings", own, &["homemenu_config"])["homemenu_config"],
            "[]"
        );
        let visible = success(&f.rpc(Subject::A, "res.users.settings", "fields_get", json!({})));
        assert!(visible.get("user_id").is_none());
        assert!(visible.get("homemenu_config").is_some());
        let admin = success(&f.rpc(
            Subject::Admin,
            "res.users.settings",
            "fields_get",
            json!({}),
        ));
        assert!(admin.get("user_id").is_some());
        let message = field_denial("User Settings", "res.users.settings", "user_id", "read");
        error(
            &f.rpc(
                Subject::A,
                "res.users.settings",
                "read",
                json!({"ids":[own],"fields":["user_id"]}),
            ),
            403,
            ACCESS,
            &message,
        );
        let password = success(&f.rpc(
            Subject::A,
            "res.users",
            "read",
            json!({"ids":[uid],"fields":["password"]}),
        ));
        assert!(
            password[0].get("password").is_some(),
            "explicit secret read omitted its field"
        );
        assert!(
            password[0]["password"].is_null() || password[0]["password"] == false,
            "secret read must be empty (body redacted)"
        );
    });
}

#[test]
fn restricted_owner_cannot_be_used_in_domains_order_groups_or_aggregates() {
    run(|f| {
        settings(f, Subject::A);
        let uid = f.user(Subject::A).id;
        let bodies = [
            ("search", json!({"domain":[["user_id","=",uid]]})),
            ("search", json!({"domain":[],"order":"user_id,id"})),
            (
                "read_group",
                json!({"domain":[],"groupby":["user_id"],"aggregates":["__count"]}),
            ),
            (
                "read_group",
                json!({"domain":[],"groupby":[],"aggregates":["user_id:count_distinct"]}),
            ),
        ];
        let message = field_denial("User Settings", "res.users.settings", "user_id", "read");
        for (method, body) in bodies {
            error(
                &f.rpc(Subject::A, "res.users.settings", method, body),
                403,
                ACCESS,
                &message,
            );
        }
        assert_eq!(
            success(&f.rpc(
                Subject::A,
                "res.users.settings",
                "search_count",
                json!({"domain":[]})
            )),
            1
        );
    });
}

#[test]
fn settings_owner_cannot_be_rebound_or_forged_on_create() {
    run(|f| {
        let own = settings(f, Subject::A);
        let other = settings(f, Subject::B);
        let message = field_denial("User Settings", "res.users.settings", "user_id", "write");
        for (method, body) in [
            (
                "write",
                json!({"ids":[own],"vals":{"user_id":f.user(Subject::B).id,"homemenu_config":"[1]"}}),
            ),
            (
                "create",
                json!({"vals_list":[{"user_id":f.user(Subject::B).id,"homemenu_config":"[]"}]}),
            ),
            (
                "create",
                json!({"vals_list":[{"homemenu_config":"[]"}],"context":{"default_user_id":f.user(Subject::B).id}}),
            ),
        ] {
            error(
                &f.rpc(Subject::A, "res.users.settings", method, body),
                403,
                ACCESS,
                &message,
            );
        }
        assert_eq!(
            admin_read(
                f,
                "res.users.settings",
                own,
                &["user_id", "homemenu_config"]
            )["homemenu_config"],
            "[]"
        );
        assert_eq!(
            relation_id(&admin_read(f, "res.users.settings", own, &["user_id"])["user_id"]),
            f.user(Subject::A).id
        );
        assert_eq!(
            relation_id(&admin_read(f, "res.users.settings", other, &["user_id"])["user_id"]),
            f.user(Subject::B).id
        );
    });
}

#[test]
fn protected_user_writes_and_nested_commands_cannot_escalate() {
    run(|f| {
        let uid = f.user(Subject::A).id;
        let before = admin_read(
            f,
            "res.users",
            uid,
            &["group_ids", "company_ids", "login", "partner_id"],
        );
        for (field, value) in [
            ("group_ids", json!([[4, f.group("group_system"), 0]])),
            ("company_ids", json!([[4, f.user(Subject::B).company, 0]])),
            ("login", json!("boundary-escalated")),
            (
                "partner_id",
                admin_read(f, "res.users", f.user(Subject::B).id, &["partner_id"])["partner_id"]
                    .clone(),
            ),
        ] {
            let value = if field == "partner_id" {
                json!(relation_id(&value))
            } else {
                value
            };
            let message = field_denial("User", "res.users", field, "write");
            error(
                &f.rpc(
                    Subject::A,
                    "res.users",
                    "write",
                    json!({"ids":[uid],"vals":{field:value,"phone":"must roll back"}}),
                ),
                403,
                ACCESS,
                &message,
            );
        }
        // group_ids itself is protected, so a nested update must never execute on its target.
        let group = f.group("group_user");
        let group_before = admin_read(f, "res.groups", group, &["name"]);
        let message = field_denial("User", "res.users", "group_ids", "write");
        error(
            &f.rpc(
                Subject::A,
                "res.users",
                "write",
                json!({"ids":[uid],"vals":{"group_ids":[[1,group,{"name":"must not execute"}]]}}),
            ),
            403,
            ACCESS,
            &message,
        );
        assert_eq!(admin_read(f, "res.groups", group, &["name"]), group_before);
        assert_eq!(
            admin_read(
                f,
                "res.users",
                uid,
                &["group_ids", "company_ids", "login", "partner_id"]
            ),
            before
        );
        assert_eq!(f.bootstrap(Subject::A)["identity"]["uid"], uid);
        success(&f.rpc(
            Subject::A,
            "res.users",
            "write",
            json!({"ids":[uid],"vals":{"phone":"allowed after denials"}}),
        ));
        assert_eq!(
            admin_read(f, "res.users", uid, &["phone"])["phone"],
            "allowed after denials"
        );
    });
}
