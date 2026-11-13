//! Compact date and naive-UTC datetime values (FIELD §4.8.9).
//!
//! [`Date`] is a day count since 1970-01-01 (4 bytes) and [`DateTime`] a microsecond count since
//! 1970-01-01 00:00:00 UTC (8 bytes), so cache columns stay plain integer vectors.

use std::fmt;

const MICROS_PER_SECOND: i64 = 1_000_000;
const MICROS_PER_DAY: i64 = 86_400 * MICROS_PER_SECOND;

/// A calendar date (proleptic Gregorian), stored as days since 1970-01-01.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Date(i32);

/// A naive datetime meaning UTC, stored as microseconds since 1970-01-01 00:00:00.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DateTime(i64);

/// Days since 1970-01-01 of a civil date (H. Hinnant's algorithm).
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = y - era * 400;
    let m = i64::from(month);
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + i64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Civil date of a day count since 1970-01-01.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = (if z >= 0 { z } else { z - 146_096 }) / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    // month in 1..=12 and day in 1..=31 by construction
    (year, month as u32, day as u32)
}

fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if (year % 4 == 0 && year % 100 != 0) || year % 400 == 0 => 29,
        2 => 28,
        _ => 0,
    }
}

impl Date {
    /// The date for `year-month-day`, or `None` when it does not exist.
    pub fn from_ymd(year: i32, month: u32, day: u32) -> Option<Self> {
        if !(1..=12).contains(&month) || day == 0 || day > days_in_month(i64::from(year), month) {
            return None;
        }
        i32::try_from(days_from_civil(i64::from(year), month, day))
            .ok()
            .map(Self)
    }

    /// Build from a raw day count since 1970-01-01.
    pub const fn from_days(days: i32) -> Self {
        Self(days)
    }

    /// Days since 1970-01-01.
    pub const fn days(self) -> i32 {
        self.0
    }

    /// `(year, month, day)`.
    pub fn ymd(self) -> (i32, u32, u32) {
        let (y, m, d) = civil_from_days(i64::from(self.0));
        (y as i32, m, d)
    }

    /// Returns `None` for anything `strptime` would reject.
    pub fn parse_odoo(text: &str) -> Option<Self> {
        let head: String = text.chars().take(10).collect();
        let parts = strptime(&head, "%Y-%m-%d")?;
        Self::from_ymd(parts.year, parts.month, parts.day)
    }

    /// The current date in UTC (`Date.today()`, the server clock read as UTC).
    pub fn today() -> Self {
        DateTime::now().date()
    }

    /// The current date as seen in the time zone `tz` (`Date.context_today(record)`): the clock
    /// converted to `tz`, then cut to its date.
    ///
    /// # Errors
    ///
    /// `Validation` when `tz` is not a known IANA zone name.
    pub fn context_today(tz: &str) -> crate::Result<Self> {
        Self::context_today_at(tz, DateTime::now())
    }

    /// Like [`context_today`](Self::context_today) for the instant `timestamp` (a naive UTC
    /// datetime) instead of the clock: `Date.context_today(record, timestamp)`.
    ///
    /// # Errors
    ///
    /// `Validation` when `tz` is not a known IANA zone name.
    pub fn context_today_at(tz: &str, timestamp: DateTime) -> crate::Result<Self> {
        timestamp
            .to_timezone(tz)
            .map(DateTime::date)
            .ok_or_else(|| crate::Error::validation(format!("unknown time zone '{tz}'")))
    }

    /// Midnight of this date.
    ///
    /// Overflows for day counts beyond about +-106 million years; use
    /// [`checked_at_midnight`](Self::checked_at_midnight) for dates of untrusted origin.
    pub fn at_midnight(self) -> DateTime {
        DateTime(i64::from(self.0) * MICROS_PER_DAY)
    }

    /// Midnight of this date, `None` when the microsecond count does not fit.
    pub fn checked_at_midnight(self) -> Option<DateTime> {
        i64::from(self.0).checked_mul(MICROS_PER_DAY).map(DateTime)
    }
}

impl fmt::Display for Date {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (y, m, d) = self.ymd();
        write!(f, "{y:04}-{m:02}-{d:02}")
    }
}

impl DateTime {
    /// Build from parts; `None` when the date or time does not exist.
    pub fn from_parts(date: Date, hour: u32, minute: u32, second: u32, micro: u32) -> Option<Self> {
        if hour > 23 || minute > 59 || second > 59 || micro > 999_999 {
            return None;
        }
        let secs = i64::from(hour) * 3600 + i64::from(minute) * 60 + i64::from(second);
        Some(Self(
            i64::from(date.0) * MICROS_PER_DAY + secs * MICROS_PER_SECOND + i64::from(micro),
        ))
    }

    /// Build from a raw microsecond count since the Unix epoch.
    pub const fn from_micros(micros: i64) -> Self {
        Self(micros)
    }

    /// Microseconds since the Unix epoch.
    pub const fn micros(self) -> i64 {
        self.0
    }

    /// The date part.
    pub fn date(self) -> Date {
        Date(self.0.div_euclid(MICROS_PER_DAY) as i32)
    }

    /// `(hour, minute, second, microsecond)`.
    pub fn time(self) -> (u32, u32, u32, u32) {
        let in_day = self.0.rem_euclid(MICROS_PER_DAY);
        let secs = in_day / MICROS_PER_SECOND;
        (
            (secs / 3600) as u32,
            ((secs / 60) % 60) as u32,
            (secs % 60) as u32,
            (in_day % MICROS_PER_SECOND) as u32,
        )
    }

    pub fn parse_odoo(text: &str) -> Option<Self> {
        const FORMAT: &str = "%Y-%m-%d %H:%M:%S";
        let cut = text.chars().count().checked_sub(2)?;
        let format = FORMAT.get(..cut.min(FORMAT.len()))?;
        let parts = strptime(text, format)?;
        let date = Date::from_ymd(parts.year, parts.month, parts.day)?;
        Self::from_parts(date, parts.hour, parts.minute, parts.second, 0)
    }

    /// The current time with microseconds zeroed (`Datetime.now()`).
    pub fn now() -> Self {
        let micros = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| i64::try_from(d.as_micros()).unwrap_or(i64::MAX))
            .unwrap_or(0);
        Self(micros - micros.rem_euclid(MICROS_PER_SECOND))
    }

    /// Today at 00:00:00 (`Datetime.today()`).
    pub fn today() -> Self {
        Self::now().date().at_midnight()
    }

    /// This UTC datetime as a naive local datetime in `tz`, or `None` for an unknown zone name.
    pub fn to_timezone(self, tz: &str) -> Option<Self> {
        let zone = jiff::tz::TimeZone::get(tz).ok()?;
        let ts = jiff::Timestamp::from_microsecond(self.0).ok()?;
        let offset = zone.to_offset(ts);
        Some(Self(
            self.0 + i64::from(offset.seconds()) * MICROS_PER_SECOND,
        ))
    }

    /// The UTC offset of `tz` at this instant in seconds, or `None` for an unknown zone name.
    pub fn offset_seconds(self, tz: &str) -> Option<i32> {
        let zone = jiff::tz::TimeZone::get(tz).ok()?;
        let ts = jiff::Timestamp::from_microsecond(self.0).ok()?;
        Some(zone.to_offset(ts).seconds())
    }
}

impl fmt::Display for DateTime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (h, m, s, _) = self.time();
        write!(f, "{} {h:02}:{m:02}:{s:02}", self.date())
    }
}

/// Date and time parts produced by [`strptime`]; missing parts take `strptime`'s defaults.
#[derive(Debug, Clone, Copy)]
struct Parts {
    year: i32,
    month: u32,
    day: u32,
    hour: u32,
    minute: u32,
    second: u32,
}

/// The whole
/// input must be consumed. An incomplete directive (a trailing `%`) is an error, as in CPython.
fn strptime(input: &str, format: &str) -> Option<Parts> {
    let mut parts = Parts {
        year: 1900,
        month: 1,
        day: 1,
        hour: 0,
        minute: 0,
        second: 0,
    };
    let bytes = input.as_bytes();
    let mut pos = 0usize;
    let mut fmt = format.bytes();
    while let Some(c) = fmt.next() {
        if c != b'%' {
            if bytes.get(pos) != Some(&c) {
                return None;
            }
            pos += 1;
            continue;
        }
        let directive = fmt.next()?;
        let (max_digits, min, max) = match directive {
            b'Y' => (4, 0, 9999),
            b'm' => (2, 1, 12),
            b'd' => (2, 1, 31),
            b'H' => (2, 0, 23),
            b'M' => (2, 0, 59),
            b'S' => (2, 0, 61),
            _ => return None,
        };
        let digits = bytes[pos..]
            .iter()
            .take(max_digits)
            .take_while(|b| b.is_ascii_digit())
            .count();
        if directive == b'Y' && digits != 4 {
            return None;
        }
        // Two digits when they form a value in range, else one (CPython's regex alternatives).
        let mut taken = None;
        for width in (1..=digits).rev() {
            let text = std::str::from_utf8(&bytes[pos..pos + width]).ok()?;
            let value: u32 = text.parse().ok()?;
            if (min..=max).contains(&value) {
                taken = Some((width, value));
                break;
            }
        }
        let (width, value) = taken?;
        pos += width;
        match directive {
            b'Y' => parts.year = value as i32,
            b'm' => parts.month = value,
            b'd' => parts.day = value,
            b'H' => parts.hour = value,
            b'M' => parts.minute = value,
            _ => parts.second = value.min(59),
        }
    }
    (pos == bytes.len()).then_some(parts)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(y: i32, m: u32, day: u32) -> Date {
        Date::from_ymd(y, m, day).expect("valid date")
    }

    fn dt(y: i32, m: u32, day: u32, h: u32, mi: u32, s: u32) -> DateTime {
        DateTime::from_parts(d(y, m, day), h, mi, s, 0).expect("valid datetime")
    }

    #[test]
    fn civil_round_trip() {
        for (y, m, day) in [
            (1970, 1, 1),
            (2000, 2, 29),
            (1969, 12, 31),
            (2024, 1, 2),
            (1, 1, 1),
        ] {
            assert_eq!(d(y, m, day).ymd(), (y, m, day));
        }
        assert_eq!(d(1970, 1, 1).days(), 0);
        assert_eq!(d(2000, 1, 1).days(), 10_957);
        assert!(Date::from_ymd(2023, 2, 29).is_none());
        assert!(Date::from_ymd(2024, 13, 1).is_none());
    }

    #[test]
    fn date_parsing_follows_reference() {
        assert_eq!(Date::parse_odoo("2012-05-01"), Some(d(2012, 5, 1)));
        assert_eq!(Date::parse_odoo("2012-05-01 10:45:00"), Some(d(2012, 5, 1)));
        assert_eq!(Date::parse_odoo("12-5-1"), None);
        assert_eq!(Date::parse_odoo("2020-4-5"), Some(d(2020, 4, 5)));
    }

    #[test]
    fn datetime_parsing_follows_reference() {
        assert_eq!(
            DateTime::parse_odoo("2012-05-01"),
            Some(dt(2012, 5, 1, 0, 0, 0))
        );
        assert_eq!(
            DateTime::parse_odoo("2012-05-01 06:00:00"),
            Some(dt(2012, 5, 1, 6, 0, 0))
        );
        assert_eq!(DateTime::parse_odoo("12-5-1"), None);
        assert_eq!(DateTime::parse_odoo("2020"), Some(dt(2020, 1, 1, 0, 0, 0)));
        assert_eq!(
            DateTime::parse_odoo("2020-04"),
            Some(dt(2020, 4, 1, 0, 0, 0))
        );
        assert_eq!(
            DateTime::parse_odoo("2020-04-05 10"),
            Some(dt(2020, 4, 5, 10, 0, 0))
        );
        assert_eq!(
            DateTime::parse_odoo("2020-04-05 10:11"),
            Some(dt(2020, 4, 5, 10, 11, 0))
        );
        assert_eq!(DateTime::parse_odoo("2020-04-05T10:11:12"), None);
        assert_eq!(DateTime::parse_odoo("2020-04-05 10:11:12.5"), None);
        assert_eq!(DateTime::parse_odoo("2020-4-5"), None);
    }

    #[test]
    fn display_uses_server_formats() {
        assert_eq!(d(2024, 1, 2).to_string(), "2024-01-02");
        assert_eq!(dt(2024, 1, 1, 20, 0, 5).to_string(), "2024-01-01 20:00:05");
    }

    #[test]
    fn timezone_conversion() {
        let utc = dt(2024, 1, 1, 20, 0, 0);
        assert_eq!(
            utc.to_timezone("Asia/Jakarta"),
            Some(dt(2024, 1, 2, 3, 0, 0))
        );
        assert_eq!(utc.to_timezone("Mars/Base"), None);
        assert_eq!(utc.offset_seconds("Asia/Jakarta"), Some(7 * 3600));
    }

    #[test]
    fn spec_field_030_datetime_helpers() {
        assert_eq!(DateTime::now().time().3, 0);
        assert_eq!(DateTime::today().time(), (0, 0, 0, 0));
        let utc = dt(2024, 1, 1, 20, 0, 0);
        assert_eq!(
            utc.to_timezone("Asia/Jakarta"),
            Some(dt(2024, 1, 2, 3, 0, 0))
        );
    }

    #[test]
    fn spec_field_030_context_today_follows_the_zone() {
        let at = DateTime::from_parts(d(2024, 1, 1), 20, 15, 30, 123_456).expect("valid datetime");
        // UTC+7: already the next day
        assert_eq!(
            Date::context_today_at("Asia/Jakarta", at).expect("known zone"),
            d(2024, 1, 2)
        );
        assert_eq!(
            Date::context_today_at("UTC", at).expect("known zone"),
            d(2024, 1, 1)
        );
        // UTC-8: still the previous day at 05:00 UTC
        let early = dt(2024, 1, 1, 5, 0, 0);
        assert_eq!(
            Date::context_today_at("America/Los_Angeles", early).expect("known zone"),
            d(2023, 12, 31)
        );
    }

    #[test]
    fn context_today_refuses_an_unknown_zone() {
        let err = Date::context_today_at("Mars/Base", dt(2024, 1, 1, 0, 0, 0))
            .expect_err("an unknown zone is refused");
        assert_eq!(err.kind(), crate::ErrorKind::Validation);
        assert!(err.message().contains("Mars/Base"), "{}", err.message());
        assert!(Date::context_today("Mars/Base").is_err());
    }

    #[test]
    fn today_reads_the_clock() {
        let before = DateTime::now().date();
        let today = Date::today();
        let in_utc = Date::context_today("UTC").expect("known zone");
        let after = DateTime::now().date();
        assert!(
            before <= today && today <= after,
            "{before} {today} {after}"
        );
        assert!(before <= in_utc && in_utc <= after);
    }
}
