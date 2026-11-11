//! Encoded output and retained construction storage share admission before copying.
use erp_views::{DescriptionBudget, DescriptionEncoder, ViewLimits};

#[test]
fn encoding_charges_shared_live_storage_and_failure_remains_sticky() {
    let budget = DescriptionBudget::with_limit(10).unwrap();
    let _arch = budget.reserve(8).unwrap();
    let encoder = DescriptionEncoder::new(&budget, &ViewLimits::DEFAULT);
    let error = encoder.encode(&"12345").unwrap_err();
    assert!(error.to_string().contains("description bytes"));
    assert!(budget.reserve(0).is_err());
}

#[test]
fn dropping_encoded_storage_releases_only_its_reservation() {
    // The final JSON is seven bytes; growth also needs the old buffer while copying.
    let budget = DescriptionBudget::with_limit(14).unwrap();
    let encoder = DescriptionEncoder::new(&budget, &ViewLimits::DEFAULT);
    let first = encoder.encode(&"12345").unwrap();
    assert_eq!(
        serde_json::from_slice::<String>(first.as_json_bytes()).unwrap(),
        "12345"
    );
    drop(first);
    let second = encoder.encode(&"54321").unwrap();
    assert_eq!(
        serde_json::from_slice::<String>(second.as_json_bytes()).unwrap(),
        "54321"
    );
}

#[test]
fn lowered_encoded_limit_poisoning_cannot_be_hidden_by_a_later_small_payload() {
    let budget = DescriptionBudget::new();
    let limits = ViewLimits::DEFAULT
        .lowered(erp_views::LimitKind::DescriptionEncodedBytes, 2)
        .unwrap();
    let encoder = DescriptionEncoder::new(&budget, &limits);
    let error = encoder.encode(&"x").unwrap_err();
    assert!(matches!(
        error,
        erp_views::ViewError::LimitExceeded {
            limit: erp_views::LimitKind::DescriptionEncodedBytes,
            max: 2,
            actual: Some(3)
        }
    ));
    assert!(encoder.encode(&"").is_err());
}

#[test]
fn serializer_errors_and_debug_output_do_not_expose_payload_or_private_diagnostics() {
    struct Broken;
    impl serde::Serialize for Broken {
        fn serialize<S: serde::Serializer>(&self, _: S) -> Result<S::Ok, S::Error> {
            Err(serde::ser::Error::custom("private diagnostic"))
        }
    }
    let budget = DescriptionBudget::new();
    let encoder = DescriptionEncoder::new(&budget, &ViewLimits::DEFAULT);
    let payload = encoder.encode(&"payload-secret").unwrap();
    assert!(!format!("{payload:?}").contains("payload-secret"));
    let error = encoder.encode(&Broken).unwrap_err();
    assert_eq!(error.to_string(), "Description serialization failed");
    assert!(budget.check().is_err());
}
