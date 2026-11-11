#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]
use super::{
    Buffer, BufferLimits, Envelope, EnvelopeParts, IdEntry, Kind, MAX_ENVELOPE_BYTES,
    MAX_ENVELOPES, MAX_TRANSACTION_BYTES,
};
use crate::correlation::CorrelationId;
use erp_fields::ErrorKind;

fn parts(id: i64) -> EnvelopeParts<'static> {
    EnvelopeParts {
        kind: Kind::Create,
        model: "res.partner",
        record_ids: vec![IdEntry::Single(id)],
        field_names: Vec::new(),
        cascade_models: Vec::new(),
        subject: Some("2".into()),
        actor_id: 2,
        actor_kind: super::capture::ActorKind::User,
        principal_kind: crate::PrincipalKind::User,
        operation_id: crate::OperationIdentity::generate()
            .unwrap()
            .operation()
            .clone(),
        delegating_user_id: None,
        company_ids: vec![1],
        correlation_id: None,
        causation_id: None,
    }
}

fn event(id: i64) -> Envelope {
    Envelope::new(parts(id)).unwrap()
}

fn ids(buffer: &Buffer) -> Vec<String> {
    buffer
        .events()
        .iter()
        .map(|q| q.envelope().record_ids().to_owned())
        .collect()
}

#[test]
fn rollback_and_release_keep_only_the_surviving_ordered_prefix() {
    let mut buffer = Buffer::new(BufferLimits::default()).unwrap();
    buffer.push(event(1), 0).unwrap();
    buffer.push(event(2), 1).unwrap();
    buffer.push(event(3), 2).unwrap();
    buffer.released_to(1);
    buffer.rolled_back_to(0);
    assert_eq!(ids(&buffer), ["[1]"]);
    assert_eq!(buffer.bytes, event(1).charged_bytes());
    buffer.push(event(4), 1).unwrap();
    buffer.released_to(0);
    buffer.push(event(5), 1).unwrap();
    buffer.rolled_back_to(0);
    assert_eq!(buffer.events().len(), 2);
    assert_eq!(ids(&buffer), ["[1]", "[4]"]);
    buffer.clear();
    assert!(buffer.events().is_empty());
    assert_eq!(buffer.bytes, 0);
    buffer.push(event(6), 0).unwrap();
}

#[test]
fn capture_limits_refuse_instead_of_truncating() {
    let mut buffer = Buffer::new(BufferLimits {
        max_envelopes: 1,
        max_bytes: MAX_TRANSACTION_BYTES,
    })
    .unwrap();
    buffer.push(event(1), 0).unwrap();
    let before = buffer.bytes;
    let error = buffer.push(event(2), 0).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::ResourceExhausted);
    assert_eq!(
        error.message(),
        "outbox CaptureLimit: transaction exceeds 1 envelopes"
    );
    assert_eq!(buffer.events().len(), 1);
    assert_eq!(buffer.bytes, before);
    buffer.freeze().unwrap();
    let error = buffer.push(event(3), 0).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(error.message(), "outbox: capture is frozen for commit");
    buffer.clear();
    buffer.push(event(4), 0).unwrap();
    for limits in [
        BufferLimits {
            max_envelopes: 0,
            max_bytes: MAX_TRANSACTION_BYTES,
        },
        BufferLimits {
            max_envelopes: MAX_ENVELOPES + 1,
            max_bytes: MAX_TRANSACTION_BYTES,
        },
        BufferLimits {
            max_envelopes: 1,
            max_bytes: 0,
        },
        BufferLimits {
            max_envelopes: 1,
            max_bytes: MAX_TRANSACTION_BYTES + 1,
        },
    ] {
        let error = Buffer::new(limits).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Programming);
        assert_eq!(error.message(), "outbox: invalid capture buffer limits");
    }
}

// Reserved-created-id range encoding is deferred with its absent S9 producer. Restore the
// helper and `created_ranges_never_cover_an_unused_reserved_id` from f22853b74387854121f09829cfc78f7f7fc44285
// when adding actual reserved-block capture; active S6 tests assert only single inserted ids.

#[test]
fn envelope_encodes_the_supplied_columns_names_only() {
    let correlation = CorrelationId::generate().unwrap();
    let mut p = parts(0);
    p.kind = Kind::Unlink;
    p.record_ids = [5, 6, 7, 9].into_iter().map(IdEntry::Single).collect();
    p.field_names = vec!["name".into()];
    p.cascade_models = vec!["res.partner.bank".into()];
    p.subject = Some("partner".into());
    p.delegating_user_id = Some(7);
    p.company_ids = vec![1, 3];
    p.correlation_id = Some(correlation.clone());
    let e = Envelope::new(p).unwrap();
    assert_eq!(e.kind().as_str(), "unlink");
    assert_eq!(e.model(), "res.partner");
    assert_eq!(e.record_ids(), "[5,6,7,9]");
    assert_eq!(e.field_names(), ["name"]);
    assert_eq!(e.cascade_models(), ["res.partner.bank"]);
    assert_eq!(e.subject(), "partner");
    assert_eq!((e.actor_id(), e.delegating_user_id()), (2, Some(7)));
    assert_eq!(e.company_ids(), [1, 3]);
    assert_eq!(e.correlation_id(), Some(&correlation));
    assert!(e.causation_id().is_none());
    assert!(e.encoded_bytes <= MAX_ENVELOPE_BYTES);
}

#[test]
fn envelope_bounds_refuse_instead_of_truncating() {
    let mut p = parts(1);
    p.subject = Some("s".repeat(1025));
    let error = Envelope::new(p).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::ResourceExhausted);
    assert!(error.message().starts_with("outbox CaptureLimit:"));

    // Both name lists fit their own count bounds, but escaping exceeds 64 KiB.
    let mut p = parts(1);
    p.kind = Kind::Unlink;
    p.field_names = (0..128).map(|_| "\\".repeat(255)).collect();
    p.cascade_models = (0..64).map(|i| format!("{i:0>255}")).collect();
    let error = Envelope::new(p).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::ResourceExhausted);
    assert_eq!(
        error.message(),
        "outbox CaptureLimit: event exceeds 65536 bytes"
    );

    let mut p = parts(1);
    p.field_names = (0..257).map(|i| format!("f{i}")).collect();
    assert_eq!(
        Envelope::new(p).unwrap_err().kind(),
        ErrorKind::ResourceExhausted
    );

    let mut p = parts(1);
    p.record_ids = (0..257).map(|i| IdEntry::Single(i * 2 + 1)).collect();
    assert_eq!(
        Envelope::new(p).unwrap_err().message(),
        "outbox CaptureLimit: event exceeds 256 id entries"
    );

    let mut p = parts(1);
    p.company_ids = (0..257).collect();
    assert_eq!(
        Envelope::new(p).unwrap_err().kind(),
        ErrorKind::ResourceExhausted
    );
}

#[test]
fn envelope_rejects_malformed_input() {
    let mut cases = Vec::new();
    let mut p = parts(1);
    p.model = "";
    cases.push(p);
    let mut p = parts(1);
    p.record_ids = Vec::new();
    cases.push(p);
    let mut p = parts(1);
    p.actor_id = 0;
    cases.push(p);
    let mut p = parts(1);
    p.cascade_models = vec!["a.b".into()];
    cases.push(p);
    let mut p = parts(1);
    p.field_names = vec!["bad\0name".into()];
    cases.push(p);
    let mut p = parts(1);
    p.field_names = vec![String::new()];
    cases.push(p);
    for p in cases {
        assert_eq!(Envelope::new(p).unwrap_err().kind(), ErrorKind::Programming);
    }
}

fn capture_env() -> crate::Env {
    use erp_registry::{AddonDescriptor, RegistryBuilder};
    use std::sync::Arc;
    static ADDON: AddonDescriptor = AddonDescriptor {
        name: "outbox_capture",
        depends: &["base"],
        exact_only: false,
    };
    crate::Env::new(
        Arc::new(RegistryBuilder::new([&ADDON]).build().unwrap()),
        crate::Principal::for_uid(crate::UserId::new(2).unwrap()),
        crate::Context::new(),
    )
}

#[test]
fn capture_context_requires_identity_and_keeps_original_actor() {
    let env = capture_env();
    let error = super::CaptureContext::from_env(&env, vec![1]).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.message(),
        "outbox: eventful mutation requires an OperationIdentity"
    );
    let identity = crate::OperationIdentity::generate().unwrap();
    let env = env
        .with_operation_identity(Some(std::sync::Arc::new(identity.clone())))
        .with_user(3);
    let capture = super::CaptureContext::from_env(&env, vec![7, 1]).unwrap();
    let event = capture
        .envelope(
            Kind::Create,
            "res.partner",
            vec![IdEntry::Single(9)],
            vec!["name".into()],
            Vec::new(),
        )
        .unwrap();
    assert_eq!(event.subject(), "3");
    assert_eq!(event.principal_kind(), "user");
    assert_eq!(event.actor_kind(), "user");
    assert_eq!(event.actor_id(), 2);
    assert_eq!(event.delegating_user_id(), Some(2));
    assert_eq!(event.operation_id(), identity.operation());
    assert_eq!(event.correlation_id(), Some(identity.correlation()));
    assert_eq!(event.company_ids(), [7, 1]);
    assert_eq!(event.field_names(), ["name"]);
}

#[test]
fn capture_context_refuses_invalid_companies() {
    let env = capture_env().with_operation_identity(Some(std::sync::Arc::new(
        crate::OperationIdentity::generate().unwrap(),
    )));
    for ids in [vec![0], vec![-1], vec![1, 1]] {
        let error = super::CaptureContext::from_env(&env, ids).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Programming);
        assert_eq!(
            error.message(),
            "outbox: capture companies must be positive and unique"
        );
    }
}

#[test]
fn actor_discriminators_preserve_every_current_actor_variant() {
    use crate::{Actor, ApiKeyId, KeyKind, LinkId, PrincipalKind, UserId};
    let subject = UserId::new(2).unwrap();
    for (kind, tag) in [
        (KeyKind::User, "api_key_user"),
        (KeyKind::Agent, "api_key_agent"),
        (KeyKind::Service, "api_key_service"),
    ] {
        let actor = Actor::ApiKey {
            id: ApiKeyId::new(8).unwrap(),
            kind,
            name: "MUST NOT LEAK".into(),
        };
        let (actor_kind, id, delegated) =
            super::capture::actor_details(PrincipalKind::User, subject, Some(&actor)).unwrap();
        assert_eq!(actor_kind.as_str(), tag);
        assert_eq!(id, 8);
        assert_eq!(
            delegated,
            if kind == KeyKind::Agent {
                Some(2)
            } else {
                None
            }
        );
    }
    let actor = Actor::Link {
        id: LinkId::new(9).unwrap(),
    };
    let (kind, id, delegated) =
        super::capture::actor_details(PrincipalKind::Link, subject, Some(&actor)).unwrap();
    assert_eq!((kind.as_str(), id, delegated), ("link", 9, None));
    let (kind, id, delegated) =
        super::capture::actor_details(PrincipalKind::System, crate::SUPERUSER_ID, None).unwrap();
    assert_eq!((kind.as_str(), id, delegated), ("system", 1, None));
}

#[test]
fn persistence_payload_preserves_ordinals_and_ragged_arrays() {
    let mut buffer = Buffer::new(BufferLimits::default()).unwrap();
    let mut one = parts(7);
    one.field_names = vec!["name".into(), "active".into()];
    one.company_ids = vec![1, 3];
    buffer.push(Envelope::new(one).unwrap(), 0).unwrap();
    let mut two = parts(2);
    two.field_names = vec!["name".into()];
    two.company_ids = vec![3];
    buffer.push(Envelope::new(two).unwrap(), 0).unwrap();
    let encoded = super::persist::encode(buffer.events()).unwrap();
    let json: serde_json::Value = serde_json::from_str(&encoded).unwrap();
    assert_eq!(json[0]["tx_ordinal"], 1);
    assert_eq!(json[1]["tx_ordinal"], 2);
    assert_eq!(json[0]["record_ids"], "[7]");
    assert_eq!(json[1]["record_ids"], "[2]");
    assert_eq!(
        json[0]["field_names"],
        serde_json::json!(["name", "active"])
    );
    assert_eq!(json[1]["field_names"], serde_json::json!(["name"]));
    assert_eq!(json[0]["company_ids"], serde_json::json!([1, 3]));
    assert_eq!(json[1]["company_ids"], serde_json::json!([3]));
    for row in json.as_array().unwrap() {
        for generated in ["id", "event_id", "xid", "create_date"] {
            assert!(row.get(generated).is_none());
        }
    }
}

#[test]
fn persistence_encoded_bytes_are_covered_by_capture_charge() {
    let mut buffer = Buffer::new(BufferLimits::default()).unwrap();
    for id in 1..=100 {
        let mut p = parts(id);
        p.field_names = (0..120).map(|_| "\\".repeat(250)).collect();
        buffer.push(Envelope::new(p).unwrap(), 0).unwrap();
    }
    assert!(buffer.bytes < MAX_TRANSACTION_BYTES);
    let encoded = super::encode(buffer.events()).unwrap();
    assert!(encoded.len() <= buffer.bytes);
    assert!(encoded.len() <= MAX_TRANSACTION_BYTES);
}

#[test]
fn impersonated_key_keeps_namespace_and_omits_key_name() {
    use crate::{ApiKeyId, Context, Env, MethodAllow, ModelAllow, PrincipalBuilder, Scope, UserId};
    let scope = Scope::builder(ApiKeyId::new(44).unwrap(), "DO-NOT-PERSIST-THIS-KEY-NAME")
        .models(ModelAllow::All)
        .methods(MethodAllow::All)
        .build()
        .unwrap();
    let principal = PrincipalBuilder::user(UserId::new(2).unwrap())
        .scope(scope)
        .build()
        .unwrap();
    let env = Env::new(capture_env().registry().clone(), principal, Context::new())
        .with_operation_identity(Some(std::sync::Arc::new(
            crate::OperationIdentity::generate().unwrap(),
        )))
        .with_user(3);
    let capture = super::CaptureContext::from_env(&env, vec![]).unwrap();
    let event = capture
        .envelope(
            Kind::Create,
            "res.partner",
            vec![IdEntry::Single(4)],
            vec!["name".into()],
            vec![],
        )
        .unwrap();
    assert_eq!(event.subject(), "3");
    assert_eq!(event.actor_kind(), "api_key_user");
    assert_eq!(event.actor_id(), 44);
    assert_eq!(event.delegating_user_id(), Some(2));
    let mut buffer = Buffer::new(BufferLimits::default()).unwrap();
    buffer.push(event, 0).unwrap();
    let encoded = super::encode(buffer.events()).unwrap();
    assert!(!encoded.contains("DO-NOT-PERSIST-THIS-KEY-NAME"));
}

#[test]
fn captures_distinguish_executions_sharing_one_root_correlation() {
    let root = CorrelationId::generate().unwrap();
    let one = crate::OperationIdentity::new(root.clone()).unwrap();
    let two = crate::OperationIdentity::new(root.clone()).unwrap();
    let event = |identity| {
        let env = capture_env().with_operation_identity(Some(std::sync::Arc::new(identity)));
        super::CaptureContext::from_env(&env, vec![])
            .unwrap()
            .envelope(
                Kind::Create,
                "res.partner",
                vec![IdEntry::Single(1)],
                vec![],
                vec![],
            )
            .unwrap()
    };
    let first = event(one);
    let second = event(two);
    assert_eq!(first.correlation_id(), Some(&root));
    assert_eq!(second.correlation_id(), Some(&root));
    assert_ne!(first.operation_id(), second.operation_id());
}

#[test]
fn envelope_enforces_distinct_field_and_cascade_name_caps() {
    let mut p = parts(1);
    p.kind = Kind::Unlink;
    p.field_names = (0..128).map(|i| format!("field_{i}")).collect();
    p.cascade_models = (0..64).map(|i| format!("model_{i}")).collect();
    let event = Envelope::new(p).unwrap();
    assert_eq!(event.field_names().len(), 128);
    assert_eq!(event.cascade_models().len(), 64);

    let mut p = parts(1);
    p.field_names = (0..129).map(|i| format!("field_{i}")).collect();
    let error = Envelope::new(p).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::ResourceExhausted);
    assert_eq!(
        error.message(),
        "outbox CaptureLimit: event exceeds 128 changed field names"
    );

    let mut p = parts(1);
    p.kind = Kind::Unlink;
    p.cascade_models = (0..65).map(|i| format!("model_{i}")).collect();
    let error = Envelope::new(p).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::ResourceExhausted);
    assert_eq!(
        error.message(),
        "outbox CaptureLimit: event exceeds 64 cascade models"
    );
}

#[test]
fn envelope_refuses_escaped_payload_exceeding_64k() {
    let mut p = parts(1);
    p.kind = Kind::Unlink;
    p.field_names = (0..128).map(|_| "\\".repeat(255)).collect();
    p.cascade_models = (0..64).map(|i| format!("{i:0>255}")).collect();
    let error = Envelope::new(p).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::ResourceExhausted);
    assert_eq!(
        error.message(),
        "outbox CaptureLimit: event exceeds 65536 bytes"
    );
}

#[test]
fn envelope_admits_exact_encoded_limit_and_refuses_one_byte_over() {
    let make = |subject: String| {
        let mut p = parts(1);
        p.field_names = (0..128).map(|_| "\\".repeat(250)).collect();
        p.subject = Some(subject);
        p
    };
    let mut buffer = Buffer::new(BufferLimits::default()).unwrap();
    buffer
        .push(Envelope::new(make(String::new())).unwrap(), 0)
        .unwrap();
    // encode has outer array brackets and ordinal 1; admission reserves ordinal 4096.
    let row_bytes = super::encode(buffer.events()).unwrap().len() - 2 + 3;
    let padding = MAX_ENVELOPE_BYTES.checked_sub(row_bytes).unwrap();
    assert!(
        padding <= 1024,
        "the subject fills the remaining bytes within its own bound"
    );
    let event = Envelope::new(make("s".repeat(padding))).unwrap();
    assert_eq!(event.encoded_bytes, MAX_ENVELOPE_BYTES);
    let error = Envelope::new(make("s".repeat(padding + 1))).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::ResourceExhausted);
    assert_eq!(
        error.message(),
        "outbox CaptureLimit: event exceeds 65536 bytes"
    );
}

#[test]
fn envelope_requires_a_subject_before_admission() {
    let mut p = parts(1);
    p.subject = None;
    let error = Envelope::new(p).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(error.message(), "outbox: an event requires a subject");
}

#[test]
fn envelope_charge_includes_reserved_heap_capacity() {
    let mut p = parts(1);
    let mut name = String::with_capacity(1024);
    name.push('x');
    p.field_names = Vec::with_capacity(128);
    p.field_names.push(name);
    p.cascade_models = Vec::with_capacity(64);
    p.company_ids = Vec::with_capacity(256);
    p.company_ids.push(1);
    let mut subject = String::with_capacity(1024);
    subject.push('2');
    p.subject = Some(subject);
    let event = Envelope::new(p).unwrap();
    let metadata_and_spare = event.field_names.capacity() * std::mem::size_of::<String>()
        + event.cascade_models.capacity() * std::mem::size_of::<String>()
        + event
            .field_names
            .iter()
            .map(|name| name.capacity() - name.len())
            .sum::<usize>()
        + event.company_ids.capacity() * std::mem::size_of::<i64>()
        + event.subject.capacity()
        - event.subject.len();
    assert!(
        event.charged_bytes()
            >= event.encoded_bytes + std::mem::size_of::<super::Queued>() + metadata_and_spare
    );
}

#[test]
fn envelope_spare_capacity_cannot_bypass_transaction_memory_limit() {
    let mut p = parts(1);
    let mut name = String::with_capacity(MAX_ENVELOPE_BYTES + 1);
    name.push('x');
    p.field_names = vec![name];
    let event = Envelope::new(p).unwrap();
    assert!(event.encoded_bytes < MAX_ENVELOPE_BYTES);
    let mut buffer = Buffer::new(BufferLimits {
        max_envelopes: MAX_ENVELOPES,
        max_bytes: MAX_ENVELOPE_BYTES,
    })
    .unwrap();
    let error = buffer.push(event, 0).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::ResourceExhausted);
    assert_eq!(
        error.message(),
        "outbox CaptureLimit: transaction exceeds 65536 bytes"
    );
    assert!(buffer.events().is_empty());
    assert_eq!(buffer.bytes, 0);
}

#[cfg(test)]
mod pending_provenance_tests {
    use super::super::{CaptureContext, Pending};
    use erp_fields::RecId;
    use erp_registry::{AddonDescriptor, FieldDecl, ModelDecl, RegistryBuilder};
    use erp_registry::{FieldId, ModelId};
    use std::sync::Arc;
    fn fixture() -> (ModelId, FieldId, Arc<CaptureContext>) {
        static ADDON: AddonDescriptor = AddonDescriptor {
            name: "pending",
            depends: &["base"],
            exact_only: false,
        };
        static MODEL: ModelDecl =
            ModelDecl::new("pending", "pending.item").fields(&[FieldDecl::char("name")]);
        let registry = Arc::new(
            RegistryBuilder::new([&ADDON])
                .with_decls([&MODEL])
                .build()
                .unwrap(),
        );
        let spec = registry.model("pending.item").unwrap();
        let key = (spec.id, spec.field_id("name").unwrap());
        let env = crate::Env::new(
            registry,
            crate::Principal::for_uid(crate::UserId::new(2).unwrap()),
            crate::Context::new(),
        )
        .with_operation_identity(Some(Arc::new(
            crate::OperationIdentity::generate().unwrap(),
        )));
        (
            key.0,
            key.1,
            Arc::new(CaptureContext::from_env(&env, vec![]).unwrap()),
        )
    }

    #[test]
    fn missing_or_overwritten_origin_fails_loudly() {
        let (m, f, origin) = fixture();
        let id = RecId::from_real(1).unwrap();
        let mut pending = Pending::default();
        let error = pending.bind(m, f, &[id]).unwrap_err();
        assert_eq!(error.kind(), erp_fields::ErrorKind::Programming);
        assert_eq!(
            error.message(),
            "outbox: event-relevant pending work is missing writer provenance"
        );
        pending.active = Some(Arc::clone(&origin));
        pending.bind(m, f, &[id]).unwrap();
        pending.bind(m, f, &[id]).unwrap();
        pending.active = Some(fixture().2);
        let error = pending.bind(m, f, &[id]).unwrap_err();
        assert_eq!(error.kind(), erp_fields::ErrorKind::Programming);
        assert_eq!(
            error.message(),
            "outbox: pending provenance would overwrite an unsettled operation"
        );
        assert!(Arc::ptr_eq(&pending.origin(m, f, id).unwrap(), &origin));
    }
}
