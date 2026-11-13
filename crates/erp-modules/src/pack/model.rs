// SPDX-License-Identifier: MIT OR Apache-2.0
//! Typed values and the model schemas a pack's records are checked against.

use std::collections::BTreeMap;
use std::fmt;

use sha2::{Digest, Sha256};

use super::{PackRule, Res, err};

/// A calendar date. Parsed strictly as `YYYY-MM-DD`; there is no "today" anywhere in this module.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Date {
    year: u16,
    month: u8,
    day: u8,
}

impl Date {
    /// Parse `YYYY-MM-DD` (year 1000..=9999, real calendar day).
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let b = text.as_bytes();
        if b.len() != 10 || b[4] != b'-' || b[7] != b'-' {
            return None;
        }
        let num = |s: &str| -> Option<u16> {
            s.bytes().all(|c| c.is_ascii_digit()).then_some(())?;
            s.parse().ok()
        };
        let (year, month, day) = (num(&text[..4])?, num(&text[5..7])?, num(&text[8..])?);
        let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
        let days = match month {
            1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
            4 | 6 | 9 | 11 => 30,
            2 if leap => 29,
            2 => 28,
            _ => return None,
        };
        (year >= 1000 && (1..=days).contains(&day)).then_some(Self {
            year,
            month: u8::try_from(month).ok()?,
            day: u8::try_from(day).ok()?,
        })
    }
}

impl fmt::Display for Date {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:04}-{:02}-{:02}", self.year, self.month, self.day)
    }
}

/// The type of a field of a pack model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FieldType {
    Text,
    Int,
    Bool,
    Date,
    /// `decimal:<value> <unit>`, exact (range).
    Decimal,
    /// `ratio:<numerator>/<denominator> <unit>`, exact.
    Ratio,
    /// A reference to a record of the named model, by `key`, in the same template.
    Ref(String),
}

/// One field of a [`ModelSchema`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldSpec {
    pub name: String,
    pub ty: FieldType,
    /// The merged record must carry a value.
    pub required: bool,
    /// The field may be explicitly cleared (`clear_fields`) or left empty.
    pub nullable: bool,
}

/// The schema of one model of a pack: its fields, whether whether its rows are dated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelSchema {
    pub model: String,
    pub fields: Vec<FieldSpec>,
    /// Dated model: the scope fields; rows need `valid_from` (required date) and `valid_until`
    /// (nullable date, exclusive) and may not overlap within one scope.
    pub dated_scope: Option<Vec<String>>,
    /// Text field padded to the template's `code_digits` (an account's code).
    pub code_field: Option<String>,
    /// Boolean field that marks an account inactive when `false`; an absent value means active.
    /// Only active codes are padded: inactive accounts are typically parents and keep their code.
    pub active_field: Option<String>,
}

impl ModelSchema {
    /// The offline schema hash a pack declares in `[schemas]` (SHA-256 of a canonical description).
    #[must_use]
    pub fn digest(&self) -> String {
        let mut s = format!("{}|{:?}", self.model, self.dated_scope);
        // Absent for a model without a code field, so schemas of part 1 keep their hash.
        if let Some(c) = &self.code_field {
            s.push_str(&format!("|code:{c}"));
        }
        if let Some(a) = &self.active_field {
            s.push_str(&format!("|active:{a}"));
        }
        for f in &self.fields {
            s.push_str(&format!(
                "|{}:{:?}:{}:{}",
                f.name, f.ty, f.required, f.nullable
            ));
        }
        hex(&Sha256::digest(s.as_bytes()))
    }

    pub(crate) fn field(&self, name: &str) -> Option<&FieldSpec> {
        self.fields.iter().find(|f| f.name == name)
    }

    /// Check the schema itself: a dated model declares its date fields and scope fields.
    /// Errors: [`PackRule::UnknownField`] when a dated model lacks `valid_from`, `valid_until` or a scope field.
    pub fn validate(&self) -> Res<()> {
        let Some(scope) = &self.dated_scope else {
            return Ok(());
        };
        let ok = |n: &str, required: bool| {
            self.field(n).is_some_and(|f| {
                f.ty == FieldType::Date && f.required == required && f.nullable != required
            })
        };
        let missing = ["valid_from"].iter().any(|n| !ok(n, true))
            || !ok("valid_until", false)
            || scope.is_empty()
            || scope.iter().any(|n| self.field(n).is_none());
        if missing {
            return Err(err(
                &self.model,
                "schema",
                PackRule::UnknownField,
                "A dated model needs required date valid_from, nullable date valid_until and existing scope fields.",
            ));
        }
        Ok(())
    }
}

/// The offline schemas a loader accepts, by model name. Supplied by the addon that owns the models.
#[derive(Debug, Clone, Default)]
pub struct SchemaSet(pub(crate) BTreeMap<String, ModelSchema>);

impl SchemaSet {
    /// Build a set; every schema is validated.
    /// Errors: The first [`ModelSchema::validate`] refusal.
    pub fn new(models: Vec<ModelSchema>) -> Res<Self> {
        for m in &models {
            m.validate()?;
        }
        Ok(Self(
            models.into_iter().map(|m| (m.model.clone(), m)).collect(),
        ))
    }

    pub(crate) fn get(&self, model: &str) -> Option<&ModelSchema> {
        self.0.get(model)
    }
}

/// A typed field value of a pack record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    /// A field a child template cleared with `clear_fields`.
    Null,
    Text(String),
    Int(i64),
    Bool(bool),
    Date(Date),
    /// Exact decimal, canonical string (no leading or trailing zeros, no negative zero) and unit.
    Decimal {
        value: String,
        unit: String,
    },
    /// Exact ratio: signed numerator, positive denominator (not reduced) and unit.
    Ratio {
        numerator: String,
        denominator: String,
        unit: String,
    },
    /// Reference to a record: `pack` is the pinned dependency that holds it, `None` for this pack.
    Ref {
        pack: Option<String>,
        key: String,
    },
}

impl Value {
    /// Canonical text, used to compare scope values.
    #[must_use]
    pub fn canon(&self) -> String {
        match self {
            Self::Null => String::new(),
            Self::Text(s) => s.clone(),
            Self::Int(i) => i.to_string(),
            Self::Bool(b) => b.to_string(),
            Self::Date(d) => d.to_string(),
            Self::Decimal { value, unit } => format!("decimal:{value} {unit}"),
            Self::Ratio {
                numerator,
                denominator,
                unit,
            } => format!("ratio:{numerator}/{denominator} {unit}"),
            Self::Ref { pack: Some(p), key } => format!("{p}/{key}"),
            Self::Ref { pack: None, key } => key.clone(),
        }
    }
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// A raw field value as a file gave it, before typing.
#[derive(Debug, Clone)]
pub(crate) enum Raw {
    Str(String),
    Int(i64),
    Bool(bool),
    Float,
    Date(String),
    Other,
}

const MAX_RATIO_DIGITS: usize = 1232; // < 4096 bits
const MAX_SCALE: usize = 28;
const MAX_COEFF: u128 = (1u128 << 96) - 1;

fn exact(f: (&str, &str, &str), tagged: &str) -> Res<Value> {
    let (file, path, pack) = f;
    let bad = || {
        err(
            file,
            path,
            PackRule::ExactValue,
            format!(
                "Pack {pack} field {} requires an exact tagged value (decimal:<n> <unit> or ratio:<n>/<d> <unit>).",
                path.rsplit('.').next().unwrap_or(path)
            ),
        )
    };
    let bounds = || {
        err(
            file,
            path,
            PackRule::ExactBounds,
            "Exact pack value exceeds supported arithmetic bounds.",
        )
    };
    let (tag, rest) = tagged.split_once(':').ok_or_else(bad)?;
    let (num, unit) = rest.split_once(' ').ok_or_else(bad)?;
    let unit_ok = !unit.is_empty()
        && unit.len() <= 16
        && unit
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c == b'_' || c == b'%');
    if !unit_ok {
        return Err(bad());
    }
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|c| c.is_ascii_digit());
    let unit = unit.to_owned();
    match tag {
        "decimal" => {
            let (neg, body) = num.strip_prefix('-').map_or((false, num), |b| (true, b));
            let (int, frac) = body.split_once('.').unwrap_or((body, ""));
            if !digits(int) || (body.contains('.') && !digits(frac)) {
                return Err(bad());
            }
            let frac = frac.trim_end_matches('0');
            let all = format!("{int}{frac}");
            let coeff = all.trim_start_matches('0');
            if frac.len() > MAX_SCALE
                || coeff.len() > 29
                || coeff.parse::<u128>().is_ok_and(|c| c > MAX_COEFF)
            {
                return Err(bounds());
            }
            let int = int.trim_start_matches('0');
            let int = if int.is_empty() { "0" } else { int };
            let zero = coeff.is_empty();
            let value = format!(
                "{}{int}{}{frac}",
                if neg && !zero { "-" } else { "" },
                if frac.is_empty() { "" } else { "." }
            );
            Ok(Value::Decimal { value, unit })
        }
        "ratio" => {
            let (n, d) = num.split_once('/').ok_or_else(bad)?;
            let n = n.strip_prefix('-').unwrap_or(n);
            if !digits(n) || !digits(d) {
                return Err(bad());
            }
            if n.len() > MAX_RATIO_DIGITS || d.len() > MAX_RATIO_DIGITS {
                return Err(bounds());
            }
            if d.bytes().all(|c| c == b'0') {
                return Err(err(
                    file,
                    path,
                    PackRule::ExactValue,
                    "A ratio denominator must be positive.",
                ));
            }
            Ok(Value::Ratio {
                numerator: num.split('/').next().unwrap_or("").to_owned(),
                denominator: d.to_owned(),
                unit,
            })
        }
        _ => Err(bad()),
    }
}

/// Type one raw value for `spec`. CSV cells are all strings and are read as the field's type.
pub(crate) fn convert(
    pack: &str,
    file: &str,
    path: &str,
    spec: &FieldSpec,
    raw: &Raw,
    csv: bool,
    max_string: u64,
) -> Res<Value> {
    let wrong = |what: &str| {
        err(
            file,
            path,
            PackRule::Type,
            format!("Field {} expects {what}.", spec.name),
        )
    };
    // Every string value, whatever its type, is under the string cap before it is parsed.
    if let Raw::Str(s) = raw
        && u64::try_from(s.len()).map_or(true, |n| n > max_string)
    {
        return Err(err(
            file,
            path,
            PackRule::Limit,
            "Pack input exceeds configured resource limit.",
        ));
    }
    match (&spec.ty, raw) {
        (FieldType::Decimal | FieldType::Ratio, Raw::Float) => Err(err(
            file,
            path,
            PackRule::ExactValue,
            format!(
                "Pack {pack} field {} requires an exact tagged value; binary float is not allowed.",
                spec.name
            ),
        )),
        (FieldType::Decimal, Raw::Str(s)) => match exact((file, path, pack), s)? {
            v @ Value::Decimal { .. } => Ok(v),
            _ => Err(wrong("a decimal")),
        },
        (FieldType::Ratio, Raw::Str(s)) => match exact((file, path, pack), s)? {
            v @ Value::Ratio { .. } => Ok(v),
            _ => Err(wrong("a ratio")),
        },
        (FieldType::Decimal | FieldType::Ratio, _) => Err(err(
            file,
            path,
            PackRule::ExactValue,
            format!(
                "Pack {pack} field {} requires an exact tagged value.",
                spec.name
            ),
        )),
        (FieldType::Text, Raw::Str(s)) => Ok(Value::Text(s.clone())),
        (FieldType::Int, Raw::Int(i)) => Ok(Value::Int(*i)),
        (FieldType::Int, Raw::Str(s)) if csv => {
            s.parse().map(Value::Int).map_err(|_| wrong("an integer"))
        }
        (FieldType::Bool, Raw::Bool(b)) => Ok(Value::Bool(*b)),
        (FieldType::Bool, Raw::Str(s)) if csv => match s.as_str() {
            "true" => Ok(Value::Bool(true)),
            "false" => Ok(Value::Bool(false)),
            _ => Err(wrong("true or false")),
        },
        (FieldType::Date, Raw::Date(s) | Raw::Str(s)) => Date::parse(s)
            .map(Value::Date)
            .ok_or_else(|| wrong("a date YYYY-MM-DD")),
        (FieldType::Ref(_), Raw::Str(s)) => {
            let (pack_part, key) = s
                .rsplit_once('/')
                .map_or((None, s.as_str()), |(p, k)| (Some(p.to_owned()), k));
            // Empty keys, comma lists and command tuples are the source grammar, not a pack value.
            if key.is_empty() || key.contains(',') || s.starts_with('(') {
                return Err(err(file, path, PackRule::SourceGrammar, super::GRAMMAR));
            }
            Ok(Value::Ref {
                pack: pack_part,
                key: key.to_owned(),
            })
        }
        (FieldType::Text, _) => Err(wrong("text")),
        (FieldType::Int, _) => Err(wrong("an integer")),
        (FieldType::Bool, _) => Err(wrong("a boolean")),
        (FieldType::Date, _) => Err(wrong("a date")),
        (FieldType::Ref(_), _) => Err(wrong("a reference string")),
    }
}
