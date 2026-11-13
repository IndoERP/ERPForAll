// SPDX-License-Identifier: MIT OR Apache-2.0
//! Fiscal-year and period arithmetic: pure functions, no database.
//!
//! A fiscal year has
//! twelve regular periods of one month each (R2): period `n` ends on the fiscal year's
//! end day in the `12 - n`-th month before the year's last month, so a year ending on 31 December
//! has the calendar months and a year ending on 15 June has periods from the 16th to the 15th.

use erp_sdk::prelude::*;
use erp_sdk::sequence::FiscalYear;

/// Regular periods per fiscal year (R2, `CHECK (number BETWEEN 1 AND 12)`).
pub const PERIODS_PER_YEAR: u32 = 12;

/// The last day of a month; `month` is always 1..=12 here (`FiscalYearEnd::new`, `shift_month`).
fn month_len(year: i32, month: u32) -> u32 {
    match month {
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// The day a fiscal year ending on `day` of `month` ends in `year`: capped at the month's last day,
/// and in February a chosen 28 or the true last day both mean the month end.
fn fix_day(year: i32, month: u32, day: u32) -> u32 {
    let max = month_len(year, month);
    if month == 2 && (day == 28 || day == max) {
        max
    } else {
        day.min(max)
    }
}

/// `(year, month)` moved by `delta` months.
fn shift_month(year: i32, month: u32, delta: i32) -> (i32, u32) {
    // exact casts: a month is 1..=12 and rem_euclid(12) is 0..=11
    let index = year * 12 + month as i32 - 1 + delta;
    (index.div_euclid(12), index.rem_euclid(12) as u32 + 1)
}

fn ymd(year: i32, month: u32, day: u32) -> Result<Date> {
    Date::from_ymd(year, month, day)
        .ok_or_else(|| Error::validation(format!("{year:04}-{month:02}-{day:02} is not a date")))
}

/// The day after `date`.
pub fn next_day(date: Date) -> Date {
    Date::from_days(date.days() + 1)
}

/// The day before `date`.
pub fn previous_day(date: Date) -> Date {
    Date::from_days(date.days() - 1)
}

/// `date` moved forward by `months` months, the day capped at the target month's end.
pub fn add_months(date: Date, months: i32) -> Result<Date> {
    let (y, m, d) = date.ymd();
    let (y, m) = shift_month(y, m, months);
    ymd(y, m, d.min(month_len(y, m)))
}

/// A company's fiscal year end: a day and a month.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FiscalYearEnd {
    day: u32,
    month: u32,
}

impl FiscalYearEnd {
    /// The fiscal year end on `day` of `month`.
    ///
    /// # Errors
    ///
    /// `Validation` naming the settings when the month is outside 1..=12 or the day does not exist
    /// in that month; 29 February is accepted, as the reference accepts it explicitly
    /// ("Invalid fiscal year last day").
    pub fn new(day: i64, month: i64) -> Result<Self> {
        let invalid = || {
            Error::validation(format!(
                "Invalid fiscal year last day: day {day} of month {month} (settings {} and {})",
                super::settings::LAST_DAY_SETTING,
                super::settings::LAST_MONTH_SETTING
            ))
        };
        let month = u32::try_from(month)
            .ok()
            .filter(|m| (1..=12).contains(m))
            .ok_or_else(invalid)?;
        // 2000 is a leap year: February accepts 29, every other month its own length
        let day = u32::try_from(day)
            .ok()
            .filter(|d| (1..=month_len(2000, month)).contains(d))
            .ok_or_else(invalid)?;
        Ok(Self { day, month })
    }

    /// Day of the month the fiscal year ends on.
    pub fn day(&self) -> u32 {
        self.day
    }

    /// Month the fiscal year ends in.
    pub fn month(&self) -> u32 {
        self.month
    }

    /// The end of a period whose last day falls in `year`-`month`.
    fn end_in(&self, year: i32, month: u32) -> Result<Date> {
        ymd(year, month, fix_day(year, month, self.day))
    }

    /// The fiscal year containing `date` (`date_utils.get_fiscal_year`).
    ///
    /// # Errors
    ///
    /// `Validation` for a fiscal year outside the years 1..=9999.
    pub fn fiscal_year_of(&self, date: Date) -> Result<FiscalYear> {
        let (year, _, _) = date.ymd();
        let end = self.end_in(year, self.month)?;
        let (from, to) = if date <= end {
            (next_day(self.end_in(year - 1, self.month)?), end)
        } else {
            (next_day(end), self.end_in(year + 1, self.month)?)
        };
        FiscalYear::new(from, to)
    }

    /// The regular period containing `date`.
    ///
    /// # Errors
    ///
    /// As [`FiscalYearEnd::fiscal_year_of`].
    pub fn period_of(&self, date: Date) -> Result<PeriodRange> {
        let fiscal_year = self.fiscal_year_of(date)?;
        let (end_year, end_month, _) = fiscal_year.date_to().ymd();
        let mut from = fiscal_year.date_from();
        for number in 1..=PERIODS_PER_YEAR {
            let back = (PERIODS_PER_YEAR - number) as i32; // 0..=11, exact
            let (y, m) = shift_month(end_year, end_month, -back);
            let to = self.end_in(y, m)?;
            if date <= to {
                return Ok(PeriodRange {
                    fiscal_year,
                    number,
                    date_from: from,
                    date_to: to,
                });
            }
            from = next_day(to);
        }
        Err(Error::internal(format!(
            "{date} lies in no period of the fiscal year {}..{}",
            fiscal_year.date_from(),
            fiscal_year.date_to()
        )))
    }

    /// The regular periods that contain a day of `date_from..=date_to`, in date order.
    ///
    /// # Errors
    ///
    /// As [`FiscalYearEnd::period_of`]; `Validation` when the range is reversed or needs more
    /// than `max` periods.
    pub fn periods_between(
        &self,
        date_from: Date,
        date_to: Date,
        max: usize,
    ) -> Result<Vec<PeriodRange>> {
        if date_to < date_from {
            return Err(Error::validation(format!(
                "the period range ends ({date_to}) before it starts ({date_from})"
            )));
        }
        let mut periods = Vec::new();
        let mut day = date_from;
        while day <= date_to {
            if periods.len() == max {
                return Err(Error::validation(format!(
                    "{date_from}..{date_to} needs more than {max} fiscal periods; create them in \
                     smaller ranges"
                )));
            }
            let period = self.period_of(day)?;
            day = next_day(period.date_to);
            periods.push(period);
        }
        Ok(periods)
    }
}

/// One regular period of a fiscal year.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeriodRange {
    /// The fiscal year it belongs to.
    pub fiscal_year: FiscalYear,
    /// 1 to 12.
    pub number: u32,
    /// First day.
    pub date_from: Date,
    /// Last day, included.
    pub date_to: Date,
}

impl PeriodRange {
    /// The stored `fiscal_year` number: the year of the fiscal year's last day.
    pub fn year(&self) -> i32 {
        self.fiscal_year.end_year()
    }

    /// The display name: `2026/03`, or `2025-26/01` for a year spanning two calendar years.
    pub fn name(&self) -> String {
        let (start, end) = (self.fiscal_year.start_year(), self.fiscal_year.end_year());
        if start == end {
            format!("{end:04}/{:02}", self.number)
        } else {
            format!("{start:04}-{:02}/{:02}", end.rem_euclid(100), self.number)
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "tests")]
    use super::*;

    fn d(y: i32, m: u32, day: u32) -> Date {
        Date::from_ymd(y, m, day).unwrap()
    }

    fn range(end: FiscalYearEnd, date: Date) -> (Date, Date) {
        let fy = end.fiscal_year_of(date).unwrap();
        (fy.date_from(), fy.date_to())
    }

    #[test]
    fn fiscal_years_follow_the_reference_arithmetic() {
        let calendar = FiscalYearEnd::new(31, 12).unwrap();
        assert_eq!(
            range(calendar, d(2026, 5, 20)),
            (d(2026, 1, 1), d(2026, 12, 31))
        );
        let march = FiscalYearEnd::new(31, 3).unwrap();
        assert_eq!(
            range(march, d(2026, 3, 31)),
            (d(2025, 4, 1), d(2026, 3, 31))
        );
        assert_eq!(range(march, d(2026, 4, 1)), (d(2026, 4, 1), d(2027, 3, 31)));
        // a February end snaps to the month end in leap years, chosen as 28 or as 29
        for day in [28, 29] {
            let feb = FiscalYearEnd::new(day, 2).unwrap();
            assert_eq!(range(feb, d(2024, 2, 29)), (d(2023, 3, 1), d(2024, 2, 29)));
            assert_eq!(range(feb, d(2025, 1, 10)), (d(2024, 3, 1), d(2025, 2, 28)));
        }
    }

    #[test]
    fn an_impossible_fiscal_year_end_is_refused_by_name() {
        for (day, month) in [(31, 4), (30, 2), (0, 12), (32, 1), (15, 0), (15, 13)] {
            let error = FiscalYearEnd::new(day, month).unwrap_err();
            assert!(
                error
                    .message()
                    .contains(super::super::settings::LAST_DAY_SETTING),
                "{}",
                error.message()
            );
        }
        assert!(FiscalYearEnd::new(29, 2).is_ok());
    }

    #[test]
    fn periods_are_twelve_contiguous_months_of_the_fiscal_year() {
        for (day, month) in [(31, 12), (31, 3), (15, 6), (30, 6), (28, 2)] {
            let end = FiscalYearEnd::new(day, month).unwrap();
            let fy = end.fiscal_year_of(d(2027, 7, 4)).unwrap();
            let periods = end
                .periods_between(fy.date_from(), fy.date_to(), 100)
                .unwrap();
            assert_eq!(periods.len(), 12, "{day}/{month}");
            assert_eq!(periods[0].date_from, fy.date_from());
            assert_eq!(periods[11].date_to, fy.date_to());
            for (i, pair) in periods.windows(2).enumerate() {
                assert_eq!(
                    next_day(pair[0].date_to),
                    pair[1].date_from,
                    "{day}/{month} #{i}"
                );
            }
            let numbers: Vec<u32> = periods.iter().map(|p| p.number).collect();
            assert_eq!(numbers, (1..=12).collect::<Vec<_>>());
        }
        let mid = FiscalYearEnd::new(15, 6).unwrap();
        let p = mid.period_of(d(2026, 7, 10)).unwrap();
        assert_eq!(
            (p.date_from, p.date_to, p.number),
            (d(2026, 6, 16), d(2026, 7, 15), 1)
        );
        assert_eq!(p.name(), "2026-27/01");
        let cal = FiscalYearEnd::new(31, 12)
            .unwrap()
            .period_of(d(2026, 2, 3))
            .unwrap();
        assert_eq!(
            (cal.date_from, cal.date_to),
            (d(2026, 2, 1), d(2026, 2, 28))
        );
        assert_eq!((cal.name(), cal.year()), ("2026/02".to_owned(), 2026));
    }

    #[test]
    fn a_range_needing_too_many_periods_is_refused() {
        let end = FiscalYearEnd::new(31, 12).unwrap();
        assert_eq!(
            end.periods_between(d(2026, 1, 15), d(2026, 3, 1), 3)
                .unwrap()
                .len(),
            3
        );
        assert!(
            end.periods_between(d(2026, 1, 15), d(2026, 4, 1), 3)
                .is_err()
        );
        assert!(
            end.periods_between(d(2026, 2, 1), d(2026, 1, 1), 3)
                .is_err()
        );
        assert_eq!(add_months(d(2026, 1, 31), 1).unwrap(), d(2026, 2, 28));
        assert_eq!(add_months(d(2026, 11, 30), 3).unwrap(), d(2027, 2, 28));
    }
}
