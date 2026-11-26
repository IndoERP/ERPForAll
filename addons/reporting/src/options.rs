// SPDX-License-Identifier: MIT OR Apache-2.0
//! The options pipeline (design §5): the request's options are a closed JSON
//! object; each shared initializer reads its key in order and either keeps the given value or
//! uses the report's default. Asking for options (`erp_report_options`) carries previous options
//! over and drops what no longer applies; computing lines (`erp_report_lines`, expand, drill) is
//! strict and refuses such a value by name. A malformed value is refused in both.
use std::collections::{BTreeMap, BTreeSet};

use erp_sdk::ErrorKind;
use erp_sdk::prelude::*;
use erp_sdk::serde_json::{Map, Value as Json, json};

use crate::definition::{self, HideZero, ReportDef};
use crate::line::{LineId, SegmentKind};
use crate::settings::{self, Bounds};

/// Characters of the search text: a protocol bound of the filter bar.
pub const MAX_SEARCH_CHARS: usize = 256;

const KEYS: [&str; 13] = [
    "report",
    "companies",
    "date",
    "comparison",
    "unfold_all",
    "unfolded",
    "loaded_pages",
    "hide_zero",
    "search",
    "provider",
    "order",
    // derived: sent back by the screen as it received them, recomputed here
    "column_groups",
    "columns",
];

/// One date or a date range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum DateMode {
    /// From a start date to an end date.
    Range,
    /// Positions at one date.
    Single,
}

/// A date preset (calendar periods; fiscal ones are accounting's).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Preset {
    /// Today (single mode only).
    Today,
    /// The current month.
    ThisMonth,
    /// The previous month.
    LastMonth,
    /// The current quarter (January start).
    ThisQuarter,
    /// The previous quarter.
    LastQuarter,
    /// The current year.
    ThisYear,
    /// The previous year.
    LastYear,
    /// An explicit date or range.
    Custom,
}

const PRESETS: [(&str, Preset); 8] = [
    ("today", Preset::Today),
    ("this_month", Preset::ThisMonth),
    ("last_month", Preset::LastMonth),
    ("this_quarter", Preset::ThisQuarter),
    ("last_quarter", Preset::LastQuarter),
    ("this_year", Preset::ThisYear),
    ("last_year", Preset::LastYear),
    ("custom", Preset::Custom),
];

impl Preset {
    /// Parse a preset name.
    ///
    /// # Errors
    /// `Validation` naming `key` and the value.
    pub fn parse(text: &str, key: &str) -> Result<Self> {
        PRESETS
            .iter()
            .find(|(name, _)| *name == text)
            .map(|(_, preset)| *preset)
            .ok_or_else(|| Error::validation(format!("{key}: '{text}' is not a date preset.")))
    }
    fn as_str(self) -> &'static str {
        PRESETS
            .iter()
            .find(|(_, preset)| *preset == self)
            // every preset is in the table
            .map_or("custom", |(name, _)| name)
    }
    /// Whether `mode` offers it.
    pub fn offered(self, mode: DateMode) -> bool {
        self != Preset::Today || mode == DateMode::Single
    }
}

/// The dates of one column group: `from` is `None` in single mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Period {
    /// First day, inclusive.
    pub from: Option<Date>,
    /// Last day, inclusive.
    pub to: Date,
}

/// How comparison periods are chosen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ComparisonMode {
    /// No comparison.
    None,
    /// The periods before the main one, each as long as it.
    PreviousPeriod,
    /// The same period in the years before.
    SameLastYear,
    /// One explicit period.
    Custom,
}

/// The sort of the lines: a report column by its label.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Order {
    /// The column's label.
    pub column: String,
    /// Largest first.
    pub descending: bool,
}

/// The resolved options of one call.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct Options {
    /// The report.
    pub report: i64,
    /// The selected companies, all allowed for the caller.
    pub companies: Vec<i64>,
    /// The report's date mode.
    pub date_mode: DateMode,
    /// The chosen preset.
    pub preset: Preset,
    /// The main period.
    pub period: Period,
    /// The comparison mode.
    pub comparison: ComparisonMode,
    /// Comparison periods asked for.
    pub periods: u32,
    /// The main period last instead of first.
    pub ascending: bool,
    /// The custom comparison period.
    pub comparison_custom: Option<Period>,
    /// Unfold every line.
    pub unfold_all: bool,
    /// The unfolded lines.
    pub unfolded: Vec<LineId>,
    /// Additional pages requested for each unfolded parent, keyed by canonical line ID.
    pub loaded_pages: BTreeMap<String, u64>,
    /// Hide zero lines (on screen in the browser; in exports on the server).
    pub hide_zero: bool,
    /// The search text.
    pub search: String,
    /// The provider's own options, checked by the provider.
    pub provider: Map<String, Json>,
    /// The periods of the column groups, in display order.
    pub column_groups: Vec<Period>,
    /// The sort, if any.
    pub order: Option<Order>,
}

impl Options {
    /// The period of the main column group.
    pub fn main_group(&self) -> usize {
        if self.ascending {
            self.column_groups.len().saturating_sub(1)
        } else {
            0
        }
    }

    /// The options as the screen gets them, with the derived `columns`.
    pub(crate) fn to_json(&self, def: &ReportDef) -> Json {
        let date = |d: Option<Date>| d.map(text);
        let columns: Vec<Json> = (0..self.column_groups.len())
            .flat_map(|group| {
                def.columns.iter().map(move |c| {
                    json!({"group": group, "name": c.name, "label": c.label,
                           "kind": c.kind.as_str(), "sortable": def.can_sort(c)})
                })
            })
            .collect();
        let custom = self.comparison_custom;
        json!({
            "report": self.report,
            "companies": self.companies,
            "date": {"mode": mode_name(self.date_mode), "preset": self.preset.as_str(),
                     "from": date(self.period.from), "to": text(self.period.to)},
            "comparison": {
                "mode": match self.comparison {
                    ComparisonMode::None => "none",
                    ComparisonMode::PreviousPeriod => "previous_period",
                    ComparisonMode::SameLastYear => "same_last_year",
                    ComparisonMode::Custom => "custom",
                },
                "periods": self.periods,
                "order": if self.ascending { "ascending" } else { "descending" },
                "from": date(custom.and_then(|p| p.from)),
                "to": custom.map(|p| text(p.to)),
            },
            "unfold_all": self.unfold_all,
            "unfolded": self.unfolded.iter().map(ToString::to_string).collect::<Vec<_>>(),
            "loaded_pages": self.loaded_pages,
            "hide_zero": self.hide_zero,
            "search": self.search,
            "provider": self.provider,
            "column_groups": self.column_groups.iter()
                .map(|p| json!({"from": date(p.from), "to": text(p.to)})).collect::<Vec<_>>(),
            "columns": columns,
            "order": self.order.as_ref().map(|o| json!({"column": o.column,
                "direction": if o.descending { "desc" } else { "asc" }})),
        })
    }
}

fn mode_name(mode: DateMode) -> &'static str {
    match mode {
        DateMode::Range => "range",
        DateMode::Single => "single",
    }
}

/// `YYYY-MM-DD`.
pub(crate) fn text(date: Date) -> String {
    let (y, m, d) = date.ymd();
    format!("{y:04}-{m:02}-{d:02}")
}

fn bad(message: impl Into<String>) -> Error {
    Error::validation(message.into())
}

fn date_of(value: Option<&Json>, key: &str) -> Result<Option<Date>> {
    match value {
        // JSON2's dynamic empty value returns through the model bridge as false.
        // Normalized options must survive that round trip for expand and restore.
        None | Some(Json::Null | Json::Bool(false)) => Ok(None),
        Some(Json::String(s)) => Date::parse_odoo(s).map(Some).ok_or_else(|| {
            bad(format!(
                "{key}: '{s}' must be written as year-month-day, such as 2026-11-30."
            ))
        }),
        Some(_) => Err(bad(format!(
            "{key} must be a text date written as year-month-day."
        ))),
    }
}

fn object<'a>(value: &'a Json, key: &str) -> Result<&'a Map<String, Json>> {
    value
        .as_object()
        .ok_or_else(|| bad(format!("{key} is not an object.")))
}

fn closed(map: &Map<String, Json>, key: &str, allowed: &[&str]) -> Result<()> {
    match map.keys().find(|k| !allowed.contains(&k.as_str())) {
        Some(unknown) => Err(bad(format!("{key}.{unknown} is not an option."))),
        None => Ok(()),
    }
}

fn flag(map: &Map<String, Json>, key: &str) -> Result<Option<bool>> {
    match map.get(key) {
        None | Some(Json::Null) => Ok(None),
        Some(Json::Bool(b)) => Ok(Some(*b)),
        Some(_) => Err(bad(format!("options.{key} is not true or false."))),
    }
}

/// The period of `preset` around `today`; `None` for `custom` or a date out of range.
pub fn preset_period(preset: Preset, mode: DateMode, today: Date) -> Option<Period> {
    let (y, m, _) = today.ymd();
    let quarter = (m - 1) / 3 * 3 + 1;
    let (from, to) = match preset {
        Preset::Today => (today, today),
        Preset::ThisMonth => (Date::from_ymd(y, m, 1)?, month_end(y, m)?),
        Preset::LastMonth => {
            let first = add_months(Date::from_ymd(y, m, 1)?, -1)?;
            let (py, pm, _) = first.ymd();
            (first, month_end(py, pm)?)
        }
        Preset::ThisQuarter => (Date::from_ymd(y, quarter, 1)?, month_end(y, quarter + 2)?),
        Preset::LastQuarter => {
            let first = add_months(Date::from_ymd(y, quarter, 1)?, -3)?;
            let (py, pm, _) = first.ymd();
            (first, month_end(py, pm + 2)?)
        }
        Preset::ThisYear => (Date::from_ymd(y, 1, 1)?, Date::from_ymd(y, 12, 31)?),
        Preset::LastYear => (Date::from_ymd(y - 1, 1, 1)?, Date::from_ymd(y - 1, 12, 31)?),
        Preset::Custom => return None,
    };
    Some(Period {
        from: (mode == DateMode::Range).then_some(from),
        to,
    })
}

fn month_end(year: i32, month: u32) -> Option<Date> {
    let next = add_months(Date::from_ymd(year, month, 1)?, 1)?;
    Some(Date::from_days(next.days().checked_sub(1)?))
}

fn is_month_end(date: Date) -> bool {
    let (y, m, _) = date.ymd();
    month_end(y, m) == Some(date)
}

/// `date` moved by `months`, the day clamped to the target month's length.
fn add_months(date: Date, months: i32) -> Option<Date> {
    let (y, m, d) = date.ymd();
    let index = y
        .checked_mul(12)?
        .checked_add(i32::try_from(m).ok()? - 1)?
        .checked_add(months)?;
    let (ny, nm) = (
        index.div_euclid(12),
        u32::try_from(index.rem_euclid(12)).ok()? + 1,
    );
    (1..=d).rev().find_map(|day| Date::from_ymd(ny, nm, day))
}

fn shift_days(date: Date, days: i32) -> Option<Date> {
    Some(Date::from_days(date.days().checked_add(days)?))
}

/// The `k`-th comparison period before `main`.
pub fn compare(main: Period, mode: ComparisonMode, k: i32) -> Option<Period> {
    let months = |p: Period| -> Option<i32> {
        let from = p.from?;
        let (fy, fm, fd) = from.ymd();
        let (ty, tm, _) = p.to.ymd();
        (fd == 1 && is_month_end(p.to))
            .then_some(())
            .and_then(|()| {
                Some((ty - fy) * 12 + i32::try_from(tm).ok()? - i32::try_from(fm).ok()? + 1)
            })
    };
    let end_of = |to: Date, by: i32| -> Option<Date> {
        if is_month_end(to) {
            let (y, m, _) = add_months(to, by)?.ymd();
            month_end(y, m)
        } else {
            add_months(to, by)
        }
    };
    match mode {
        ComparisonMode::None | ComparisonMode::Custom => None,
        ComparisonMode::SameLastYear => {
            let by = k.checked_mul(-12)?;
            Some(Period {
                from: match main.from {
                    Some(from) => Some(add_months(from, by)?),
                    None => None,
                },
                to: end_of(main.to, by)?,
            })
        }
        ComparisonMode::PreviousPeriod => match (main.from, months(main)) {
            (Some(from), Some(n)) => {
                let by = n.checked_mul(-k)?;
                Some(Period {
                    from: Some(add_months(from, by)?),
                    to: end_of(main.to, by)?,
                })
            }
            (None, _) if is_month_end(main.to) => Some(Period {
                from: None,
                to: end_of(main.to, k.checked_neg()?)?,
            }),
            (from, _) => {
                let length = match from {
                    Some(from) => main.to.days().checked_sub(from.days())?.checked_add(1)?,
                    None => 1,
                };
                let by = length.checked_mul(-k)?;
                Some(Period {
                    from: match from {
                        Some(from) => Some(shift_days(from, by)?),
                        None => None,
                    },
                    to: shift_days(main.to, by)?,
                })
            }
        },
    }
}

/// Bound the work of all requested page replays together, before asking a provider for rows.
fn loaded_pages(
    input: &Map<String, Json>,
    def: &ReportDef,
    bounds: &Bounds,
    unfolded: &[LineId],
    unfold_all: bool,
    same_report: bool,
    strict: bool,
) -> Result<BTreeMap<String, u64>> {
    let mut pages = BTreeMap::new();
    let Some(given) = input.get("loaded_pages") else {
        return Ok(pages);
    };
    let given = object(given, "options.loaded_pages")?;
    let max_pages = bounds.max_screen_lines as u64 / def.load_more_limit;
    let too_many = || {
        bad(format!(
            "options.loaded_pages exceeds the {}-line screen bound (setting {}).",
            bounds.max_screen_lines,
            settings::MAX_SCREEN_LINES
        ))
    };
    if given.len() as u64 > max_pages {
        return Err(too_many());
    }
    let open: BTreeSet<String> = unfolded.iter().map(ToString::to_string).collect();
    let mut total = 0_u64;
    for (key, value) in given {
        let count = value
            .as_u64()
            .filter(|n| *n > 0)
            .ok_or_else(|| bad("options.loaded_pages counts must be positive integers."))?;
        total = total
            .checked_add(count)
            .filter(|n| *n <= max_pages)
            .ok_or_else(too_many)?;
        let id = LineId::parse(key)?;
        if id.to_string() != *key {
            return Err(bad("options.loaded_pages needs canonical parent line IDs."));
        }
        let parent = matches!(
            id.segments().last().map(|s| s.kind()),
            Some(SegmentKind::Line(_) | SegmentKind::Group(_))
        );
        if !same_report || id.report() != def.id || !parent || !(unfold_all || open.contains(key)) {
            if strict {
                return Err(bad(format!(
                    "options.loaded_pages: {key} is not an unfolded parent of this report."
                )));
            }
            continue;
        }
        if def.provider == definition::DEFINITION
            && let Err(error) = definition::check_page_parent(def, &id, bounds, |parent| {
                unfold_all || open.contains(&parent.to_string())
            })
        {
            if strict || error.kind() != ErrorKind::Validation {
                return Err(error);
            }
            continue;
        }
        pages.insert(key.clone(), count);
    }
    Ok(pages)
}

/// Resolve `input` for `def`. `strict` refuses a value this report cannot use instead of
/// replacing it with the default.
///
/// # Errors
/// `Validation` naming the key: an unknown key, a malformed value, a date `from` after `to`, a
/// preset the report's mode does not offer, a company the caller may not use, a number of periods
/// outside `1..=reporting.max_comparison_periods`, a search text over [`MAX_SEARCH_CHARS`];
/// when `strict`, any value this report does not offer.
pub(crate) async fn resolve(
    env: &Env,
    tx: &mut Transaction,
    def: &ReportDef,
    input: &Json,
    strict: bool,
    bounds: &Bounds,
) -> Result<Options> {
    let empty = Map::new();
    let input = match input {
        Json::Null => &empty,
        other => object(other, "options")?,
    };
    closed(input, "options", &KEYS)?;
    let refuse =
        |message: String| -> Result<()> { if strict { Err(bad(message)) } else { Ok(()) } };

    // report (order 0)
    let same_report = match input.get("report") {
        None if strict => return Err(bad("options.report is required.")),
        None => true,
        Some(value) => {
            let id = value
                .as_i64()
                .ok_or_else(|| bad("options.report is not a report id."))?;
            if id != def.id {
                refuse(format!(
                    "options.report is {id}, not this report ({}).",
                    def.id
                ))?;
            }
            id == def.id
        }
    };

    // companies (order 10)
    let allowed: Vec<i64> = tx.companies(env).await?.iter().map(|c| c.get()).collect();
    let mut companies = Vec::new();
    if let Some(given) = input.get("companies") {
        let given = given
            .as_array()
            .ok_or_else(|| bad("options.companies is not a list of ids."))?;
        for id in given {
            let id = id
                .as_i64()
                .ok_or_else(|| bad("options.companies is not a list of ids."))?;
            if !allowed.contains(&id) {
                if strict {
                    return Err(bad(format!(
                        "options.companies: company {id} is not one of the companies you may use."
                    )));
                }
            } else if !companies.contains(&id) {
                companies.push(id);
            }
        }
        if companies.len() > 1 && !def.multi_company {
            refuse("options.companies: this report covers one company.".into())?;
            companies.truncate(1);
        }
    }
    if companies.is_empty() {
        companies = if def.multi_company {
            allowed.clone()
        } else {
            allowed.first().copied().into_iter().collect()
        };
    }

    // date (order 30)
    let today = env.context_today()?;
    let default_period = preset_period(def.date_default, def.date_mode, today)
        .ok_or_else(|| Error::internal("the report's default date preset gives no period"))?;
    let (mut preset, mut period) = (def.date_default, default_period);
    if let Some(date) = input.get("date") {
        let date = object(date, "options.date")?;
        closed(date, "options.date", &["mode", "preset", "from", "to"])?;
        let mode_ok = match date.get("mode") {
            None | Some(Json::Null) => true,
            Some(Json::String(m)) if m == "range" || m == "single" => m == mode_name(def.date_mode),
            Some(_) => return Err(bad("options.date.mode is not range or single.")),
        };
        let given = match date.get("preset") {
            None | Some(Json::Null) => def.date_default,
            Some(Json::String(p)) => Preset::parse(p, "options.date.preset")?,
            Some(_) => return Err(bad("options.date.preset is not a preset name.")),
        };
        let from = date_of(date.get("from"), "options.date.from")?;
        let to = date_of(date.get("to"), "options.date.to")?;
        if let (Some(from), Some(to)) = (from, to)
            && from > to
        {
            return Err(bad("options.date: from is after to."));
        }
        if !mode_ok {
            refuse(format!(
                "options.date.mode: this report uses {}.",
                mode_name(def.date_mode)
            ))?;
        } else if def.current_only
            && (given != Preset::Today || from.is_some() || to.is_some_and(|to| to != today))
        {
            refuse("options.date: this report reads current stock or balances. Open it with Today to refresh.".into())?;
        } else if !given.offered(def.date_mode) {
            refuse(format!(
                "options.date.preset: {} is not offered in {} mode.",
                given.as_str(),
                mode_name(def.date_mode)
            ))?;
        } else if given == Preset::Custom {
            let to = to.ok_or_else(|| bad("options.date.to is required for a custom date."))?;
            let from = match def.date_mode {
                DateMode::Range => Some(
                    from.ok_or_else(|| bad("options.date.from is required for a custom range."))?,
                ),
                DateMode::Single => None,
            };
            (preset, period) = (given, Period { from, to });
        } else {
            let found = preset_period(given, def.date_mode, today)
                .ok_or_else(|| bad("options.date.preset gives no period for today."))?;
            (preset, period) = (given, found);
        }
    }

    // comparison (order 50)
    let (mut comparison, mut periods, mut ascending, mut comparison_custom) =
        (ComparisonMode::None, 1_u32, false, None);
    if let Some(given) = input.get("comparison") {
        let given = object(given, "options.comparison")?;
        closed(
            given,
            "options.comparison",
            &["mode", "periods", "order", "from", "to"],
        )?;
        let mode = match given.get("mode").and_then(Json::as_str) {
            None | Some("none") => ComparisonMode::None,
            Some("previous_period") => ComparisonMode::PreviousPeriod,
            Some("same_last_year") => ComparisonMode::SameLastYear,
            Some("custom") => ComparisonMode::Custom,
            Some(other) => {
                return Err(bad(format!(
                    "options.comparison.mode: '{other}' is not a comparison."
                )));
            }
        };
        let count = match given.get("periods") {
            None | Some(Json::Null) => 1,
            Some(n) => n
                .as_u64()
                .and_then(|n| u32::try_from(n).ok())
                .ok_or_else(|| bad("options.comparison.periods is not a number."))?,
        };
        if !(1..=bounds.max_comparison_periods).contains(&count) {
            return Err(bad(format!(
                "options.comparison.periods must be from 1 to {} (setting {}).",
                bounds.max_comparison_periods,
                settings::MAX_COMPARISON_PERIODS
            )));
        }
        let asc = match given.get("order").and_then(Json::as_str) {
            None | Some("descending") => false,
            Some("ascending") => true,
            Some(other) => {
                return Err(bad(format!(
                    "options.comparison.order: '{other}' is not ascending or descending."
                )));
            }
        };
        let from = date_of(given.get("from"), "options.comparison.from")?;
        let to = date_of(given.get("to"), "options.comparison.to")?;
        if let (Some(from), Some(to)) = (from, to)
            && from > to
        {
            return Err(bad("options.comparison: from is after to."));
        }
        if mode != ComparisonMode::None && !def.comparison {
            refuse("options.comparison: this report offers no comparison.".into())?;
        } else {
            if mode == ComparisonMode::Custom {
                let to = to.ok_or_else(|| {
                    bad("options.comparison.to is required for a custom comparison.")
                })?;
                let from = match def.date_mode {
                    DateMode::Range => Some(from.ok_or_else(|| {
                        bad("options.comparison.from is required for a custom comparison.")
                    })?),
                    DateMode::Single => None,
                };
                comparison_custom = Some(Period { from, to });
            }
            (comparison, periods, ascending) = (mode, count, asc);
        }
    }
    let mut groups = vec![period];
    match comparison {
        ComparisonMode::None => {}
        ComparisonMode::Custom => groups.extend(comparison_custom),
        mode => {
            for k in 1..=periods {
                let k = i32::try_from(k)
                    .map_err(|_| bad("options.comparison.periods is too large."))?;
                groups.push(compare(period, mode, k).ok_or_else(|| {
                    bad("options.comparison: a comparison period is out of the calendar.")
                })?);
            }
        }
    }
    if ascending {
        groups.reverse();
    }

    // unfold_all, unfolded (order 200)
    let mut unfold_all = flag(input, "unfold_all")?.unwrap_or(false);
    if unfold_all && !def.unfold_all {
        refuse("options.unfold_all: this report does not offer unfold all.".into())?;
        unfold_all = false;
    }
    let mut unfolded = Vec::new();
    if let Some(given) = input.get("unfolded") {
        let given = given
            .as_array()
            .ok_or_else(|| bad("options.unfolded is not a list of line ids."))?;
        if given.len() > bounds.max_screen_lines {
            return Err(bad(format!(
                "options.unfolded lists more than {} lines (setting {}).",
                bounds.max_screen_lines,
                settings::MAX_SCREEN_LINES
            )));
        }
        for id in given {
            let id = id
                .as_str()
                .ok_or_else(|| bad("options.unfolded is not a list of line ids."))?;
            let id = LineId::parse(id)?;
            if id.report() != def.id {
                refuse(format!(
                    "options.unfolded: line {id} is not a line of this report."
                ))?;
            } else if same_report && !unfolded.contains(&id) {
                unfolded.push(id);
            }
        }
    }

    let loaded_pages = loaded_pages(
        input,
        def,
        bounds,
        &unfolded,
        unfold_all,
        same_report,
        strict,
    )?;

    // hide_zero (order 200)
    let hide_zero = match (def.hide_zero, flag(input, "hide_zero")?) {
        (HideZero::Never, Some(true)) => {
            refuse("options.hide_zero: this report never hides zero lines.".into())?;
            false
        }
        (HideZero::Never, _) => false,
        (_, Some(given)) => given,
        (flagged, None) => flagged == HideZero::ByDefault,
    };

    // search (order 200)
    let mut search = match input.get("search") {
        None | Some(Json::Null) => String::new(),
        Some(Json::String(s)) => s.clone(),
        Some(_) => return Err(bad("options.search is not text.")),
    };
    if search.chars().count() > MAX_SEARCH_CHARS {
        return Err(bad(format!(
            "options.search is longer than {MAX_SEARCH_CHARS} characters."
        )));
    }
    if !search.is_empty() && !def.search_bar {
        refuse("options.search: this report has no search bar.".into())?;
        search.clear();
    }

    // provider (order 300): checked by the provider's own initializer
    let provider = match input.get("provider") {
        None | Some(Json::Null) => Map::new(),
        Some(given) => object(given, "options.provider")?.clone(),
    };

    // order (order 1020)
    let order = match input.get("order") {
        None | Some(Json::Null | Json::Bool(false)) => None,
        Some(given) => {
            let given = object(given, "options.order")?;
            closed(given, "options.order", &["column", "direction"])?;
            let column = given
                .get("column")
                .and_then(Json::as_str)
                .ok_or_else(|| bad("options.order.column is not a column label."))?;
            let descending = match given.get("direction").and_then(Json::as_str) {
                Some("desc") => true,
                Some("asc") => false,
                _ => return Err(bad("options.order.direction is not asc or desc.")),
            };
            match def.columns.iter().find(|c| c.label == column) {
                Some(c) if def.can_sort(c) => Some(Order {
                    column: column.to_owned(),
                    descending,
                }),
                _ => {
                    refuse(format!("options.order: column {column} is not sortable."))?;
                    None
                }
            }
        }
    };

    Ok(Options {
        report: def.id,
        companies,
        date_mode: def.date_mode,
        preset,
        period,
        comparison,
        periods,
        ascending,
        comparison_custom,
        unfold_all,
        unfolded,
        loaded_pages,
        hide_zero,
        search,
        provider,
        column_groups: groups,
        order,
    })
}

#[cfg(test)]
#[path = "tests/options.rs"]
mod tests;
