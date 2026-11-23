// SPDX-License-Identifier: MIT OR Apache-2.0
//! The request's client address reaches model methods only from the transport
//! (`RpcCall::client_address`), through every environment derived in the call, and never from the
//! client's context; a transaction opened without one (jobs, the command line, internal code) has
//! none.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod support;

use erp_orm::{BeginOptions, RequestInfo, UserId};
use erp_rpc::RpcCall;
use serde_json::json;
use support::rpc::*;

const PROBE: &str = "/json/2/test_rpc.effects/client_address";

async fn probe(
    h: &erp_testkit::RpcHarness,
    body: &str,
    address: Option<&str>,
) -> serde_json::Value {
    let mut call = RpcCall::json2(PROBE, body.as_bytes().to_vec(), alice());
    if let Some(address) = address {
        call = call.client_address(address.parse().unwrap());
    }
    ok(&h.service().call(call).await)
}

#[tokio::test]
async fn a_method_sees_the_address_the_transport_set_in_every_derived_environment() {
    let h = harness().await;
    // this environment, sudo(), with_user, with_context, then the context's own key
    assert_eq!(
        probe(&h, "{}", Some("192.0.2.7")).await,
        json!(["192.0.2.7", "192.0.2.7", "192.0.2.7", "192.0.2.7", false])
    );
    assert_eq!(
        probe(&h, "{}", Some("2001:db8::7")).await,
        json!([
            "2001:db8::7",
            "2001:db8::7",
            "2001:db8::7",
            "2001:db8::7",
            false
        ])
    );
    // as the socket reported it: a mapped IPv4 client stays mapped (IpNetwork accounts for it)
    assert_eq!(
        probe(&h, "{}", Some("::ffff:192.0.2.7")).await[0],
        "::ffff:192.0.2.7"
    );
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn without_a_transport_address_the_request_is_unknown() {
    let h = harness().await;
    assert_eq!(
        probe(&h, "{}", None).await,
        json!([false, false, false, false, false])
    );
    // a transaction opened by server code without a request: jobs, the command line, tests
    let tx = h
        .orm()
        .begin_opts(BeginOptions::new(UserId::new(ALICE).unwrap()))
        .await
        .unwrap();
    assert!(tx.env().request().is_none());
    drop(tx);
    let tx = h
        .orm()
        .begin_opts(
            BeginOptions::new(UserId::new(ALICE).unwrap())
                .request(RequestInfo::new("198.51.100.4".parse().unwrap())),
        )
        .await
        .unwrap();
    assert_eq!(
        tx.env().request().map(RequestInfo::client_address),
        Some("198.51.100.4".parse().unwrap())
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn a_client_context_key_of_the_same_name_changes_nothing() {
    let h = harness().await;
    let body = r#"{"context": {"client_address": "10.0.0.1"}}"#;
    // the key stays an ordinary context value the method may read; the request is the transport's
    assert_eq!(
        probe(&h, body, Some("192.0.2.9")).await,
        json!([
            "192.0.2.9",
            "192.0.2.9",
            "192.0.2.9",
            "192.0.2.9",
            "10.0.0.1"
        ])
    );
    assert_eq!(
        probe(&h, body, None).await,
        json!([false, false, false, false, "10.0.0.1"])
    );
    h.cleanup().await.unwrap();
}
