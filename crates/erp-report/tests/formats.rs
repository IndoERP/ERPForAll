// SPDX-License-Identifier: MIT OR Apache-2.0
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions"
)]
use erp_expr::{EvalContext, ExprValue};
use erp_report::{
    DocumentOptions, EvalOptions, LangFormat, Language, Node, PageSetup, RecordStore, ReportError,
    ReportLimits, ReportProgram, TemplateSource,
};

fn print(value: ExprValue, options: &str, settings: EvalOptions) -> Result<String, ReportError> {
    let options = options.replace('&', "&amp;").replace('"', "&quot;");
    let xml = format!("<p t-out='value' t-options=\"{options}\"/>");
    let program = ReportProgram::compile(
        "test.formats",
        &[TemplateSource::new("test.formats", &xml)],
        &Language::new("en_US").unwrap(),
        ReportLimits::default(),
    )?;
    let doc = program.evaluate(
        &EvalContext::standard().with_var("value", value),
        &RecordStore::new(),
        &settings,
        DocumentOptions::new(
            "formats",
            "Formats",
            PageSetup::new(595.0, 842.0, 40.0).unwrap(),
        ),
    )?;
    Ok(doc
        .nodes()
        .iter()
        .filter_map(|n| {
            if let Node::Text(s) = n {
                Some(s.as_str())
            } else {
                None
            }
        })
        .collect())
}
fn duration(value: f64, options: &str) -> String {
    print(
        ExprValue::Float(value),
        &format!("{{'widget': 'duration', {options}}}"),
        EvalOptions::default(),
    )
    .unwrap()
}

/// SPEC-RPT-129 (partial: English/digital duration, signs, rounding and unit options)
#[test]
fn durations_convert_units_and_round_before_decomposition() {
    assert_eq!(
        duration(1.75, "'unit': 'hour', 'round': 'minute', 'digital': True"),
        "01:45"
    );
    assert_eq!(
        duration(-1.75, "'unit': 'hour', 'round': 'minute', 'digital': True"),
        "-01:45"
    );
    assert_eq!(
        duration(1.75, "'unit': 'hour', 'round': 'minute'"),
        "1 hour 45 minutes"
    );
    assert_eq!(
        duration(90.0, "'unit': 'second', 'round': 'minute', 'digital': True"),
        "02"
    );
    assert_eq!(
        duration(
            150.0,
            "'unit': 'second', 'round': 'minute', 'digital': True"
        ),
        "02"
    );
    assert_eq!(
        duration(25.0, "'unit': 'hour', 'round': 'minute', 'digital': True"),
        "25:00"
    );
}

/// SPEC-RPT-104 (partial: locale calendar names; numeric patterns remain covered by evaluate.rs)
#[test]
fn named_dates_use_the_calendar_date() {
    let format = LangFormat::new(".", ",", &[3, 0], "%A %d %B %Y (%a %b)", "%H:%M:%S").unwrap();
    let date = time::Date::from_calendar_date(2024, time::Month::February, 29).unwrap();
    assert_eq!(
        print(
            ExprValue::Date(date),
            "{'widget':'date'}",
            EvalOptions::default().with_formats(format)
        )
        .unwrap(),
        "Thursday 29 February 2024 (Thu Feb)"
    );
}

#[test]
fn invalid_duration_options_are_contextual_errors() {
    let err = print(
        ExprValue::Float(1.0),
        "{'widget':'duration','unit':'fortnight'}",
        EvalOptions::default(),
    )
    .unwrap_err();
    assert!(matches!(err, ReportError::Evaluate { .. }), "{err:?}");
    assert_eq!(
        err.to_string(),
        "test.formats:1: unknown duration unit `fortnight`"
    );
}

#[test]
fn localized_names_and_duration_labels_are_selected_explicitly() {
    let date = time::Date::from_calendar_date(2024, time::Month::February, 29).unwrap();
    for (code, expected, span) in [
        ("fr_FR", "jeudi 29 février 2024", "1\u{a0}heure 45 minutes"),
        ("id_ID", "Kamis 29 Februari 2024", "1 jam 45 menit"),
    ] {
        let formats = LangFormat::default().with_language(code).unwrap();
        let options = EvalOptions::default().with_formats(formats);
        assert_eq!(
            print(
                ExprValue::Date(date),
                "{'widget':'date','format':'EEEE d MMMM yyyy'}",
                options.clone()
            )
            .unwrap(),
            expected
        );
        assert_eq!(
            print(
                ExprValue::Float(1.75),
                "{'widget':'duration','unit':'hour','round':'minute'}",
                options
            )
            .unwrap(),
            span
        );
    }
}

/// SPEC-RPT-105 (partial: caller-provided timezone offset with localized calendar names)
#[test]
fn calendar_names_follow_timezone_rollover_and_ldml_padding() {
    let date = time::Date::from_calendar_date(2026, time::Month::November, 16).unwrap();
    let stamp = date.with_hms(20, 30, 0).unwrap();
    let options = EvalOptions::default()
        .with_formats(LangFormat::default().with_language("id_ID").unwrap())
        .with_utc_offset_seconds(7 * 3600)
        .unwrap();
    assert_eq!(
        print(
            ExprValue::DateTime(stamp),
            "{'widget':'datetime','format':\"EEEE d MMMM yyyy H:m:s 'at' h a\"}",
            options
        )
        .unwrap(),
        "Selasa 17 November 2026 3:30:0 at 3 AM"
    );
}

#[test]
fn unsupported_locales_keep_numeric_formats_and_refuse_text() {
    let date = time::Date::from_calendar_date(2024, time::Month::February, 29).unwrap();
    let settings =
        EvalOptions::default().with_formats(LangFormat::default().with_language("de_DE").unwrap());
    assert_eq!(
        print(ExprValue::Date(date), "{'widget':'date'}", settings.clone()).unwrap(),
        "02/29/2024"
    );
    let err = print(
        ExprValue::Date(date),
        "{'widget':'date','format':'MMMM'}",
        settings,
    )
    .unwrap_err();
    assert!(matches!(err, ReportError::Evaluate { .. }));
    assert_eq!(
        err.to_string(),
        "test.formats:1: locale `de_DE`: no report locale text resource; supply locale data or install a supported locale"
    );
}

#[test]
fn duration_options_and_arithmetic_refuse_bad_values() {
    for (options, message) in [
        ("'round':'fortnight'", "unknown duration round `fortnight`"),
        ("'digital':1", "duration option `digital` must be a boolean"),
        ("'unit':False", "duration option `unit` must be a string"),
        (
            "'format':'enormous'",
            "duration format `enormous` must be long, short or narrow",
        ),
    ] {
        let err = print(
            ExprValue::Float(1.0),
            &format!("{{'widget':'duration',{options}}}"),
            EvalOptions::default(),
        )
        .unwrap_err();
        assert!(matches!(err, ReportError::Evaluate { .. }), "{err}");
        assert_eq!(err.to_string(), format!("test.formats:1: {message}"));
    }
    for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, 1e30] {
        let err = print(
            ExprValue::Float(value),
            "{'widget':'duration'}",
            EvalOptions::default(),
        )
        .unwrap_err();
        assert!(matches!(err, ReportError::Evaluate { .. }), "{err}");
        let message = if value.is_finite() {
            "duration exceeds the exact whole-second range"
        } else {
            "duration value must be a finite number within the exact whole-second range"
        };
        assert_eq!(err.to_string(), format!("test.formats:1: {message}"));
    }
    assert_eq!(
        duration(0.0, "'unit':'hour','round':'minute','digital':True"),
        "00:00"
    );
    assert_eq!(duration(-0.1, "'unit':'second','digital':True"), "-00");
    assert_eq!(duration(0.0, ""), "");
    assert_eq!(duration(1.0, "'unit':'month'"), "1 month");
    assert_eq!(duration(2.0, "'unit':'year'"), "2 years");
    assert_eq!(
        duration(1.75, "'unit':'hour','round':'minute','format':'short'"),
        "1 hr 45 min"
    );
}

#[test]
fn custom_locale_data_is_validated_and_printed_as_literal_text() {
    let source = include_str!("../locales/en.json");
    let source = source.replace("February", "<b>Snow</b>");
    let date = time::Date::from_calendar_date(2024, time::Month::February, 29).unwrap();
    let format = LangFormat::default()
        .with_locale_data("en_US", &source)
        .unwrap();
    assert_eq!(
        print(
            ExprValue::Date(date),
            "{'widget':'date','format':'MMMM'}",
            EvalOptions::default().with_formats(format)
        )
        .unwrap(),
        "<b>Snow</b>"
    );
    for (invalid, message) in [
        ("{}".to_string(), "invalid locale data shape"),
        (
            source.replace("{0}", "{1}"),
            "locale unit pattern must have one {0} slot and at most 256 bytes without controls",
        ),
        (
            source.replace("January", ""),
            "locale names must be nonempty, at most 256 bytes and contain no controls",
        ),
        (" ".repeat(65_537), "locale data exceeds 65536 bytes"),
    ] {
        let err = LangFormat::default()
            .with_locale_data("en_US", &invalid)
            .unwrap_err();
        assert!(matches!(err, ReportError::InvalidDocument { .. }), "{err}");
        assert_eq!(
            err.to_string(),
            format!("invalid report document: {message}")
        );
    }
}

#[test]
fn date_format_overrides_fail_loudly() {
    let date = time::Date::from_calendar_date(2024, time::Month::February, 29).unwrap();
    for (format, message) in [
        ("'yyyy zzzz'", "unsupported LDML token in date format"),
        ("'%Q'", "unsupported percent directive in date format"),
        ("15", "date format option must be a string"),
        ("\"yyyy 'open\"", "unclosed quote in date format"),
    ] {
        let err = print(
            ExprValue::Date(date),
            &format!("{{'widget':'date','format':{format}}}"),
            EvalOptions::default(),
        )
        .unwrap_err();
        assert!(matches!(err, ReportError::Evaluate { .. }), "{err}");
        assert_eq!(err.to_string(), format!("test.formats:1: {message}"));
    }
}

#[test]
fn directional_duration_remains_explicitly_unsupported() {
    let err = print(
        ExprValue::Float(1.0),
        "{'widget':'duration','add_direction':True}",
        EvalOptions::default(),
    )
    .unwrap_err();
    assert!(matches!(err, ReportError::Unsupported { .. }));
    assert_eq!(
        err.to_string(),
        "report directional duration labels requires a later release"
    );
}

#[test]
fn every_month_and_weekday_has_the_expected_calendar_name() {
    for (month, name) in [
        "January",
        "February",
        "March",
        "April",
        "May",
        "June",
        "July",
        "August",
        "September",
        "October",
        "November",
        "December",
    ]
    .into_iter()
    .enumerate()
    {
        let date = time::Date::from_calendar_date(
            2024,
            time::Month::try_from(u8::try_from(month + 1).unwrap()).unwrap(),
            1,
        )
        .unwrap();
        assert_eq!(
            print(
                ExprValue::Date(date),
                "{'widget':'date','format':'MMMM'}",
                EvalOptions::default()
            )
            .unwrap(),
            name
        );
    }
    for (day, name) in [
        (3, "Sunday"),
        (4, "Monday"),
        (5, "Tuesday"),
        (6, "Wednesday"),
        (7, "Thursday"),
        (8, "Friday"),
        (9, "Saturday"),
    ] {
        let date = time::Date::from_calendar_date(2024, time::Month::March, day).unwrap();
        assert_eq!(
            print(
                ExprValue::Date(date),
                "{'widget':'date','format':'EEEE'}",
                EvalOptions::default()
            )
            .unwrap(),
            name
        );
    }
    let date = time::Date::from_calendar_date(1, time::Month::January, 2).unwrap();
    assert_eq!(
        print(
            ExprValue::Date(date),
            "{'widget':'date','format':'y-M-d'}",
            EvalOptions::default()
        )
        .unwrap(),
        "1-1-2"
    );
}

#[test]
fn locale_data_refuses_duplicate_keys_unknown_fields_and_invalid_cardinalities() {
    let source = include_str!("../locales/en.json");
    for input in [
        source.replacen(
            "\"year\": {",
            "\"year\": {\"one\":\"{0}\",\"other\":\"{0}\"}, \"year\": {",
            1,
        ),
        source.replacen("\"plural_rule\":", "\"unknown\":true,\"plural_rule\":", 1),
        source.replacen("\"Jan\",", "", 1),
    ] {
        let err = LangFormat::default()
            .with_locale_data("en_US", &input)
            .unwrap_err();
        assert!(matches!(err, ReportError::InvalidDocument { .. }), "{err}");
        assert_eq!(
            err.to_string(),
            "invalid report document: invalid locale data shape"
        );
    }
    let err = LangFormat::default()
        .with_locale_data("en_US", &source.replace("January", &"a".repeat(257)))
        .unwrap_err();
    assert!(matches!(err, ReportError::InvalidDocument { .. }), "{err}");
    assert_eq!(
        err.to_string(),
        "invalid report document: locale names must be nonempty, at most 256 bytes and contain no controls"
    );
}

#[test]
fn duration_refuses_unknown_option_keys() {
    for (options, message) in [
        (
            "{'widget':'duration','rounding':'day'}",
            "unknown duration option `rounding`",
        ),
        (
            "{'widget':'duration',1:True}",
            "duration option keys must be strings",
        ),
    ] {
        let err = print(ExprValue::Float(1.0), options, EvalOptions::default()).unwrap_err();
        assert!(matches!(err, ReportError::Evaluate { .. }), "{err}");
        assert_eq!(err.to_string(), format!("test.formats:1: {message}"));
    }
}
