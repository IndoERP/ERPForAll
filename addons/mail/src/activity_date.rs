//! Date arithmetic of activities: deadlines from a delay, reschedule targets and the
//! overdue/today/planned state. Every sum is checked; nothing wraps or saturates.

use erp_sdk::prelude::*;

fn out_of_range() -> Error {
    Error::validation("The deadline is outside the supported date range.")
}

/// `base` plus `days` days.
///
/// # Errors
/// `Validation` when the result is not a date.
pub(crate) fn add_days(base: Date, days: i32) -> Result<Date> {
    base.days()
        .checked_add(days)
        .map(Date::from_days)
        .ok_or_else(out_of_range)
}

/// Days of `month` of `year`.
fn month_length(year: i32, month: u32) -> Result<u32> {
    let first = Date::from_ymd(year, month, 1).ok_or_else(out_of_range)?;
    let (next_year, next_month) = if month == 12 {
        (year.checked_add(1).ok_or_else(out_of_range)?, 1)
    } else {
        (year, month + 1)
    };
    let next = Date::from_ymd(next_year, next_month, 1).ok_or_else(out_of_range)?;
    u32::try_from(next.days() - first.days()).map_err(|_| out_of_range())
}

/// `base` plus `months` calendar months; the day of the month is cut to the target month's length,
/// as Python's `relativedelta` does (January 31 plus one month is the last day of February).
///
/// # Errors
/// `Validation` when the result is not a date.
pub(crate) fn add_months(base: Date, months: i32) -> Result<Date> {
    let (year, month, day) = base.ymd();
    let index = i64::from(year) * 12 + i64::from(month) - 1 + i64::from(months);
    let year = i32::try_from(index.div_euclid(12)).map_err(|_| out_of_range())?;
    let month = u32::try_from(index.rem_euclid(12)).map_err(|_| out_of_range())? + 1;
    let day = day.min(month_length(year, month)?);
    Date::from_ymd(year, month, day).ok_or_else(out_of_range)
}

/// `base` plus `count` of `unit` (`days`, `weeks` or `months`; a negative count goes back).
///
/// # Errors
/// `Validation` for another unit or a result that is not a date.
pub(crate) fn add_delay(base: Date, unit: &str, count: i32) -> Result<Date> {
    match unit {
        "days" => add_days(base, count),
        "weeks" => add_days(base, count.checked_mul(7).ok_or_else(out_of_range)?),
        "months" => add_months(base, count),
        other => Err(Error::validation(format!(
            "The delay unit is days, weeks or months, not '{other}'."
        ))),
    }
}

/// The Monday on or before `date` (Monday is 0; 1970-01-01 was a Thursday).
pub(crate) fn monday_on_or_before(date: Date) -> Result<Date> {
    let weekday = (i64::from(date.days()) + 3).rem_euclid(7);
    let back = i32::try_from(weekday).map_err(|_| out_of_range())?;
    add_days(date, -back)
}

pub(crate) fn state_of(active: bool, deadline: Date, today: Date) -> &'static str {
    if !active {
        "done"
    } else if deadline < today {
        "overdue"
    } else if deadline == today {
        "today"
    } else {
        "planned"
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "tests")]

    use super::*;

    fn d(y: i32, m: u32, day: u32) -> Date {
        Date::from_ymd(y, m, day).unwrap()
    }

    #[test]
    fn months_cut_the_day_to_the_target_month() {
        assert_eq!(add_months(d(2026, 1, 31), 1).unwrap(), d(2026, 2, 28));
        assert_eq!(add_months(d(2028, 1, 31), 1).unwrap(), d(2028, 2, 29));
        assert_eq!(add_months(d(2026, 11, 30), 3).unwrap(), d(2027, 2, 28));
        assert_eq!(add_months(d(2026, 3, 31), -1).unwrap(), d(2026, 2, 28));
        assert_eq!(add_months(d(2026, 12, 15), 1).unwrap(), d(2027, 1, 15));
        assert_eq!(add_months(d(2026, 1, 15), -13).unwrap(), d(2024, 12, 15));
    }

    #[test]
    fn weeks_and_days_add_exactly() {
        assert_eq!(
            add_delay(d(2026, 10, 10), "days", 5).unwrap(),
            d(2026, 10, 15)
        );
        assert_eq!(
            add_delay(d(2026, 12, 28), "weeks", 2).unwrap(),
            d(2027, 1, 11)
        );
        assert_eq!(
            add_delay(d(2026, 3, 1), "days", -1).unwrap(),
            d(2026, 2, 28)
        );
    }

    #[test]
    fn out_of_range_and_unknown_units_are_refused() {
        let error = add_delay(d(2026, 1, 1), "years", 1).unwrap_err();
        assert_eq!(
            error.message(),
            "The delay unit is days, weeks or months, not 'years'."
        );
        let error = add_delay(Date::from_days(i32::MAX), "days", 1).unwrap_err();
        assert_eq!(
            error.message(),
            "The deadline is outside the supported date range."
        );
        assert!(add_delay(d(2026, 1, 1), "weeks", i32::MAX).is_err());
    }

    #[test]
    fn mondays() {
        // 2026-11-07 is a Saturday; the Monday before is 2026-11-02, a Monday maps to itself
        assert_eq!(monday_on_or_before(d(2026, 11, 7)).unwrap(), d(2026, 11, 2));
        assert_eq!(monday_on_or_before(d(2026, 11, 2)).unwrap(), d(2026, 11, 2));
        assert_eq!(monday_on_or_before(d(2026, 11, 8)).unwrap(), d(2026, 11, 2));
        assert_eq!(
            monday_on_or_before(d(1969, 12, 28)).unwrap(),
            d(1969, 12, 22)
        );
    }

    #[test]
    fn states() {
        let today = d(2026, 11, 7);
        assert_eq!(state_of(true, d(2026, 11, 6), today), "overdue");
        assert_eq!(state_of(true, today, today), "today");
        assert_eq!(state_of(true, d(2026, 11, 8), today), "planned");
        assert_eq!(state_of(false, d(2026, 11, 6), today), "done");
    }
}
