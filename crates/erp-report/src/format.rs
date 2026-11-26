// SPDX-License-Identifier: MIT OR Apache-2.0
//! Number and date formatting by a language's settings (the `res.lang` row of the document's
//! language, read by the data stage). Calendar names and duration labels use bundled locale data.
use crate::{
    ReportError,
    error::invalid,
    locale::{self, LocaleText},
};

/// How one language writes numbers, dates and times.
#[derive(Debug, Clone)]
pub struct LangFormat {
    decimal_point: String,
    thousands_sep: String,
    grouping: Vec<i8>,
    date_format: String,
    time_format: String,
    language: String,
    locale: Option<std::sync::Arc<LocaleText>>,
}
impl Default for LangFormat {
    /// The settings of `en_US`.
    fn default() -> Self {
        Self {
            decimal_point: ".".into(),
            thousands_sep: ",".into(),
            grouping: vec![3, 0],
            date_format: "%m/%d/%Y".into(),
            time_format: "%H:%M:%S".into(),
            language: "en_US".into(),
            locale: None,
        }
    }
}
impl LangFormat {
    /// `grouping` lists digit group sizes from the right: `0` repeats the size before it, `-1` ends
    /// grouping, and a list that ends without either groups no further. The two formats use the
    /// directives `%d %m %Y %y %H %M %S %I %p %j %a %A %b %B %%`.
    pub fn new(
        decimal_point: &str,
        thousands_sep: &str,
        grouping: &[i8],
        date_format: &str,
        time_format: &str,
    ) -> Result<Self, ReportError> {
        if decimal_point.is_empty() || decimal_point.len() > 8 || thousands_sep.len() > 8 {
            return Err(invalid(
                "decimal point must be 1..=8 bytes and the thousands separator at most 8",
            ));
        }
        if grouping.len() > 8 || grouping.iter().any(|n| !(-1..=9).contains(n)) {
            return Err(invalid("digit grouping takes at most 8 sizes, each -1..=9"));
        }
        for pattern in [date_format, time_format] {
            if pattern.is_empty() || pattern.len() > 64 {
                return Err(invalid("date and time formats must be 1..=64 bytes"));
            }
            let mut chars = pattern.chars();
            while let Some(c) = chars.next() {
                if c != '%' {
                    continue;
                }
                match chars.next() {
                    Some(
                        'd' | 'm' | 'Y' | 'y' | 'H' | 'M' | 'S' | 'I' | 'p' | 'j' | '%' | 'a' | 'A'
                        | 'b' | 'B',
                    ) => {}
                    _ => return Err(invalid("date format directive is not supported")),
                }
            }
        }
        Ok(Self {
            decimal_point: decimal_point.into(),
            thousands_sep: thousands_sep.into(),
            grouping: grouping.to_vec(),
            date_format: date_format.into(),
            time_format: time_format.into(),
            language: "en_US".into(),
            locale: None,
        })
    }
    /// Select textual calendar names and duration labels independently of numeric patterns.
    /// Built-ins: en/en_US, fr/fr_FR, id/id_ID. An unprovided locale is refused only when
    /// textual names or duration labels are requested; numeric formats remain usable.
    pub fn with_language(mut self, code: &str) -> Result<Self, ReportError> {
        crate::Language::new(code)?;
        self.language = code.into();
        self.locale = None;
        Ok(self)
    }
    /// Supply a custom locale resource in the documented report-locale JSON format.
    /// Fixed array sizes, seven duration units, widths and literal slots are validated;
    /// input is limited to 65536 bytes and each name/pattern to 256 bytes.
    pub fn with_locale_data(mut self, code: &str, json: &str) -> Result<Self, ReportError> {
        crate::Language::new(code)?;
        let locale = LocaleText::parse(json).map_err(invalid)?;
        self.language = code.into();
        self.locale = Some(std::sync::Arc::new(locale));
        Ok(self)
    }
    pub(crate) fn locale(&self) -> Result<&LocaleText, String> {
        if let Some(locale) = &self.locale {
            return Ok(locale);
        }
        locale::builtin(&self.language)
            .map_err(|reason| format!("locale `{}`: {reason}", self.language))
    }
    pub(crate) fn date_format(&self) -> &str {
        &self.date_format
    }
    pub(crate) fn time_format(&self) -> &str {
        &self.time_format
    }
    /// Group the digits of an integer part.
    fn group(&self, digits: &str) -> String {
        if self.thousands_sep.is_empty() {
            return digits.into();
        }
        let mut parts = Vec::new();
        let mut rest = digits;
        let mut sizes = self.grouping.iter().copied();
        let mut size = 0usize;
        let mut repeat = false;
        loop {
            if !repeat {
                match sizes.next() {
                    None | Some(-1) => break,
                    Some(0) => repeat = true,
                    Some(n) => size = usize::try_from(n).unwrap_or(0),
                }
            }
            if size == 0 || rest.len() <= size {
                break;
            }
            let (head, tail) = rest.split_at(rest.len() - size);
            parts.push(tail);
            rest = head;
        }
        parts.push(rest);
        parts.reverse();
        parts.join(&self.thousands_sep)
    }
    /// Join a split number: grouped integer digits, the decimal point, the decimals.
    pub(crate) fn number(&self, negative: bool, integer: &str, decimals: &str) -> String {
        let mut out = String::new();
        if negative {
            out.push('-');
        }
        out.push_str(&self.group(integer));
        if !decimals.is_empty() {
            out.push_str(&self.decimal_point);
            out.push_str(decimals);
        }
        out
    }
    /// An integer with digit grouping.
    pub(crate) fn integer(&self, value: i64) -> String {
        self.number(value < 0, &value.unsigned_abs().to_string(), "")
    }
}
/// Round `value` half away from zero at `digits` decimals and split it into sign, integer digits
/// and decimal digits. A value that is zero after rounding has no sign.
pub(crate) fn split(value: f64, digits: usize) -> Result<(bool, String, String), ReportError> {
    if !value.is_finite() || digits > 14 {
        return Err(invalid(
            "a number to format must be finite, with at most 14 decimals",
        ));
    }
    let scaled = value.abs() * 10f64.powi(i32::try_from(digits).unwrap_or(14));
    // The margin keeps 2.675 at two decimals from rounding down on its binary representation.
    let rounded = (scaled + scaled * f64::EPSILON + 0.5).floor();
    if rounded >= 1e30 {
        return Err(invalid("a number to format is too large"));
    }
    let all = format!("{rounded:.0}");
    let all = format!("{all:0>width$}", width = digits + 1);
    let (integer, decimals) = all.split_at(all.len() - digits);
    Ok((
        value < 0.0 && rounded != 0.0,
        integer.into(),
        decimals.into(),
    ))
}
/// Write a date and time by a `strftime`-style pattern whose directives [`LangFormat::new`] checked.
pub(crate) fn datetime(
    formats: &LangFormat,
    pattern: &str,
    (year, month, day): (i32, u8, u8),
    (hour, minute, second): (u8, u8, u8),
    ordinal: u16,
) -> Result<String, String> {
    let mut out = String::new();
    let mut chars = pattern.chars();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        let token = chars.next();
        let (token, unpadded) = if token == Some('_') {
            (chars.next(), true)
        } else {
            (token, false)
        };
        let part = match token {
            Some('d') => format!("{day:02}"),
            Some('m') => format!("{month:02}"),
            Some('Y') => format!("{year:04}"),
            Some('y') => format!("{:02}", year.rem_euclid(100)),
            Some('H') => format!("{hour:02}"),
            Some('M') => format!("{minute:02}"),
            Some('S') => format!("{second:02}"),
            Some('I') => format!("{:02}", (hour + 11) % 12 + 1),
            Some('p') => formats.locale()?.periods[usize::from(hour >= 12)].clone(),
            Some(kind @ ('a' | 'A' | 'b' | 'B')) => {
                let locale = formats.locale()?;
                let month_value =
                    time::Month::try_from(month).map_err(|_| "invalid calendar month")?;
                let date = time::Date::from_calendar_date(year, month_value, day)
                    .map_err(|_| "invalid calendar date")?;
                match kind {
                    'a' => locale.days_short[usize::from(date.weekday().number_days_from_sunday())]
                        .clone(),
                    'A' => locale.days_long[usize::from(date.weekday().number_days_from_sunday())]
                        .clone(),
                    'b' => locale.months_short[usize::from(month - 1)].clone(),
                    _ => locale.months_long[usize::from(month - 1)].clone(),
                }
            }
            Some('j') => format!("{ordinal:03}"),
            Some('%') => String::from('%'),
            _ => return Err("date format directive is not supported".into()),
        };
        if unpadded {
            let digits = part.trim_start_matches('0');
            out.push_str(if digits.is_empty() { "0" } else { digits });
        } else {
            out.push_str(&part);
        }
    }
    Ok(out)
}

/// Widget overrides use a bounded LDML subset or the language's percent-style dialect.
/// Unsupported alphabetic tokens fail rather than printing a plausible but wrong date.
pub(crate) fn widget_pattern(input: &str) -> Result<String, &'static str> {
    if input.is_empty() || input.len() > 64 {
        return Err("date format must be 1..=64 bytes");
    }
    if input.contains('%') {
        LangFormat::new(".", "", &[], input, "%H:%M:%S")
            .map_err(|_| "unsupported percent directive in date format")?;
        return Ok(input.into());
    }
    let mut out = String::new();
    let mut chars = input.chars().peekable();
    let mut quoted = false;
    while let Some(c) = chars.next() {
        if c == '\'' {
            if chars.peek() == Some(&'\'') {
                chars.next();
                out.push('\'');
            } else {
                quoted = !quoted;
            }
            continue;
        }
        if quoted || !c.is_ascii_alphabetic() {
            out.push(c);
            continue;
        }
        let mut count = 1;
        while chars.peek() == Some(&c) {
            chars.next();
            count += 1;
        }
        let token = match (c, count) {
            ('y', 2) => "%y",
            ('y', 1) => "%_Y",
            ('y', 4) => "%Y",
            ('M', 1) => "%_m",
            ('M', 2) => "%m",
            ('M', 3) => "%b",
            ('M', 4) => "%B",
            ('d', 1) => "%_d",
            ('d', 2) => "%d",
            ('E', 1..=3) => "%a",
            ('E', 4) => "%A",
            ('H', 1) => "%_H",
            ('H', 2) => "%H",
            ('h', 1) => "%_I",
            ('h', 2) => "%I",
            ('m', 1) => "%_M",
            ('m', 2) => "%M",
            ('s', 1) => "%_S",
            ('s', 2) => "%S",
            ('a', 1) => "%p",
            _ => return Err("unsupported LDML token in date format"),
        };
        out.push_str(token);
    }
    if quoted {
        return Err("unclosed quote in date format");
    }
    Ok(out)
}
