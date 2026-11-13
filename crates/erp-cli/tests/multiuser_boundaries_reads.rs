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
fn indirect_category_reads_filter_the_same_foreign_rows() {
    run(|f| {
        let s = Categories::seed(f);
        for (subject, expected) in [
            (Subject::A, vec![s.own, s.shared]),
            (Subject::B, vec![s.foreign, s.shared]),
        ] {
            let domain = json!([["id", "in", s.ids()]]);
            let rows = success(&f.rpc(
                subject,
                CATEGORY,
                "search_read",
                json!({"domain":domain,"fields":["id","name"],"order":"id"}),
            ));
            assert_eq!(row_ids(&rows), expected);
            let names = success(&f.rpc(
                subject,
                CATEGORY,
                "name_search",
                json!({"domain":domain,"name":"","limit":20}),
            ));
            assert_eq!(pair_ids(&names), expected);
            assert_eq!(
                success(&f.rpc(subject, CATEGORY, "search_count", json!({"domain":domain}))),
                2
            );
            let groups = success(&f.rpc(
                subject,
                CATEGORY,
                "read_group",
                json!({"domain":domain,"groupby":[],"aggregates":["__count"]}),
            ));
            assert_eq!(groups, json!([[2]]));
            let web = success(&f.rpc(subject, CATEGORY, "web_search_read", json!({"domain":domain,"specification":{"id":{},"name":{}},"order":"id","limit":20})));
            assert_eq!(row_ids(&web["records"]), expected);
            assert_eq!(web["length"], 2);
            let web = success(&f.rpc(
                subject,
                CATEGORY,
                "web_read",
                json!({"ids":expected,"specification":{"id":{},"name":{}}}),
            ));
            assert_eq!(row_ids(&web), expected);
        }
        let empty = success(&f.rpc(
            Subject::A,
            CATEGORY,
            "search_read",
            json!({"domain":[["id","=",s.foreign]],"fields":["id"]}),
        ));
        assert_eq!(empty, json!([]));
        let message = model_denial(
            "Access",
            "ir.access",
            "access",
            &["Settings Administration"],
        );
        error(
            &f.rpc(Subject::A, "ir.access", "search", json!({"domain":[]})),
            403,
            ACCESS,
            &message,
        );
    });
}

#[test]
fn parent_owned_lines_do_not_contribute_foreign_values() {
    run(|f| {
        let s = Prices::seed(f);
        let domain = json!([["id", "in", s.items]]);
        let rows = success(&f.rpc(
            Subject::A,
            ITEM,
            "search_read",
            json!({"domain":domain,"fields":["id","fixed_price"],"order":"id"}),
        ));
        assert_eq!(row_ids(&rows), vec![s.items[0], s.items[2]]);
        let groups = success(&f.rpc(
            Subject::A,
            ITEM,
            "read_group",
            json!({"domain":domain,"groupby":[],"aggregates":["__count","fixed_price:sum"]}),
        ));
        assert_eq!(groups.as_array().unwrap().len(), 1);
        assert_eq!(groups[0][0], 2);
        assert_eq!(groups[0][1].as_f64(), Some(40.0));
        let label = success(&f.rpc(
            Subject::A,
            ITEM,
            "read",
            json!({"ids":[s.items[0]],"fields":["pricelist_id"]}),
        ));
        assert_eq!(relation_id(&label[0]["pricelist_id"]), s.lists[0]);
        assert_eq!(
            label[0]["pricelist_id"],
            json!([s.lists[0], "boundary price 0"])
        );
    });
}

#[test]
fn shared_parent_children_and_mixed_reads_keep_record_restrictions() {
    run(|f| {
        let s = Categories::seed(f);
        let raw = success(&f.rpc(
            Subject::A,
            CATEGORY,
            "read",
            json!({"ids":[s.root],"fields":["child_id"]}),
        ));
        assert_eq!(ids(&raw[0]["child_id"]), vec![s.own, s.shared]);
        let web = success(&f.rpc(Subject::A,CATEGORY,"web_read",json!({"ids":[s.root],"specification":{"child_id":{"fields":{"id":{},"name":{}},"order":"id"}}})));
        assert_eq!(row_ids(&web[0]["child_id"]), vec![s.own, s.shared]);
        let message = record_denial(
            f.user(Subject::A),
            "Product Category",
            CATEGORY,
            "read",
            true,
        );
        for method in ["read", "web_read"] {
            let body = if method == "read" {
                json!({"ids":[s.own,s.foreign],"fields":["id","name"]})
            } else {
                json!({"ids":[s.own,s.foreign],"specification":{"id":{},"name":{}}})
            };
            error(
                &f.rpc(Subject::A, CATEGORY, method, body),
                403,
                ACCESS,
                &message,
            );
        }
    });
}

#[test]
fn own_settings_are_filtered_before_count_and_web_expansion() {
    run(|f| {
        let own = settings(f, Subject::A);
        let other = settings(f, Subject::B);
        let domain = json!([["id", "in", [own, other]]]);
        let rows = success(&f.rpc(
            Subject::A,
            "res.users.settings",
            "search_read",
            json!({"domain":domain,"fields":["id","homemenu_config"]}),
        ));
        assert_eq!(row_ids(&rows), vec![own]);
        assert_eq!(
            success(&f.rpc(
                Subject::A,
                "res.users.settings",
                "search_count",
                json!({"domain":domain})
            )),
            1
        );
        let web = success(&f.rpc(
            Subject::A,
            "res.users.settings",
            "web_search_read",
            json!({"domain":domain,"specification":{"id":{},"homemenu_config":{}},"limit":20}),
        ));
        assert_eq!(row_ids(&web["records"]), vec![own]);
        let message = record_denial(
            f.user(Subject::A),
            "User Settings",
            "res.users.settings",
            "read",
            false,
        );
        error(
            &f.rpc(
                Subject::A,
                "res.users.settings",
                "read",
                json!({"ids":[other],"fields":["id"]}),
            ),
            403,
            ACCESS,
            &message,
        );
    });
}

#[test]
fn related_parent_name_is_company_bound_even_when_contact_is_shared() {
    run(|f| {
        let customer = create(
            f,
            "res.partner",
            json!({"name":"boundary foreign parent","company_id":f.user(Subject::B).company}),
        );
        let row = admin_read(f, "res.users", f.user(Subject::B).id, &["partner_id"]);
        let contact = relation_id(&row["partner_id"]);
        write(f, "res.partner", &[contact], json!({"parent_id":customer}));
        let allowed = success(&f.rpc(
            Subject::A,
            "res.partner",
            "read",
            json!({"ids":[contact],"fields":["id"]}),
        ));
        assert_eq!(row_ids(&allowed), vec![contact]);
        let message = format!(
            "You cannot read “Parent Name” (res.partner.parent_name) of Contact (res.partner: {contact}): it is read through Contact (res.partner: {customer}), which belongs to a company outside your active companies.{OUTSIDE}"
        );
        error(
            &f.rpc(
                Subject::A,
                "res.partner",
                "read",
                json!({"ids":[contact],"fields":["parent_name"]}),
            ),
            403,
            ACCESS,
            &message,
        );
    });
}
