//! The core never reads the system clock (L1, wasm32-wasip2, deterministic tests): `now()`,
//! `today()`, `time.time()` and `time.strftime()` use the clock the host injects with
//! [`crate::EvalContext::with_clock`], and fail loudly without one.

use std::fmt;

use time::PrimitiveDateTime;

use crate::args::{Sig, int_arg, optional, required, str_arg};
use crate::context::Clock;
use crate::datetime::{self, RelativeDelta, TimeDelta, Weekday, make_date, make_time};
use crate::error::{ExprError, ExprResult};
use crate::limits::Budget;
use crate::value::{ExprCallable, ExprObject, ExprValue, Kwargs};

use super::builtin;

fn now(clock: &Option<Clock>, what: &str) -> ExprResult<PrimitiveDateTime> {
    match clock {
        Some(c) => Ok(c()),
        None => Err(ExprError::value_error(format!(
            "{what} needs a clock: the host must provide one with EvalContext::with_clock"
        ))),
    }
}

/// The `time` module (only `time()` and `strftime()`).
pub(crate) struct TimeModule {
    pub clock: Option<Clock>,
}

impl fmt::Debug for TimeModule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<module 'time'>")
    }
}

impl ExprObject for TimeModule {
    fn type_name(&self) -> &str {
        "module"
    }

    fn repr(&self) -> String {
        "<module 'time' (built-in)>".to_string()
    }

    fn get_attr(&self, name: &str, _budget: &mut Budget) -> ExprResult<ExprValue> {
        match name {
            "time" | "strftime" => Ok(ExprValue::Callable(std::sync::Arc::new(TimeFunction {
                name: if name == "time" { "time" } else { "strftime" },
                clock: self.clock.clone(),
            }))),
            _ => Err(ExprError::module_attribute_error("time", name)),
        }
    }

    fn self_charging(&self) -> bool {
        true
    }
}

struct TimeFunction {
    name: &'static str,
    clock: Option<Clock>,
}

impl fmt::Debug for TimeFunction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<built-in function {}>", self.name)
    }
}

impl ExprCallable for TimeFunction {
    fn name(&self) -> &str {
        self.name
    }

    fn call(
        &self,
        args: &[ExprValue],
        kwargs: &Kwargs,
        budget: &mut Budget,
    ) -> ExprResult<ExprValue> {
        if self.name == "time" {
            Sig {
                name: "time",
                params: &[],
                required: 0,
                pos_only: 0,
                max_pos: 0,
            }
            .bind(args, kwargs)?;
            let t = now(&self.clock, "time.time()")?.assume_utc();
            let secs = t.unix_timestamp() as f64 + f64::from(t.microsecond()) / 1e6;
            return Ok(ExprValue::Float(secs));
        }
        let sig = Sig {
            name: "strftime",
            params: &["format"],
            required: 1,
            pos_only: 1,
            max_pos: 1,
        };
        let slots = sig.bind(args, kwargs)?;
        let fmt = str_arg("strftime", "format", required(&sig, &slots, 0)?)?;
        let t = now(&self.clock, "time.strftime()")?;
        Ok(ExprValue::Str(datetime::strftime(
            t.date(),
            t.time(),
            fmt,
            budget,
        )?))
    }

    fn self_charging(&self) -> bool {
        true
    }
}

pub(crate) struct DateTimeClass {
    pub clock: Option<Clock>,
}

impl fmt::Debug for DateTimeClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<class 'datetime.datetime'>")
    }
}

impl ExprObject for DateTimeClass {
    fn type_name(&self) -> &str {
        "type"
    }

    fn repr(&self) -> String {
        "<class 'datetime.datetime'>".to_string()
    }

    fn get_attr(&self, name: &str, _budget: &mut Budget) -> ExprResult<ExprValue> {
        match name {
            "now" | "today" => Ok(ExprValue::Callable(std::sync::Arc::new(Now {
                name: if name == "now" { "now" } else { "today" },
                clock: self.clock.clone(),
            }))),
            _ => Err(ExprError::type_attribute_error("datetime.datetime", name)),
        }
    }

    fn call(
        &self,
        args: &[ExprValue],
        kwargs: &Kwargs,
        _budget: &mut Budget,
    ) -> ExprResult<ExprValue> {
        let sig = Sig {
            name: "datetime",
            params: &[
                "year",
                "month",
                "day",
                "hour",
                "minute",
                "second",
                "microsecond",
            ],
            required: 3,
            pos_only: 0,
            max_pos: 7,
        };
        let slots = sig.bind(args, kwargs)?;
        let field = |i: usize| -> ExprResult<i64> {
            match optional(&slots, i) {
                None => Ok(0),
                Some(v) => int_arg("datetime", sig.params.get(i).copied().unwrap_or("?"), v),
            }
        };
        let date = make_date(
            int_arg("datetime", "year", required(&sig, &slots, 0)?)?,
            int_arg("datetime", "month", required(&sig, &slots, 1)?)?,
            int_arg("datetime", "day", required(&sig, &slots, 2)?)?,
        )?;
        let time = make_time(field(3)?, field(4)?, field(5)?, field(6)?)?;
        Ok(ExprValue::DateTime(PrimitiveDateTime::new(date, time)))
    }

    fn self_charging(&self) -> bool {
        true
    }
}

struct Now {
    name: &'static str,
    clock: Option<Clock>,
}

impl fmt::Debug for Now {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<built-in method {}>", self.name)
    }
}

impl ExprCallable for Now {
    fn name(&self) -> &str {
        self.name
    }

    fn call(
        &self,
        args: &[ExprValue],
        kwargs: &Kwargs,
        _budget: &mut Budget,
    ) -> ExprResult<ExprValue> {
        if self.name == "now" {
            let sig = Sig {
                name: "now",
                params: &["tz"],
                required: 0,
                pos_only: 0,
                max_pos: 1,
            };
            let slots = sig.bind(args, kwargs)?;
            if !matches!(optional(&slots, 0), None | Some(ExprValue::None)) {
                return Err(ExprError::value_error(
                    "timezone-aware datetimes are not supported in expressions (naive UTC only)",
                ));
            }
        } else {
            Sig {
                name: "today",
                params: &[],
                required: 0,
                pos_only: 0,
                max_pos: 0,
            }
            .bind(args, kwargs)?;
        }
        Ok(ExprValue::DateTime(now(&self.clock, "datetime.now()")?))
    }

    fn self_charging(&self) -> bool {
        true
    }
}

/// A number argument of `timedelta`/`relativedelta` in microseconds of `unit`.
fn micros_of(fname: &str, param: &str, v: &ExprValue, unit: i128) -> ExprResult<i128> {
    match v {
        ExprValue::Int(_) | ExprValue::Bool(_) => {
            let i = int_arg(fname, param, v)?;
            i128::from(i)
                .checked_mul(unit)
                .ok_or_else(datetime::date_range_error)
        }
        ExprValue::Float(f) => datetime::unit_micros(*f, unit),
        other => Err(ExprError::type_error(format!(
            "unsupported type for {fname} {param} component: {}",
            other.type_name()
        ))),
    }
}

const MICRO: i128 = 1;
const MILLI: i128 = 1_000;
const SECOND: i128 = 1_000_000;
const MINUTE: i128 = 60 * SECOND;
const HOUR: i128 = 60 * MINUTE;
const DAY: i128 = 24 * HOUR;
const WEEK: i128 = 7 * DAY;

fn timedelta(args: &[ExprValue], kwargs: &Kwargs, _budget: &mut Budget) -> ExprResult<ExprValue> {
    let sig = Sig {
        name: "timedelta",
        params: &[
            "days",
            "seconds",
            "microseconds",
            "milliseconds",
            "minutes",
            "hours",
            "weeks",
        ],
        required: 0,
        pos_only: 0,
        max_pos: 7,
    };
    let units = [DAY, SECOND, MICRO, MILLI, MINUTE, HOUR, WEEK];
    let slots = sig.bind(args, kwargs)?;
    let mut total: i128 = 0;
    for (i, (param, unit)) in sig.params.iter().zip(units).enumerate() {
        if let Some(v) = optional(&slots, i) {
            let part = micros_of("timedelta", param, v, unit)?;
            total = total
                .checked_add(part)
                .ok_or_else(datetime::date_range_error)?;
        }
    }
    Ok(ExprValue::TimeDelta(TimeDelta::from_micros(total)?))
}

fn relativedelta(
    args: &[ExprValue],
    kwargs: &Kwargs,
    _budget: &mut Budget,
) -> ExprResult<ExprValue> {
    if !args.is_empty() {
        return Err(ExprError::type_error(
            "relativedelta(dt1, dt2) is not supported in expressions; pass keyword arguments",
        ));
    }
    let mut rd = RelativeDelta::default();
    let mut float_micros: i128 = 0;
    for (key, v) in kwargs {
        let int = |v: &ExprValue| -> ExprResult<i64> {
            match v {
                ExprValue::Float(f) => crate::num::float_as_int(*f).ok_or_else(|| {
                    ExprError::value_error(format!(
                        "relativedelta argument '{key}' must be an integer, got {}",
                        v.repr()
                    ))
                }),
                other => int_arg("relativedelta", key, other),
            }
        };
        match key.as_str() {
            "years" | "months" => {
                let x = match v {
                    ExprValue::Float(f) if crate::num::float_as_int(*f).is_none() => {
                        return Err(ExprError::value_error(
                            "Non-integer years and months are ambiguous and not currently supported.",
                        ));
                    }
                    other => int(other)?,
                };
                if key == "years" {
                    rd.years = x;
                } else {
                    rd.months = x;
                }
            }
            "leapdays" => rd.leapdays = int(v)?,
            "weeks" | "days" | "hours" | "minutes" | "seconds" | "microseconds" => {
                let unit = match key.as_str() {
                    "weeks" => WEEK,
                    "days" => DAY,
                    "hours" => HOUR,
                    "minutes" => MINUTE,
                    "seconds" => SECOND,
                    _ => MICRO,
                };
                if let ExprValue::Float(f) = v
                    && crate::num::float_as_int(*f).is_none()
                {
                    // dateutil keeps fractional values; adding them is the same as adding the
                    // equivalent microseconds, which is how they are stored here.
                    float_micros = float_micros
                        .checked_add(datetime::unit_micros(*f, unit)?)
                        .ok_or_else(datetime::date_range_error)?;
                    continue;
                }
                let x = int(v)?;
                let add =
                    |slot: i64, x: i64| slot.checked_add(x).ok_or_else(datetime::date_range_error);
                match key.as_str() {
                    "weeks" => {
                        let days = x.checked_mul(7).ok_or_else(datetime::date_range_error)?;
                        rd.days = add(rd.days, days)?;
                    }
                    "days" => rd.days = add(rd.days, x)?,
                    "hours" => rd.hours = x,
                    "minutes" => rd.minutes = x,
                    "seconds" => rd.seconds = x,
                    _ => rd.microseconds = x,
                }
            }
            "year" => rd.year = Some(int(v)?),
            "month" => {
                let m = int(v)?;
                if !(1..=12).contains(&m) {
                    return Err(ExprError::value_error("month must be in 1..12"));
                }
                rd.month = Some(m);
            }
            "day" => {
                let d = int(v)?;
                if !(1..=31).contains(&d) {
                    return Err(ExprError::value_error("day must be in 1..31"));
                }
                rd.day = Some(d);
            }
            "hour" => rd.hour = Some(int(v)?),
            "minute" => rd.minute = Some(int(v)?),
            "second" => rd.second = Some(int(v)?),
            "microsecond" => rd.microsecond = Some(int(v)?),
            "weekday" => {
                let w = int(v)?;
                let day = match w {
                    0..=6 => w,
                    -7..=-1 => w + 7,
                    _ => return Err(ExprError::index_error("tuple index out of range")),
                };
                rd.weekday = Some(Weekday {
                    day: u8::try_from(day)
                        .map_err(|_| ExprError::index_error("tuple index out of range"))?,
                    n: None,
                });
            }
            "yearday" | "nlyearday" => {
                return Err(ExprError::value_error(format!(
                    "relativedelta({key}=...) is not supported in expressions"
                )));
            }
            other => {
                return Err(ExprError::type_error(format!(
                    "relativedelta.__init__() got an unexpected keyword argument '{other}'"
                )));
            }
        }
    }
    if float_micros != 0 {
        rd.microseconds = rd
            .microseconds
            .checked_add(i64::try_from(float_micros).map_err(|_| datetime::date_range_error())?)
            .ok_or_else(datetime::date_range_error)?;
    }
    rd.normalize()?;
    Ok(ExprValue::RelativeDelta(Box::new(rd)))
}

pub(crate) fn date_names(clock: Option<Clock>) -> Vec<(&'static str, ExprValue)> {
    let dt = || {
        ExprValue::Object(std::sync::Arc::new(DateTimeClass {
            clock: clock.clone(),
        }))
    };
    vec![
        (
            "time",
            ExprValue::Object(std::sync::Arc::new(TimeModule {
                clock: clock.clone(),
            })),
        ),
        ("datetime", dt()),
        ("DateTime", dt()),
        ("timedelta", builtin("timedelta", timedelta)),
        ("relativedelta", builtin("relativedelta", relativedelta)),
    ]
}
