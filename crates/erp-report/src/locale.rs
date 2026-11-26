// SPDX-License-Identifier: MIT OR Apache-2.0
//! Bounded, immutable locale data. Text is data, never a template or an expression.
use serde::Deserialize;
use std::sync::OnceLock;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LocaleText {
    pub(crate) months_short: [String; 12],
    pub(crate) months_long: [String; 12],
    pub(crate) days_short: [String; 7],
    pub(crate) days_long: [String; 7],
    pub(crate) periods: [String; 2],
    plural_rule: Plural,
    duration: Widths,
}
#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Plural {
    One,
    Other,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Patterns {
    one: String,
    other: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Widths {
    long: Units,
    short: Units,
    narrow: Units,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Units {
    year: Patterns,
    month: Patterns,
    week: Patterns,
    day: Patterns,
    hour: Patterns,
    minute: Patterns,
    second: Patterns,
}
impl Units {
    fn all(&self) -> [&Patterns; 7] {
        [
            &self.year,
            &self.month,
            &self.week,
            &self.day,
            &self.hour,
            &self.minute,
            &self.second,
        ]
    }
    fn get(&self, name: &str) -> Option<&Patterns> {
        match name {
            "year" => Some(&self.year),
            "month" => Some(&self.month),
            "week" => Some(&self.week),
            "day" => Some(&self.day),
            "hour" => Some(&self.hour),
            "minute" => Some(&self.minute),
            "second" => Some(&self.second),
            _ => None,
        }
    }
}
/// Seconds per semantic unit, descending; the reference defines months/years as fixed spans.
pub(crate) const UNITS: [(&str, u64); 7] = [
    ("year", 31_536_000),
    ("month", 2_592_000),
    ("week", 604_800),
    ("day", 86_400),
    ("hour", 3_600),
    ("minute", 60),
    ("second", 1),
];
impl LocaleText {
    pub(crate) fn parse(json: &str) -> Result<Self, &'static str> {
        // Admission bounds for this data protocol, not tunable output preferences.
        if json.len() > 65_536 {
            return Err("locale data exceeds 65536 bytes");
        }
        let text: Self = serde_json::from_str(json).map_err(|_| "invalid locale data shape")?;
        let valid = |s: &str| !s.is_empty() && s.len() <= 256 && !s.chars().any(char::is_control);
        if text
            .months_short
            .iter()
            .chain(&text.months_long)
            .chain(&text.days_short)
            .chain(&text.days_long)
            .chain(&text.periods)
            .any(|s| !valid(s))
        {
            return Err("locale names must be nonempty, at most 256 bytes and contain no controls");
        }
        for units in [
            &text.duration.long,
            &text.duration.short,
            &text.duration.narrow,
        ] {
            for pair in units.all() {
                for pattern in [&pair.one, &pair.other] {
                    if !valid(pattern)
                        || pattern.matches("{0}").count() != 1
                        || pattern.replace("{0}", "").contains(['{', '}'])
                    {
                        return Err(
                            "locale unit pattern must have one {0} slot and at most 256 bytes without controls",
                        );
                    }
                }
            }
        }
        Ok(text)
    }
    pub(crate) fn duration(
        &self,
        width: &str,
        unit: &str,
        count: u64,
    ) -> Result<String, &'static str> {
        let units = match width {
            "long" => Some(&self.duration.long),
            "short" => Some(&self.duration.short),
            "narrow" => Some(&self.duration.narrow),
            _ => None,
        };
        let pair = units
            .and_then(|units| units.get(unit))
            .ok_or("duration format must be long, short or narrow")?;
        let pattern = if count == 1 && matches!(self.plural_rule, Plural::One) {
            &pair.one
        } else {
            &pair.other
        };
        Ok(pattern.replace("{0}", &count.to_string()))
    }
}

pub(crate) fn builtin(code: &str) -> Result<&'static LocaleText, &'static str> {
    static EN: OnceLock<Result<LocaleText, &'static str>> = OnceLock::new();
    static FR: OnceLock<Result<LocaleText, &'static str>> = OnceLock::new();
    static ID: OnceLock<Result<LocaleText, &'static str>> = OnceLock::new();
    let (cell, source) = match code {
        "en" | "en_US" => (&EN, include_str!("../locales/en.json")),
        "fr" | "fr_FR" => (&FR, include_str!("../locales/fr.json")),
        "id" | "id_ID" => (&ID, include_str!("../locales/id.json")),
        _ => {
            return Err(
                "no report locale text resource; supply locale data or install a supported locale",
            );
        }
    };
    cell.get_or_init(|| LocaleText::parse(source))
        .as_ref()
        .map_err(|reason| *reason)
}
