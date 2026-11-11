//! HTML conversion must never silently publish unexamined markup.
use erp_fields::convert::{FieldRef, to_cell, validate_cell};
use erp_fields::{Cell, ErrorKind, FieldType, Value};
use std::sync::Arc;

fn field() -> FieldRef<'static> {
    FieldRef {
        model: "test.html",
        field: "body",
        label: "Body",
    }
}

#[test]
fn dynamic_html_rejects_nul_without_echoing_the_body() {
    let result = to_cell(
        field(),
        &FieldType::Html,
        &Value::Str(Arc::from("<p>secret\0body</p>")),
    );
    let error = result.expect_err("NUL must not enter an Html cell");
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "test.html.body: input contains a NUL byte at offset 9"
    );
    assert!(std::error::Error::source(&error).is_some());
}

#[test]
fn typed_html_rejects_nul_without_echoing_the_body() {
    let result = validate_cell(
        field(),
        &FieldType::Html,
        Cell::Str(Arc::from("<p>secret\0body</p>")),
    );
    let error = result.expect_err("typed values must not bypass HTML admission");
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "test.html.body: input contains a NUL byte at offset 9"
    );
    assert!(std::error::Error::source(&error).is_some());
}

#[test]
fn html_null_and_already_canonical_markup_keep_their_value() {
    assert!(
        to_cell(field(), &FieldType::Html, &Value::False)
            .unwrap()
            .is_null()
    );
    assert!(
        validate_cell(field(), &FieldType::Html, Cell::Null)
            .unwrap()
            .is_null()
    );
    let result = to_cell(
        field(),
        &FieldType::Html,
        &Value::Str(Arc::from("<p>Halo <strong>dunia</strong></p>")),
    )
    .unwrap();
    assert!(result.same_value(&Cell::Str(Arc::from("<p>Halo <strong>dunia</strong></p>"))));
}

#[test]
fn reporting_conversion_returns_cleaned_value_and_report() {
    use erp_fields::html::*;
    let policy = FieldPolicy::Sanitize(SanitizePolicy::Default(DefaultPolicy::new()));
    let conversion = convert_value(
        field(),
        &Value::Str(Arc::from("<p onclick=\"secret\">Hello</p>")),
        &policy,
        &Limits::recommended(),
        SanitizeMode::Clean,
    )
    .unwrap();
    assert!(
        conversion
            .cell()
            .same_value(&Cell::Str(Arc::from("<p>Hello</p>")))
    );
    assert_eq!(conversion.report().total_count(), 1);
    assert_eq!(conversion.report().entries()[0].name(), "onclick");
}

#[test]
fn legacy_conversion_refuses_removal_and_preserves_typed_report() {
    use erp_fields::html::HtmlSanitizeError;
    let error = to_cell(
        field(),
        &FieldType::Html,
        &Value::Str(Arc::from("<p onclick=\"secret\">Hello</p>")),
    )
    .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "test.html.body: strict mode: the policy removes 1 item(s) in 1 group(s)"
    );
    let cause = std::error::Error::source(&error)
        .unwrap()
        .downcast_ref::<HtmlSanitizeError>()
        .unwrap();
    match cause {
        HtmlSanitizeError::DisallowedContent { report } => assert_eq!(report.total_count(), 1),
        other => panic!("unexpected error: {other:?}"),
    }
}

#[test]
fn html_byte_ingress_preserves_invalid_utf8_kind() {
    use erp_fields::html::HtmlSanitizeError;
    let error = to_cell(
        field(),
        &FieldType::Html,
        &Value::Bytes(Arc::from([255_u8])),
    )
    .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "test.html.body: input is not valid UTF-8 (valid up to byte 0)"
    );
    assert!(matches!(
        std::error::Error::source(&error)
            .unwrap()
            .downcast_ref::<HtmlSanitizeError>(),
        Some(HtmlSanitizeError::InvalidUtf8 { .. })
    ));
}
