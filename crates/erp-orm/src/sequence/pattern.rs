//! Sequence name patterns without a counter (H21 widening,.2).
//!
//! Three pure operations, no database access and no counter row:
//!
//! - [`SequencePattern::render`]: the document name for a counter value and a date, from a declared
//!   pattern (`INV/2026/00001`, `JRNL/15-16/03/0010`).
//! - [`resolve_period_key`]: the reset period a date falls in (`never`, `2026`, `2026-07`,
//!   `2025-26`, `2025-26-07`), the partition key of `account_sequence_chain` (2).
//!
//! The one known difference: only the
//! ASCII digits `0-9` are digits; Python's `\d` also matches other Unicode decimal digits (a name
//! written with Arabic-Indic digits), which we read as text.

use std::fmt;

use erp_fields::{Date, Error, Result};

/// Longest sequence name [`SequencePattern::infer_from_name`] reads, in bytes.
pub const MAX_NAME_BYTES: usize = 256;

/// Largest counter width ([`SequencePatternBuilder::padding`]) and longest digit run read as the
/// counter by inference; the same bound as `ir.sequence.padding` (`MAX_PADDING`).
pub const MAX_PATTERN_PADDING: usize = 64;

/// Most bytes of fixed text (prefix, separators and suffix together) in a built pattern
/// ([`SequencePatternBuilder::build`]), so a rendered name always stays below [`MAX_NAME_BYTES`]
/// and can be read back.
pub const MAX_PATTERN_TEXT_BYTES: usize = 128;

const NO_MATCH_MESSAGE: &str = "The sequence regex should at least contain the seq grouping keys. \
     For instance:\n^(?P<prefix1>.*?)(?P<seq>\\d*)(?P<suffix>\\D*?)$";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SequenceReset {
    /// One counter for all dates.
    Never,
    /// Restarts every calendar year.
    Year,
    /// Restarts every calendar month.
    Month,
    /// Restarts every fiscal year (`15-16`, `2015-2016`).
    YearRange,
    /// Restarts every month; the name carries the fiscal year and the month.
    YearRangeMonth,
}

impl SequenceReset {
    /// # Errors
    ///
    /// `Validation` naming the value for anything else.
    pub fn parse(text: &str) -> Result<Self> {
        match text {
            "never" => Ok(Self::Never),
            "year" => Ok(Self::Year),
            "month" => Ok(Self::Month),
            "year_range" => Ok(Self::YearRange),
            "year_range_month" => Ok(Self::YearRangeMonth),
            other => Err(Error::validation(format!(
                "unknown sequence reset {other:?}; expected never, year, month, year_range or \
                 year_range_month"
            ))),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Never => "never",
            Self::Year => "year",
            Self::Month => "month",
            Self::YearRange => "year_range",
            Self::YearRangeMonth => "year_range_month",
        }
    }

    /// # Errors
    ///
    /// As [`SequencePattern::infer_from_name`].
    pub fn infer_from_name(name: &str) -> Result<Self> {
        Ok(SequencePattern::infer_from_name(name)?.reset())
    }

    fn uses_fiscal_year(self) -> bool {
        matches!(self, Self::YearRange | Self::YearRangeMonth)
    }
}

impl fmt::Display for SequenceReset {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// How many digits of a year a name shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum YearDigits {
    /// `26`.
    Two,
    /// `2026`.
    Four,
}

impl YearDigits {
    /// 2 or 4.
    pub fn count(self) -> usize {
        match self {
            Self::Two => 2,
            Self::Four => 4,
        }
    }

    /// The year cut to this many digits, zero-filled (`2026` → `26`; `2005` → `05`).
    fn render(self, year: i32) -> String {
        match self {
            Self::Two => format!("{:02}", year.rem_euclid(100)),
            Self::Four => format!("{:04}", year.rem_euclid(10_000)),
        }
    }

    fn from_len(len: usize) -> Option<Self> {
        match len {
            2 => Some(Self::Two),
            4 => Some(Self::Four),
            _ => None,
        }
    }
}

/// Its start and end years are the years a `year_range` name shows: a calendar fiscal year gives
/// `(2026, 2026)`, an April–March year containing 2026-03-15 gives `(2025, 2026)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FiscalYear {
    date_from: Date,
    date_to: Date,
}

impl FiscalYear {
    /// The fiscal year from `date_from` to `date_to`, both included.
    ///
    /// # Errors
    ///
    /// `Validation` when `date_to` is before `date_from`, when either year is outside 1..=9999, or
    /// when the end year is neither the start year nor the next one (a range name has exactly two
    /// consecutive years).
    ///
    /// ```
    /// use erp_fields::Date;
    /// use erp_orm::sequence::FiscalYear;
    /// let d = |y, m, d| Date::from_ymd(y, m, d).unwrap();
    /// let fy = FiscalYear::new(d(2025, 4, 1), d(2026, 3, 31)).unwrap();
    /// assert_eq!((fy.start_year(), fy.end_year()), (2025, 2026));
    /// assert!(FiscalYear::new(d(2026, 1, 1), d(2027, 12, 31)).is_ok());
    /// assert!(FiscalYear::new(d(2026, 1, 1), d(2028, 1, 1)).is_err());
    /// ```
    pub fn new(date_from: Date, date_to: Date) -> Result<Self> {
        if date_to < date_from {
            return Err(Error::validation(format!(
                "the fiscal year ends ({date_to}) before it starts ({date_from})"
            )));
        }
        let (start, _, _) = date_from.ymd();
        let (end, _, _) = date_to.ymd();
        for (what, year) in [("start", start), ("end", end)] {
            if !(1..=9999).contains(&year) {
                return Err(Error::validation(format!(
                    "the fiscal year {what} year {year} is outside 1..=9999"
                )));
            }
        }
        if end != start && Some(end) != start.checked_add(1) {
            return Err(Error::validation(format!(
                "the fiscal year {date_from}..{date_to} spans {start} to {end}; a fiscal year ends \
                 in its start year or the next one"
            )));
        }
        Ok(Self { date_from, date_to })
    }

    /// First day.
    pub fn date_from(&self) -> Date {
        self.date_from
    }

    /// Last day.
    pub fn date_to(&self) -> Date {
        self.date_to
    }

    /// The year of the first day.
    pub fn start_year(&self) -> i32 {
        self.date_from.ymd().0
    }

    /// The year of the last day.
    pub fn end_year(&self) -> i32 {
        self.date_to.ymd().0
    }

    /// Whether `date` falls in this fiscal year.
    pub fn contains(&self, date: Date) -> bool {
        self.date_from <= date && date <= self.date_to
    }
}

/// The reset period a date falls in: `never`, `2026`, `2026-07`, `2025-26` or `2025-26-07`
/// (2), the partition key of a numbering chain. The fiscal forms always show four
/// digits of the start year and two of the end year, whatever the pattern renders.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PeriodKey(String);

impl PeriodKey {
    /// The key text.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The key text, owned.
    pub fn into_string(self) -> String {
        self.0
    }
}

impl fmt::Display for PeriodKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The fiscal-year resets need a fiscal year containing the date; the others ignore it.
fn fiscal_years(
    reset: SequenceReset,
    date: Date,
    fiscal_year: Option<&FiscalYear>,
) -> Result<Option<(i32, i32)>> {
    if !reset.uses_fiscal_year() {
        return Ok(None);
    }
    let Some(fy) = fiscal_year else {
        return Err(Error::programming(format!(
            "reset {reset} needs the company's fiscal year for {date}; pass Some(FiscalYear) \
             (the reference ERP: res.company.compute_fiscalyear_dates)"
        )));
    };
    if !fy.contains(date) {
        return Err(Error::programming(format!(
            "{date} is outside the fiscal year {}..{} passed for reset {reset}",
            fy.date_from, fy.date_to
        )));
    }
    Ok(Some((fy.start_year(), fy.end_year())))
}

fn date_parts(date: Date) -> Result<(i32, u32)> {
    let (year, month, _) = date.ymd();
    if !(1..=9999).contains(&year) {
        return Err(Error::validation(format!(
            "sequence dates must be in the years 1..=9999, got {date}"
        )));
    }
    Ok((year, month))
}

/// The period key of `date` under `reset` (2).
/// A fiscal year passed
/// for the other resets is not read.
///
/// # Errors
///
/// `Programming` when a fiscal reset gets no fiscal year or one that does not contain the date;
/// `Validation` for a date outside the years 1..=9999.
///
/// ```
/// use erp_fields::Date;
/// use erp_orm::sequence::{FiscalYear, SequenceReset, resolve_period_key};
/// let d = |y, m, d| Date::from_ymd(y, m, d).unwrap();
/// let fy = FiscalYear::new(d(2025, 4, 1), d(2026, 3, 31)).unwrap();
/// let key = resolve_period_key(SequenceReset::YearRangeMonth, d(2026, 3, 15), Some(&fy)).unwrap();
/// assert_eq!(key.as_str(), "2025-26-03");
/// assert!(resolve_period_key(SequenceReset::YearRange, d(2026, 3, 15), None).is_err());
/// ```
pub fn resolve_period_key(
    reset: SequenceReset,
    date: Date,
    fiscal_year: Option<&FiscalYear>,
) -> Result<PeriodKey> {
    let (year, month) = date_parts(date)?;
    let years = fiscal_years(reset, date, fiscal_year)?;
    let key = match (reset, years) {
        (SequenceReset::Never, _) => "never".to_owned(),
        (SequenceReset::Year, _) => format!("{year:04}"),
        (SequenceReset::Month, _) => format!("{year:04}-{month:02}"),
        (SequenceReset::YearRange, Some((start, end))) => {
            format!("{start:04}-{:02}", end.rem_euclid(100))
        }
        (SequenceReset::YearRangeMonth, Some((start, end))) => {
            format!("{start:04}-{:02}-{month:02}", end.rem_euclid(100))
        }
        (SequenceReset::YearRange | SequenceReset::YearRangeMonth, None) => {
            return Err(Error::programming(format!(
                "fiscal years missing for reset {reset}"
            )));
        }
    };
    Ok(PeriodKey(key))
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Layout {
    /// `{prefix1}{seq}{suffix}`.
    Never,
    /// `{prefix1}{year}{prefix2}{seq}{suffix}`.
    Year { digits: YearDigits, number: String },
    /// `{prefix1}{year}{prefix2}{month}{prefix3}{seq}{suffix}`.
    Month {
        digits: YearDigits,
        date: String,
        number: String,
    },
    /// `{prefix1}{year}{prefix2}{year_end}{prefix3}{seq}{suffix}`.
    YearRange {
        digits: YearDigits,
        date: String,
        number: String,
    },
    /// `{prefix1}{year}{prefix2}{year_end}{prefix3}{month}{prefix4}{seq}{suffix}`.
    YearRangeMonth {
        digits: YearDigits,
        date: String,
        month: String,
        number: String,
    },
}

impl Layout {
    fn reset(&self) -> SequenceReset {
        match self {
            Self::Never => SequenceReset::Never,
            Self::Year { .. } => SequenceReset::Year,
            Self::Month { .. } => SequenceReset::Month,
            Self::YearRange { .. } => SequenceReset::YearRange,
            Self::YearRangeMonth { .. } => SequenceReset::YearRangeMonth,
        }
    }
}

/// A document numbering pattern: prefix, date part (per reset rule), counter width and suffix.
/// Built with [`SequencePattern::builder`] (validated) or read from a name with
/// [`SequencePattern::infer_from_name`]; it renders names and period keys without a counter row.
///
/// ```
/// use erp_fields::Date;
/// use erp_orm::sequence::{SequencePattern, SequenceReset};
/// let pattern = SequencePattern::builder(SequenceReset::Year)
///     .prefix("INV/")
///     .padding(5)
///     .build()
///     .unwrap();
/// let date = Date::from_ymd(2026, 3, 12).unwrap();
/// assert_eq!(pattern.render(1, date, None).unwrap(), "INV/2026/00001");
/// assert_eq!(pattern.period_key(date, None).unwrap().as_str(), "2026");
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SequencePattern {
    prefix: String,
    layout: Layout,
    padding: usize,
    suffix: String,
}

impl SequencePattern {
    /// A builder for a declared pattern with reset rule `reset`.
    pub fn builder(reset: SequenceReset) -> SequencePatternBuilder {
        SequencePatternBuilder {
            reset,
            prefix: String::new(),
            suffix: String::new(),
            padding: 0,
            year_digits: None,
            date_separator: None,
            month_separator: None,
            number_separator: None,
        }
    }

    /// The reset rule.
    pub fn reset(&self) -> SequenceReset {
        self.layout.reset()
    }

    /// Text before the date part (or before the counter for `never`).
    pub fn prefix(&self) -> &str {
        &self.prefix
    }

    /// Text after the counter.
    pub fn suffix(&self) -> &str {
        &self.suffix
    }

    /// Counter width: numbers are zero-filled to it and widen past it (0 = no fill).
    pub fn padding(&self) -> usize {
        self.padding
    }

    /// Digits shown for each year, `None` for `never`.
    pub fn year_digits(&self) -> Option<YearDigits> {
        match &self.layout {
            Layout::Never => None,
            Layout::Year { digits, .. }
            | Layout::Month { digits, .. }
            | Layout::YearRange { digits, .. }
            | Layout::YearRangeMonth { digits, .. } => Some(*digits),
        }
    }

    pub fn date_separator(&self) -> Option<&str> {
        match &self.layout {
            Layout::Never | Layout::Year { .. } => None,
            Layout::Month { date, .. }
            | Layout::YearRange { date, .. }
            | Layout::YearRangeMonth { date, .. } => Some(date),
        }
    }

    pub fn month_separator(&self) -> Option<&str> {
        match &self.layout {
            Layout::YearRangeMonth { month, .. } => Some(month),
            _ => None,
        }
    }

    /// Between the date part and the counter; `None` for `never`.
    pub fn number_separator(&self) -> Option<&str> {
        match &self.layout {
            Layout::Never => None,
            Layout::Year { number, .. }
            | Layout::Month { number, .. }
            | Layout::YearRange { number, .. }
            | Layout::YearRangeMonth { number, .. } => Some(number),
        }
    }

    /// The period key of `date`; see [`resolve_period_key`].
    ///
    /// # Errors
    ///
    /// As [`resolve_period_key`].
    pub fn period_key(&self, date: Date, fiscal_year: Option<&FiscalYear>) -> Result<PeriodKey> {
        resolve_period_key(self.reset(), date, fiscal_year)
    }

    /// Years and month come as for
    /// [`resolve_period_key`]; the fiscal resets show the fiscal year's start and end years cut to
    /// [`YearDigits`].
    ///
    /// # Errors
    ///
    /// `Validation` when `number` is below 1 or the date is outside the years 1..=9999;
    /// `Programming` for a fiscal reset without a fiscal year containing the date.
    pub fn render(
        &self,
        number: i64,
        date: Date,
        fiscal_year: Option<&FiscalYear>,
    ) -> Result<String> {
        if number < 1 {
            return Err(Error::validation(format!(
                "a sequence number starts at 1, got {number}"
            )));
        }
        let (year, month) = date_parts(date)?;
        let years = fiscal_years(self.reset(), date, fiscal_year)?;
        let seq = format!("{number:0width$}", width = self.padding);
        let mut out = String::with_capacity(self.prefix.len() + self.suffix.len() + seq.len() + 16);
        out.push_str(&self.prefix);
        match (&self.layout, years) {
            (Layout::Never, _) => {}
            (Layout::Year { digits, number }, _) => {
                out.push_str(&digits.render(year));
                out.push_str(number);
            }
            (
                Layout::Month {
                    digits,
                    date,
                    number,
                },
                _,
            ) => {
                out.push_str(&digits.render(year));
                out.push_str(date);
                out.push_str(&format!("{month:02}"));
                out.push_str(number);
            }
            (
                Layout::YearRange {
                    digits,
                    date,
                    number,
                },
                Some((start, end)),
            ) => {
                out.push_str(&digits.render(start));
                out.push_str(date);
                out.push_str(&digits.render(end));
                out.push_str(number);
            }
            (
                Layout::YearRangeMonth {
                    digits,
                    date,
                    month: month_sep,
                    number,
                },
                Some((start, end)),
            ) => {
                out.push_str(&digits.render(start));
                out.push_str(date);
                out.push_str(&digits.render(end));
                out.push_str(month_sep);
                out.push_str(&format!("{month:02}"));
                out.push_str(number);
            }
            (Layout::YearRange { .. } | Layout::YearRangeMonth { .. }, None) => {
                return Err(Error::programming(format!(
                    "fiscal years missing for reset {}",
                    self.reset()
                )));
            }
        }
        out.push_str(&seq);
        out.push_str(&self.suffix);
        Ok(out)
    }

    /// The name
    /// is read as is, never trimmed.
    ///
    /// # Errors
    ///
    /// ```
    /// use erp_orm::sequence::{SequencePattern, SequenceReset};
    /// let p = SequencePattern::infer_from_name("MISC/15-16/01/0001").unwrap();
    /// assert_eq!((p.reset(), p.prefix(), p.padding()), (SequenceReset::YearRangeMonth, "MISC/", 4));
    /// // a branch code is not a year/month
    /// let p = SequencePattern::infer_from_name("SHOP/0510/00042").unwrap();
    /// assert_eq!((p.reset(), p.prefix()), (SequenceReset::Never, "SHOP/0510/"));
    /// ```
    pub fn infer_from_name(name: &str) -> Result<Self> {
        if name.len() > MAX_NAME_BYTES {
            return Err(Error::validation(format!(
                "the sequence name is {} bytes; at most {MAX_NAME_BYTES} are read",
                name.len()
            )));
        }
        let chars = Chars::new(name);
        let matched = chars
            .deduce()
            .ok_or_else(|| Error::validation(NO_MATCH_MESSAGE))?;
        matched.into_pattern(&chars)
    }

    fn validated(self) -> Result<Self> {
        if self.padding > MAX_PATTERN_PADDING {
            return Err(Error::validation(format!(
                "sequence padding is {}; at most {MAX_PATTERN_PADDING} is supported",
                self.padding
            )));
        }
        let mut texts = vec![
            ("prefix", self.prefix.as_str()),
            ("suffix", self.suffix.as_str()),
        ];
        for (what, sep) in [
            ("date separator", self.date_separator()),
            ("month separator", self.month_separator()),
            ("number separator", self.number_separator()),
        ] {
            if let Some(sep) = sep {
                texts.push((what, sep));
            }
        }
        let text_bytes: usize = texts.iter().map(|(_, t)| t.len()).sum();
        if text_bytes > MAX_PATTERN_TEXT_BYTES {
            return Err(Error::validation(format!(
                "the sequence pattern has {text_bytes} bytes of fixed text; at most \
                 {MAX_PATTERN_TEXT_BYTES} are supported"
            )));
        }
        for (what, text) in &texts {
            if text.contains(['\n', '\r']) {
                return Err(Error::validation(format!(
                    "the sequence {what} {text:?} contains a line break"
                )));
            }
            if *what != "prefix" && text.chars().any(|c| c.is_ascii_digit()) {
                return Err(Error::validation(format!(
                    "the sequence {what} {text:?} contains a digit; only the date part and the \
                     counter may"
                )));
            }
        }
        let probe = self.probe()?;
        let back = Self::infer_from_name(&probe)?;
        let mut expected = self.clone();
        expected.padding = expected.padding.max(1);
        if back != expected {
            return Err(Error::validation(format!(
                "the sequence pattern renders {probe:?}, which reads back as reset {} with prefix \
                 {:?}; change the prefix, separators or padding so the parts stay apart",
                back.reset(),
                back.prefix()
            )));
        }
        Ok(self)
    }

    /// The name for counter 1 on 2026-01-15 in the fiscal year 2025-04-01..2026-03-31.
    fn probe(&self) -> Result<String> {
        let date = |y, m, d| {
            Date::from_ymd(y, m, d)
                .ok_or_else(|| Error::programming(format!("invalid probe date {y}-{m}-{d}")))
        };
        let fiscal_year = FiscalYear::new(date(2025, 4, 1)?, date(2026, 3, 31)?)?;
        self.render(1, date(2026, 1, 15)?, Some(&fiscal_year))
    }
}

/// Builder of a declared [`SequencePattern`].
#[derive(Debug, Clone)]
pub struct SequencePatternBuilder {
    reset: SequenceReset,
    prefix: String,
    suffix: String,
    padding: usize,
    year_digits: Option<YearDigits>,
    date_separator: Option<String>,
    month_separator: Option<String>,
    number_separator: Option<String>,
}

impl SequencePatternBuilder {
    /// Text before the date part (before the counter for `never`).
    #[must_use]
    pub fn prefix(mut self, prefix: impl Into<String>) -> Self {
        self.prefix = prefix.into();
        self
    }

    /// Text after the counter (no digits).
    #[must_use]
    pub fn suffix(mut self, suffix: impl Into<String>) -> Self {
        self.suffix = suffix.into();
        self
    }

    /// Counter width, at most [`MAX_PATTERN_PADDING`]; 0 shows the plain number.
    #[must_use]
    pub fn padding(mut self, padding: usize) -> Self {
        self.padding = padding;
        self
    }

    /// Digits of each year (not for `never`).
    #[must_use]
    pub fn year_digits(mut self, digits: YearDigits) -> Self {
        self.year_digits = Some(digits);
        self
    }

    /// Between year and month (`month`, may be empty) or between the two fiscal years (one
    /// character); not for `never` and `year`.
    #[must_use]
    pub fn date_separator(mut self, separator: impl Into<String>) -> Self {
        self.date_separator = Some(separator.into());
        self
    }

    /// Between the fiscal years and the month, one character (`year_range_month` only).
    #[must_use]
    pub fn month_separator(mut self, separator: impl Into<String>) -> Self {
        self.month_separator = Some(separator.into());
        self
    }

    /// Between the date part and the counter, at least one character (not for `never`).
    #[must_use]
    pub fn number_separator(mut self, separator: impl Into<String>) -> Self {
        self.number_separator = Some(separator.into());
        self
    }

    /// The validated pattern.
    ///
    /// # Errors
    pub fn build(self) -> Result<SequencePattern> {
        let reset = self.reset;
        let unused =
            |what: &str| Error::programming(format!("a {reset} sequence pattern has no {what}"));
        let one_char = |what: &str, sep: &str| {
            if sep.chars().count() == 1 {
                Ok(())
            } else {
                Err(Error::validation(format!(
                    "the {what} of a {reset} sequence pattern is one character, got {sep:?}"
                )))
            }
        };
        let not_empty = |what: &str, sep: &str| {
            if sep.is_empty() {
                Err(Error::validation(format!(
                    "the {what} of a {reset} sequence pattern cannot be empty"
                )))
            } else {
                Ok(())
            }
        };
        let fiscal_digits = YearDigits::Two;
        let layout = match reset {
            SequenceReset::Never => {
                if self.year_digits.is_some() {
                    return Err(unused("year digits"));
                }
                if self.date_separator.is_some() {
                    return Err(unused("date separator"));
                }
                if self.month_separator.is_some() {
                    return Err(unused("month separator"));
                }
                if self.number_separator.is_some() {
                    return Err(unused("number separator"));
                }
                Layout::Never
            }
            SequenceReset::Year => {
                if self.date_separator.is_some() {
                    return Err(unused("date separator"));
                }
                if self.month_separator.is_some() {
                    return Err(unused("month separator"));
                }
                let number = self.number_separator.unwrap_or_else(|| "/".to_owned());
                not_empty("number separator", &number)?;
                Layout::Year {
                    digits: self.year_digits.unwrap_or(YearDigits::Four),
                    number,
                }
            }
            SequenceReset::Month => {
                if self.month_separator.is_some() {
                    return Err(unused("month separator"));
                }
                let number = self.number_separator.unwrap_or_else(|| "/".to_owned());
                not_empty("number separator", &number)?;
                Layout::Month {
                    digits: self.year_digits.unwrap_or(YearDigits::Four),
                    date: self.date_separator.unwrap_or_else(|| "/".to_owned()),
                    number,
                }
            }
            SequenceReset::YearRange => {
                if self.month_separator.is_some() {
                    return Err(unused("month separator"));
                }
                let date = self.date_separator.unwrap_or_else(|| "-".to_owned());
                one_char("date separator", &date)?;
                let number = self.number_separator.unwrap_or_else(|| "/".to_owned());
                not_empty("number separator", &number)?;
                Layout::YearRange {
                    digits: self.year_digits.unwrap_or(fiscal_digits),
                    date,
                    number,
                }
            }
            SequenceReset::YearRangeMonth => {
                let date = self.date_separator.unwrap_or_else(|| "-".to_owned());
                one_char("date separator", &date)?;
                let month = self.month_separator.unwrap_or_else(|| "/".to_owned());
                one_char("month separator", &month)?;
                let number = self.number_separator.unwrap_or_else(|| "/".to_owned());
                not_empty("number separator", &number)?;
                Layout::YearRangeMonth {
                    digits: self.year_digits.unwrap_or(fiscal_digits),
                    date,
                    month,
                    number,
                }
            }
        };
        SequencePattern {
            prefix: self.prefix,
            layout,
            padding: self.padding,
            suffix: self.suffix,
        }
        .validated()
    }
}

/// Render a name from a pattern; the same as [`SequencePattern::render`].
///
/// # Errors
///
/// As [`SequencePattern::render`].
pub fn render_sequence_format(
    pattern: &SequencePattern,
    number: i64,
    date: Date,
    fiscal_year: Option<&FiscalYear>,
) -> Result<String> {
    pattern.render(number, date, fiscal_year)
}

// ---------------------------------------------------------------------------------------------
// The matcher.
// Every index access goes through `Chars::at`.
// ---------------------------------------------------------------------------------------------

/// A half-open range of character positions.
#[derive(Debug, Clone, Copy)]
struct Span(usize, usize);

impl Span {
    fn len(self) -> usize {
        self.1.saturating_sub(self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Family {
    YearRangeMonth,
    Month,
    YearRange,
    Year,
    Never,
}

#[derive(Debug, Clone, Copy)]
struct Match {
    family: Family,
    prefix1: Span,
    year: Option<Span>,
    prefix2: Option<Span>,
    year_end: Option<Span>,
    prefix3: Option<Span>,
    prefix4: Option<Span>,
    seq: Span,
    suffix: Span,
}

impl Match {
    fn new(family: Family, prefix1: Span, seq: Span, suffix: Span) -> Self {
        Self {
            family,
            prefix1,
            year: None,
            prefix2: None,
            year_end: None,
            prefix3: None,
            prefix4: None,
            seq,
            suffix,
        }
    }

    fn into_pattern(self, chars: &Chars) -> Result<SequencePattern> {
        let padding = self.seq.len();
        if padding > MAX_PATTERN_PADDING {
            return Err(Error::validation(format!(
                "the sequence number in the name has {padding} digits; at most \
                 {MAX_PATTERN_PADDING} are supported"
            )));
        }
        let text = |span: Option<Span>| {
            span.ok_or_else(|| {
                Error::programming("a sequence name group is missing from its match")
            })
            .and_then(|s| chars.text(s))
        };
        let digits = || {
            self.year
                .and_then(|y| YearDigits::from_len(y.len()))
                .ok_or_else(|| {
                    Error::programming("a dated sequence name matched without a 2- or 4-digit year")
                })
        };
        let layout = match self.family {
            Family::Never => Layout::Never,
            Family::Year => Layout::Year {
                digits: digits()?,
                number: text(self.prefix2)?,
            },
            Family::Month => Layout::Month {
                digits: digits()?,
                date: text(self.prefix2)?,
                number: text(self.prefix3)?,
            },
            Family::YearRange => Layout::YearRange {
                digits: digits()?,
                date: text(self.prefix2)?,
                number: text(self.prefix3)?,
            },
            Family::YearRangeMonth => Layout::YearRangeMonth {
                digits: digits()?,
                date: text(self.prefix2)?,
                month: text(self.prefix3)?,
                number: text(self.prefix4)?,
            },
        };
        // `_get_sequence_format_param`: without a counter, the text
        // after the date part (the suffix) becomes the prefix.
        let (prefix, suffix) = if padding == 0 {
            (chars.text(self.suffix)?, String::new())
        } else {
            (chars.text(self.prefix1)?, chars.text(self.suffix)?)
        };
        Ok(SequencePattern {
            prefix,
            layout,
            padding,
            suffix,
        })
    }
}

/// The name as characters, so positions never fall inside a multi-byte character.
struct Chars {
    chars: Vec<char>,
    /// Position of the last digit, so "no digit from here on" is one comparison.
    last_digit: Option<usize>,
}

impl Chars {
    fn new(name: &str) -> Self {
        let chars: Vec<char> = name.chars().collect();
        let last_digit = chars.iter().rposition(char::is_ascii_digit);
        Self { chars, last_digit }
    }

    fn len(&self) -> usize {
        self.chars.len()
    }

    fn at(&self, i: usize) -> Option<char> {
        self.chars.get(i).copied()
    }

    /// `\d` (ASCII only, see the module docs).
    fn digit(&self, i: usize) -> bool {
        self.at(i).is_some_and(|c| c.is_ascii_digit())
    }

    /// `\D`: a character that is not a digit (a newline included).
    fn non_digit(&self, i: usize) -> bool {
        self.at(i).is_some_and(|c| !c.is_ascii_digit())
    }

    fn text(&self, span: Span) -> Result<String> {
        self.chars
            .get(span.0..span.1)
            .map(|cs| cs.iter().collect())
            .ok_or_else(|| Error::programming("a sequence name group lies outside the name"))
    }

    /// Ends of the lazy `.*?` prefix, shortest first: it cannot cross a newline.
    fn prefix_ends(&self) -> std::ops::RangeInclusive<usize> {
        // without a newline the prefix may take the whole name
        let stop = self
            .chars
            .iter()
            .position(|&c| c == '\n')
            .unwrap_or(self.len());
        0..=stop
    }

    /// The lookbehind of a year: at the start or after a non-digit.
    fn run_start(&self, i: usize) -> bool {
        i == 0 || i.checked_sub(1).is_some_and(|p| self.non_digit(p))
    }

    /// The year group of the dated families at `i`: four digits
    /// starting with 19, 20 or 21, or two digits followed by a non-digit; always after a non-digit
    /// or at the start. The two forms cannot both match at one position.
    fn year(&self, i: usize) -> Option<usize> {
        if !self.run_start(i) {
            return None;
        }
        let century = matches!(
            (self.at(i), self.at(i + 1)),
            (Some('1'), Some('9')) | (Some('2'), Some('0' | '1'))
        );
        if century && self.digit(i + 2) && self.digit(i + 3) {
            return Some(i + 4);
        }
        if self.digit(i) && self.digit(i + 1) && self.non_digit(i + 2) {
            return Some(i + 2);
        }
        None
    }

    /// The year of the yearly family: an optional 19/20/21 century then
    /// two digits, after a non-digit or at the start; the century form is tried first.
    fn yearly_years(&self, i: usize) -> [Option<usize>; 2] {
        if !self.run_start(i) || !(self.digit(i) && self.digit(i + 1)) {
            return [None, None];
        }
        let century = matches!(
            (self.at(i), self.at(i + 1)),
            (Some('1'), Some('9')) | (Some('2'), Some('0' | '1'))
        ) && self.digit(i + 2)
            && self.digit(i + 3);
        [century.then_some(i + 4), Some(i + 2)]
    }

    /// `01`..`12` at `i`.
    fn month(&self, i: usize) -> Option<usize> {
        match (self.at(i), self.at(i + 1)) {
            (Some('0'), Some('1'..='9')) | (Some('1'), Some('0'..='2')) => Some(i + 2),
            _ => None,
        }
    }

    /// The counter (`\d*` greedy, or `\d{0,cap}`) then the lazy suffix `\D*?` and `$`, from `i`.
    /// A shorter counter would leave a digit where the suffix starts, so only the longest can
    /// match.
    fn seq_tail(&self, i: usize, cap: Option<usize>) -> Option<(Span, Span)> {
        let mut t = i;
        while self.digit(t) && cap.is_none_or(|cap| t - i < cap) {
            t += 1;
        }
        if self.last_digit.is_some_and(|last| last >= t) {
            return None;
        }
        // `$` matches first just before a final newline, if the suffix has not consumed it
        let end = if t < self.len() && self.chars.last() == Some(&'\n') {
            self.len() - 1
        } else {
            self.len()
        };
        Some((Span(i, t), Span(t, end)))
    }

    /// A lazy `\D+?` separator from `i`, then the counter and suffix: the first length that lets
    /// the rest match.
    fn lazy_sep_then_seq(&self, i: usize) -> Option<(Span, Span, Span)> {
        let mut j = i;
        while self.non_digit(j) {
            j += 1;
            if let Some((seq, suffix)) = self.seq_tail(j, None) {
                return Some((Span(i, j), seq, suffix));
            }
        }
        None
    }

    /// `^{prefix}{year}{prefix2:\D}{year_end}{prefix3:\D}{month}{prefix4:\D+?}{seq}{suffix}$`.
    fn year_range_month(&self) -> Option<Match> {
        for p in self.prefix_ends() {
            let Some(y) = self.year(p) else { continue };
            if !self.non_digit(y) {
                continue;
            }
            let Some(ye) = self.year(y + 1) else { continue };
            if !self.non_digit(ye) {
                continue;
            }
            let Some(m) = self.month(ye + 1) else {
                continue;
            };
            if let Some((prefix4, seq, suffix)) = self.lazy_sep_then_seq(m) {
                return Some(Match {
                    year: Some(Span(p, y)),
                    prefix2: Some(Span(y, y + 1)),
                    year_end: Some(Span(y + 1, ye)),
                    prefix3: Some(Span(ye, ye + 1)),
                    prefix4: Some(prefix4),
                    ..Match::new(Family::YearRangeMonth, Span(0, p), seq, suffix)
                });
            }
        }
        None
    }

    /// `^{prefix}{year}{prefix2:\D*?}{month}{prefix3:\D+?}{seq}{suffix}$`.
    fn month_family(&self) -> Option<Match> {
        for p in self.prefix_ends() {
            let Some(y) = self.year(p) else { continue };
            let mut k = y;
            loop {
                if let Some(m) = self.month(k)
                    && let Some((prefix3, seq, suffix)) = self.lazy_sep_then_seq(m)
                {
                    return Some(Match {
                        year: Some(Span(p, y)),
                        prefix2: Some(Span(y, k)),
                        prefix3: Some(prefix3),
                        ..Match::new(Family::Month, Span(0, p), seq, suffix)
                    });
                }
                if !self.non_digit(k) {
                    break;
                }
                k += 1;
            }
        }
        None
    }

    /// `^(?:{prefix}{year}{prefix2:\D}{year_end}{prefix3:\D+?})?{seq}{suffix}$`.
    /// Without the optional group the match has no years, which fails
    /// the family's requirements (`:220`), so only matches with the group are returned.
    fn year_range(&self) -> Option<Match> {
        for p in self.prefix_ends() {
            let Some(y) = self.year(p) else { continue };
            if !self.non_digit(y) {
                continue;
            }
            let Some(ye) = self.year(y + 1) else { continue };
            if let Some((prefix3, seq, suffix)) = self.lazy_sep_then_seq(ye) {
                return Some(Match {
                    year: Some(Span(p, y)),
                    prefix2: Some(Span(y, y + 1)),
                    year_end: Some(Span(y + 1, ye)),
                    prefix3: Some(prefix3),
                    ..Match::new(Family::YearRange, Span(0, p), seq, suffix)
                });
            }
        }
        None
    }

    /// `^{prefix}{year:(19|20|21)?\d{2}}{prefix2:\D+?}{seq}{suffix}$`.
    fn yearly(&self) -> Option<Match> {
        for p in self.prefix_ends() {
            for y in self.yearly_years(p).into_iter().flatten() {
                if let Some((prefix2, seq, suffix)) = self.lazy_sep_then_seq(y) {
                    return Some(Match {
                        year: Some(Span(p, y)),
                        prefix2: Some(prefix2),
                        ..Match::new(Family::Year, Span(0, p), seq, suffix)
                    });
                }
            }
        }
        None
    }

    /// `^{prefix}{seq:\d{0,9}}{suffix}$`.
    fn fixed(&self) -> Option<Match> {
        self.prefix_ends().find_map(|p| {
            self.seq_tail(p, Some(9))
                .map(|(seq, suffix)| Match::new(Family::Never, Span(0, p), seq, suffix))
        })
    }

    /// `_validate_fiscalyear_difference` and the length check (
    /// `:227-228`): the end year is the start year plus one, cut to the start year's length.
    fn consecutive_years(&self, m: &Match) -> bool {
        let (Some(year), Some(year_end)) = (m.year, m.year_end) else {
            return false;
        };
        if year.len() < year_end.len() {
            return false;
        }
        let value = |span: Span| -> Option<u32> {
            (span.0..span.1).try_fold(0u32, |acc, i| {
                let d = self.at(i)?.to_digit(10)?;
                acc.checked_mul(10)?.checked_add(d)
            })
        };
        let (Some(start), Some(end)) = (value(year), value(year_end)) else {
            return false;
        };
        let modulus = if year.len() == 4 { 10_000 } else { 100 };
        (start + 1) % modulus == end
    }

    fn deduce(&self) -> Option<Match> {
        if let Some(m) = self.year_range_month()
            && self.consecutive_years(&m)
        {
            return Some(m);
        }
        if let Some(m) = self.month_family() {
            return Some(m);
        }
        if let Some(m) = self.year_range()
            && self.consecutive_years(&m)
        {
            return Some(m);
        }
        self.yearly().or_else(|| self.fixed())
    }
}

#[cfg(test)]
mod tests {
    use erp_fields::ErrorKind;

    use super::*;

    fn d(y: i32, m: u32, day: u32) -> Date {
        Date::from_ymd(y, m, day).unwrap()
    }

    fn fy(from: Date, to: Date) -> FiscalYear {
        FiscalYear::new(from, to).unwrap()
    }

    #[test]
    fn spec_seq_001_yearly_reset() {
        let pattern = SequencePattern::builder(SequenceReset::Year)
            .prefix("INV/")
            .padding(5)
            .build()
            .unwrap();
        let date = d(2026, 3, 12);
        assert_eq!(pattern.period_key(date, None).unwrap().as_str(), "2026");
        assert_eq!(pattern.render(1, date, None).unwrap(), "INV/2026/00001");
        // the next year starts a new period
        assert_eq!(
            pattern.period_key(d(2027, 1, 1), None).unwrap().as_str(),
            "2027"
        );
    }

    #[test]
    fn spec_seq_002_monthly_reset() {
        let pattern = SequencePattern::builder(SequenceReset::Month)
            .prefix("MISC/")
            .padding(4)
            .build()
            .unwrap();
        let date = d(2016, 4, 12);
        assert_eq!(pattern.period_key(date, None).unwrap().as_str(), "2016-04");
        assert_eq!(pattern.render(1, date, None).unwrap(), "MISC/2016/04/0001");
    }

    #[test]
    fn spec_seq_003_year_range_reset() {
        let year = fy(d(2015, 4, 1), d(2016, 3, 31));
        let date = d(2016, 3, 12);
        let two = SequencePattern::builder(SequenceReset::YearRange)
            .prefix("JRNL/")
            .padding(5)
            .build()
            .unwrap();
        let four = SequencePattern::builder(SequenceReset::YearRange)
            .prefix("JRNL/")
            .padding(5)
            .year_digits(YearDigits::Four)
            .build()
            .unwrap();
        assert_eq!(
            two.render(1, date, Some(&year)).unwrap(),
            "JRNL/15-16/00001"
        );
        assert_eq!(
            four.render(1, date, Some(&year)).unwrap(),
            "JRNL/2015-2016/00001"
        );
        // one key format whatever the pattern shows
        assert_eq!(
            two.period_key(date, Some(&year)).unwrap().as_str(),
            "2015-16"
        );
        assert_eq!(
            four.period_key(date, Some(&year)).unwrap().as_str(),
            "2015-16"
        );
        // test_journal_sequence_format: the next fiscal year (2017-03-12) is 2016-2017
        let next = fy(d(2016, 4, 1), d(2017, 3, 31));
        assert_eq!(
            four.render(1, d(2017, 3, 12), Some(&next)).unwrap(),
            "JRNL/2016-2017/00001"
        );
        let calendar = fy(d(2026, 1, 1), d(2026, 12, 31));
        assert_eq!(
            two.render(7, d(2026, 6, 1), Some(&calendar)).unwrap(),
            "JRNL/26-26/00007"
        );
        assert_eq!(
            two.period_key(d(2026, 6, 1), Some(&calendar))
                .unwrap()
                .as_str(),
            "2026-26"
        );
    }

    #[test]
    fn spec_seq_004_year_range_month_reset() {
        let pattern = SequencePattern::builder(SequenceReset::YearRangeMonth)
            .prefix("MISC/")
            .padding(4)
            .build()
            .unwrap();
        // test_sequence_change_date (fiscal year ends 31 March)
        let fy16 = fy(d(2015, 4, 1), d(2016, 3, 31));
        assert_eq!(
            pattern.render(1, d(2016, 1, 1), Some(&fy16)).unwrap(),
            "MISC/15-16/01/0001"
        );
        assert_eq!(
            pattern
                .period_key(d(2016, 1, 1), Some(&fy16))
                .unwrap()
                .as_str(),
            "2015-16-01"
        );
        let fy20 = fy(d(2019, 4, 1), d(2020, 3, 31));
        assert_eq!(
            pattern.render(1, d(2020, 2, 2), Some(&fy20)).unwrap(),
            "MISC/19-20/02/0001"
        );
        assert_eq!(
            pattern.render(10, d(2016, 3, 12), Some(&fy16)).unwrap(),
            "MISC/15-16/03/0010"
        );
    }

    #[test]
    fn spec_seq_005_never_reset() {
        let pattern = SequencePattern::builder(SequenceReset::Never)
            .prefix("SO")
            .padding(5)
            .build()
            .unwrap();
        let date = d(2026, 3, 12);
        assert_eq!(pattern.period_key(date, None).unwrap().as_str(), "never");
        assert_eq!(pattern.render(42, date, None).unwrap(), "SO00042");
    }

    #[test]
    fn spec_seq_006_reference_test_names_and_review_cases() {
        // test_journal_sequence_format's names, then names the round-1 grammar got wrong
        let cases = [
            ("JRNL/2016/00001", SequenceReset::Year, "JRNL/", 5),
            ("JRNL/2015-2016/00001", SequenceReset::YearRange, "JRNL/", 5),
            ("JRNL/2015-16/00001", SequenceReset::Year, "JRNL/2015-", 5),
            ("JRNL/15-16/00001", SequenceReset::YearRange, "JRNL/", 5),
            ("1234567", SequenceReset::Never, "", 7),
            ("20190910", SequenceReset::Never, "", 8),
            ("2016-0910", SequenceReset::Year, "", 4),
            ("201603-10", SequenceReset::Month, "", 2),
            ("16-03-10", SequenceReset::Month, "", 2),
            ("2016-10", SequenceReset::Year, "", 2),
            ("045-001-000002", SequenceReset::Never, "045-001-", 6),
            ("JRNL/2016/00001suffix", SequenceReset::Year, "JRNL/", 5),
            (
                "MISC/15-16/01/0001",
                SequenceReset::YearRangeMonth,
                "MISC/",
                4,
            ),
            // M1: multi-byte characters before a digit
            ("Xé1/0001", SequenceReset::Never, "Xé1/", 4),
            ("ВЫП1/0001", SequenceReset::Never, "ВЫП1/", 4),
            ("فاتورة1/0001", SequenceReset::Never, "فاتورة1/", 4),
            ("ใบ1/0001", SequenceReset::Never, "ใบ1/", 4),
            ("ВЫП/2026/03/0001", SequenceReset::Month, "ВЫП/", 4),
            // M2: branch codes are not years or months
            ("SHOP/0510/00042", SequenceReset::Never, "SHOP/0510/", 5),
            ("BR0512/0001", SequenceReset::Never, "BR0512/", 4),
            ("X/9912/0001", SequenceReset::Never, "X/9912/", 4),
            (
                "INV/2026/1301/0001",
                SequenceReset::Never,
                "INV/2026/1301/",
                4,
            ),
            ("X/3015-3016/0001", SequenceReset::Never, "X/3015-3016/", 4),
            (
                "X/3015-3016/03/0001",
                SequenceReset::Year,
                "X/3015-3016/",
                4,
            ),
            // S3: an empty counter after a year, a digit run longer than 9, no trimming
            ("INV/2026/", SequenceReset::Year, "", 0),
            ("1234567890123", SequenceReset::Never, "1234", 9),
            ("  INV/2026/0001  ", SequenceReset::Year, "  INV/", 4),
        ];
        for (name, reset, prefix, padding) in cases {
            let p = SequencePattern::infer_from_name(name).unwrap();
            assert_eq!(
                (p.reset(), p.prefix(), p.padding()),
                (reset, prefix, padding),
                "{name:?}"
            );
        }
        let p = SequencePattern::infer_from_name("  INV/2026/0001  ").unwrap();
        assert_eq!(p.suffix(), "  ");
    }

    #[test]
    fn spec_seq_007_names_without_digits_and_unmatched_names() {
        for name in ["", "ABC", "INV/ABC/", "NO_DIGITS_HERE", "é/"] {
            let p = SequencePattern::infer_from_name(name).unwrap();
            assert_eq!(
                (p.reset(), p.prefix(), p.padding(), p.suffix()),
                (SequenceReset::Never, name, 0, ""),
                "{name:?}"
            );
        }
        // the next number of such a pattern is the text followed by the plain counter
        let p = SequencePattern::infer_from_name("INV/ABC/").unwrap();
        assert_eq!(p.render(1, d(2026, 1, 1), None).unwrap(), "INV/ABC/1");
        // only a newline before the counter leaves every family without a match
        for name in ["A\n1", "INV\n2026/0001"] {
            let err = SequencePattern::infer_from_name(name).unwrap_err();
            assert_eq!(err.kind(), ErrorKind::Validation);
            assert_eq!(
                err.message(),
                "The sequence regex should at least contain the seq grouping keys. For \
                 instance:\n^(?P<prefix1>.*?)(?P<seq>\\d*)(?P<suffix>\\D*?)$"
            );
        }
        // a final newline is not part of the name's parts
        let p = SequencePattern::infer_from_name("INV/0001\n").unwrap();
        assert_eq!((p.prefix(), p.padding(), p.suffix()), ("INV/", 4, ""));
    }

    #[test]
    fn spec_seq_011_padding_zero_and_width_growth() {
        let date = d(2026, 3, 12);
        let plain = SequencePattern::builder(SequenceReset::Year)
            .prefix("INV/")
            .build()
            .unwrap();
        // padding 0 keeps the counter (round 1 rendered "INV/2026/" for every number)
        assert_eq!(plain.render(1, date, None).unwrap(), "INV/2026/1");
        assert_eq!(plain.render(24, date, None).unwrap(), "INV/2026/24");
        // test_journal_next_sequence: "TEST_ORDER/2016/1" continues with 2..24
        let inferred = SequencePattern::infer_from_name("TEST_ORDER/2016/1").unwrap();
        assert_eq!(inferred.padding(), 1);
        assert_eq!(
            inferred.render(24, d(2016, 1, 1), None).unwrap(),
            "TEST_ORDER/2016/24"
        );
        // a number wider than the padding widens, never wraps
        let five = SequencePattern::builder(SequenceReset::Year)
            .prefix("INV/")
            .padding(5)
            .build()
            .unwrap();
        assert_eq!(five.render(100_000, date, None).unwrap(), "INV/2026/100000");
        assert_eq!(
            five.render(i64::MAX, date, None).unwrap(),
            format!("INV/2026/{}", i64::MAX)
        );
    }

    #[test]
    fn spec_seq_012_fiscal_year_is_required_and_validated() {
        let pattern = SequencePattern::builder(SequenceReset::YearRangeMonth)
            .prefix("BILL/")
            .padding(4)
            .build()
            .unwrap();
        // test_sequence_staggered_year: the fiscal year ends on 15 April
        let fy24 = fy(d(2023, 4, 16), d(2024, 4, 15));
        let fy25 = fy(d(2024, 4, 16), d(2025, 4, 15));
        assert_eq!(
            pattern.render(1, d(2024, 4, 10), Some(&fy24)).unwrap(),
            "BILL/23-24/04/0001"
        );
        assert_eq!(
            pattern.render(1, d(2024, 4, 17), Some(&fy25)).unwrap(),
            "BILL/24-25/04/0001"
        );
        let k1 = pattern.period_key(d(2024, 4, 10), Some(&fy24)).unwrap();
        let k2 = pattern.period_key(d(2024, 4, 17), Some(&fy25)).unwrap();
        assert_eq!((k1.as_str(), k2.as_str()), ("2023-24-04", "2024-25-04"));

        // no implicit fiscal year
        let err = pattern.render(1, d(2026, 3, 15), None).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Programming);
        assert_eq!(
            err.message(),
            "reset year_range_month needs the company's fiscal year for 2026-03-15; pass \
             Some(FiscalYear) (the reference ERP: res.company.compute_fiscalyear_dates)"
        );
        let err = resolve_period_key(SequenceReset::YearRange, d(2026, 3, 15), None).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Programming);
        // a fiscal year that does not contain the date
        let err = pattern
            .period_key(d(2026, 3, 15), Some(&fy(d(2030, 1, 1), d(2030, 12, 31))))
            .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Programming);
        assert_eq!(
            err.message(),
            "2026-03-15 is outside the fiscal year 2030-01-01..2030-12-31 passed for reset \
             year_range_month"
        );
        // the fiscal year itself
        let err = FiscalYear::new(d(2026, 1, 1), d(2025, 12, 31)).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Validation);
        assert_eq!(
            err.message(),
            "the fiscal year ends (2025-12-31) before it starts (2026-01-01)"
        );
        let err = FiscalYear::new(d(2026, 1, 1), d(2028, 1, 1)).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Validation);
        assert_eq!(
            err.message(),
            "the fiscal year 2026-01-01..2028-01-01 spans 2026 to 2028; a fiscal year ends in \
             its start year or the next one"
        );
        // the other resets do not read it
        let yearly = SequencePattern::builder(SequenceReset::Year)
            .build()
            .unwrap();
        assert_eq!(
            yearly.render(3, d(2026, 3, 15), Some(&fy24)).unwrap(),
            "2026/3"
        );
    }

    #[test]
    fn spec_seq_013_declared_patterns_and_inputs_are_bounded_and_validated() {
        let date = d(2026, 3, 12);
        let check = |result: Result<SequencePattern>, kind: ErrorKind, message: &str| {
            let err = result.unwrap_err();
            assert_eq!((err.kind(), err.message()), (kind, message));
        };
        check(
            SequencePattern::builder(SequenceReset::Year)
                .padding(65)
                .build(),
            ErrorKind::Validation,
            "sequence padding is 65; at most 64 is supported",
        );
        check(
            SequencePattern::builder(SequenceReset::Never)
                .padding(usize::MAX)
                .build(),
            ErrorKind::Validation,
            &format!(
                "sequence padding is {}; at most 64 is supported",
                usize::MAX
            ),
        );
        check(
            SequencePattern::builder(SequenceReset::Never)
                .prefix("x".repeat(129))
                .build(),
            ErrorKind::Validation,
            "the sequence pattern has 129 bytes of fixed text; at most 128 are supported",
        );
        check(
            SequencePattern::builder(SequenceReset::Year)
                .suffix("/2")
                .build(),
            ErrorKind::Validation,
            "the sequence suffix \"/2\" contains a digit; only the date part and the counter may",
        );
        check(
            SequencePattern::builder(SequenceReset::Year)
                .number_separator("")
                .build(),
            ErrorKind::Validation,
            "the number separator of a year sequence pattern cannot be empty",
        );
        check(
            SequencePattern::builder(SequenceReset::YearRange)
                .date_separator("--")
                .build(),
            ErrorKind::Validation,
            "the date separator of a year_range sequence pattern is one character, got \"--\"",
        );
        check(
            SequencePattern::builder(SequenceReset::Year)
                .month_separator("/")
                .build(),
            ErrorKind::Programming,
            "a year sequence pattern has no month separator",
        );
        check(
            SequencePattern::builder(SequenceReset::Never)
                .prefix("A\nB")
                .build(),
            ErrorKind::Validation,
            "the sequence prefix \"A\\nB\" contains a line break",
        );
        // names that would read back as another reset rule
        check(
            SequencePattern::builder(SequenceReset::Never)
                .prefix("INV/2026/")
                .padding(4)
                .build(),
            ErrorKind::Validation,
            "the sequence pattern renders \"INV/2026/0001\", which reads back as reset year with \
             prefix \"INV/\"; change the prefix, separators or padding so the parts stay apart",
        );
        check(
            SequencePattern::builder(SequenceReset::Year)
                .padding(2)
                .suffix("/A")
                .build(),
            ErrorKind::Validation,
            "the sequence pattern renders \"2026/01/A\", which reads back as reset month with \
             prefix \"A\"; change the prefix, separators or padding so the parts stay apart",
        );
        // numbers start at 1; dates stay in 1..=9999
        let p = SequencePattern::builder(SequenceReset::Never)
            .build()
            .unwrap();
        for number in [0, -1, i64::MIN] {
            let err = p.render(number, date, None).unwrap_err();
            assert_eq!(
                (err.kind(), err.message()),
                (
                    ErrorKind::Validation,
                    format!("a sequence number starts at 1, got {number}").as_str()
                )
            );
        }
        let err = p.render(1, Date::from_days(i32::MAX), None).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Validation);
        // inference input bounds
        let long = format!("INV/2026/{}", "1".repeat(MAX_NAME_BYTES));
        let err = SequencePattern::infer_from_name(&long).unwrap_err();
        assert_eq!(
            (err.kind(), err.message()),
            (
                ErrorKind::Validation,
                format!(
                    "the sequence name is {} bytes; at most 256 are read",
                    long.len()
                )
                .as_str()
            )
        );
        let wide = format!("INV/2026/{}", "1".repeat(65));
        let err = SequencePattern::infer_from_name(&wide).unwrap_err();
        assert_eq!(
            (err.kind(), err.message()),
            (
                ErrorKind::Validation,
                "the sequence number in the name has 65 digits; at most 64 are supported"
            )
        );
        // a name of exactly the bound is read
        let edge = format!("{}{}", "é".repeat(126), "0001");
        assert_eq!(edge.len(), MAX_NAME_BYTES);
        assert_eq!(
            SequencePattern::infer_from_name(&edge).unwrap().padding(),
            4
        );
        // the reset identifier is strict
        let err = SequenceReset::parse("yearly").unwrap_err();
        assert_eq!(
            (err.kind(), err.message()),
            (
                ErrorKind::Validation,
                "unknown sequence reset \"yearly\"; expected never, year, month, year_range or \
                 year_range_month"
            )
        );
        assert_eq!(
            SequenceReset::parse("year_range_month").unwrap(),
            SequenceReset::YearRangeMonth
        );
    }

    /// Every declared pattern's own rendered names read back to it (the builder's guarantee),
    /// across resets, digits, separators and counters.
    #[test]
    fn declared_patterns_read_back() {
        let year = fy(d(2025, 4, 1), d(2026, 3, 31));
        let date = d(2026, 2, 28);
        for reset in [
            SequenceReset::Never,
            SequenceReset::Year,
            SequenceReset::Month,
            SequenceReset::YearRange,
            SequenceReset::YearRangeMonth,
        ] {
            let mut builder = SequencePattern::builder(reset).prefix("R/INV/").padding(4);
            if reset != SequenceReset::Never {
                builder = builder.year_digits(YearDigits::Four);
            }
            let pattern = builder.build().unwrap();
            for number in [1_i64, 9, 10, 12, 13, 9999, 10_000, 123_456_789] {
                let name = pattern.render(number, date, Some(&year)).unwrap();
                let back = SequencePattern::infer_from_name(&name).unwrap();
                // a number wider than the padding reads back with its own width
                let width = number.to_string().len().max(4);
                let mut expected = pattern.clone();
                expected.padding = width;
                assert_eq!(back, expected, "{name:?}");
            }
        }
    }
}
