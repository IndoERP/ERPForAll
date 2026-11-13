// SPDX-License-Identifier: MIT OR Apache-2.0
#![allow(clippy::unwrap_used, reason = "tests")]
use erp_orm::audit::{AttachmentProbeRecorded, ProbeOutcome};
#[test]
fn attachment_probe_event_rejects_invalid_store_and_nil_session() {
    let session = erp_fields::Uuid::parse("12345678-1234-4234-8234-123456789abc").unwrap();
    let e =
        AttachmentProbeRecorded::new(0, session, ProbeOutcome::Verified, &["node-a"]).unwrap_err();
    assert_eq!(e.kind(), erp_fields::ErrorKind::Programming);
    assert_eq!(
        e.message(),
        "Attachment probe audit requires a positive store and nonnil session"
    );
    let e = AttachmentProbeRecorded::new(
        1,
        erp_fields::Uuid::from_bytes([0; 16]),
        ProbeOutcome::Verified,
        &["node-a"],
    )
    .unwrap_err();
    assert_eq!(e.kind(), erp_fields::ErrorKind::Programming);
    assert_eq!(
        e.message(),
        "Attachment probe audit requires a positive store and nonnil session"
    );
}

#[test]
fn attachment_probe_event_bounds_participant_identifiers() {
    let session = erp_fields::Uuid::parse("12345678-1234-4234-8234-123456789abc").unwrap();
    let long = "x".repeat(64);
    for nodes in [
        vec![],
        vec![""],
        vec!["same", "same"],
        vec!["a", "b", "c"],
        vec!["non ascii ä"],
        vec![long.as_str()],
    ] {
        let error =
            AttachmentProbeRecorded::new(1, session, ProbeOutcome::Verified, &nodes).unwrap_err();
        assert_eq!(error.kind(), erp_fields::ErrorKind::Programming);
        assert_eq!(
            error.message(),
            "Attachment probe audit requires one or two distinct bounded node identifiers"
        );
    }
}
