//! Date granularities of grouped reads (`READ_GROUP_TIME_GRANULARITY` and
//! `READ_GROUP_NUMBER_GRANULARITY`), shared by the query AST and the ORM's `read_group`.

/// Date granularities (`READ_GROUP_TIME_GRANULARITY` and `READ_GROUP_NUMBER_GRANULARITY`): the
/// part after the colon of a date group-by spec (`date:month`).
///
/// ```
/// use erp_domain::Granularity;
///
/// assert_eq!(Granularity::parse("month"), Some(Granularity::Month));
/// assert!(Granularity::Month.is_bucket());
/// assert_eq!(Granularity::parse("fortnight"), None);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Granularity {
    /// `hour` bucket.
    Hour,
    /// `day` bucket.
    Day,
    /// `week` bucket (starts on the language's first weekday).
    Week,
    /// `month` bucket.
    Month,
    /// `quarter` bucket.
    Quarter,
    /// `year` bucket.
    Year,
    /// `year_number`.
    YearNumber,
    /// `quarter_number`.
    QuarterNumber,
    /// `month_number`.
    MonthNumber,
    /// `iso_week_number`.
    IsoWeekNumber,
    /// `day_of_year`.
    DayOfYear,
    /// `day_of_month`.
    DayOfMonth,
    /// `day_of_week` (Sunday = 0 … Saturday = 6).
    DayOfWeek,
    /// `hour_number`.
    HourNumber,
    /// `minute_number`.
    MinuteNumber,
    /// `second_number`.
    SecondNumber,
}

impl Granularity {
    pub const ALL: [Granularity; 16] = [
        Self::Hour,
        Self::Day,
        Self::Week,
        Self::Month,
        Self::Quarter,
        Self::Year,
        Self::YearNumber,
        Self::QuarterNumber,
        Self::MonthNumber,
        Self::IsoWeekNumber,
        Self::DayOfYear,
        Self::DayOfMonth,
        Self::DayOfWeek,
        Self::HourNumber,
        Self::MinuteNumber,
        Self::SecondNumber,
    ];

    /// The granularity named `name`.
    pub fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "hour" => Self::Hour,
            "day" => Self::Day,
            "week" => Self::Week,
            "month" => Self::Month,
            "quarter" => Self::Quarter,
            "year" => Self::Year,
            "year_number" => Self::YearNumber,
            "quarter_number" => Self::QuarterNumber,
            "month_number" => Self::MonthNumber,
            "iso_week_number" => Self::IsoWeekNumber,
            "day_of_year" => Self::DayOfYear,
            "day_of_month" => Self::DayOfMonth,
            "day_of_week" => Self::DayOfWeek,
            "hour_number" => Self::HourNumber,
            "minute_number" => Self::MinuteNumber,
            "second_number" => Self::SecondNumber,
            _ => return None,
        })
    }

    /// The name in specs.
    pub fn name(self) -> &'static str {
        match self {
            Self::Hour => "hour",
            Self::Day => "day",
            Self::Week => "week",
            Self::Month => "month",
            Self::Quarter => "quarter",
            Self::Year => "year",
            Self::YearNumber => "year_number",
            Self::QuarterNumber => "quarter_number",
            Self::MonthNumber => "month_number",
            Self::IsoWeekNumber => "iso_week_number",
            Self::DayOfYear => "day_of_year",
            Self::DayOfMonth => "day_of_month",
            Self::DayOfWeek => "day_of_week",
            Self::HourNumber => "hour_number",
            Self::MinuteNumber => "minute_number",
            Self::SecondNumber => "second_number",
        }
    }

    /// A time bucket (`date_trunc`), as opposed to a number part (`date_part`).
    pub fn is_bucket(self) -> bool {
        matches!(
            self,
            Self::Hour | Self::Day | Self::Week | Self::Month | Self::Quarter | Self::Year
        )
    }

    /// The PostgreSQL `date_part` field of a number granularity (`None` for a bucket).
    pub fn date_part(self) -> Option<&'static str> {
        Some(match self {
            Self::YearNumber => "year",
            Self::QuarterNumber => "quarter",
            Self::MonthNumber => "month",
            Self::IsoWeekNumber => "week",
            Self::DayOfYear => "doy",
            Self::DayOfMonth => "day",
            Self::DayOfWeek => "dow",
            Self::HourNumber => "hour",
            Self::MinuteNumber => "minute",
            Self::SecondNumber => "second",
            _ => return None,
        })
    }
}
