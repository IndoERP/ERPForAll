#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]
//! Unit tests of the audit types that need no database.

use std::sync::Arc;

use erp_fields::ErrorKind;
use erp_registry::{AddonDescriptor, RegistryBuilder};

use super::buffer::{Attribution, AuditBuffer};
use super::elevation::{ElevationGrant, ElevationObservation, ElevationReason, FullCause};
use super::*;
use crate::{ApiKeyId, Context, Env, OperationIdentity, Principal, UserId};

fn env(uid: i64) -> Env {
    static ADDON: AddonDescriptor = AddonDescriptor {
        name: "audit_unit_env",
        depends: &["base"],
        exact_only: false,
    };
    Env::new(
        Arc::new(RegistryBuilder::new([&ADDON]).build().unwrap()),
        Principal::for_uid(UserId::new(uid).unwrap()),
        Context::new(),
    )
    .with_operation_identity(Some(Arc::new(OperationIdentity::generate().unwrap())))
}

fn action(id: i64) -> SuperuserAction {
    SuperuserAction {
        action_id: id,
        root_action_id: id,
        group_ids: Box::new([4]),
        flag_write_uid: Some(1),
        flag_write_date_utc_micros: None,
    }
}

fn observe(
    buffer: &mut AuditBuffer,
    depth: usize,
    uid: i64,
    action_id: i64,
) -> erp_fields::Result<()> {
    let env = env(uid);
    buffer.observe(
        depth,
        ElevationObservation {
            reason: ElevationReason::new("base.server_action.run_as_superuser").unwrap(),
            grant: ElevationGrant::Full {
                cause: FullCause::ServerAction(action(action_id)),
            },
            original: Attribution::from_env(&env).unwrap(),
            effective_subject: UserId::new(uid).unwrap(),
        },
        &env,
    )
}

#[test]
fn elevation_reason_pattern() {
    for ok in ["a.b", "base.server_action.run_as_superuser", "a1_b.c_2"] {
        assert!(ElevationReason::new(ok).is_ok(), "{ok}");
    }
    let long: &'static str = Box::leak(format!("a.{}", "b".repeat(127)).into_boxed_str());
    for bad in [
        "", "a", "A.b", "a..b", ".a.b", "a.b.", "a.B", "1a.b", "a.b-c", long,
    ] {
        let error = ElevationReason::new(bad).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Programming, "{bad}");
        assert_eq!(
            error.message(),
            format!(
                "a later release (elevation): elevation reason {bad:?} must be dotted lowercase names of \
                 at most 128 bytes"
            )
        );
    }
}

/// non-positive ids are refused.
#[test]
fn target_ids_compress_runs_in_caller_order() {
    let ids = TargetIds::from_ids(&[5, 6, 7, 3, 9, 10, 1]).unwrap();
    assert_eq!(ids.runs(), [(5, 7), (3, 3), (9, 10), (1, 1)]);
    assert_eq!(ids.to_json(), "[[5,7],3,[9,10],1]");
    assert_eq!(TargetIds::none().to_json(), "[]");
    // a descending pair is two runs, never merged
    assert_eq!(
        TargetIds::from_ids(&[2, 1]).unwrap().runs(),
        [(2, 2), (1, 1)]
    );
    for bad in [0, -4] {
        let error = TargetIds::from_ids(&[1, bad]).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Programming);
        assert_eq!(
            error.message(),
            format!("a later release (invalid): target ids must be positive, got {bad}")
        );
    }
    assert_eq!(
        TargetIds::from_ids(&[i64::MAX - 1, i64::MAX])
            .unwrap()
            .runs()
            .len(),
        1
    );
}

#[test]
fn call_names_are_bounded() {
    let ok = |model: Option<&str>, method: &str| {
        AuditCall::new(
            model.map(Into::into),
            method.into(),
            Effect::Write,
            TargetIds::none(),
            None,
        )
    };
    assert!(ok(Some("res.partner"), "write").is_ok());
    assert!(ok(None, "m").is_ok());
    assert!(ok(Some(&"m".repeat(256)), "write").is_ok());
    for (model, method, what) in [
        (Some(""), "write", "model"),
        (Some("x"), "", "method"),
        (Some("a\0b"), "write", "model"),
        (Some("x"), "w\0", "method"),
    ] {
        let error = ok(model, method).unwrap_err();
        assert_eq!(
            error.message(),
            format!(
                "a later release (invalid): the {what} name must be 1 to 256 bytes without NUL"
            )
        );
    }
    assert!(ok(Some(&"m".repeat(257)), "write").is_err());
}

#[test]
fn collected_keys_are_bounded() {
    let key = |n: i64| ApiKeyId::new(n).unwrap();
    assert!(ApiKeysCollected::new(vec![key(1)]).is_ok());
    assert!(ApiKeysCollected::new((1..=1000).map(key).collect()).is_ok());
    for n in [0usize, 1001] {
        let error = ApiKeysCollected::new((1..=n as i64).map(key).collect()).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Validation);
        assert_eq!(
            error.message(),
            format!("audit: a collection event names 1 to 1000 keys, got {n}")
        );
    }
}

/// by binding, keeps different subjects and actions apart, and bounds distinct entries.
#[test]
fn hook_deduplicates_by_binding_and_bounds_entries() {
    let mut buffer = AuditBuffer::new();
    observe(&mut buffer, 0, 2, 10).unwrap();
    observe(&mut buffer, 0, 2, 10).unwrap();
    observe(&mut buffer, 1, 2, 10).unwrap();
    // another subject, and another action, are separate entries
    observe(&mut buffer, 0, 3, 10).unwrap();
    observe(&mut buffer, 0, 2, 11).unwrap();
    let entries = buffer.elevations();
    assert_eq!(entries.len(), 3);
    assert_eq!(entries[0].count, 3);
    assert_eq!((entries[1].count, entries[2].count), (1, 1));
    for n in 0..(MAX_ELEVATION_ENTRIES - 3) {
        observe(&mut buffer, 0, 2, 100 + n as i64).unwrap();
    }
    assert_eq!(buffer.elevations().len(), MAX_ELEVATION_ENTRIES);
    // the 129th distinct entry is refused; a repeat of an existing one is not
    let error = observe(&mut buffer, 0, 2, 9_999).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::ResourceExhausted);
    assert_eq!(
        error.message(),
        "a later release (capture-limit): CaptureLimit: more than 128 distinct elevations in one transaction"
    );
    observe(&mut buffer, 0, 2, 10).unwrap();
    assert_eq!(buffer.elevations()[0].count, 4);
}

/// keeps the elevation evidence, marked rolled back; a release merges depths.
#[test]
fn savepoints_drop_auth_events_and_keep_elevation_evidence() {
    let env = env(2);
    let user = UserId::new(2).unwrap();
    let mut buffer = AuditBuffer::new();
    let auth = |n: i32| {
        AuthEvent::PasswordChanged(PasswordChanged::new(
            user,
            UserId::new(i64::from(n)).unwrap(),
        ))
    };
    buffer.push_auth(0, auth(5), &env).unwrap();
    buffer.push_auth(1, auth(6), &env).unwrap();
    observe(&mut buffer, 1, 2, 10).unwrap();
    observe(&mut buffer, 0, 2, 11).unwrap();
    observe(&mut buffer, 1, 2, 11).unwrap();
    buffer.rolled_back_to(0);
    assert_eq!(buffer.auth().len(), 1);
    let entries = buffer.elevations();
    // action 10 ran only under the rolled back savepoint; 11 ran once outside it
    assert_eq!(
        (entries[0].count, entries[0].state()),
        (1, "savepoint_rolled_back")
    );
    assert_eq!(
        (entries[1].count, entries[1].rolled_back, entries[1].state()),
        (2, 1, "effective")
    );
    // a release moves deeper uses to the surviving depth: a later rollback to 0 now keeps them
    let mut buffer = AuditBuffer::new();
    observe(&mut buffer, 2, 2, 10).unwrap();
    buffer.released_to(1);
    buffer.rolled_back_to(1);
    assert_eq!(buffer.elevations()[0].state(), "effective");
    buffer.rolled_back_to(0);
    assert_eq!(buffer.elevations()[0].state(), "savepoint_rolled_back");
}

/// reopens it; a second call summary is refused.
#[test]
fn frozen_and_header_twice_are_programming_errors() {
    let env = env(2);
    let call = || {
        AuditCall::new(
            Some("res.partner".into()),
            "write".into(),
            Effect::Write,
            TargetIds::none(),
            None,
        )
        .unwrap()
    };
    let mut buffer = AuditBuffer::new();
    buffer.set_call(call(), &env).unwrap();
    let error = buffer.set_call(call(), &env).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.message(),
        "a later release (header-twice): the call summary was already set for this transaction attempt"
    );
    buffer.freeze().unwrap();
    for error in [
        buffer.set_origin(AuditOrigin::Job).unwrap_err(),
        observe(&mut buffer, 0, 2, 1).unwrap_err(),
        buffer.freeze().unwrap_err(),
    ] {
        assert_eq!(error.kind(), ErrorKind::Programming);
        assert_eq!(
            error.message(),
            "a later release (frozen): the audit buffer is frozen for commit"
        );
    }
    buffer.clear();
    assert!(!buffer.wants_row());
    buffer.set_origin(AuditOrigin::Job).unwrap();
}

/// carries ids and enums only: the JSON has no field a name, prefix or secret could use.
#[test]
fn auth_event_json_has_only_ids_enums_and_counts() {
    let event = AuthEvent::ApiKeyIssued(ApiKeyIssued::new(
        ApiKeyId::new(91).unwrap(),
        UserId::new(7).unwrap(),
        UserId::new(7).unwrap(),
        crate::KeyKind::Agent,
        KeyRouteScope::Rpc,
        true,
        false,
        Some(1_700_000_000_000_000),
        Some(2),
        None,
    ));
    let json = event.to_json();
    let mut keys: Vec<&str> = json
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "capabilities_subject",
            "created_by",
            "event",
            "expires_utc_micros",
            "key_id",
            "key_kind",
            "method_allowlist_len",
            "model_allowlist_len",
            "owner",
            "read_only",
            "route",
            "type"
        ]
    );
    for value in json.as_object().unwrap().values() {
        assert!(!value.is_f64(), "integers only");
    }
}
