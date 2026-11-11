//! The error table (RPC-013's rendering half): key order, names, statuses, arguments, concealed
//! database messages.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

use std::sync::Arc;

use erp_fields::{Error, ErrorKind, Value, ValueMap};

use super::*;

fn key() -> IdempotencyKey {
    IdempotencyKey::parse("018f3a1c-7b2d-7c4e-9a10-3f2b1c0d9e8a").unwrap()
}

/// The body's keys, in order, and its parts.
fn parts(body: &[u8]) -> (Vec<String>, String, String, String, String, String) {
    let text = String::from_utf8(body.to_vec()).unwrap();
    let parsed = crate::json::parse(body, 64).unwrap();
    let crate::json::Json::Object(members) = parsed else {
        panic!("not an object: {text}");
    };
    let keys = members.iter().map(|(k, _)| k.to_string()).collect();
    let field = |name: &str| {
        let start = text.find(&format!("\"{name}\":")).unwrap() + name.len() + 3;
        let rest = &text[start..];
        let end = [
            ",\"message\"",
            ",\"arguments\"",
            ",\"timestamp\"",
            ",\"context\"",
            ",\"debug\"",
            ",\"request_id\"",
        ]
        .iter()
        .filter_map(|m| rest.find(m))
        .min()
        .unwrap_or(rest.len() - 1);
        rest[..end].to_owned()
    };
    (
        keys,
        field("name"),
        field("message"),
        field("arguments"),
        field("context"),
        field("debug"),
    )
}

#[test]
fn every_body_has_references_keys_in_order() {
    let failures: Vec<RpcFailure> = vec![
        Error::user("u").into(),
        ProtocolError::BadRequest("b".into()).into(),
        InfraError::IdempotencyAccessChanged.into(),
    ];
    for failure in failures {
        let (keys, ..) = parts(&failure.body("", 1, None));
        assert_eq!(
            keys,
            [
                "name",
                "message",
                "arguments",
                "timestamp",
                "context",
                "debug"
            ]
        );
    }
}

#[test]
fn the_request_id_is_one_more_key_after_references_and_only_when_there_is_one() {
    let failure: RpcFailure = ProtocolError::BadRequest("b".into()).into();
    let id = "0194f5a0-7b1c-7d2e-8f3a-123456789abc";
    let (keys, name, message, arguments, context, debug) = parts(&failure.body("dbg", 1, Some(id)));
    assert_eq!(
        keys,
        [
            "name",
            "message",
            "arguments",
            "timestamp",
            "context",
            "debug",
            "request_id"
        ]
    );
    let (_, plain_name, plain_message, plain_arguments, plain_context, plain_debug) =
        parts(&failure.body("dbg", 1, None));
    assert_eq!(
        (name, message, arguments, context, debug),
        (
            plain_name,
            plain_message,
            plain_arguments,
            plain_context,
            plain_debug
        )
    );
    let text = String::from_utf8(failure.body("dbg", 1, Some(id))).unwrap();
    assert!(
        text.ends_with(&format!(",\"request_id\":\"{id}\"}}")),
        "{text}"
    );
    assert!(
        !String::from_utf8(failure.body("dbg", 1, None))
            .unwrap()
            .contains("request_id")
    );
}

#[test]
fn domain_kinds_keep_references_status_name_and_text() {
    let cases = [
        (ErrorKind::UserError, 422, "erp.exceptions.UserError"),
        (ErrorKind::Validation, 422, "erp.exceptions.ValidationError"),
        (ErrorKind::Access, 403, "erp.exceptions.AccessError"),
        (ErrorKind::AccessDenied, 403, "erp.exceptions.AccessDenied"),
        (
            ErrorKind::SessionExpired,
            403,
            "erp.http.session.SessionExpiredException",
        ),
        (ErrorKind::Missing, 404, "erp.exceptions.MissingError"),
        (ErrorKind::Lock, 409, "erp.exceptions.LockError"),
        (ErrorKind::Redirect, 500, "erp.exceptions.RedirectWarning"),
        (ErrorKind::Programming, 500, "erp.programming"),
        (ErrorKind::Internal, 500, "erp.internal"),
        (ErrorKind::ResourceExhausted, 500, "erp.resource_exhausted"),
        (ErrorKind::Conflict, 409, "erp.conflict"),
    ];
    for (kind, status, name) in cases {
        let failure = RpcFailure::Domain(Error::new(kind, "the text"));
        assert_eq!(
            (failure.status(), failure.name()),
            (status, name),
            "{kind:?}"
        );
        let (_, body_name, message, arguments, context, debug) = parts(&failure.body("", 1, None));
        assert_eq!(body_name, format!("\"{name}\""));
        assert_eq!(message, "\"the text\"");
        assert_eq!(arguments, "[\"the text\"]");
        assert_eq!(context, "{}");
        assert_eq!(debug, "\"\"");
    }
}

/// A stale row version (`expect_versions`) is `409` with the name `erp.conflict` and the
/// ORM's text, which names the record and the versions and carries nothing the caller may not
/// know (error policy).
#[test]
fn a_stale_row_version_is_409_erp_conflict() {
    let text = "Record 'Scale Quant' (scale.quant) 7 was changed by someone else since you read \
                it (version 2, now 3). Reload it and apply your change again.";
    let failure = RpcFailure::Domain(Error::conflict(text));
    assert_eq!((failure.status(), failure.name()), (409, "erp.conflict"));
    assert_eq!(failure.message(), text);
    let (_, name, message, arguments, ..) = parts(&failure.body("", 1, None));
    assert_eq!(name, "\"erp.conflict\"");
    assert_eq!(message, format!("{text:?}"));
    assert_eq!(arguments, format!("[{text:?}]"));
}

#[test]
fn concurrency_and_database_errors_are_concealed() {
    let serial = RpcFailure::Domain(
        Error::new(
            ErrorKind::Concurrency,
            "could not serialize ... DETAIL: row (7, 'x')",
        )
        .with_sqlstate(Some("40001".into())),
    );
    assert_eq!(serial.name(), "psycopg2.errors.SerializationFailure");
    assert_eq!(
        serial.message(),
        "could not serialize access due to concurrent update"
    );
    let deadlock = RpcFailure::Domain(
        Error::new(
            ErrorKind::Concurrency,
            "deadlock detected\nDETAIL: Process 1 …",
        )
        .with_sqlstate(Some("40P01".into())),
    );
    assert_eq!(
        (deadlock.name(), deadlock.message().as_str()),
        ("psycopg2.errors.DeadlockDetected", "deadlock detected")
    );
    let superseded = RpcFailure::Domain(
        Error::new(ErrorKind::Concurrency, "the ORM was replaced")
            .with_sqlstate(Some("ERP01".into())),
    );
    assert_eq!(superseded.name(), "erp.exceptions.ConcurrencyError");
    assert_eq!(superseded.message(), "the ORM was replaced");

    let db = RpcFailure::Domain(
        Error::new(
            ErrorKind::Database,
            "value \"secret-sentinel\" violates check constraint",
        )
        .with_sqlstate(Some("23514".into())),
    );
    assert_eq!(db.message(), "A database error occurred (SQLSTATE 23514).");
    let body = String::from_utf8(db.body("", 1, None)).unwrap();
    assert!(!body.contains("secret-sentinel"), "{body}");
    let (_, name, _, arguments, ..) = parts(db.body("", 1, None).as_slice());
    assert_eq!(name, "\"erp.database\"");
    assert_eq!(
        arguments,
        "[\"A database error occurred (SQLSTATE 23514).\",\"23514\"]"
    );
    let no_state = RpcFailure::Domain(Error::new(ErrorKind::Database, "closed"));
    let (_, _, message, arguments, ..) = parts(&no_state.body("", 1, None));
    assert_eq!(message, "\"A database error occurred.\"");
    assert_eq!(arguments, "[\"A database error occurred.\",null]");
}

#[test]
fn protocol_and_infra_rows() {
    let rows: Vec<(RpcFailure, u16, &str, &str, &str)> = vec![
        (
            ProtocolError::BadRequest("could not parse the body as json: x".into()).into(),
            400,
            "werkzeug.exceptions.BadRequest",
            "could not parse the body as json: x",
            "[\"could not parse the body as json: x\",400]",
        ),
        (
            ProtocolError::NotFound("the model 'x' does not exist".into()).into(),
            404,
            "werkzeug.exceptions.NotFound",
            "the model 'x' does not exist",
            "[\"the model 'x' does not exist\",404]",
        ),
        (
            ProtocolError::RequestEntityTooLarge { limit: 2_097_152 }.into(),
            413,
            "werkzeug.exceptions.RequestEntityTooLarge",
            "The request body is larger than the server limit of 2097152 bytes \
             (limits.rpc_body_bytes).",
            "[\"The request body is larger than the server limit of 2097152 bytes \
             (limits.rpc_body_bytes).\",413]",
        ),
        (
            ProtocolError::UnprocessableEntity("missing a required argument: 'domain'".into())
                .into(),
            422,
            "werkzeug.exceptions.UnprocessableEntity",
            "missing a required argument: 'domain'",
            "[\"missing a required argument: 'domain'\",422]",
        ),
        (
            ProtocolError::SessionExpired("Session expired".into()).into(),
            403,
            "erp.http.session.SessionExpiredException",
            "Session expired",
            "[\"Session expired\"]",
        ),
        (
            InfraError::IdempotencyInProgress {
                key: key(),
                retry_after: 1,
            }
            .into(),
            409,
            "erp.idempotency_in_progress",
            "A request with Idempotency-Key 018f3a1c-7b2d-7c4e-9a10-3f2b1c0d9e8a is still \
             running; retry after 1 seconds.",
            "[\"A request with Idempotency-Key 018f3a1c-7b2d-7c4e-9a10-3f2b1c0d9e8a is still \
             running; retry after 1 seconds.\"]",
        ),
        (
            InfraError::IdempotencyAccessChanged.into(),
            409,
            "erp.idempotency_access_changed",
            "idempotency replay refused: access changed since the original call; the original \
             call committed",
            "[\"idempotency replay refused: access changed since the original call; the \
             original call committed\"]",
        ),
        (
            InfraError::IdempotencyResultExpired.into(),
            409,
            "erp.idempotency_result_expired",
            "the original call committed; its result is no longer available",
            "[\"the original call committed; its result is no longer available\"]",
        ),
        (
            InfraError::IdempotencyMismatch { key: key() }.into(),
            422,
            "erp.idempotency_mismatch",
            "Idempotency-Key 018f3a1c-7b2d-7c4e-9a10-3f2b1c0d9e8a was already used for a \
             different request; use a new key.",
            "[\"Idempotency-Key 018f3a1c-7b2d-7c4e-9a10-3f2b1c0d9e8a was already used for a \
             different request; use a new key.\"]",
        ),
        (
            InfraError::IdempotencyNotAllowed(IdempotencyRefusal::ReadEffect {
                entry: "res.users.read".into(),
            })
            .into(),
            422,
            "erp.idempotency_not_allowed",
            "Idempotency-Key is accepted only on calls that change data; res.users.read has \
             effect 'read'.",
            "[\"Idempotency-Key is accepted only on calls that change data; res.users.read has \
             effect 'read'.\"]",
        ),
        (
            InfraError::IdempotencyNotAllowed(IdempotencyRefusal::CallerKind { kind: "public" })
                .into(),
            422,
            "erp.idempotency_not_allowed",
            "Idempotency-Key is not accepted from public callers: they have no private key \
             space.",
            "[\"Idempotency-Key is not accepted from public callers: they have no private key \
             space.\"]",
        ),
        (
            InfraError::ResultTooLarge {
                entry: "m.f".into(),
                limit: 1_048_576,
            }
            .into(),
            422,
            "erp.result_too_large",
            "The result of m.f is larger than the server limit of 1048576 bytes \
             (limits.rpc_result_bytes); ask for fewer records or fields, or set bin_size.",
            "[\"The result of m.f is larger than the server limit of 1048576 bytes \
             (limits.rpc_result_bytes); ask for fewer records or fields, or set bin_size.\"]",
        ),
        (
            InfraError::RateLimited { retry_after: 3 }.into(),
            429,
            "erp.rate_limited",
            "Rate limit exceeded; retry after 3 seconds.",
            "[\"Rate limit exceeded; retry after 3 seconds.\"]",
        ),
        (
            InfraError::CommitUnknown {
                entry: "m.f".into(),
                keyed: true,
            }
            .into(),
            500,
            "erp.commit_unknown",
            "The database connection was lost while committing m.f; the change may or may not \
             have been applied. Retry with the same Idempotency-Key to get the outcome.",
            "[\"The database connection was lost while committing m.f; the change may or may \
             not have been applied. Retry with the same Idempotency-Key to get the outcome.\"]",
        ),
        (
            InfraError::CommitUnknown {
                entry: "m.f".into(),
                keyed: false,
            }
            .into(),
            500,
            "erp.commit_unknown",
            "The database connection was lost while committing m.f; the change may or may not \
             have been applied. Check the data before retrying.",
            "[\"The database connection was lost while committing m.f; the change may or may \
             not have been applied. Check the data before retrying.\"]",
        ),
        (
            InfraError::NotImplemented {
                entry: "res.users.copy".into(),
                task: "a later release".into(),
            }
            .into(),
            501,
            "erp.not_implemented",
            "res.users.copy is not supported until a later release.",
            "[\"res.users.copy is not supported until a later release.\"]",
        ),
        (
            InfraError::Overloaded { retry_after: 2 }.into(),
            503,
            "erp.overloaded",
            "Server busy; retry after 2 seconds.",
            "[\"Server busy; retry after 2 seconds.\"]",
        ),
        (
            InfraError::Maintenance {
                database: "db1".into(),
                reason: "upgrade".into(),
            }
            .into(),
            503,
            "erp.maintenance",
            "Database db1 is under maintenance (upgrade). Retry later.",
            "[\"Database db1 is under maintenance (upgrade). Retry later.\"]",
        ),
        (
            InfraError::DeadlineExceeded { secs: 120 }.into(),
            504,
            "erp.deadline_exceeded",
            "The request exceeded its deadline of 120 s (limits.rpc_deadline_secs).",
            "[\"The request exceeded its deadline of 120 s (limits.rpc_deadline_secs).\"]",
        ),
    ];
    for (failure, status, name, message, arguments) in rows {
        assert_eq!(failure.status(), status, "{name}");
        assert_eq!(failure.name(), name);
        assert_eq!(failure.message(), message);
        let (_, _, _, body_arguments, ..) = parts(&failure.body("", 1, None));
        assert_eq!(body_arguments, arguments, "{name}");
    }
    assert_eq!(
        RpcFailure::from(InfraError::Overloaded { retry_after: 2 }).retry_after(),
        Some(2)
    );
    assert_eq!(
        RpcFailure::from(InfraError::IdempotencyAccessChanged).retry_after(),
        None
    );
}

#[test]
fn a_domain_context_dictionary_is_carried() {
    let context = Value::Map(Arc::new(ValueMap::from([(
        "blocking_model".to_owned(),
        Value::str("test_rpc.model_a"),
    )])));
    let failure = RpcFailure::Domain(Error::validation("blocked").with_context(context));
    let (_, _, _, _, context, _) = parts(&failure.body("", 1, None));
    assert_eq!(context, "{\"blocking_model\":\"test_rpc.model_a\"}");
}

#[test]
fn database_lock_texts_are_replaced() {
    let pg = RpcFailure::Domain(
        Error::new(
            ErrorKind::Lock,
            "could not obtain lock on row in relation \"secret_table\"",
        )
        .with_sqlstate(Some("55P03".into())),
    );
    assert_eq!(
        (pg.status(), pg.name(), pg.message().as_str()),
        (
            409,
            "erp.exceptions.LockError",
            "Cannot grab a lock on records"
        )
    );
    let orm = RpcFailure::Domain(Error::new(ErrorKind::Lock, "Cannot lock the order"));
    assert_eq!(orm.message(), "Cannot lock the order");
}

#[test]
fn debug_text_holds_the_chain_never_the_detail() {
    let failure = RpcFailure::Domain(
        Error::new(ErrorKind::Programming, "boom").with_source(Error::validation("inner reason")),
    );
    let info = DebugInfo {
        correlation: Some("01234567-89ab-cdef-0123-456789abcdef".into()),
        entry: Some("m.f".into()),
        statement: Some("UPDATE \"t\" SET \"a\" = $1 WHERE id = ANY($2)".into()),
    };
    assert_eq!(
        info.render(&failure),
        "kind: erp.programming (500)\nerror kind: Programming\nerror: boom\ncaused by: inner \
         reason\nstatement: UPDATE \"t\" SET \"a\" = $1 WHERE id = ANY($2)\nspan: rpc.call \
         m.f\ncorrelation: 01234567-89ab-cdef-0123-456789abcdef"
    );
}

#[test]
fn an_unwritable_context_is_left_out() {
    let context = Value::Map(Arc::new(ValueMap::from([(
        "ratio".to_owned(),
        Value::Float(f64::NAN),
    )])));
    let failure = RpcFailure::Domain(Error::validation("blocked").with_context(context));
    let (_, _, message, _, context, _) = parts(&failure.body("", 1, None));
    assert_eq!(message, "\"blocked\"");
    assert_eq!(context, "{}");
}

#[test]
fn erp_namespace_is_emitted_by_domain_failures() {
    let failure: RpcFailure = Error::validation("invalid").into();
    assert_eq!(failure.name(), "erp.exceptions.ValidationError");
    let (_, name, ..) = parts(&failure.body("", 1, None));
    assert_eq!(name, "\"erp.exceptions.ValidationError\"");
    let expired: RpcFailure = ProtocolError::SessionExpired("expired".into()).into();
    assert_eq!(expired.name(), "erp.http.session.SessionExpiredException");
    assert_eq!(expired.status(), 403);
}
