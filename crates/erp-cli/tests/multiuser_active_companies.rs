// SPDX-License-Identifier: MIT OR Apache-2.0
//! The active companies a user confirmed in the switcher, stored in `res.users.settings`, are
//! where the web shell starts when its address names none. Real sessions, no mocked responses.
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

use multiuser_fixture::{Subject, UserSpec, error, run, success};
use serde_json::{Value, json};

const SETTINGS: &str = "res.users.settings";
const NOT_YOURS: &str = "One of the selected companies does not exist or is not one of yours.";

fn store(user: &multiuser_fixture::User, port: u16, companies: Value) -> native_http::Reply {
    user.rpc_on(
        port,
        SETTINGS,
        "set_res_users_settings",
        json!({"new_settings":{"active_company_ids":companies}}),
    )
}

fn stored(user: &multiuser_fixture::User, port: u16) -> Value {
    success(&user.rpc_on(port, SETTINGS, "get_res_users_settings", json!({})))["active_company_ids"]
        .clone()
}

fn active(bootstrap: &Value) -> (Value, Value) {
    (
        bootstrap["active_company_ids"].clone(),
        bootstrap["current_company_id"].clone(),
    )
}

#[test]
fn the_shell_starts_with_the_stored_companies_and_never_beyond_the_allowed_ones() {
    run(|f| {
        let port = f.native.port;
        let a = f.user(Subject::A).company;
        let b = f.user(Subject::B).company;
        let user = f.provision(UserSpec {
            login: "active-companies",
            company: a,
            companies: vec![a, b],
            groups: vec![f.group("group_user")],
        });
        // nothing stored: the default companies, main company first
        assert_eq!(stored(&user, port), json!(false));
        assert_eq!(active(&user.bootstrap(port, "/erp")).1, json!(a));

        // a confirmed choice is stored in its order and the shell without `cids` starts with it
        let reply = success(&store(&user, port, json!([b, a])));
        assert_eq!(reply["active_company_ids"], json!([b, a]));
        assert_eq!(stored(&user, port), json!([b, a]));
        assert_eq!(
            active(&user.bootstrap(port, "/erp")),
            (json!([b, a]), json!(b))
        );

        // an address with `cids` wins for that load and leaves the stored choice alone
        assert_eq!(
            active(&user.bootstrap(port, &format!("/erp?cids={a}"))),
            (json!([a]), json!(a))
        );
        assert_eq!(stored(&user, port), json!([b, a]));

        // a company the user does not have is refused and nothing changes; so is a malformed list
        let main = f.user(Subject::Admin).company;
        error(
            &store(&user, port, json!([main])),
            403,
            "erp.exceptions.AccessError",
            NOT_YOURS,
        );
        for bad in [
            json!([]),
            json!([a, a]),
            json!([0]),
            json!("[1]"),
            json!([a.to_string()]),
        ] {
            assert_eq!(store(&user, port, bad.clone()).status, 422, "{bad}");
        }
        assert_eq!(stored(&user, port), json!([b, a]));

        // the generic write path is held to the owner's companies too
        let own = success(&user.rpc_on(port, SETTINGS, "search", json!({"domain":[]})));
        let own = own[0].as_i64().unwrap();
        let reply = user.rpc_on(
            port,
            SETTINGS,
            "write",
            json!({"ids":[own],"vals":{"active_company_config":format!("[{main}]")}}),
        );
        assert_eq!(reply.status, 422, "{}", reply.body);
        assert_eq!(stored(&user, port), json!([b, a]));

        // another user can neither read nor write this row
        let foreign = f.rpc(
            Subject::A,
            SETTINGS,
            "write",
            json!({"ids":[own],"vals":{"active_company_config":format!("[{a}]")}}),
        );
        assert_ne!(foreign.status, 200, "{}", foreign.body);
        assert_eq!(stored(&user, port), json!([b, a]));

        // a company taken from the user is dropped on read; the rest keeps its order
        success(&f.rpc(
            Subject::Admin,
            "res.users",
            "write",
            json!({"ids":[user.id],"vals":{"company_ids":[[6,0,[a]]]}}),
        ));
        assert_eq!(stored(&user, port), json!([a]));
        assert_eq!(
            active(&user.bootstrap(port, "/erp")),
            (json!([a]), json!(a))
        );

        // False clears the choice: the default companies again
        let reply = success(&store(&user, port, json!(false)));
        assert_eq!(reply["active_company_ids"], json!(false));
        assert_eq!(active(&user.bootstrap(port, "/erp")).1, json!(a));
    });
}

#[test]
fn a_stored_choice_with_no_allowed_company_left_falls_back_to_the_default() {
    run(|f| {
        let port = f.native.port;
        let a = f.user(Subject::A).company;
        let b = f.user(Subject::B).company;
        let user = f.provision(UserSpec {
            login: "active-companies-gone",
            company: a,
            companies: vec![a, b],
            groups: vec![f.group("group_user")],
        });
        success(&store(&user, port, json!([b])));
        success(&f.rpc(
            Subject::Admin,
            "res.users",
            "write",
            json!({"ids":[user.id],"vals":{"company_ids":[[6,0,[a]]]}}),
        ));
        assert_eq!(stored(&user, port), json!(false));
        let bootstrap = user.bootstrap(port, "/erp");
        assert_eq!(active(&bootstrap), (json!([a]), json!(a)));
        assert_eq!(bootstrap["allowed_company_ids"], json!([a]));
    });
}
