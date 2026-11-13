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
fn company_context_uses_live_membership_and_empty_means_defaults() {
    run(|f| {
        let s = Categories::seed(f);
        let a = f.user(Subject::A).company;
        let b = f.user(Subject::B).company;
        for context in [
            json!({}),
            json!({"allowed_company_ids":[]}),
            json!({"allowed_company_ids":[a]}),
        ] {
            let rows = success(&f.rpc(
                Subject::A,
                CATEGORY,
                "search",
                json!({"domain":[["id","in",s.ids()]],"context":context,"order":"id"}),
            ));
            assert_eq!(ids(&rows), vec![s.own, s.shared]);
        }
        error(
            &f.rpc(
                Subject::A,
                CATEGORY,
                "search",
                json!({"domain":[],"context":{"allowed_company_ids":[b]}}),
            ),
            403,
            ACCESS,
            "Access to unauthorized or invalid companies.",
        );
        error(
            &f.rpc(
                Subject::A,
                CATEGORY,
                "search",
                json!({"domain":[],"context":{"allowed_company_ids":[a,a]}}),
            ),
            422,
            "werkzeug.exceptions.UnprocessableEntity",
            &format!("context key 'allowed_company_ids': company {a} is listed twice"),
        );
        error(
            &f.rpc(
                Subject::A,
                CATEGORY,
                "search",
                json!({"domain":[],"context":{"allowed_company_ids":[0]}}),
            ),
            422,
            "werkzeug.exceptions.UnprocessableEntity",
            "context key 'allowed_company_ids': expected a list of positive integers, got array",
        );
        let user = multi_user(f);
        for (selection, expected) in [
            (vec![a], vec![s.own, s.shared]),
            (vec![b], vec![s.foreign, s.shared]),
            (vec![b, a], s.ids()),
        ] {
            let rows=success(&user.rpc_on(f.native.port,CATEGORY,"search",json!({"domain":[["id","in",s.ids()]],"context":{"allowed_company_ids":selection},"order":"id"})));
            assert_eq!(ids(&rows), expected);
        }
        success(&user.rpc_on(
            f.native.port,
            "res.users",
            "write",
            json!({"ids":[user.id],"vals":{"company_id":b}}),
        ));
        assert_eq!(
            relation_id(&admin_read(f, "res.users", user.id, &["company_id"])["company_id"]),
            b
        );
        let rows=success(&user.rpc_on(f.native.port,CATEGORY,"search",json!({"domain":[["id","in",s.ids()]],"context":{"allowed_company_ids":[]},"order":"id"})));
        assert_eq!(ids(&rows), s.ids());
    });
}

#[test]
fn declared_ancestor_and_internal_contact_exceptions_are_preserved() {
    run(|f| {
        let a = f.user(Subject::A).company;
        let b = f.user(Subject::B).company;
        let currency =
            relation_id(&admin_read(f, "res.company", a, &["currency_id"])["currency_id"]);
        let ancestor = create(
            f,
            "res.company",
            json!({"name":"boundary ancestor","currency_id":currency}),
        );
        write(
            f,
            "res.users",
            &[f.user(Subject::Admin).id],
            json!({"company_ids":[[4,ancestor,0]]}),
        );
        // The hierarchy is immutable after create: establish the branch in supported creates.
        let branch = create(
            f,
            "res.company",
            json!({"name":"boundary branch","currency_id":currency,"parent_id":ancestor}),
        );
        write(
            f,
            "res.users",
            &[f.user(Subject::Admin).id],
            json!({"company_ids":[[4,branch,0]]}),
        );
        let reader = f.provision(multiuser_fixture::UserSpec {
            login: "boundary-ancestor-reader",
            company: branch,
            companies: vec![branch],
            groups: vec![f.group("group_user")],
        });
        let ancestor_record = create(
            f,
            CATEGORY,
            json!({"name":"boundary ancestor","company_id":ancestor}),
        );
        let descendant = create(
            f,
            "res.company",
            json!({"name":"boundary descendant","currency_id":currency,"parent_id":branch}),
        );
        write(
            f,
            "res.users",
            &[f.user(Subject::Admin).id],
            json!({"company_ids":[[4,descendant,0]]}),
        );
        let descendant_record = create(
            f,
            CATEGORY,
            json!({"name":"boundary descendant","company_id":descendant}),
        );
        let rows = success(&reader.rpc_on(
            f.native.port,
            CATEGORY,
            "search",
            json!({"domain":[["id","in",[ancestor_record,descendant_record]]]}),
        ));
        assert_eq!(ids(&rows), vec![ancestor_record]);
        let customer = create(
            f,
            "res.partner",
            json!({"name":"boundary foreign customer","company_id":b}),
        );
        let internal = relation_id(
            &admin_read(f, "res.users", f.user(Subject::B).id, &["partner_id"])["partner_id"],
        );
        let shared = create(
            f,
            "res.partner",
            json!({"name":"boundary shared customer","company_id":null}),
        );
        let rows = success(&f.rpc(
            Subject::A,
            "res.partner",
            "search",
            json!({"domain":[["id","in",[customer,internal,shared]]],"order":"id"}),
        ));
        let mut expected = vec![internal, shared];
        expected.sort();
        assert_eq!(ids(&rows), expected);
        let message = record_denial(f.user(Subject::A), "Contact", "res.partner", "read", true);
        error(
            &f.rpc(
                Subject::A,
                "res.partner",
                "read",
                json!({"ids":[customer],"fields":["id"]}),
            ),
            403,
            ACCESS,
            &message,
        );
    });
}

#[test]
fn a_write_capable_manager_cannot_move_records_outside_allowed_companies() {
    run(|f| {
        let s = Categories::seed(f);
        let manager = product_manager(f);
        let port = f.native.port;
        success(&manager.rpc_on(
            port,
            CATEGORY,
            "write",
            json!({"ids":[s.own],"vals":{"name":"boundary allowed write"}}),
        ));
        let before = admin_read(f, CATEGORY, s.own, &["name", "company_id"]);
        let message = record_denial(&manager, "Product Category", CATEGORY, "write", true);
        error(&manager.rpc_on(port,CATEGORY,"write",json!({"ids":[s.own],"vals":{"company_id":f.user(Subject::B).company,"name":"must roll back"}})),403,ACCESS,&message);
        assert_eq!(
            admin_read(f, CATEGORY, s.own, &["name", "company_id"]),
            before
        );
        let shared_before = admin_read(f, CATEGORY, s.shared, &["name", "company_id"]);
        error(
            &manager.rpc_on(
                port,
                CATEGORY,
                "write",
                json!({"ids":[s.shared,s.foreign],"vals":{"name":"mixed must roll back"}}),
            ),
            403,
            ACCESS,
            &message,
        );
        assert_eq!(
            admin_read(f, CATEGORY, s.shared, &["name", "company_id"]),
            shared_before
        );
        assert_eq!(
            admin_read(f, CATEGORY, s.foreign, &["name"])["name"],
            "boundary foreign"
        );
        let prices = Prices::seed(f);
        success(&manager.rpc_on(
            port,
            ITEM,
            "write",
            json!({"ids":[prices.items[0]],"vals":{"fixed_price":12}}),
        ));
        let message = record_denial(&manager, "Pricelist Rule", ITEM, "write", false);
        error(&manager.rpc_on(port,ITEM,"write",json!({"ids":[prices.items[0]],"vals":{"pricelist_id":prices.lists[1],"fixed_price":999}})),403,ACCESS,&message);
        let row = admin_read(f, ITEM, prices.items[0], &["pricelist_id", "fixed_price"]);
        assert_eq!(relation_id(&row["pricelist_id"]), prices.lists[0]);
        assert_eq!(row["fixed_price"].as_f64(), Some(12.0));
    });
}
