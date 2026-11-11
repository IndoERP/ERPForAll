//! The describe-only export: `describe` answers from the source it is
//! given, every other export refuses with a typed error naming the slice that adds it.

#![allow(clippy::expect_used, clippy::panic, reason = "test assertions")]

use erp_guest::bindings::exports::erpforall::addon::addon_guest::Guest;
use erp_guest::{
    BoundaryError, BoundaryErrorKind, CallContext, DISPATCH_SLICE, DescribeOnly, DescribeSource,
    RecordBatch,
};

struct Fixed;

impl DescribeSource for Fixed {
    fn describe() -> String {
        r#"{"schema":1,"contributions":[]}"#.to_owned()
    }
}

type Export = DescribeOnly<Fixed>;

fn context() -> CallContext {
    CallContext {
        user_id: 2,
        company_id: 1,
        allowed_company_ids: vec![1],
        lang: "en_US".to_owned(),
        tz: "UTC".to_owned(),
        context_json: "{}".to_owned(),
        cache_epoch: 0,
    }
}

fn batch() -> RecordBatch {
    RecordBatch {
        model: "x.y".to_owned(),
        ids: vec![],
        columns: vec![],
    }
}

fn assert_unsupported(result: Result<impl std::fmt::Debug, BoundaryError>, what: &str) {
    let e = result.expect_err("the export must refuse");
    assert_eq!(e.kind, BoundaryErrorKind::Unsupported, "{what}");
    assert!(
        e.message.contains(DISPATCH_SLICE) && e.message.contains(what),
        "message must name the operation and the slice: {}",
        e.message
    );
}

#[test]
fn describe_comes_from_the_source() {
    assert_eq!(Export::describe(), Fixed::describe());
}

#[test]
fn the_other_exports_refuse_naming_the_dispatch_slice() {
    assert_unsupported(
        Export::invoke_method(context(), "x.y".into(), "go".into(), vec![1], "[]".into()),
        "invoke-method",
    );
    assert_unsupported(
        Export::compute_fields(context(), batch(), vec!["f".into()]),
        "compute-fields",
    );
    assert_unsupported(
        Export::validate_constraints(context(), batch(), vec!["c".into()]),
        "validate-constraints",
    );
    assert_unsupported(
        Export::onchange(context(), "x.y".into(), vec![], vec![]),
        "onchange",
    );
}
