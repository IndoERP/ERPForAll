//! The documents the TypeScript side is generated from must name things exactly as the validated
//! types do: a drift here would make the generated declarations lie about the wire.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use erp_store_wire::doc::{ErrorCodeDoc, FieldKindDoc, LimitDoc, RefusalDoc, SyncClassDoc};
use erp_store_wire::{FieldKind, Limit, SyncClass, WireError};
use serde_json::{Value, json};

fn name_of<T: serde::Serialize>(value: &T) -> Value {
    serde_json::to_value(value).expect("serializes")
}

fn path() -> String {
    "p".to_owned()
}

#[test]
fn error_code_documents_use_the_wire_codes() {
    let errors = vec![
        WireError::TypeMismatch {
            path: path(),
            expected: "x",
        },
        WireError::ValueOutOfRange {
            path: path(),
            reason: "r".to_owned(),
        },
        WireError::DuplicateId {
            path: path(),
            id: 1,
        },
        WireError::DuplicateField { path: path() },
        WireError::AddRemoveOverlap {
            path: path(),
            id: 1,
        },
        WireError::EmptyOperation { path: path() },
        WireError::OperationNotAllowed {
            path: path(),
            reason: "r".to_owned(),
        },
        WireError::ZeroDelta { path: path() },
        WireError::UnknownModel {
            path: path(),
            model: "m".to_owned(),
        },
        WireError::UnknownField {
            path: path(),
            field: "f".to_owned(),
        },
        WireError::UnknownOperation {
            path: path(),
            operation: "o".to_owned(),
        },
        WireError::BadId {
            path: path(),
            value: "0".to_owned(),
        },
        WireError::DeleteWithFields { path: path() },
        WireError::BadWritten {
            path: path(),
            reason: "r".to_owned(),
        },
        WireError::Malformed {
            path: path(),
            reason: "r".to_owned(),
        },
        WireError::WireMajorUnsupported { found: 2 },
        WireError::ClientOutdated {
            client: 2,
            supported: 1,
        },
        WireError::BadDescriptor {
            path: path(),
            reason: "r".to_owned(),
        },
        WireError::BadSchema {
            path: path(),
            reason: "r".to_owned(),
        },
        WireError::BadName {
            path: path(),
            reason: "r".to_owned(),
        },
        WireError::LimitExceeded {
            limit: "bytes",
            path: path(),
            maximum: 1,
        },
        WireError::BadLimit {
            limit: "bytes",
            reason: "r".to_owned(),
        },
    ];
    for error in &errors {
        assert_eq!(
            name_of(&ErrorCodeDoc::from(error)),
            json!(error.code()),
            "{error}"
        );
    }
    let mut codes: Vec<&str> = errors.iter().map(WireError::code).collect();
    codes.sort_unstable();
    codes.dedup();
    assert_eq!(codes.len(), errors.len(), "every refusal has its own code");
}

#[test]
fn refusal_document_carries_code_path_and_limit() {
    let error = WireError::LimitExceeded {
        limit: "ids_per_list",
        path: "a.b".to_owned(),
        maximum: 3,
    };
    let doc = RefusalDoc::from(&error);
    assert_eq!(
        name_of(&doc),
        json!({"code": "limit_exceeded", "path": "a.b", "limit": "ids_per_list"})
    );
    let plain = RefusalDoc::from(&WireError::WireMajorUnsupported { found: 2 });
    assert_eq!(
        name_of(&plain),
        json!({"code": "wire_major_unsupported", "path": null, "limit": null})
    );
}

#[test]
fn kind_sync_and_limit_documents_use_the_wire_names() {
    for kind in [
        FieldKind::Bool,
        FieldKind::Int,
        FieldKind::Float,
        FieldKind::String,
        FieldKind::Html,
        FieldKind::Date,
        FieldKind::Datetime,
        FieldKind::Selection,
        FieldKind::Many2One,
        FieldKind::Reference,
        FieldKind::X2Many,
        FieldKind::Counter,
    ] {
        assert_eq!(name_of(&FieldKindDoc::from(kind)), json!(kind.name()));
    }
    assert_eq!(
        name_of(&SyncClassDoc::from(SyncClass::State)),
        json!("state")
    );
    assert_eq!(
        name_of(&SyncClassDoc::from(SyncClass::Counter)),
        json!("counter")
    );
    assert_eq!(
        name_of(&SyncClassDoc::from(SyncClass::AnswerOnly)),
        json!("answer_only")
    );
    for limit in Limit::ALL {
        assert_eq!(name_of(&LimitDoc::from(limit)), json!(limit.name()));
    }
}
