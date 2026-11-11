//! correlation primitives; no database or authority is involved.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]
use erp_orm::{CorrelationId, OperationId, OperationIdentity};

#[test]
fn correlation_header_refuses_noncanonical_nil_and_unsupported_uuid() {
    for (value, reason) in [
        ("", "malformed"),
        ("00000000-0000-0000-0000-000000000000", "nil"),
        ("018F0000-0000-7000-8000-000000000001", "noncanonical"),
        (
            "018f0000-0000-1000-8000-000000000001",
            "unsupported version",
        ),
        (
            "018f0000-0000-7000-0000-000000000001",
            "unsupported variant",
        ),
    ] {
        let error = CorrelationId::parse_header(value).unwrap_err();
        assert_eq!(error.kind(), erp_fields::ErrorKind::Validation);
        assert_eq!(error.message(), format!("X-Correlation-ID: {reason}"));
    }
    for value in [
        "018f0000-0000-7000-8000-000000000001",
        "018f0000-0000-4000-8000-000000000001",
    ] {
        assert_eq!(CorrelationId::parse_header(value).unwrap().as_str(), value);
    }
    // Existing X-Request-ID/RPC callers retain the general UUID parser.
    assert_eq!(
        CorrelationId::parse("01234567-89AB-CDEF-0123-456789ABCDEF")
            .unwrap()
            .as_str(),
        "01234567-89ab-cdef-0123-456789abcdef"
    );
}

#[test]
fn operation_identity_separates_untrusted_correlation_from_operation() {
    let correlation = CorrelationId::parse_header("018f0000-0000-7000-8000-000000000001").unwrap();
    let first = OperationIdentity::new(correlation.clone()).unwrap();
    let second = OperationIdentity::new(correlation.clone()).unwrap();
    assert_eq!(first.correlation(), &correlation);
    assert_ne!(first.operation(), second.operation());
    assert_eq!(first.operation().as_str().as_bytes()[14], b'7');
    assert_eq!(
        OperationId::parse(first.operation().as_str()).unwrap(),
        *first.operation()
    );
    let generated = CorrelationId::generate().unwrap();
    assert_eq!(
        CorrelationId::parse_header(generated.as_str()).unwrap(),
        generated
    );
}
