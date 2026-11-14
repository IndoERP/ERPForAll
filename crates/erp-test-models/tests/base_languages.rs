//! Language constraints and formatter regression tests.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use erp_fields::{Cell, ErrorKind, FieldType, RecId, Value};
use erp_orm::{RawVals, Spec};
use erp_test_models::base_users;
use erp_testkit::Harness;

async fn base_harness() -> Harness {
    Harness::builder([&base_users::ADDON, &erp_addon_base::ADDON])
        .build()
        .await
        .expect("harness")
}

// Custom addons can provide their own date/time selection providers. This fixture lets
// the production constraint/onchange hooks see formats outside base's fixed choices.
mod custom_formats {
    use erp_addon_base::ResLang;
    use erp_sdk::prelude::*;

    erp_sdk::addon!(name = "test_lang_formats", depends = ["base"]);

    #[extend(ResLang, addon = "test_lang_formats")]
    pub struct CustomFormats {
        #[field(selection, selection_method = "_test_formats")]
        pub date_format: Option<Arc<str>>,
        #[field(selection, selection_method = "_test_formats")]
        pub time_format: Option<Arc<str>>,
    }
}

async fn custom_format_harness() -> Harness {
    Harness::builder([
        &base_users::ADDON,
        &erp_addon_base::ADDON,
        &custom_formats::ADDON,
    ])
    .build()
    .await
    .expect("custom format harness")
}

async fn create_lang(
    tx: &mut erp_orm::Transaction,
    name: &str,
    code: &str,
    url_code: &str,
) -> RecId {
    let mut vals = RawVals::new();
    vals.insert("name".into(), Cell::Str(name.to_string().into()));
    vals.insert("code".into(), Cell::Str(code.to_string().into()));
    vals.insert("url_code".into(), Cell::Str(url_code.to_string().into()));
    vals.insert("active".into(), Cell::Bool(true));
    vals.insert("direction".into(), Cell::Str("ltr".into()));
    vals.insert("date_format".into(), Cell::Str("%m/%d/%Y".into()));
    vals.insert("time_format".into(), Cell::Str("%H:%M:%S".into()));
    vals.insert("week_start".into(), Cell::Str("7".into()));
    vals.insert("grouping".into(), Cell::Str("[3,0]".into()));
    vals.insert("decimal_point".into(), Cell::Str(".".into()));
    vals.insert("thousands_sep".into(), Cell::Str(",".into()));

    let records = tx.env().records("res.lang").unwrap();
    let created = records.create(tx, vec![vals]).await.unwrap();
    created.rec_ids()[0]
}

#[test]
fn spec_base_074_res_lang_international_number_grouping() {
    let formatted = erp_addon_base::format_float(123456789.5, 2, "[3,0]", ",", ".").unwrap();
    assert_eq!(formatted, "123,456,789.50");
}

#[test]
fn spec_base_075_res_lang_indian_number_grouping() {
    let formatted = erp_addon_base::format_float(123456789.5, 2, "[3,2,0]", ",", ".").unwrap();
    assert_eq!(formatted, "12,34,56,789.50");
}

#[test]
fn language_schema_uses_selections_and_no_active_default() {
    let registry = erp_sdk::RegistryBuilder::new([&erp_addon_base::ADDON])
        .build()
        .unwrap();
    let lang = registry.model("res.lang").unwrap();
    assert!(lang.field("active").unwrap().default.is_none());
    for (field, expected) in [
        (
            "date_format",
            vec![
                "%d/%m/%Y", "%m/%d/%Y", "%Y/%m/%d", "%d-%m-%Y", "%m-%d-%Y", "%Y-%m-%d", "%d.%m.%Y",
                "%m.%d.%Y", "%Y.%m.%d",
            ],
        ),
        ("time_format", vec!["%H:%M:%S", "%I:%M:%S %p"]),
        ("week_start", vec!["1", "2", "3", "4", "5", "6", "7"]),
        ("grouping", vec!["[3,0]", "[3,2,0]"]),
    ] {
        let FieldType::Selection {
            values: Some(values),
            ..
        } = &lang.field(field).unwrap().ty
        else {
            panic!("{field} must expose a selection");
        };
        assert_eq!(
            values
                .iter()
                .map(|(key, _)| key.as_str())
                .collect::<Vec<_>>(),
            expected
        );
    }
}

// This fixture does not derive from our constant.
const DISALLOWED: [&str; 23] = [
    "%C", "%D", "%e", "%E", "%F", "%g", "%G", "%h", "%k", "%l", "%n", "%O", "%P", "%R", "%r", "%s",
    "%T", "%t", "%u", "%V", "%+", "%z", "%Z",
];

#[test]
fn directive_list_matches_reference_except_allowed_short_year() {
    assert_eq!(erp_addon_base::DISALLOWED_DATETIME_PATTERNS, DISALLOWED);
    assert!(!erp_addon_base::DISALLOWED_DATETIME_PATTERNS.contains(&"%y"));
}

#[tokio::test]
async fn spec_base_076_both_formats_reject_every_disallowed_directive() {
    let h = custom_format_harness().await;
    let mut tx = h.begin().await.unwrap();
    let langs = tx.env().records("res.lang").unwrap();
    for field in ["date_format", "time_format"] {
        for directive in DISALLOWED {
            let mut vals = RawVals::new();
            vals.insert("date_format".into(), Cell::Str("%m/%d/%Y".into()));
            vals.insert("time_format".into(), Cell::Str("%H:%M:%S".into()));
            vals.insert(field.into(), Cell::Str(directive.into()));
            let record = tx.new_record(&langs, vals, None, None).await.unwrap();
            let err = record
                .call::<(), ()>(&mut tx, "_check_format", ())
                .await
                .unwrap_err();
            assert_eq!(err.kind(), ErrorKind::Validation, "{field} {directive}");
            assert_eq!(
                err.message(),
                "Invalid date/time format directive specified. Please refer to the list of allowed directives, displayed when you edit a language."
            );
        }
    }
}

#[tokio::test]
async fn spec_base_077_onchange_fixes_both_formats_and_preserves_unchanged_inputs() {
    let h = custom_format_harness().await;
    let mut tx = h.begin().await.unwrap();
    let langs = tx.env().records("res.lang").unwrap();
    for (field, input, expected) in [
        ("time_format", "%H:%M:%S %p", "%I:%M:%S %p"),
        ("date_format", "%Y-%m-%d %H:%M %p", "%Y-%m-%d %I:%M %p"),
    ] {
        let mut form = erp_sdk::ValueMap::new();
        form.insert(field.into(), Value::str(input));
        let result = langs
            .onchange(&mut tx, form, vec![field.into()], Spec::new().field(field))
            .await
            .unwrap();
        let Value::Map(result) = result else {
            panic!("expected onchange map")
        };
        let Some(Value::Map(values)) = result.get("value") else {
            panic!("expected changed values")
        };
        assert_eq!(values.get(field), Some(&Value::str(expected)));
        let Some(Value::Map(warning)) = result.get("warning") else {
            panic!("expected warning map")
        };
        assert_eq!(
            warning.get("title"),
            Some(&Value::str(
                "Using 24-hour clock format with AM/PM can cause issues."
            ))
        );
        assert_eq!(
            warning.get("message"),
            Some(&Value::str("Changing to 12-hour clock format instead."))
        );
        assert_eq!(warning.get("type"), Some(&Value::str("notification")));
    }
    for vals in [
        RawVals::new()
            .set_cell("date_format", Cell::Null)
            .set_cell("time_format", Cell::Null),
        RawVals::new()
            .set_cell("date_format", Cell::Str("%m/%d/%Y".into()))
            .set_cell("time_format", Cell::Str("%H:%M:%S".into())),
    ] {
        let record = tx
            .new_record(&langs, vals.clone(), None, None)
            .await
            .unwrap();
        let result: erp_sdk::OnchangeResult =
            record.call(&mut tx, "_onchange_format", ()).await.unwrap();
        assert!(result.warning.is_none());
        for (field, original) in vals.iter() {
            assert_eq!(
                record.cells(&mut tx, field).await.unwrap(),
                vec![original.clone()]
            );
        }
    }
}

#[tokio::test]
async fn spec_base_078_last_active_guard_uses_trusted_install_state_only() {
    let h = base_harness().await;
    let mut tx = h.begin().await.unwrap();
    let first = create_lang(&mut tx, "First", "first", "first").await;
    let second = create_lang(&mut tx, "Second", "second", "second").await;
    let langs = tx.env().records("res.lang").unwrap();
    let inactive = RawVals::new().set_cell("active", Cell::Bool(false));
    let first = langs.browse([first.get()]).unwrap();
    first.write(&mut tx, inactive.clone()).await.unwrap();
    assert_eq!(
        first.values(&mut tx, "active").await.unwrap(),
        vec![Value::Bool(false)]
    );
    let second = langs.browse([second.get()]).unwrap();
    for install_mode in [false, true] {
        let record = second.with_context([("install_mode", Value::Bool(install_mode))]);
        let vals = inactive.clone();
        let err = tx
            .savepoint(async |tx| record.write(tx, vals).await)
            .await
            .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Validation);
        assert_eq!(err.message(), "At least one language must be active.");
    }
    erp_testkit::base_install::begin_base_install(&mut tx).unwrap();
    second.write(&mut tx, inactive).await.unwrap();
    assert_eq!(
        second.values(&mut tx, "active").await.unwrap(),
        vec![Value::Bool(false)]
    );
    erp_testkit::base_install::end_base_install(&mut tx);
    let error = second
        .call::<(), ()>(&mut tx, "_check_active", ())
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(error.message(), "At least one language must be active.");
}

#[tokio::test]
async fn duplicate_language_name_is_rejected() {
    let h = base_harness().await;
    let mut tx = h.begin().await.unwrap();
    create_lang(&mut tx, "Duplicate", "one", "one").await;
    let vals = RawVals::new()
        .set_cell("name", Cell::Str("Duplicate".into()))
        .set_cell("code", Cell::Str("two".into()))
        .set_cell("url_code", Cell::Str("two".into()))
        .set_cell("active", Cell::Bool(true));
    let records = tx.env().records("res.lang").unwrap();
    let err = records.create(&mut tx, vec![vals]).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.message(),
        "The database refused the change: The name of the language must be unique!"
    );
}

#[test]
fn intersperse_reference_examples_and_space_boundary() {
    use erp_addon_base::intersperse;
    for input in [
        "",
        "0",
        "012",
        "1",
        "12",
        "123",
        "1234",
        "123456789",
        "&ab%#@1",
    ] {
        assert_eq!(intersperse(input, &[], ",").unwrap(), (input.into(), 0));
    }
    for counts in [&[][..], &[1], &[2], &[200]] {
        assert_eq!(intersperse("0", counts, ",").unwrap(), ("0".into(), 0));
    }
    for (counts, expected, separators) in [
        (&[1][..], "1234567.8", 1),
        (&[2][..], "123456.78", 1),
        (&[2, 1][..], "12345.6.78", 2),
        (&[2, 0][..], "12.34.56.78", 3),
        (&[-1, 2][..], "12345678", 0),
        (&[2, -1][..], "123456.78", 1),
        (&[2, 0, 1][..], "12.34.56.78", 3),
        (&[2, 0, 0][..], "12.34.56.78", 3),
        (&[2, 0, -1][..], "12.34.56.78", 3),
        (&[3, 3, 3, 3][..], "12.345.678", 2),
    ] {
        assert_eq!(
            intersperse("12345678", counts, ".").unwrap(),
            (expected.into(), separators)
        );
    }
    for (input, counts, expected, separators) in [
        ("abc1234567xy", &[2][..], "abc1234567.xy", 1),
        ("abc1234567xy8", &[2][..], "abc1234567x.y8", 1),
        ("abc12", &[3][..], "abc12", 0),
        ("abc12", &[2][..], "abc12", 0),
        ("abc12", &[1][..], "abc1.2", 1),
        (
            "USD 123456 extra words",
            &[3, 0][..],
            "USD 123,456 extra words",
            1,
        ),
        ("123456 ", &[3, 0][..], "123,456 ", 1),
        ("é123éé", &[2, 0][..], "é1,23,éé", 2),
    ] {
        assert_eq!(
            intersperse(input, counts, ",").unwrap(),
            (expected.replace('.', ","), separators)
        );
    }
    assert_eq!(
        erp_addon_base::split_counts("é日🙂abcd", &[2, 0]).unwrap(),
        ["é日", "🙂a", "bc", "d"]
    );
}

fn assert_validation<T: std::fmt::Debug>(result: erp_sdk::Result<T>, message: &str) {
    let error = result.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(error.message(), message);
}

#[test]
fn grouping_parser_rejects_every_malformed_token_and_excessive_inputs() {
    use erp_addon_base::parse_grouping;
    for (input, expected) in [
        ("[]", vec![]),
        ("[ ]", vec![]),
        (" [3, 2, 0] ", vec![3, 2, 0]),
        ("[-1]", vec![-1]),
        ("[2147483647]", vec![i32::MAX]),
    ] {
        assert_eq!(parse_grouping(input).unwrap(), expected);
    }
    for input in ["", "3,0", "[3,0", "3,0]", "[3,0]tail"] {
        assert_validation(
            parse_grouping(input),
            "Grouping must be a bracketed list of integer counts",
        );
    }
    for (input, token) in [
        ("[3,no,0]", "no"),
        ("[3,]", ""),
        ("[[3]]", "[3]"),
        ("[2147483648]", "2147483648"),
        ("[é]", "é"),
    ] {
        assert_validation(
            parse_grouping(input),
            &format!("Invalid grouping token '{token}': expected a 32-bit integer"),
        );
    }
    for value in [-2, i32::MIN] {
        assert_validation(
            parse_grouping(&format!("[{value}]")),
            &format!("Invalid grouping count {value}: expected -1, 0, or a positive integer"),
        );
    }
    assert_validation(
        parse_grouping(&" ".repeat(1025)),
        "Grouping input exceeds the 1024-byte limit",
    );
    assert_validation(
        parse_grouping(&format!("[{}]", vec!["1"; 129].join(","))),
        "Grouping exceeds the 128-count limit",
    );
    assert_eq!(
        parse_grouping(&format!("[{}]", vec!["1"; 128].join(",")))
            .unwrap()
            .len(),
        128
    );
}

#[test]
fn format_helpers_reject_resource_abuse_and_preserve_unicode() {
    use erp_addon_base::{format_float, intersperse, split_counts};
    assert_validation(
        format_float(1.0, usize::MAX, "[3,0]", ",", "."),
        "Decimal precision exceeds the 1000-place limit",
    );
    assert_validation(
        format_float(1.0, 1001, "[3,0]", ",", "."),
        "Decimal precision exceeds the 1000-place limit",
    );
    for number in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        assert_validation(
            format_float(number, 2, "[3,0]", ",", "."),
            "Cannot format a non-finite number",
        );
    }
    assert_validation(
        format_float(1.0, 2, "[3,no]", ",", "."),
        "Invalid grouping token 'no': expected a 32-bit integer",
    );
    assert_validation(
        format_float(1.0, 2, "[]", &"x".repeat(1025), "."),
        "Thousands separator exceeds the 1024-byte limit",
    );
    assert_validation(
        format_float(1.0, 2, "[]", ",", &"x".repeat(1025)),
        "Decimal separator exceeds the 1024-byte limit",
    );
    assert_validation(
        intersperse(&"1".repeat(1025), &[], ","),
        "Number input exceeds the 1024-byte limit",
    );
    assert_validation(
        intersperse("123", &[-2], ","),
        "Invalid grouping count -2: expected -1, 0, or a positive integer",
    );
    assert_validation(
        split_counts("123", &[1; 129]),
        "Grouping exceeds the 128-count limit",
    );
    assert_eq!(split_counts("é日🙂", &[0]).unwrap(), ["é日🙂"]);
    assert_eq!(split_counts("é日🙂", &[i32::MAX, 0]).unwrap(), ["é日🙂"]);
    assert_eq!(intersperse("", &[0], ",").unwrap(), (String::new(), 0));
    assert_eq!(
        format_float(-12345.5, 2, "[3,0]", " ", ",").unwrap(),
        "-12 345,50"
    );
    assert_eq!(format_float(1.0, 1000, "[]", ",", ".").unwrap().len(), 1002);
    assert_eq!(format_float(1234.0, 0, "[3,0]", ",", ".").unwrap(), "1,234");
}

fn independent_language_orm(h: &Harness) -> erp_orm::Orm {
    use erp_testkit::deadpool_postgres::{Manager, ManagerConfig, Pool, RecyclingMethod, Runtime};
    let manager = Manager::from_config(
        h.db().config(),
        erp_testkit::tokio_postgres::NoTls,
        ManagerConfig {
            recycling_method: RecyclingMethod::Fast,
        },
    );
    let pool = Pool::builder(manager)
        .max_size(2)
        .runtime(Runtime::Tokio1)
        .build()
        .unwrap();
    erp_orm::Orm::builder(std::sync::Arc::clone(h.registry()), pool)
        .policy(erp_orm::StaticPolicy::AllowAll)
        .build()
        .unwrap()
}

async fn active_language_count(tx: &mut erp_orm::Transaction) -> i64 {
    tx.query("SELECT count(*) FROM res_lang WHERE active", &[])
        .await
        .unwrap()[0]
        .get(0)
}

#[tokio::test]
async fn independent_nodes_cannot_deactivate_the_last_two_languages_with_stale_snapshots() {
    let h = base_harness().await;
    let mut seed = h.begin().await.unwrap();
    let first = create_lang(&mut seed, "First node", "node_one", "node_one").await;
    let second = create_lang(&mut seed, "Second node", "node_two", "node_two").await;
    assert_eq!(active_language_count(&mut seed).await, 2);
    seed.commit().await.unwrap();
    drop(seed);

    // Independent ORM runtimes/pools share only PostgreSQL. Establish both repeatable-read
    // snapshots before either deactivation, then commit node one's update before node two writes.
    // This deterministic schedule exposes write skew without timing or process-local locks.
    let node_one = independent_language_orm(&h);
    let node_two = independent_language_orm(&h);
    let mut one = node_one.begin(erp_orm::SUPERUSER_ID).await.unwrap();
    let mut two = node_two.begin(erp_orm::SUPERUSER_ID).await.unwrap();
    assert_eq!(active_language_count(&mut one).await, 2);
    assert_eq!(active_language_count(&mut two).await, 2);
    let one_record = one
        .env()
        .records("res.lang")
        .unwrap()
        .browse([first.get()])
        .unwrap();
    let two_record = two
        .env()
        .records("res.lang")
        .unwrap()
        .browse([second.get()])
        .unwrap();
    one_record
        .write(
            &mut one,
            RawVals::new().set_cell("active", Cell::Bool(false)),
        )
        .await
        .unwrap();
    assert_eq!(active_language_count(&mut one).await, 1);
    one.commit().await.unwrap();
    drop(one);

    let result = two_record
        .write(
            &mut two,
            RawVals::new().set_cell("active", Cell::Bool(false)),
        )
        .await;
    match result {
        Ok(()) => two.commit().await.unwrap(),
        Err(error) => {
            assert_eq!(error.kind(), ErrorKind::Concurrency);
            assert_eq!(error.sqlstate(), Some("40001"));
            assert_eq!(
                error.message(),
                "could not serialize access due to concurrent update"
            );
            two.rollback().await.unwrap();
        }
    }
    drop(two);

    let mut retry = node_two.begin(erp_orm::SUPERUSER_ID).await.unwrap();
    assert_eq!(
        active_language_count(&mut retry).await,
        1,
        "the two nodes must preserve an active language"
    );
    let remaining = retry
        .env()
        .records("res.lang")
        .unwrap()
        .browse([second.get()])
        .unwrap();
    let error = remaining
        .write(
            &mut retry,
            RawVals::new().set_cell("active", Cell::Bool(false)),
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(error.message(), "At least one language must be active.");
    retry.rollback().await.unwrap();
    drop(retry);

    let mut verification = h.begin().await.unwrap();
    assert_eq!(active_language_count(&mut verification).await, 1);
}

#[tokio::test]
async fn active_language_guard_uses_one_query_after_warmup() {
    let h = base_harness().await;
    let mut tx = h.begin().await.unwrap();
    let id = create_lang(&mut tx, "Query budget", "budget", "budget").await;
    let record = tx
        .env()
        .records("res.lang")
        .unwrap()
        .browse([id.get()])
        .unwrap();
    record
        .call::<(), ()>(&mut tx, "_check_active", ())
        .await
        .unwrap();
    erp_testkit::assert_query_count!(tx, >= 1, <= 1, {
        record.call::<(), ()>(&mut tx, "_check_active", ()).await.unwrap();
    });
    assert_eq!(active_language_count(&mut tx).await, 1);
}

#[tokio::test]
async fn spec_base_076_stored_create_and_write_dispatch_format_constraints() {
    let h = custom_format_harness().await;
    for field in ["date_format", "time_format"] {
        for directive in DISALLOWED {
            // A dynamic selection extension admits custom formats to the ORM conversion
            // layer, so the production constraint must reject the stored create itself.
            let mut tx = h.begin().await.unwrap();
            let mut vals = RawVals::new()
                .set_cell("name", Cell::Str("Stored invalid format".into()))
                .set_cell("code", Cell::Str("stored_invalid".into()))
                .set_cell("url_code", Cell::Str("stored-invalid".into()))
                .set_cell("active", Cell::Bool(true))
                .set_cell("direction", Cell::Str("ltr".into()))
                .set_cell("date_format", Cell::Str("%m/%d/%Y".into()))
                .set_cell("time_format", Cell::Str("%H:%M:%S".into()))
                .set_cell("week_start", Cell::Str("7".into()))
                .set_cell("grouping", Cell::Str("[3,0]".into()))
                .set_cell("decimal_point", Cell::Str(".".into()))
                .set_cell("thousands_sep", Cell::Str(",".into()));
            vals.insert(field.into(), Cell::Str(directive.into()));
            let langs = tx.env().records("res.lang").unwrap();
            let error = langs.create(&mut tx, vec![vals]).await.unwrap_err();
            assert_eq!(
                error.kind(),
                ErrorKind::Validation,
                "create {field} {directive}"
            );
            assert_eq!(
                error.message(),
                "Invalid date/time format directive specified. Please refer to the list of allowed directives, displayed when you edit a language."
            );
            tx.rollback().await.unwrap();
            drop(tx);

            // A real language begins valid; changing either stored field must run the hook
            // without an explicit method call from the test.
            let mut tx = h.begin().await.unwrap();
            let id = create_lang(
                &mut tx,
                "Stored valid format",
                "stored_valid",
                "stored-valid",
            )
            .await;
            let lang = tx
                .env()
                .records("res.lang")
                .unwrap()
                .browse([id.get()])
                .unwrap();
            let error = lang
                .write(
                    &mut tx,
                    RawVals::new().set_cell(field, Cell::Str(directive.into())),
                )
                .await
                .unwrap_err();
            assert_eq!(
                error.kind(),
                ErrorKind::Validation,
                "write {field} {directive}"
            );
            assert_eq!(
                error.message(),
                "Invalid date/time format directive specified. Please refer to the list of allowed directives, displayed when you edit a language."
            );
            tx.rollback().await.unwrap();
        }
    }
    h.cleanup().await.unwrap();
}
