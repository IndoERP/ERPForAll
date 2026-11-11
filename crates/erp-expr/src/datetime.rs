//! `datetime.timedelta`, `dateutil.relativedelta`, calendar arithmetic and `strftime`, with
//! Python's semantics and range checks (years 1..=9999, `|timedelta.days| <= 999999999`).
//!
//! All arithmetic is checked: out-of-range results are `OverflowError`s, never panics.

use time::{Date, Duration, Month, PrimitiveDateTime, Time};

use crate::error::{ExprError, ExprResult};
use crate::limits::Budget;

const MICROS_PER_SECOND: i128 = 1_000_000;
const MICROS_PER_DAY: i128 = 86_400 * MICROS_PER_SECOND;
pub(crate) const MIN_YEAR: i64 = 1;
pub(crate) const MAX_YEAR: i64 = 9999;

pub(crate) fn date_range_error() -> ExprError {
    ExprError::overflow("date value out of range")
}

/// Python `datetime.timedelta`, normalized: `0 <= seconds < 86400`, `0 <= microseconds < 10**6`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct TimeDelta {
    days: i32,
    seconds: i32,
    microseconds: i32,
}

impl TimeDelta {
    /// Python's bound on `abs(days)`.
    pub const MAX_DAYS: i64 = 999_999_999;

    /// Normalizes a total number of microseconds; out of range is an `OverflowError`.
    pub fn from_micros(total: i128) -> ExprResult<Self> {
        let days = total.div_euclid(MICROS_PER_DAY);
        let rem = total.rem_euclid(MICROS_PER_DAY);
        if days.abs() > i128::from(Self::MAX_DAYS) {
            return Err(ExprError::overflow(format!(
                "days={days}; must have magnitude <= {}",
                Self::MAX_DAYS
            )));
        }
        let seconds = rem / MICROS_PER_SECOND;
        let micros = rem % MICROS_PER_SECOND;
        Ok(Self {
            days: i32::try_from(days).map_err(|_| date_range_error())?,
            seconds: i32::try_from(seconds).map_err(|_| date_range_error())?,
            microseconds: i32::try_from(micros).map_err(|_| date_range_error())?,
        })
    }

    pub fn days(&self) -> i64 {
        i64::from(self.days)
    }

    pub fn seconds(&self) -> i64 {
        i64::from(self.seconds)
    }

    pub fn microseconds(&self) -> i64 {
        i64::from(self.microseconds)
    }

    pub fn total_micros(&self) -> i128 {
        i128::from(self.days) * MICROS_PER_DAY
            + i128::from(self.seconds) * MICROS_PER_SECOND
            + i128::from(self.microseconds)
    }

    pub fn total_seconds(&self) -> f64 {
        self.total_micros() as f64 / 1e6
    }

    fn to_duration(self) -> Duration {
        // |days| <= 999999999, so the seconds fit an i64 comfortably.
        let secs = i64::from(self.days) * 86_400 + i64::from(self.seconds);
        Duration::new(secs, self.microseconds * 1_000)
    }

    pub(crate) fn to_py_repr(self) -> String {
        let mut parts = Vec::new();
        if self.days != 0 {
            parts.push(format!("days={}", self.days));
        }
        if self.seconds != 0 {
            parts.push(format!("seconds={}", self.seconds));
        }
        if self.microseconds != 0 {
            parts.push(format!("microseconds={}", self.microseconds));
        }
        if parts.is_empty() {
            "datetime.timedelta(0)".to_string()
        } else {
            format!("datetime.timedelta({})", parts.join(", "))
        }
    }

    pub(crate) fn to_py_str(self) -> String {
        let h = self.seconds / 3600;
        let m = (self.seconds % 3600) / 60;
        let s = self.seconds % 60;
        let mut out = String::new();
        if self.days != 0 {
            let plural = if self.days.abs() == 1 { "" } else { "s" };
            out.push_str(&format!("{} day{plural}, ", self.days));
        }
        out.push_str(&format!("{h}:{m:02}:{s:02}"));
        if self.microseconds != 0 {
            out.push_str(&format!(".{:06}", self.microseconds));
        }
        out
    }
}

/// Converts a timedelta-style argument to microseconds: integers exactly, floats rounded half to
/// even to the microsecond (Python's rule).
pub(crate) fn unit_micros(value: f64, unit_micros: i128) -> ExprResult<i128> {
    let total = value * unit_micros as f64;
    if !total.is_finite() || total.abs() > 1e30 {
        return Err(ExprError::overflow("timedelta value out of range"));
    }
    Ok(total.round_ties_even() as i128)
}

/// A weekday of `relativedelta(weekday=...)`: day 0 = Monday, `n` the nth occurrence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Weekday {
    pub(crate) day: u8,
    pub(crate) n: Option<i64>,
}

const WEEKDAY_NAMES: [&str; 7] = ["MO", "TU", "WE", "TH", "FR", "SA", "SU"];

/// `dateutil.relativedelta.relativedelta`: relative fields (plural) are added, absolute fields
/// (singular) replace.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RelativeDelta {
    pub(crate) years: i64,
    pub(crate) months: i64,
    pub(crate) days: i64,
    pub(crate) leapdays: i64,
    pub(crate) hours: i64,
    pub(crate) minutes: i64,
    pub(crate) seconds: i64,
    pub(crate) microseconds: i64,
    pub(crate) year: Option<i64>,
    pub(crate) month: Option<i64>,
    pub(crate) day: Option<i64>,
    pub(crate) weekday: Option<Weekday>,
    pub(crate) hour: Option<i64>,
    pub(crate) minute: Option<i64>,
    pub(crate) second: Option<i64>,
    pub(crate) microsecond: Option<i64>,
}

fn sign(x: i64) -> i64 {
    if x < 0 { -1 } else { 1 }
}

/// dateutil's carry of one field into the next larger one, preserving the sign.
fn carry(small: &mut i64, large: &mut i64, base: i64, limit: i64) -> ExprResult<()> {
    if small.unsigned_abs() > limit.unsigned_abs() {
        let s = sign(*small);
        let magnitude = small.checked_mul(s).ok_or_else(date_range_error)?;
        let (div, rem) = (magnitude / base, magnitude % base);
        *small = rem * s;
        *large = large
            .checked_add(div.checked_mul(s).ok_or_else(date_range_error)?)
            .ok_or_else(date_range_error)?;
    }
    Ok(())
}

impl RelativeDelta {
    /// dateutil's `_fix`: carries overflowing relative fields upward.
    pub(crate) fn normalize(&mut self) -> ExprResult<()> {
        carry(
            &mut self.microseconds,
            &mut self.seconds,
            1_000_000,
            999_999,
        )?;
        carry(&mut self.seconds, &mut self.minutes, 60, 59)?;
        carry(&mut self.minutes, &mut self.hours, 60, 59)?;
        carry(&mut self.hours, &mut self.days, 24, 23)?;
        carry(&mut self.months, &mut self.years, 12, 11)?;
        Ok(())
    }

    pub(crate) fn is_zero(&self) -> bool {
        *self == RelativeDelta::default()
    }

    fn has_time(&self) -> bool {
        self.hours != 0
            || self.minutes != 0
            || self.seconds != 0
            || self.microseconds != 0
            || self.hour.is_some()
            || self.minute.is_some()
            || self.second.is_some()
            || self.microsecond.is_some()
    }

    /// `-rd`: relative fields negated, absolute fields kept.
    pub(crate) fn negated(&self) -> ExprResult<Self> {
        let n = |x: i64| x.checked_neg().ok_or_else(date_range_error);
        Ok(Self {
            years: n(self.years)?,
            months: n(self.months)?,
            days: n(self.days)?,
            leapdays: n(self.leapdays)?,
            hours: n(self.hours)?,
            minutes: n(self.minutes)?,
            seconds: n(self.seconds)?,
            microseconds: n(self.microseconds)?,
            ..self.clone()
        })
    }

    /// `self + other` between relativedeltas: relative fields add, `other`'s absolute fields win.
    pub(crate) fn plus(&self, other: &RelativeDelta) -> ExprResult<Self> {
        let a = |x: i64, y: i64| x.checked_add(y).ok_or_else(date_range_error);
        let mut r = Self {
            years: a(self.years, other.years)?,
            months: a(self.months, other.months)?,
            days: a(self.days, other.days)?,
            leapdays: if other.leapdays != 0 {
                other.leapdays
            } else {
                self.leapdays
            },
            hours: a(self.hours, other.hours)?,
            minutes: a(self.minutes, other.minutes)?,
            seconds: a(self.seconds, other.seconds)?,
            microseconds: a(self.microseconds, other.microseconds)?,
            year: other.year.or(self.year),
            month: other.month.or(self.month),
            day: other.day.or(self.day),
            weekday: other.weekday.or(self.weekday),
            hour: other.hour.or(self.hour),
            minute: other.minute.or(self.minute),
            second: other.second.or(self.second),
            microsecond: other.microsecond.or(self.microsecond),
        };
        r.normalize()?;
        Ok(r)
    }

    /// `date + rd`: a date, or a datetime when the delta has a time component (dateutil).
    pub(crate) fn add_to_date(&self, date: Date) -> ExprResult<DateOrTime> {
        if self.has_time() {
            let dt = PrimitiveDateTime::new(date, Time::MIDNIGHT);
            return Ok(DateOrTime::DateTime(self.add_to_datetime(dt)?));
        }
        let (y, m, d) = self.calendar_target(date)?;
        let mut days = self.days;
        if self.leapdays != 0 && m > 2 && is_leap(y) {
            days = days
                .checked_add(self.leapdays)
                .ok_or_else(date_range_error)?;
        }
        let base = make_date(y, m, d)?;
        let ret = add_days(base, days)?;
        Ok(DateOrTime::Date(self.apply_weekday(ret)?))
    }

    /// `datetime + rd`.
    pub(crate) fn add_to_datetime(&self, dt: PrimitiveDateTime) -> ExprResult<PrimitiveDateTime> {
        let (y, m, d) = self.calendar_target(dt.date())?;
        let mut days = self.days;
        if self.leapdays != 0 && m > 2 && is_leap(y) {
            days = days
                .checked_add(self.leapdays)
                .ok_or_else(date_range_error)?;
        }
        let hour = self.hour.unwrap_or(i64::from(dt.hour()));
        let minute = self.minute.unwrap_or(i64::from(dt.minute()));
        let second = self.second.unwrap_or(i64::from(dt.second()));
        let micro = self.microsecond.unwrap_or(i64::from(dt.microsecond()));
        let base =
            PrimitiveDateTime::new(make_date(y, m, d)?, make_time(hour, minute, second, micro)?);
        let delta = i128::from(days) * MICROS_PER_DAY
            + i128::from(self.hours) * 3_600 * MICROS_PER_SECOND
            + i128::from(self.minutes) * 60 * MICROS_PER_SECOND
            + i128::from(self.seconds) * MICROS_PER_SECOND
            + i128::from(self.microseconds);
        let ret = datetime_add(base, TimeDelta::from_micros(delta)?)?;
        let wd = self.apply_weekday(ret.date())?;
        Ok(PrimitiveDateTime::new(wd, ret.time()))
    }

    /// Year, month and clamped day after applying `year`/`month`/`day` and `years`/`months`.
    fn calendar_target(&self, date: Date) -> ExprResult<(i64, i64, i64)> {
        let mut year = self
            .year
            .unwrap_or(i64::from(date.year()))
            .checked_add(self.years)
            .ok_or_else(date_range_error)?;
        let mut month = self.month.unwrap_or(i64::from(u8::from(date.month())));
        if !(1..=12).contains(&month) {
            return Err(ExprError::value_error("month must be in 1..12"));
        }
        if self.months != 0 {
            month += self.months;
            if month > 12 {
                year = year.checked_add(1).ok_or_else(date_range_error)?;
                month -= 12;
            } else if month < 1 {
                year = year.checked_sub(1).ok_or_else(date_range_error)?;
                month += 12;
            }
        }
        if !(MIN_YEAR..=MAX_YEAR).contains(&year) {
            return Err(ExprError::value_error(format!(
                "year {year} is out of range"
            )));
        }
        let day = self.day.unwrap_or(i64::from(date.day()));
        let max_day = i64::from(days_in_month(year, month)?);
        Ok((year, month, day.min(max_day)))
    }

    fn apply_weekday(&self, date: Date) -> ExprResult<Date> {
        let Some(wd) = self.weekday else {
            return Ok(date);
        };
        let nth = match wd.n {
            Some(0) | None => 1,
            Some(n) => n,
        };
        let current = i64::from(date.weekday().number_days_from_monday());
        let target = i64::from(wd.day);
        let mut jump = nth
            .checked_abs()
            .and_then(|a| a.checked_sub(1))
            .and_then(|a| a.checked_mul(7))
            .ok_or_else(date_range_error)?;
        if nth > 0 {
            jump += (7 - current + target).rem_euclid(7);
        } else {
            jump += (current - target).rem_euclid(7);
            jump = -jump;
        }
        add_days(date, jump)
    }

    pub(crate) fn to_py_repr(&self) -> String {
        let mut parts = Vec::new();
        for (name, v) in [
            ("years", self.years),
            ("months", self.months),
            ("days", self.days),
            ("leapdays", self.leapdays),
            ("hours", self.hours),
            ("minutes", self.minutes),
            ("seconds", self.seconds),
            ("microseconds", self.microseconds),
        ] {
            if v != 0 {
                parts.push(format!("{name}={v:+}"));
            }
        }
        for (name, v) in [
            ("year", self.year),
            ("month", self.month),
            ("day", self.day),
        ] {
            if let Some(v) = v {
                parts.push(format!("{name}={v}"));
            }
        }
        if let Some(wd) = self.weekday {
            let name = WEEKDAY_NAMES
                .get(usize::from(wd.day))
                .copied()
                .unwrap_or("??");
            match wd.n {
                Some(n) if n != 0 => parts.push(format!("weekday={name}({n:+})")),
                _ => parts.push(format!("weekday={name}")),
            }
        }
        for (name, v) in [
            ("hour", self.hour),
            ("minute", self.minute),
            ("second", self.second),
            ("microsecond", self.microsecond),
        ] {
            if let Some(v) = v {
                parts.push(format!("{name}={v}"));
            }
        }
        format!("relativedelta({})", parts.join(", "))
    }
}

/// Result of adding a relativedelta to a date.
pub(crate) enum DateOrTime {
    Date(Date),
    DateTime(PrimitiveDateTime),
}

pub(crate) fn is_leap(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

fn month_of(month: i64) -> ExprResult<Month> {
    u8::try_from(month)
        .ok()
        .and_then(|m| Month::try_from(m).ok())
        .ok_or_else(|| ExprError::value_error("month must be in 1..12"))
}

pub(crate) fn days_in_month(year: i64, month: i64) -> ExprResult<u8> {
    let y = i32::try_from(year)
        .map_err(|_| ExprError::value_error(format!("year {year} is out of range")))?;
    Ok(month_of(month)?.length(y))
}

/// A Python-valid date (`ValueError` for out-of-range fields, as `datetime.date()` raises).
pub(crate) fn make_date(year: i64, month: i64, day: i64) -> ExprResult<Date> {
    if !(MIN_YEAR..=MAX_YEAR).contains(&year) {
        return Err(ExprError::value_error(format!(
            "year {year} is out of range"
        )));
    }
    let m = month_of(month)?;
    let max = i64::from(days_in_month(year, month)?);
    if !(1..=max).contains(&day) {
        return Err(ExprError::value_error("day is out of range for month"));
    }
    let y = i32::try_from(year).map_err(|_| date_range_error())?;
    let d =
        u8::try_from(day).map_err(|_| ExprError::value_error("day is out of range for month"))?;
    Date::from_calendar_date(y, m, d)
        .map_err(|_| ExprError::value_error("day is out of range for month"))
}

pub(crate) fn make_time(hour: i64, minute: i64, second: i64, micro: i64) -> ExprResult<Time> {
    let field = |v: i64, max: i64, name: &str| -> ExprResult<u8> {
        if (0..=max).contains(&v) {
            u8::try_from(v)
                .map_err(|_| ExprError::value_error(format!("{name} must be in 0..{max}")))
        } else {
            Err(ExprError::value_error(format!(
                "{name} must be in 0..{max}"
            )))
        }
    };
    let h = field(hour, 23, "hour")?;
    let m = field(minute, 59, "minute")?;
    let s = field(second, 59, "second")?;
    if !(0..=999_999).contains(&micro) {
        return Err(ExprError::value_error("microsecond must be in 0..999999"));
    }
    let us = u32::try_from(micro)
        .map_err(|_| ExprError::value_error("microsecond must be in 0..999999"))?;
    Time::from_hms_micro(h, m, s, us).map_err(|e| ExprError::value_error(e.to_string()))
}

fn check_year(d: Date) -> ExprResult<Date> {
    if (MIN_YEAR..=MAX_YEAR).contains(&i64::from(d.year())) {
        Ok(d)
    } else {
        Err(date_range_error())
    }
}

pub(crate) fn add_days(d: Date, days: i64) -> ExprResult<Date> {
    if days.unsigned_abs() > 4_000_000 {
        return Err(date_range_error());
    }
    let r = d
        .checked_add(Duration::days(days))
        .ok_or_else(date_range_error)?;
    check_year(r)
}

/// `date + timedelta`: only the days part counts, as in Python.
pub(crate) fn date_add(d: Date, td: TimeDelta) -> ExprResult<Date> {
    add_days(d, td.days())
}

pub(crate) fn datetime_add(dt: PrimitiveDateTime, td: TimeDelta) -> ExprResult<PrimitiveDateTime> {
    let r = dt
        .checked_add(td.to_duration())
        .ok_or_else(date_range_error)?;
    check_year(r.date())?;
    Ok(r)
}

pub(crate) fn date_diff(a: Date, b: Date) -> ExprResult<TimeDelta> {
    let days = i128::from(a.to_julian_day()) - i128::from(b.to_julian_day());
    TimeDelta::from_micros(days * MICROS_PER_DAY)
}

pub(crate) fn datetime_diff(a: PrimitiveDateTime, b: PrimitiveDateTime) -> ExprResult<TimeDelta> {
    let micros = |dt: PrimitiveDateTime| {
        i128::from(dt.date().to_julian_day()) * MICROS_PER_DAY
            + i128::from(dt.time().hour()) * 3_600 * MICROS_PER_SECOND
            + i128::from(dt.time().minute()) * 60 * MICROS_PER_SECOND
            + i128::from(dt.time().second()) * MICROS_PER_SECOND
            + i128::from(dt.time().microsecond())
    };
    TimeDelta::from_micros(micros(a) - micros(b))
}

pub(crate) fn date_iso(d: Date) -> String {
    format!("{:04}-{:02}-{:02}", d.year(), u8::from(d.month()), d.day())
}

pub(crate) fn datetime_iso(dt: PrimitiveDateTime, sep: char) -> String {
    let mut s = format!(
        "{}{sep}{:02}:{:02}:{:02}",
        date_iso(dt.date()),
        dt.hour(),
        dt.minute(),
        dt.second()
    );
    if dt.microsecond() != 0 {
        s.push_str(&format!(".{:06}", dt.microsecond()));
    }
    s
}

pub(crate) fn datetime_repr(dt: PrimitiveDateTime) -> String {
    let mut s = format!(
        "datetime.datetime({}, {}, {}, {}, {}",
        dt.year(),
        u8::from(dt.month()),
        dt.day(),
        dt.hour(),
        dt.minute()
    );
    if dt.second() != 0 || dt.microsecond() != 0 {
        s.push_str(&format!(", {}", dt.second()));
    }
    if dt.microsecond() != 0 {
        s.push_str(&format!(", {}", dt.microsecond()));
    }
    s.push(')');
    s
}

const DAY_NAMES: [&str; 7] = [
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
    "Sunday",
];
const MONTH_NAMES: [&str; 12] = [
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
];

/// Python `strftime` (C locale) for a naive date/time. Unknown directives are a `ValueError`
/// rather than platform-dependent output.
pub(crate) fn strftime(
    date: Date,
    time: Time,
    fmt: &str,
    budget: &mut Budget,
) -> ExprResult<String> {
    budget.scan(fmt.len())?;
    let mut out = budget.out();
    let mut chars = fmt.chars();
    let weekday = usize::from(date.weekday().number_days_from_monday());
    let month = usize::from(u8::from(date.month()));
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c)?;
            continue;
        }
        let Some(d) = chars.next() else {
            return Err(ExprError::value_error(
                "stray % at the end of the strftime format",
            ));
        };
        let day_name = DAY_NAMES.get(weekday).copied().unwrap_or("");
        let month_name = MONTH_NAMES
            .get(month.saturating_sub(1))
            .copied()
            .unwrap_or("");
        let hour12 = match time.hour() % 12 {
            0 => 12,
            h => h,
        };
        let piece = match d {
            'Y' => date.year().to_string(),
            'y' => format!("{:02}", date.year().rem_euclid(100)),
            'm' => format!("{month:02}"),
            'd' => format!("{:02}", date.day()),
            'H' => format!("{:02}", time.hour()),
            'I' => format!("{hour12:02}"),
            'M' => format!("{:02}", time.minute()),
            'S' => format!("{:02}", time.second()),
            'f' => format!("{:06}", time.microsecond()),
            'p' => if time.hour() < 12 { "AM" } else { "PM" }.to_string(),
            'j' => format!("{:03}", date.ordinal()),
            'w' => date.weekday().number_days_from_sunday().to_string(),
            'u' => date.weekday().number_from_monday().to_string(),
            'a' => day_name.get(..3).unwrap_or("").to_string(),
            'A' => day_name.to_string(),
            'b' | 'h' => month_name.get(..3).unwrap_or("").to_string(),
            'B' => month_name.to_string(),
            'U' => format!("{:02}", date.sunday_based_week()),
            'W' => format!("{:02}", date.monday_based_week()),
            'V' => format!("{:02}", date.iso_week()),
            'G' => date.to_iso_week_date().0.to_string(),
            'z' | 'Z' => String::new(),
            'c' => format!(
                "{} {} {:>2} {:02}:{:02}:{:02} {}",
                day_name.get(..3).unwrap_or(""),
                month_name.get(..3).unwrap_or(""),
                date.day(),
                time.hour(),
                time.minute(),
                time.second(),
                date.year()
            ),
            'x' => format!(
                "{month:02}/{:02}/{:02}",
                date.day(),
                date.year().rem_euclid(100)
            ),
            'X' => format!(
                "{:02}:{:02}:{:02}",
                time.hour(),
                time.minute(),
                time.second()
            ),
            'D' => format!(
                "{month:02}/{:02}/{:02}",
                date.day(),
                date.year().rem_euclid(100)
            ),
            'F' => date_iso(date),
            'T' => format!(
                "{:02}:{:02}:{:02}",
                time.hour(),
                time.minute(),
                time.second()
            ),
            'e' => format!("{:>2}", date.day()),
            '%' => "%".to_string(),
            other => {
                return Err(ExprError::value_error(format!(
                    "unsupported strftime directive '%{other}'"
                )));
            }
        };
        out.push_str(&piece)?;
    }
    budget.finish(out)
}
