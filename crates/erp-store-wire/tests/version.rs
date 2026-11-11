//! Wire major, schema generation and cursor tests (the header half of
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use erp_store_wire::{
    BusCursor, Limit, SchemaGeneration, WireError, WireLimits, WireMajor, Xid8, accept_client_major,
};

#[test]
fn client_major_acceptance() {
    let current = u64::from(WireMajor::CURRENT.get());
    assert_eq!(accept_client_major(current), Ok(WireMajor::CURRENT));
    // An older and a newer major are both refused with both numbers named.
    for client in [current - 1, current + 1, u64::MAX] {
        let err = accept_client_major(client).expect_err("refused");
        let expected = WireError::ClientOutdated {
            client,
            supported: WireMajor::CURRENT.get(),
        };
        assert_eq!(err, expected);
        assert_eq!(err.code(), "client_outdated");
        assert_eq!(err.path(), None);
        assert_eq!(
            err.to_string(),
            format!(
                "client wire major {client} is not supported (server speaks {})",
                WireMajor::CURRENT.get()
            )
        );
    }
}

#[test]
fn schema_generation_is_a_bounded_token() {
    let g = SchemaGeneration::new("g-2026.11:a_1").expect("valid");
    assert_eq!(g.as_str(), "g-2026.11:a_1");
    assert_eq!(g.to_string(), "g-2026.11:a_1");
    let bad_name = |reason: &str| WireError::BadName {
        path: "schema".to_owned(),
        reason: reason.to_owned(),
    };
    assert_eq!(
        SchemaGeneration::new(""),
        Err(bad_name("empty generation token"))
    );
    for token in ["a b", "a/b", "é", "a\"b"] {
        let err = SchemaGeneration::new(token).expect_err("refused");
        assert_eq!(
            err,
            bad_name("generation token has a character outside A-Z a-z 0-9 . _ - :")
        );
        assert_eq!(
            err.to_string(),
            "schema: bad name: generation token has a character outside A-Z a-z 0-9 . _ - :"
        );
    }
    let maximum = Limit::SchemaTokenBytes.maximum() as usize;
    assert!(SchemaGeneration::new(&"a".repeat(maximum)).is_ok());
    let err = SchemaGeneration::new(&"a".repeat(maximum + 1)).expect_err("refused");
    assert_eq!(
        err,
        WireError::LimitExceeded {
            limit: "schema_token_bytes",
            path: "schema".to_owned(),
            maximum: Limit::SchemaTokenBytes.maximum(),
        }
    );
    let lowered = WireLimits::default()
        .lowered(Limit::SchemaTokenBytes, 3)
        .expect("lower");
    assert!(SchemaGeneration::new_with_limits("abc", &lowered).is_ok());
    assert_eq!(
        SchemaGeneration::new_with_limits("abcd", &lowered)
            .expect_err("refused")
            .code(),
        "limit_exceeded"
    );
}

#[test]
fn cursor_orders_numerically_never_as_strings() {
    let cursor = |xid: &str, id: &str| BusCursor::from_wire_strings(xid, id).expect("valid cursor");
    // As strings "9" > "10"; as numbers 9 < 10.
    assert!("9" > "10");
    assert!(cursor("9", "1") < cursor("10", "1"));
    assert!(cursor("10", "9") < cursor("10", "10"));
    assert!(cursor("10", "10") < cursor("11", "1"));
    assert_eq!(cursor("10", "10"), cursor("10", "10"));
    let c = cursor("104", "5512");
    assert_eq!((c.xid().get(), c.id()), (104, 5512));
    assert_eq!(BusCursor::new(Xid8::new(104).expect("xid"), 5512), Ok(c));
    // Refusals carry the path of the member.
    let bad = |path: &str, reason: &str| WireError::BadDescriptor {
        path: path.to_owned(),
        reason: reason.to_owned(),
    };
    assert_eq!(
        BusCursor::from_wire_strings("0104", "1"),
        Err(bad("cursor.xid", "leading zero"))
    );
    assert_eq!(
        BusCursor::from_wire_strings("104", "-1"),
        Err(bad("cursor.id", "not a decimal digit"))
    );
    let outside = bad("cursor.id", "event id is outside 1..=2^63-1");
    assert_eq!(
        BusCursor::from_wire_strings("104", "0"),
        Err(outside.clone())
    );
    assert_eq!(
        BusCursor::from_wire_strings("104", "9223372036854775808"),
        Err(outside)
    );
    assert!(BusCursor::from_wire_strings("104", "9223372036854775807").is_ok());
}

#[test]
fn header_on_every_payload() {
    use common::{cursor_104, read_only, schema, writing};
    use erp_store_wire::{AnswerBuilder, FrameBuilder};
    let schema = schema();
    let current = i64::from(WireMajor::CURRENT.get());
    let check = |bytes: Vec<u8>| {
        let value: serde_json::Value = serde_json::from_slice(&bytes).expect("valid JSON");
        assert_eq!(value["wire"], serde_json::json!(current));
        assert_eq!(
            value["schema"],
            serde_json::json!(schema.generation().as_str())
        );
    };
    check(
        AnswerBuilder::new(&schema, read_only())
            .encode()
            .expect("encodes"),
    );
    check(
        FrameBuilder::new(&schema, cursor_104(), writing())
            .expect("frame")
            .encode()
            .expect("encodes"),
    );
    check(schema.encode().expect("encodes"));
}
