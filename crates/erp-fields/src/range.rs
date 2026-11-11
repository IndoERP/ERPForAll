//! Range values: a lower and an upper end of dates or naive-UTC datetimes, each optional
//! (`date_range` / `datetime_range` fields, design §5.2).
//!
//! A range value does not carry its bounds: the field declares them ([`RangeBounds`]) and the value
//! means "from `lower` to `upper`, with the field's bounds".
//! Whether a range is empty depends on the
//! bounds: `[a, a)` holds nothing, `[a, a]` holds `a`. Range fields never store empty ranges: the
//! value converter refuses them ([`crate::convert::validate_cell`]).
//!
//! ```
//! use erp_fields::{Date, DateRange, RangeBounds};
//!
//! let d1 = Date::from_ymd(2026, 1, 1).unwrap();
//! let d2 = Date::from_ymd(2026, 2, 1).unwrap();
//! let january = DateRange::new(Some(d1), Some(d2));
//! assert!(!january.is_empty(RangeBounds::HalfOpen));
//! assert!(DateRange::new(Some(d1), Some(d1)).is_empty(RangeBounds::HalfOpen));
//! assert!(!DateRange::new(Some(d1), Some(d1)).is_empty(RangeBounds::Closed));
//! let open_ended = DateRange::new(Some(d2), None);
//! assert!(!january.overlaps(&open_ended, RangeBounds::HalfOpen), "adjacent periods");
//! assert!(january.overlaps(&open_ended, RangeBounds::Closed), "they share 2026-02-01");
//! ```

use std::cmp::Ordering;
use std::fmt;

use crate::temporal::{Date, DateTime};

/// Which ends of a range are included, declared on a range field or on the period of an exclusion
/// constraint (`bounds = "[)"` or `bounds = "[]"`).
///
// Deliberately exhaustive: the design allows exactly these two forms (the lower end is always
// included), and every SQL builder must decide what each one means.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RangeBounds {
    /// `[)`: the lower end is included, the upper end is not, so adjacent periods do not overlap.
    HalfOpen,
    /// `[]`: both ends are included, so periods that share an end overlap.
    Closed,
}

impl RangeBounds {
    /// The declaration text, also the bounds argument of PostgreSQL's range constructors.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::HalfOpen => "[)",
            Self::Closed => "[]",
        }
    }

    /// Parse a declaration (`"[)"` or `"[]"`); `None` for anything else, including the other
    /// PostgreSQL forms (`"()"`, `"(]"`), which no declaration may use.
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "[)" => Some(Self::HalfOpen),
            "[]" => Some(Self::Closed),
            _ => None,
        }
    }

    /// Whether the range from `lower` to `upper` with these bounds holds nothing: `lower` after
    /// `upper`, or equal to it under `[)`.
    pub fn is_empty<T: Ord>(self, lower: &T, upper: &T) -> bool {
        match self {
            Self::HalfOpen => lower >= upper,
            Self::Closed => lower > upper,
        }
    }

    /// The relation the lower end must have to the upper end, in words (`"before"` /
    /// `"not after"`), for messages.
    pub const fn order_words(self) -> &'static str {
        match self {
            Self::HalfOpen => "be before",
            Self::Closed => "not be after",
        }
    }
}

impl fmt::Display for RangeBounds {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A range from `lower` to `upper`; `None` on a side means unbounded on that side. The bounds
/// (which ends are included) come from the field or period that holds the range.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Range<T> {
    lower: Option<T>,
    upper: Option<T>,
}

/// A range of dates (`date_range` fields, PostgreSQL `daterange`).
pub type DateRange = Range<Date>;

/// A range of naive-UTC datetimes (`datetime_range` fields, PostgreSQL `tsrange`).
pub type DatetimeRange = Range<DateTime>;

impl<T: Copy + Ord> Range<T> {
    /// The range from `lower` to `upper` (`None`: unbounded on that side). Any pair is accepted
    /// here; whether it is empty depends on the bounds of the field it is written to, which
    /// refuses empty ranges.
    pub const fn new(lower: Option<T>, upper: Option<T>) -> Self {
        Self { lower, upper }
    }

    /// The lower end, `None` when unbounded.
    pub fn lower(&self) -> Option<T> {
        self.lower
    }

    /// The upper end, `None` when unbounded.
    pub fn upper(&self) -> Option<T> {
        self.upper
    }

    /// Whether the range holds nothing under `bounds` (both ends given, and the lower one after
    /// the upper one, or equal under `[)`). A range with an unbounded side is never empty.
    pub fn is_empty(&self, bounds: RangeBounds) -> bool {
        match (&self.lower, &self.upper) {
            (Some(lower), Some(upper)) => bounds.is_empty(lower, upper),
            _ => false,
        }
    }

    /// Whether two non-empty ranges with the same `bounds` share a point (PostgreSQL's `&&`).
    pub fn overlaps(&self, other: &Self, bounds: RangeBounds) -> bool {
        if self.is_empty(bounds) || other.is_empty(bounds) {
            return false;
        }
        // each range starts before the other one ends
        starts_before_end(self.lower.as_ref(), other.upper.as_ref(), bounds)
            && starts_before_end(other.lower.as_ref(), self.upper.as_ref(), bounds)
    }

    /// Whether `point` is in the range under `bounds` (PostgreSQL's `@>` with an element).
    pub fn contains(&self, point: T, bounds: RangeBounds) -> bool {
        let after_lower = self.lower.is_none_or(|lower| lower <= point);
        let before_upper = self.upper.is_none_or(|upper| match bounds {
            RangeBounds::HalfOpen => point < upper,
            RangeBounds::Closed => point <= upper,
        });
        after_lower && before_upper && !self.is_empty(bounds)
    }

    /// Map both ends.
    pub fn map<U: Copy + Ord>(self, mut f: impl FnMut(T) -> U) -> Range<U> {
        Range {
            lower: self.lower.map(&mut f),
            upper: self.upper.map(&mut f),
        }
    }
}

/// Whether a range starting at `lower` begins before a range ending at `upper` ends (`None`:
/// unbounded, which always does).
fn starts_before_end<T: Ord>(lower: Option<&T>, upper: Option<&T>, bounds: RangeBounds) -> bool {
    match (lower, upper) {
        (Some(lower), Some(upper)) => match bounds {
            RangeBounds::HalfOpen => lower.cmp(upper) == Ordering::Less,
            RangeBounds::Closed => lower.cmp(upper) != Ordering::Greater,
        },
        _ => true,
    }
}

/// A range on the dynamic surface ([`crate::Value::Range`]): JSON-2, Luau, data files and reads
/// carry it as `[lower, upper]` with `false` for an unbounded end.
///
// Deliberately exhaustive: one variant per range field kind, so a new kind is a compile error
// wherever dynamic values are converted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RangeValue {
    /// A range of dates.
    Date(DateRange),
    /// A range of naive-UTC datetimes.
    Datetime(DatetimeRange),
}

impl RangeValue {
    /// The ends as `[lower, upper]` text, `None` for an unbounded end (the JSON and repr forms).
    pub fn ends_text(&self) -> [Option<String>; 2] {
        match self {
            Self::Date(r) => [
                r.lower().map(|d| d.to_string()),
                r.upper().map(|d| d.to_string()),
            ],
            Self::Datetime(r) => [
                r.lower().map(|d| d.to_string()),
                r.upper().map(|d| d.to_string()),
            ],
        }
    }
}

impl From<DateRange> for RangeValue {
    fn from(range: DateRange) -> Self {
        Self::Date(range)
    }
}

impl From<DatetimeRange> for RangeValue {
    fn from(range: DatetimeRange) -> Self {
        Self::Datetime(range)
    }
}

impl fmt::Display for RangeValue {
    /// Python `repr()`-like, as a two-element list: `['2026-01-01', False]`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let [lower, upper] = self.ends_text();
        let end = |e: &Option<String>| {
            e.as_ref()
                .map_or_else(|| "False".to_owned(), |t| format!("'{t}'"))
        };
        write!(f, "[{}, {}]", end(&lower), end(&upper))
    }
}

/// An element type of a range (dates step by one day; datetimes are continuous).
#[cfg_attr(not(feature = "pg"), allow(dead_code))]
pub(crate) trait RangeElement: Copy + Ord {
    /// The next (`+1`) or previous (`-1`) element of a discrete type; `None` for a continuous one,
    /// or when the step leaves the type's range.
    fn step(self, delta: i32) -> Option<Self>;
}

impl RangeElement for Date {
    fn step(self, delta: i32) -> Option<Self> {
        self.days().checked_add(delta).map(Date::from_days)
    }
}

impl RangeElement for DateTime {
    fn step(self, _delta: i32) -> Option<Self> {
        None
    }
}

/// One end of a range as PostgreSQL stores it: the value and whether it is included.
#[cfg_attr(not(feature = "pg"), allow(dead_code))]
pub(crate) type StoredEnd<T> = Option<(T, bool)>;

/// A range as PostgreSQL returns it, turned into a [`Range`] with the declared `bounds`. Discrete
/// ranges (dates) are shifted by one day where the stored inclusivity differs from the declared one
/// (PostgreSQL stores every `daterange` as `[)`); a continuous range (datetimes) must already have
/// the declared inclusivity.
///
/// # Errors
///
/// A message when the stored range is empty, or when its inclusivity cannot be expressed with the
/// declared bounds (a datetime range written by raw SQL with other bounds).
#[cfg_attr(not(feature = "pg"), allow(dead_code))]
pub(crate) fn from_stored<T: RangeElement + fmt::Display>(
    empty: bool,
    lower: StoredEnd<T>,
    upper: StoredEnd<T>,
    bounds: RangeBounds,
) -> Result<Range<T>, String> {
    if empty {
        return Err("the database holds an empty range, which a range field never stores".into());
    }
    let mismatch = |end: &str, value: &T, inclusive: bool| {
        format!(
            "the database holds a range whose {end} end {value} is {} while the field declares \
             bounds '{bounds}'",
            if inclusive { "included" } else { "excluded" }
        )
    };
    // both declared bounds include the lower end
    let lower = match lower {
        None => None,
        Some((value, true)) => Some(value),
        Some((value, false)) => Some(
            value
                .step(1)
                .ok_or_else(|| mismatch("lower", &value, false))?,
        ),
    };
    let upper_included = matches!(bounds, RangeBounds::Closed);
    let upper = match upper {
        None => None,
        Some((value, inclusive)) if inclusive == upper_included => Some(value),
        Some((value, inclusive)) => {
            let delta = if upper_included { -1 } else { 1 };
            Some(
                value
                    .step(delta)
                    .ok_or_else(|| mismatch("upper", &value, inclusive))?,
            )
        }
    };
    Ok(Range::new(lower, upper))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(day: u32) -> Date {
        Date::from_ymd(2026, 1, day).unwrap()
    }

    #[test]
    fn bounds_parse_only_the_declared_forms() {
        assert_eq!(RangeBounds::parse("[)"), Some(RangeBounds::HalfOpen));
        assert_eq!(RangeBounds::parse("[]"), Some(RangeBounds::Closed));
        for other in ["()", "(]", "", "[ )", "[)x"] {
            assert_eq!(RangeBounds::parse(other), None, "{other:?}");
        }
        assert_eq!(RangeBounds::HalfOpen.to_string(), "[)");
    }

    #[test]
    fn emptiness_depends_on_the_bounds() {
        let same = DateRange::new(Some(d(1)), Some(d(1)));
        assert!(same.is_empty(RangeBounds::HalfOpen));
        assert!(!same.is_empty(RangeBounds::Closed));
        let inverted = DateRange::new(Some(d(2)), Some(d(1)));
        assert!(inverted.is_empty(RangeBounds::HalfOpen));
        assert!(inverted.is_empty(RangeBounds::Closed));
        assert!(!DateRange::new(None, Some(d(1))).is_empty(RangeBounds::HalfOpen));
        assert!(!DateRange::new(None, None).is_empty(RangeBounds::HalfOpen));
    }

    #[test]
    fn overlap_and_containment_follow_postgresql() {
        let hb = RangeBounds::HalfOpen;
        let cb = RangeBounds::Closed;
        let a = DateRange::new(Some(d(1)), Some(d(5)));
        let adjacent = DateRange::new(Some(d(5)), Some(d(9)));
        assert!(!a.overlaps(&adjacent, hb));
        assert!(a.overlaps(&adjacent, cb));
        let unbounded = DateRange::new(None, None);
        assert!(a.overlaps(&unbounded, hb));
        let empty = DateRange::new(Some(d(3)), Some(d(3)));
        assert!(!a.overlaps(&empty, hb), "an empty range overlaps nothing");
        assert!(a.contains(d(1), hb) && !a.contains(d(5), hb) && a.contains(d(5), cb));
        assert!(DateRange::new(Some(d(2)), None).contains(d(31), hb));
    }

    #[test]
    fn stored_ranges_take_the_declared_bounds() {
        // PostgreSQL returns a daterange declared `[]` as `[a, b+1)`
        let r = from_stored(
            false,
            Some((d(1), true)),
            Some((d(6), false)),
            RangeBounds::Closed,
        )
        .unwrap();
        assert_eq!(r, DateRange::new(Some(d(1)), Some(d(5))));
        let r = from_stored(
            false,
            Some((d(1), true)),
            Some((d(6), false)),
            RangeBounds::HalfOpen,
        )
        .unwrap();
        assert_eq!(r, DateRange::new(Some(d(1)), Some(d(6))));
        let open = from_stored::<Date>(false, None, None, RangeBounds::Closed).unwrap();
        assert_eq!(open, DateRange::new(None, None));
        // a datetime range cannot be shifted
        let t = d(1).at_midnight();
        let err = from_stored(false, Some((t, false)), None, RangeBounds::HalfOpen).unwrap_err();
        assert_eq!(
            err,
            "the database holds a range whose lower end 2026-01-01 00:00:00 is excluded while the \
             field declares bounds '[)'"
        );
        let err = from_stored::<Date>(true, None, None, RangeBounds::HalfOpen).unwrap_err();
        assert_eq!(
            err,
            "the database holds an empty range, which a range field never stores"
        );
        // stepping out of the representable days is an error, not a wrap
        let last = Date::from_days(i32::MAX);
        let err = from_stored(false, Some((last, false)), None, RangeBounds::HalfOpen).unwrap_err();
        assert!(
            err.starts_with("the database holds a range whose lower end"),
            "{err}"
        );
    }

    #[test]
    fn range_values_render_as_lists() {
        let v = RangeValue::Date(DateRange::new(Some(d(1)), None));
        assert_eq!(v.to_string(), "['2026-01-01', False]");
        assert_eq!(v.ends_text(), [Some("2026-01-01".to_owned()), None]);
    }
}
