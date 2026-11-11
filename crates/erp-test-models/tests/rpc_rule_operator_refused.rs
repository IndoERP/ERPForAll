//! Amendment 1: a declared access-rule operator (`thread_access`) is not a domain
//! operator, so a JSON-2 call that sends it is refused while its arguments are read, with the
//! ordinary invalid-operator error and before any ORM call.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

// `support::rpc` resolves the fixture as `crate::currency_fixture`
#[allow(unused_imports)]
use support::currency_fixture;

mod support;

use support::rpc::*;

/// Every search-shaped method refuses the spelling the same way, as `Validation` (HTTP 422).
#[tokio::test]
async fn rpc_search_refuses_a_declared_rule_operator() {
    let h = harness().await;
    for method in ["search", "search_count"] {
        let r = call(
            &h,
            &format!("/json/2/res.users/{method}"),
            r#"{"domain": [["id", "thread_access", "read"]]}"#,
            alice(),
        )
        .await;
        assert_error(
            &r,
            422,
            "erp.exceptions.ValidationError",
            &format!(
                "argument 'domain' of res.users.{method}: Invalid operator in condition \
                 ('id', 'thread_access', 'read')"
            ),
        );
    }
}
