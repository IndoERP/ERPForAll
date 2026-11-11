//! Date and datetime values (SPEC-DOM §4.5 date rules, §4.9 relative dates).

// Test crate: helpers outside `#[test]` functions may unwrap and panic too (clippy.toml intent).
#![allow(clippy::unwrap_used, clippy::panic)]

mod common;

use common::*;
use erp_domain::{DateContext, Domain, ErrorReason, Operator, Utc, Value, parse_date};
use time::{PrimitiveDateTime, UtcOffset, Weekday};

const MIXED: &str = "test_orm.mixed";

fn opt_tz(tz: &dyn erp_domain::TimeZone, domain: &Domain) -> Domain {
    domain.optimize(&model(MIXED).with_tz(tz)).unwrap()
}

/// `moment >= start AND moment < end` in canonical order.
fn second_range(start: PrimitiveDateTime, end: PrimitiveDateTime) -> Domain {
    cond("moment", Operator::Lt, end) & cond("moment", Operator::Ge, start)
}

#[test]
fn spec_dom_015_date_value_typing() {
    let jan5 = cond("date", Operator::In, set([date(2024, 1, 5)]));
    assert_eq!(opt(MIXED, &c("date", "=", date(2024, 1, 5))), jan5);
    assert_eq!(
        opt(MIXED, &c("date", "=", datetime(2024, 1, 5, 12, 0, 0))),
        jan5
    );
    assert_eq!(opt(MIXED, &c("date", "=", "2024-01-05")), jan5);
    assert_eq!(
        opt(MIXED, &c("date", "=", "20240105")),
        jan5,
        "ISO basic format"
    );
    assert_eq!(opt(MIXED, &c("date", "=", "2024-01-05 10:00:00")), jan5);
    assert_eq!(
        opt(MIXED, &c("date", "=like", "2024%")),
        c("date", "=like", "2024%")
    );
    assert_eq!(
        opt(MIXED, &c("date", ">", "2024-01-01")),
        cond("date", Operator::Gt, date(2024, 1, 1))
    );
    assert_eq!(opt(MIXED, &c("date", ">", false)), Domain::FALSE);
    assert_eq!(
        opt(
            MIXED,
            &c(
                "date",
                "not in",
                list([Value::from("2024-01-05"), date(2023, 1, 1).into()])
            )
        ),
        cond(
            "date",
            Operator::NotIn,
            set([date(2024, 1, 5), date(2023, 1, 1)])
        )
    );
    // `= False` stays a "not set" test.
    assert_eq!(
        opt(MIXED, &c("date", "=", false)),
        cond("date", Operator::In, set([false]))
    );
    // Relative dates are validated and kept for DYNAMIC_VALUES.
    assert_eq!(
        opt(MIXED, &c("date", ">", "today")),
        c("date", ">", "today")
    );
    assert_eq!(
        opt(MIXED, &c("date", "=", "today")),
        cond("date", Operator::In, set(["today"]))
    );

    for bad in ["hello", "2024-13-01", "2024-02-30", "2024-1-5"] {
        let err = opt_err(MIXED, &c("date", ">", bad));
        assert_eq!(err.python_class(), "ValueError", "{bad}: {err}");
    }
    assert!(matches!(
        opt_err(MIXED, &c("date", ">", "hello")).reason(),
        ErrorReason::InvalidDateTerm { .. }
    ));
    assert_eq!(
        opt_err(MIXED, &c("date", ">", "2024-13-01")).to_string(),
        "month must be in 1..12"
    );
    assert!(matches!(
        opt_err(MIXED, &c("date", ">", 5)).reason(),
        ErrorReason::NotADate(_)
    ));
}

#[test]
fn spec_dom_016_datetime_typing_utc() {
    let midnight = datetime(2024, 1, 5, 0, 0, 0);
    let one_second = datetime(2024, 1, 5, 0, 0, 1);
    assert_eq!(
        opt(MIXED, &c("moment", "=", date(2024, 1, 5))),
        second_range(midnight, one_second)
    );
    assert_eq!(
        opt(MIXED, &c("moment", "=", "2024-01-05")),
        second_range(midnight, one_second)
    );
    assert_eq!(
        opt(MIXED, &c("moment", "=like", "2024%")),
        c("moment", "=like", "2024%")
    );
    assert_eq!(
        opt(MIXED, &c("moment", ">", "2024-01-01 10:00:00")),
        cond("moment", Operator::Ge, datetime(2024, 1, 1, 10, 0, 1))
    );
    assert_eq!(
        opt(MIXED, &c("moment", ">", "2024-01-01")),
        cond("moment", Operator::Ge, datetime(2024, 1, 2, 0, 0, 0))
    );
    assert_eq!(
        opt(MIXED, &c("moment", "<", "2024-01-01")),
        cond("moment", Operator::Lt, datetime(2024, 1, 1, 0, 0, 0))
    );
    assert_eq!(
        opt(MIXED, &c("moment", "<=", "2024-01-01")),
        cond("moment", Operator::Lt, datetime(2024, 1, 2, 0, 0, 0))
    );
    assert_eq!(opt(MIXED, &c("moment", ">", false)), Domain::FALSE);
    let not_set = || cond("moment", Operator::In, set([false]));
    assert_eq!(
        opt(
            MIXED,
            &c(
                "moment",
                "not in",
                list([
                    Value::from("2024-01-05"),
                    datetime(2023, 1, 1, 0, 0, 0).into()
                ])
            )
        ),
        (not_set()
            | cond("moment", Operator::Lt, datetime(2024, 1, 5, 0, 0, 0))
            | cond("moment", Operator::Ge, datetime(2024, 1, 5, 0, 0, 1)))
            & (not_set()
                | cond("moment", Operator::Lt, datetime(2023, 1, 1, 0, 0, 0))
                | cond("moment", Operator::Ge, datetime(2023, 1, 1, 0, 0, 1)))
    );
    // Beyond the largest datetime.
    let max = datetime(9999, 12, 31, 23, 59, 59);
    assert_eq!(opt(MIXED, &c("moment", ">", max)), Domain::FALSE);
    assert_eq!(
        opt(MIXED, &c("moment", "<=", max)),
        cond("moment", Operator::NotIn, set([false]))
    );

    assert!(matches!(
        opt_err(MIXED, &c("moment", ">", "hello")).reason(),
        ErrorReason::InvalidDateTerm { .. }
    ));
    let err = opt_err(MIXED, &c("moment", "=", "1970-01-01'); --"));
    assert_eq!(
        err.to_string(),
        "Invalid isoformat string: \"1970-01-01'); --\""
    );
    assert_eq!(err.python_class(), "ValueError");
    let err = opt_err(MIXED, &c("moment", "=", "2024-01-05T10:00:00+02:00"));
    assert!(
        matches!(err.reason(), ErrorReason::TimezoneNotAllowed(_)),
        "{err}"
    );
    // Relative values are kept for DYNAMIC_VALUES.
    assert_eq!(
        opt(MIXED, &c("moment", ">=", "today")),
        c("moment", ">=", "today")
    );
}

#[test]
fn spec_dom_016_datetime_typing_timezone() {
    let brussels = Brussels;
    assert_eq!(
        opt_tz(&brussels, &c("moment", ">=", "2024-01-01 10:00:00")),
        cond("moment", Operator::Ge, datetime(2024, 1, 1, 10, 0, 0)),
        "timezone has no effect on datetimes"
    );
    assert_eq!(
        opt_tz(&brussels, &c("moment", ">=", "2024-07-02")),
        cond("moment", Operator::Ge, datetime(2024, 7, 1, 22, 0, 0)),
        "dates are midnight in the user's timezone"
    );
    assert_eq!(
        opt_tz(&brussels, &c("moment", ">=", "2024-01-02")),
        cond("moment", Operator::Ge, datetime(2024, 1, 1, 23, 0, 0))
    );
    // Years 1 and 9999 are not converted.
    assert_eq!(
        opt_tz(&brussels, &c("moment", ">=", "9999-12-31")),
        cond("moment", Operator::Ge, datetime(9999, 12, 31, 0, 0, 0))
    );
    let jakarta = Fixed(UtcOffset::from_hms(7, 0, 0).unwrap(), "Asia/Jakarta");
    assert_eq!(
        opt_tz(&jakarta, &c("moment", "=", date(2024, 1, 5))),
        second_range(
            datetime(2024, 1, 4, 17, 0, 0),
            datetime(2024, 1, 4, 17, 0, 1)
        )
    );
    assert_eq!(
        opt_tz(&Utc, &c("moment", ">", date(2024, 1, 5))),
        cond("moment", Operator::Ge, datetime(2024, 1, 6, 0, 0, 0))
    );
}

#[test]
fn spec_dom_016_datetime_millisecond() {
    let range = second_range(
        datetime(2024, 1, 5, 11, 6, 2),
        datetime(2024, 1, 5, 11, 6, 3),
    );
    assert_eq!(
        opt(MIXED, &c("moment", "=", "2024-01-05 11:06:02.123")),
        range
    );
    assert_eq!(opt(MIXED, &c("moment", "=", "2024-01-05 11:06:02")), range);
    assert_eq!(
        opt(MIXED, &c("moment", "=", "2024-01-05T11:06:02,5")),
        range
    );
    assert_eq!(
        opt(MIXED, &c("moment", "=", datetime(2024, 1, 5, 11, 6, 2))),
        range
    );
    let with_micros = datetime(2024, 1, 5, 11, 6, 2)
        .replace_microsecond(123_456)
        .unwrap();
    assert_eq!(opt(MIXED, &c("moment", "=", with_micros)), range);
    assert_eq!(
        opt(MIXED, &c("moment", ">=", "2024-01-05 11:06:02.123")),
        cond("moment", Operator::Ge, datetime(2024, 1, 5, 11, 6, 2))
    );
    assert_eq!(
        opt(MIXED, &c("moment", ">=", "2024-01-05 11:06:02")),
        cond("moment", Operator::Ge, datetime(2024, 1, 5, 11, 6, 2))
    );
    // Hours only, and the basic format.
    assert_eq!(
        opt(MIXED, &c("moment", ">=", "2024-01-05T11")),
        cond("moment", Operator::Ge, datetime(2024, 1, 5, 11, 0, 0))
    );
    assert_eq!(
        opt(MIXED, &c("moment", ">=", "20240105T110602")),
        cond("moment", Operator::Ge, datetime(2024, 1, 5, 11, 6, 2))
    );
}

/// timezone, the week starts on Monday.
#[test]
fn spec_dom_018_relative_date_grammar() {
    let now = datetime(2024, 1, 10, 15, 30, 0);
    let ctx = DateContext::new(now, &Utc, Weekday::Monday);
    let parse = |s: &str| parse_date(s, &ctx).unwrap();
    assert_eq!(parse("today"), Value::Date(date(2024, 1, 10)));
    assert_eq!(parse("now"), Value::Datetime(now));
    assert_eq!(
        parse("now -1d"),
        Value::Datetime(datetime(2024, 1, 9, 15, 30, 0))
    );
    assert_eq!(
        parse("-1d"),
        Value::Datetime(datetime(2024, 1, 9, 15, 30, 0))
    );
    assert_eq!(parse("today =1d"), Value::Date(date(2024, 1, 1)));
    assert_eq!(
        parse("=6m"),
        Value::Datetime(datetime(2024, 6, 10, 0, 0, 0))
    );
    assert_eq!(
        parse("=3H"),
        Value::Datetime(datetime(2024, 1, 10, 3, 0, 0))
    );
    assert_eq!(parse("today +monday"), Value::Date(date(2024, 1, 15)));
    assert_eq!(parse("today -monday"), Value::Date(date(2024, 1, 8)));
    assert_eq!(parse("today =week_start"), Value::Date(date(2024, 1, 8)));
    assert_eq!(
        parse("today +wednesday"),
        Value::Date(date(2024, 1, 10)),
        "no move"
    );
    assert_eq!(
        parse("=monday"),
        Value::Datetime(datetime(2024, 1, 8, 0, 0, 0))
    );
    assert_eq!(parse("today +1m"), Value::Date(date(2024, 2, 10)));
    assert_eq!(parse("today +1y -1w"), Value::Date(date(2025, 1, 3)));
    assert_eq!(
        parse("now +2H +30M"),
        Value::Datetime(datetime(2024, 1, 10, 18, 0, 0))
    );
    assert_eq!(parse("2024-03-04"), Value::Date(date(2024, 3, 4)));
    // Month arithmetic clamps the day.
    let jan31 = DateContext::new(datetime(2024, 1, 31, 8, 0, 0), &Utc, Weekday::Monday);
    assert_eq!(
        parse_date("today +1m", &jan31).unwrap(),
        Value::Date(date(2024, 2, 29))
    );
    assert_eq!(
        parse_date("today =40d", &jan31).unwrap(),
        Value::Date(date(2024, 1, 31))
    );
    // A week starting on Sunday.
    let sunday_start = DateContext::new(now, &Utc, Weekday::Sunday);
    assert_eq!(
        parse_date("today =week_start", &sunday_start).unwrap(),
        Value::Date(date(2024, 1, 7))
    );

    // The user's timezone: local wall-clock arithmetic, UTC result.
    let jakarta = Fixed(UtcOffset::from_hms(7, 0, 0).unwrap(), "Asia/Jakarta");
    let local = DateContext::new(datetime(2024, 1, 10, 8, 30, 0), &jakarta, Weekday::Monday);
    assert_eq!(
        parse_date("now -1d", &local).unwrap(),
        Value::Datetime(datetime(2024, 1, 9, 8, 30, 0))
    );
    assert_eq!(
        parse_date("=3H", &local).unwrap(),
        Value::Datetime(datetime(2024, 1, 9, 20, 0, 0))
    );
    assert_eq!(
        parse_date("today", &local).unwrap(),
        Value::Date(date(2024, 1, 10))
    );

    // Errors.
    for (input, message) in [
        (
            "today =2w",
            "Invalid term '=2w' in expression date: 'today =2w'",
        ),
        ("+1x", "Invalid term '+1x' in expression date: '+1x'"),
        ("today +", "Invalid term '+' in expression date: 'today +'"),
        ("=13m", "Invalid term '=13m' in expression date: '=13m'"),
        ("=25H", "Invalid term '=25H' in expression date: '=25H'"),
        ("hello", "Invalid term 'hello' in expression date: 'hello'"),
        ("", "Empty date value"),
        ("   ", "Empty date value"),
    ] {
        let err = parse_date(input, &ctx).unwrap_err();
        assert_eq!(err.to_string(), message, "{input:?}");
        assert_eq!(err.python_class(), "ValueError");
    }
    assert!(matches!(
        parse_date("+100000000d", &ctx).unwrap_err().reason(),
        ErrorReason::DateOverflow
    ));
}

/// substitution into conditions happens at DYNAMIC_VALUES (S2).
#[test]
fn spec_dom_017_relative_values_resolve() {
    let ctx = DateContext::new(datetime(2024, 1, 5, 13, 5, 0), &Utc, Weekday::Monday);
    assert_eq!(
        parse_date("today", &ctx).unwrap(),
        Value::Date(date(2024, 1, 5))
    );
    assert_eq!(
        parse_date("+12H", &ctx).unwrap(),
        Value::Datetime(datetime(2024, 1, 6, 1, 5, 0))
    );
    assert_eq!(
        parse_date("today +12H", &ctx).unwrap(),
        Value::Datetime(datetime(2024, 1, 5, 12, 0, 0))
    );
    // 24 hours normalize to one day, so the result stays a date.
    assert_eq!(
        parse_date("today +24H", &ctx).unwrap(),
        Value::Date(date(2024, 1, 6))
    );
}
