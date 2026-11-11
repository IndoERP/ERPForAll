//! The next run of a schedule: the interval is added in the
//! cron user's time zone, so a daily run keeps its wall-clock hour across daylight saving
//! changes, and repeated until the result lies strictly in the future.

use erp_sdk::{Error, Result};
use jiff::tz::TimeZone;
use jiff::{Span, Timestamp};

/// Largest number of additions one catch-up may make (a schedule years behind at one minute).
const MAX_STEPS: u32 = 5_000_000;

/// The unit of a schedule's interval (`interval_type`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum IntervalUnit {
    /// Minutes.
    Minutes,
    /// Hours.
    Hours,
    /// Days (wall-clock days in the user's zone).
    Days,
    /// Weeks (seven days).
    Weeks,
    /// Calendar months, clipping to the month's end.
    Months,
}

impl IntervalUnit {
    /// The `interval_type` selection value.
    pub fn parse(text: &str) -> Result<Self> {
        match text {
            "minutes" => Ok(Self::Minutes),
            "hours" => Ok(Self::Hours),
            "days" => Ok(Self::Days),
            "weeks" => Ok(Self::Weeks),
            "months" => Ok(Self::Months),
            other => Err(Error::validation(format!(
                "'{other}' is not an interval unit (minutes, hours, days, weeks or months)"
            ))),
        }
    }

    fn span(self, number: i64) -> Result<Span> {
        let span = Span::new();
        let made = match self {
            Self::Minutes => span.try_minutes(number),
            Self::Hours => span.try_hours(number),
            Self::Days => span.try_days(number),
            Self::Weeks => span.try_weeks(number),
            Self::Months => span.try_months(number),
        };
        made.map_err(|_| Error::validation(format!("the interval {number} is out of range")))
    }
}

/// The first run after `nextcall` that is strictly after `now` (both in microseconds since the
/// Unix epoch, UTC), adding `number` `unit`s in the zone `tz` (an IANA name; an unknown name is
/// an error, not UTC).
///
/// # Errors
///
/// `Validation` for a non-positive `number`, an unknown zone or a result out of range;
/// `Programming` when the catch-up would take more than five million steps.
pub fn next_call(
    nextcall: i64,
    number: i64,
    unit: IntervalUnit,
    tz: &str,
    now: i64,
) -> Result<i64> {
    if number < 1 {
        return Err(Error::validation(
            "The interval number must be a strictly positive number.",
        ));
    }
    let zone = TimeZone::get(tz)
        .map_err(|_| Error::validation(format!("'{tz}' is not a known time zone")))?;
    let span = unit.span(number)?;
    let mut current = nextcall;
    let mut steps = 0u32;
    while current <= now {
        steps += 1;
        if steps > MAX_STEPS {
            return Err(Error::programming(
                "a schedule is too far behind to catch up one interval at a time",
            ));
        }
        let at = Timestamp::from_microsecond(current)
            .map_err(|_| Error::validation("the next call is out of range"))?
            .to_zoned(zone.clone());
        let next = at
            .checked_add(span)
            .map_err(|_| Error::validation("the next call is out of range"))?;
        current = next.timestamp().as_microsecond();
    }
    Ok(current)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn micros(text: &str) -> i64 {
        text.parse::<Timestamp>().unwrap().as_microsecond()
    }

    #[test]
    fn every_unit_advances_until_strictly_in_the_future() {
        let start = micros("2026-01-31T10:00:00Z");
        let now = micros("2026-01-31T10:00:00Z");
        let step = |n, unit| next_call(start, n, unit, "UTC", now).unwrap();
        assert_eq!(
            step(5, IntervalUnit::Minutes),
            micros("2026-01-31T10:05:00Z")
        );
        assert_eq!(step(2, IntervalUnit::Hours), micros("2026-01-31T12:00:00Z"));
        assert_eq!(step(1, IntervalUnit::Days), micros("2026-02-01T10:00:00Z"));
        assert_eq!(step(1, IntervalUnit::Weeks), micros("2026-02-07T10:00:00Z"));
        // a month from the 31st clips to the end of February
        assert_eq!(
            step(1, IntervalUnit::Months),
            micros("2026-02-28T10:00:00Z")
        );
        // behind by two and a half hours at a one hour interval: caught up in the loop, and an
        // exactly equal result is not "in the future"
        let now = micros("2026-01-31T12:30:00Z");
        assert_eq!(
            next_call(start, 1, IntervalUnit::Hours, "UTC", now).unwrap(),
            micros("2026-01-31T13:00:00Z")
        );
        let on_the_hour = micros("2026-01-31T12:00:00Z");
        assert_eq!(
            next_call(start, 1, IntervalUnit::Hours, "UTC", on_the_hour).unwrap(),
            micros("2026-01-31T13:00:00Z")
        );
    }

    /// daylight saving changes)
    #[test]
    fn a_daily_run_keeps_its_wall_clock_hour_across_dst() {
        let zone = "Europe/Brussels";
        // 2026-03-28 09:00 CET is 08:00 UTC; the clocks go forward on 2026-03-29
        let before = micros("2026-03-28T08:00:00Z");
        let after = next_call(before, 1, IntervalUnit::Days, zone, before).unwrap();
        assert_eq!(
            after,
            micros("2026-03-29T07:00:00Z"),
            "09:00 CEST is 07:00 UTC"
        );
        // and back: 2026-10-24 09:00 CEST is 07:00 UTC; the clocks go back on 2026-10-25
        let before = micros("2026-10-24T07:00:00Z");
        let after = next_call(before, 1, IntervalUnit::Days, zone, before).unwrap();
        assert_eq!(
            after,
            micros("2026-10-25T08:00:00Z"),
            "09:00 CET is 08:00 UTC"
        );
    }

    #[test]
    fn the_interval_and_the_zone_are_checked() {
        assert_eq!(
            next_call(0, 0, IntervalUnit::Days, "UTC", 1)
                .unwrap_err()
                .message(),
            "The interval number must be a strictly positive number."
        );
        assert_eq!(
            next_call(0, 1, IntervalUnit::Days, "Nowhere/Land", 1)
                .unwrap_err()
                .message(),
            "'Nowhere/Land' is not a known time zone"
        );
        assert_eq!(
            IntervalUnit::parse("years").unwrap_err().message(),
            "'years' is not an interval unit (minutes, hours, days, weeks or months)"
        );
        assert!(next_call(i64::MAX - 1, 1, IntervalUnit::Months, "UTC", i64::MAX).is_err());
    }
}
