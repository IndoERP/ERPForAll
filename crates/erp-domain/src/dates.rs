//! Date values in domains: ISO strings and relative dates (SPEC-DOM §4.9).
//!
//! BASIC optimization only validates relative strings; resolving them needs "now" and belongs to
//! the DYNAMIC_VALUES level.

use time::{Date, Duration, Month, PrimitiveDateTime, Time, Weekday};

use crate::error::{DomainError, ErrorReason};
use crate::schema::TimeZone;
use crate::value::{Value, clip, py_str_repr};

/// What a relative date is resolved against.
#[derive(Clone, Copy)]
pub struct DateContext<'a> {
    now: PrimitiveDateTime,
    tz: &'a dyn TimeZone,
    week_start: Weekday,
}

impl<'a> DateContext<'a> {
    pub fn new(now_utc: PrimitiveDateTime, tz: &'a dyn TimeZone, week_start: Weekday) -> Self {
        DateContext {
            now: now_utc,
            tz,
            week_start,
        }
    }
}

/// Errors are [`crate::ErrorKind::Programming`]; callers handling external input reclassify.
pub fn parse_date(value: &str, ctx: &DateContext<'_>) -> Result<Value, DomainError> {
    parse_date_inner(value, ctx).map_err(DomainError::programming)
}

pub(crate) fn parse_date_inner(value: &str, ctx: &DateContext<'_>) -> Result<Value, ErrorReason> {
    if looks_iso(value) {
        return parse_iso(value);
    }
    let mut terms = value.split_whitespace().peekable();
    let Some(first) = terms.peek().copied() else {
        return Err(ErrorReason::EmptyDateValue);
    };
    let local_now = ctx
        .tz
        .utc_to_local(ctx.now)
        .ok_or(ErrorReason::DateOverflow)?;
    let mut moment = if first == "today" {
        terms.next();
        Moment::Date(local_now.date())
    } else {
        if first == "now" {
            terms.next();
        }
        Moment::Local(local_now)
    };
    for term in terms {
        moment = apply_term(moment, term, value, ctx.week_start)?;
    }
    Ok(match moment {
        Moment::Date(d) => Value::Date(d),
        Moment::Naive(dt) => Value::Datetime(dt),
        Moment::Local(local) => Value::Datetime(
            ctx.tz
                .local_to_utc(local)
                .ok_or(ErrorReason::DateOverflow)?,
        ),
    })
}

/// Check a relative date string without resolving it against the real clock (BASIC level). Uses
/// a fixed reference instant; returns whether the expression yields a date (vs a datetime).
pub(crate) fn validate_relative(value: &str) -> Result<bool, ErrorReason> {
    let reference = PrimitiveDateTime::new(
        Date::from_calendar_date(2000, Month::June, 15).map_err(|_| ErrorReason::DateOverflow)?,
        Time::MIDNIGHT,
    );
    let ctx = DateContext::new(reference, &crate::schema::Utc, Weekday::Monday);
    Ok(matches!(parse_date_inner(value, &ctx)?, Value::Date(_)))
}

/// `re.match(r'\d+-', value)`: one or more ASCII digits followed by a dash.
fn looks_iso(value: &str) -> bool {
    let digits = value.bytes().take_while(u8::is_ascii_digit).count();
    digits > 0 && value.as_bytes().get(digits) == Some(&b'-')
}

/// A point in time during relative-date evaluation.
#[derive(Clone, Copy)]
enum Moment {
    /// A calendar date (`today`).
    Date(Date),
    /// A wall-clock time in the user's timezone (`now`), converted to UTC at the end.
    Local(PrimitiveDateTime),
    Naive(PrimitiveDateTime),
}

impl Moment {
    fn weekday(self) -> Weekday {
        match self {
            Moment::Date(d) => d.weekday(),
            Moment::Local(dt) | Moment::Naive(dt) => dt.weekday(),
        }
    }

    fn add_days(self, days: i64) -> Result<Moment, ErrorReason> {
        let delta = Duration::days(days);
        Ok(match self {
            Moment::Date(d) => Moment::Date(d.checked_add(delta).ok_or(ErrorReason::DateOverflow)?),
            Moment::Local(dt) => {
                Moment::Local(dt.checked_add(delta).ok_or(ErrorReason::DateOverflow)?)
            }
            Moment::Naive(dt) => {
                Moment::Naive(dt.checked_add(delta).ok_or(ErrorReason::DateOverflow)?)
            }
        })
    }

    fn map_datetime(self, f: impl FnOnce(PrimitiveDateTime) -> PrimitiveDateTime) -> Moment {
        match self {
            Moment::Date(d) => Moment::Date(d),
            Moment::Local(dt) => Moment::Local(f(dt)),
            Moment::Naive(dt) => Moment::Naive(f(dt)),
        }
    }
}

const WEEKDAYS: [(&str, u8); 7] = [
    ("monday", 0),
    ("tuesday", 1),
    ("wednesday", 2),
    ("thursday", 3),
    ("friday", 4),
    ("saturday", 5),
    ("sunday", 6),
];

fn truncate(dt: PrimitiveDateTime, unit: char) -> PrimitiveDateTime {
    let time = dt.time();
    let truncated = match unit {
        'd' | 'm' | 'y' | 'w' => Time::MIDNIGHT,
        'H' => Time::from_hms(time.hour(), 0, 0).unwrap_or(time),
        'M' => Time::from_hms(time.hour(), time.minute(), 0).unwrap_or(time),
        _ => Time::from_hms(time.hour(), time.minute(), time.second()).unwrap_or(time),
    };
    PrimitiveDateTime::new(dt.date(), truncated)
}

fn apply_term(
    moment: Moment,
    term: &str,
    value: &str,
    week_start: Weekday,
) -> Result<Moment, ErrorReason> {
    let invalid = || ErrorReason::InvalidDateTerm {
        term: clip(py_str_repr(term)),
        value: clip(py_str_repr(value)),
    };
    let mut chars = term.chars();
    let operator = chars.next().ok_or_else(invalid)?;
    if !matches!(operator, '+' | '-' | '=') || term.chars().count() < 3 {
        return Err(invalid());
    }
    let rest = chars.as_str();

    // Weekday terms.
    let weekday = if rest == "week_start" {
        Some(week_start.number_days_from_monday())
    } else {
        WEEKDAYS
            .iter()
            .find(|(name, _)| *name == rest)
            .map(|(_, n)| *n)
    };
    if let Some(weekday) = weekday {
        let start = i64::from(week_start.number_days_from_monday());
        let current = i64::from(moment.weekday().number_days_from_monday());
        let mut offset =
            (i64::from(weekday) - start).rem_euclid(7) - (current - start).rem_euclid(7);
        let mut moment = moment;
        match operator {
            '+' if offset < 0 => offset += 7,
            '-' if offset > 0 => offset -= 7,
            '=' => moment = moment.map_datetime(|dt| truncate(dt, 'd')),
            _ => {}
        }
        return moment.add_days(offset);
    }

    // Operations on dates: [+-]N<unit> adds, =N<unit> sets.
    let unit = term.chars().last().ok_or_else(invalid)?;
    if !matches!(unit, 'd' | 'm' | 'y' | 'w' | 'H' | 'M' | 'S') {
        return Err(invalid());
    }
    let number_text = &term[..term.len() - unit.len_utf8()];
    if operator == '=' {
        let number = parse_py_int(&number_text[1..]).ok_or_else(invalid)?;
        let moment = moment.map_datetime(|dt| truncate(dt, unit));
        set_unit(moment, unit, number).ok_or_else(invalid)
    } else {
        let number = parse_py_int(number_text).ok_or_else(invalid)?;
        add_unit(moment, unit, number, &invalid)
    }
}

/// Python `int()` on ASCII text: optional sign, digits, single underscores between digits.
/// Returns `None` for invalid text; values beyond `i64` saturate (they overflow later anyway).
fn parse_py_int(text: &str) -> Option<i128> {
    let (negative, digits) = match text.as_bytes().first() {
        Some(b'+') => (false, &text[1..]),
        Some(b'-') => (true, &text[1..]),
        _ => (false, text),
    };
    if digits.is_empty()
        || digits.starts_with('_')
        || digits.ends_with('_')
        || digits.contains("__")
    {
        return None;
    }
    let mut number: i128 = 0;
    for b in digits.bytes() {
        match b {
            b'0'..=b'9' => {
                number = number
                    .saturating_mul(10)
                    .saturating_add(i128::from(b - b'0'));
            }
            b'_' => {}
            _ => return None,
        }
    }
    Some(if negative { -number } else { number })
}

/// `dt += relativedelta(<unit>s=number)`.
fn add_unit(
    moment: Moment,
    unit: char,
    number: i128,
    invalid: &dyn Fn() -> ErrorReason,
) -> Result<Moment, ErrorReason> {
    match unit {
        'm' | 'y' => {
            let months = if unit == 'y' {
                number.saturating_mul(12)
            } else {
                number
            };
            add_months(moment, months).ok_or_else(invalid)
        }
        _ => {
            // relativedelta normalizes hours/minutes/seconds into days first; whatever remains
            // below a day is a time component, which turns a date into a datetime.
            let seconds_per_unit: i128 = match unit {
                'd' => 86_400,
                'w' => 7 * 86_400,
                'H' => 3_600,
                'M' => 60,
                _ => 1,
            };
            let total = number.saturating_mul(seconds_per_unit);
            let has_time = total % 86_400 != 0;
            let total = i64::try_from(total).map_err(|_| ErrorReason::DateOverflow)?;
            let delta = Duration::seconds(total);
            let moment = match moment {
                Moment::Date(d) if has_time => {
                    Moment::Naive(PrimitiveDateTime::new(d, Time::MIDNIGHT))
                }
                other => other,
            };
            Ok(match moment {
                Moment::Date(d) => {
                    Moment::Date(d.checked_add(delta).ok_or(ErrorReason::DateOverflow)?)
                }
                Moment::Local(dt) => {
                    Moment::Local(dt.checked_add(delta).ok_or(ErrorReason::DateOverflow)?)
                }
                Moment::Naive(dt) => {
                    Moment::Naive(dt.checked_add(delta).ok_or(ErrorReason::DateOverflow)?)
                }
            })
        }
    }
}

/// Month arithmetic of `relativedelta(months=n)`: the day is clamped to the target month.
fn add_months(moment: Moment, months: i128) -> Option<Moment> {
    let date = match moment {
        Moment::Date(d) => d,
        Moment::Local(dt) | Moment::Naive(dt) => dt.date(),
    };
    let index = i128::from(date.year()) * 12 + i128::from(u8::from(date.month())) - 1 + months;
    let year = i32::try_from(index.div_euclid(12)).ok()?;
    let month = u8::try_from(index.rem_euclid(12) + 1).ok()?;
    let new_date = clamped_date(year, month, date.day())?;
    Some(replace_date(moment, new_date))
}

fn clamped_date(year: i32, month: u8, day: u8) -> Option<Date> {
    if !(1..=9999).contains(&year) {
        return None;
    }
    let month = Month::try_from(month).ok()?;
    let day = day.min(month.length(year));
    Date::from_calendar_date(year, month, day).ok()
}

fn replace_date(moment: Moment, date: Date) -> Moment {
    match moment {
        Moment::Date(_) => Moment::Date(date),
        Moment::Local(dt) => Moment::Local(PrimitiveDateTime::new(date, dt.time())),
        Moment::Naive(dt) => Moment::Naive(PrimitiveDateTime::new(date, dt.time())),
    }
}

/// `dt += relativedelta(<unit>=number)` (absolute value). `None` when invalid, including weeks
/// (`=Nw` is not supported). Zero day/month/year means "unchanged", as in `dateutil`.
fn set_unit(moment: Moment, unit: char, number: i128) -> Option<Moment> {
    match unit {
        'd' | 'm' | 'y' => {
            let date = match moment {
                Moment::Date(d) => d,
                Moment::Local(dt) | Moment::Naive(dt) => dt.date(),
            };
            let mut year = date.year();
            let mut month = u8::from(date.month());
            let mut day = date.day();
            if number != 0 {
                match unit {
                    'y' => year = i32::try_from(number).ok()?,
                    'm' => month = u8::try_from(number).ok().filter(|m| (1..=12).contains(m))?,
                    _ => {
                        // dateutil: day = min(days in month, N); a negative N is rejected later.
                        let n = u8::try_from(number.clamp(-1, 255)).ok()?;
                        day = n;
                    }
                }
            }
            let month_enum = Month::try_from(month).ok()?;
            if !(1..=9999).contains(&year) {
                return None;
            }
            let day = day.min(month_enum.length(year));
            Some(replace_date(
                moment,
                Date::from_calendar_date(year, month_enum, day).ok()?,
            ))
        }
        'H' | 'M' | 'S' => {
            let n = u8::try_from(number).ok()?;
            let dt = match moment {
                Moment::Date(d) => PrimitiveDateTime::new(d, Time::MIDNIGHT),
                Moment::Local(dt) | Moment::Naive(dt) => dt,
            };
            let time = dt.time();
            let new_time = match unit {
                'H' => Time::from_hms_micro(n, time.minute(), time.second(), time.microsecond()),
                'M' => Time::from_hms_micro(time.hour(), n, time.second(), time.microsecond()),
                _ => Time::from_hms_micro(time.hour(), time.minute(), n, time.microsecond()),
            }
            .ok()?;
            let new_dt = PrimitiveDateTime::new(dt.date(), new_time);
            Some(match moment {
                Moment::Local(_) => Moment::Local(new_dt),
                Moment::Date(_) | Moment::Naive(_) => Moment::Naive(new_dt),
            })
        }
        _ => None,
    }
}

// ----- ISO 8601 (Python `date.fromisoformat` / `datetime.fromisoformat`) ----------------------

pub(crate) fn parse_iso(value: &str) -> Result<Value, ErrorReason> {
    let invalid = || ErrorReason::InvalidIsoFormat(clip(py_str_repr(value)));
    if !value.is_ascii() {
        return Err(invalid());
    }
    if value.len() <= 10 {
        let (date, rest) = parse_iso_date_part(value).ok_or_else(invalid)?;
        if !rest.is_empty() {
            return Err(invalid());
        }
        return Ok(Value::Date(date?));
    }
    let (date, rest) = parse_iso_date_part(value).ok_or_else(invalid)?;
    let date = date?;
    if rest.is_empty() {
        return Ok(Value::Datetime(PrimitiveDateTime::new(
            date,
            Time::MIDNIGHT,
        )));
    }
    // Any single separator character.
    let time_text = &rest[1..];
    let (time, tz) = parse_iso_time(time_text).ok_or_else(invalid)?;
    let time = time?;
    if tz {
        return Err(ErrorReason::TimezoneNotAllowed(clip(py_str_repr(value))));
    }
    Ok(Value::Datetime(PrimitiveDateTime::new(date, time)))
}

type Parsed<T> = Result<T, ErrorReason>;

fn digits(text: &str, n: usize) -> Option<u32> {
    let part = text.get(..n)?;
    if !part.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    part.parse().ok()
}

/// `YYYY-MM-DD` or `YYYYMMDD`; returns the date (or a range error) and the remaining text.
fn parse_iso_date_part(text: &str) -> Option<(Parsed<Date>, &str)> {
    let year = digits(text, 4)?;
    let (month, day, rest) = if text.as_bytes().get(4) == Some(&b'-') {
        if text.as_bytes().get(7) != Some(&b'-') {
            return None;
        }
        (
            digits(&text[5..], 2)?,
            digits(text.get(8..)?, 2)?,
            text.get(10..)?,
        )
    } else {
        (
            digits(&text[4..], 2)?,
            digits(text.get(6..)?, 2)?,
            text.get(8..)?,
        )
    };
    Some((make_date(year, month, day), rest))
}

fn make_date(year: u32, month: u32, day: u32) -> Parsed<Date> {
    if year == 0 {
        return Err(ErrorReason::DateOutOfRange("year 0 is out of range"));
    }
    let month = u8::try_from(month)
        .ok()
        .and_then(|m| Month::try_from(m).ok())
        .ok_or(ErrorReason::DateOutOfRange("month must be in 1..12"))?;
    let year =
        i32::try_from(year).map_err(|_| ErrorReason::DateOutOfRange("year is out of range"))?;
    u8::try_from(day)
        .ok()
        .and_then(|d| Date::from_calendar_date(year, month, d).ok())
        .ok_or(ErrorReason::DateOutOfRange("day is out of range for month"))
}

/// `HH[:MM[:SS[.ffffff]]]` (or the basic forms without colons), with an optional timezone suffix.
/// Returns the time (or a range error) and whether a timezone was present.
fn parse_iso_time(text: &str) -> Option<(Parsed<Time>, bool)> {
    let tz_pos = text.find(['+', '-', 'Z']);
    let (time_text, tz_text) = match tz_pos {
        Some(pos) => (&text[..pos], Some(&text[pos..])),
        None => (text, None),
    };
    if tz_text.is_some_and(|tz| !valid_tz(tz)) {
        return None;
    }
    let (main, fraction) = match time_text.find(['.', ',']) {
        Some(pos) => (&time_text[..pos], Some(&time_text[pos + 1..])),
        None => (time_text, None),
    };
    let parts: Vec<u32> = if main.contains(':') {
        main.split(':')
            .map(|p| if p.len() == 2 { digits(p, 2) } else { None })
            .collect::<Option<Vec<u32>>>()?
    } else {
        if main.len() % 2 != 0 || main.is_empty() {
            return None;
        }
        (0..main.len() / 2)
            .map(|i| digits(&main[i * 2..], 2))
            .collect::<Option<Vec<u32>>>()?
    };
    if parts.is_empty() || parts.len() > 3 {
        return None;
    }
    if fraction.is_some() && parts.len() != 3 {
        return None;
    }
    let micro = match fraction {
        Some(f) => {
            if f.is_empty() || !f.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            let mut padded: String = f.chars().take(6).collect();
            while padded.len() < 6 {
                padded.push('0');
            }
            padded.parse::<u32>().ok()?
        }
        None => 0,
    };
    let hour = parts.first().copied().unwrap_or(0);
    let minute = parts.get(1).copied().unwrap_or(0);
    let second = parts.get(2).copied().unwrap_or(0);
    Some((make_time(hour, minute, second, micro), tz_text.is_some()))
}

fn make_time(hour: u32, minute: u32, second: u32, micro: u32) -> Parsed<Time> {
    let component = |value: u32, max: u8, message: &'static str| {
        u8::try_from(value)
            .ok()
            .filter(|v| *v <= max)
            .ok_or(ErrorReason::DateOutOfRange(message))
    };
    let h = component(hour, 23, "hour must be in 0..23")?;
    let m = component(minute, 59, "minute must be in 0..59")?;
    let s = component(second, 59, "second must be in 0..59")?;
    Time::from_hms_micro(h, m, s, micro)
        .map_err(|_| ErrorReason::DateOutOfRange("microsecond must be in 0..999999"))
}

fn valid_tz(tz: &str) -> bool {
    if tz == "Z" {
        return true;
    }
    let body = &tz[1..];
    let body = body.replace(':', "");
    !body.is_empty()
        && body.len().is_multiple_of(2)
        && body.len() <= 6
        && body.bytes().all(|b| b.is_ascii_digit())
}

// ----- formatting -----------------------------------------------------------------------------

/// `YYYY-MM-DD` (Python `str(date)`).
pub(crate) fn format_date(d: Date) -> String {
    format!("{:04}-{:02}-{:02}", d.year(), u8::from(d.month()), d.day())
}

pub(crate) fn format_datetime_server(dt: PrimitiveDateTime) -> String {
    format!(
        "{} {:02}:{:02}:{:02}",
        format_date(dt.date()),
        dt.hour(),
        dt.minute(),
        dt.second()
    )
}

/// Python `str(datetime)`: seconds always, microseconds when non-zero.
pub(crate) fn format_datetime_str(dt: PrimitiveDateTime) -> String {
    let mut out = format_datetime_server(dt);
    if dt.microsecond() != 0 {
        out.push_str(&format!(".{:06}", dt.microsecond()));
    }
    out
}

/// Python `repr(datetime)`: `datetime.datetime(2024, 1, 5, 12, 0)`, seconds and microseconds only
/// when non-zero.
pub(crate) fn datetime_repr(dt: PrimitiveDateTime) -> String {
    let mut out = format!(
        "datetime.datetime({}, {}, {}, {}, {}",
        dt.year(),
        u8::from(dt.month()),
        dt.day(),
        dt.hour(),
        dt.minute()
    );
    if dt.second() != 0 || dt.microsecond() != 0 {
        out.push_str(&format!(", {}", dt.second()));
    }
    if dt.microsecond() != 0 {
        out.push_str(&format!(", {}", dt.microsecond()));
    }
    out.push(')');
    out
}
