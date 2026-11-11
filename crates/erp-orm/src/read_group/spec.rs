//! The `_read_group` specification grammar (GRP §4.1): group-by and aggregate specs
//! `name(.path)?(:func)?`, read-group order terms and `having` clauses.
//!
//! Their kind follows provenance: `Validation`
//! when the specs came from client input (JSON-2, the web methods), `Programming` when Rust code
//! wrote them.

use erp_fields::{Error, Result};

pub use erp_domain::Granularity;

pub(crate) fn invalid_spec(spec: &str) -> String {
    format!(
        "Invalid aggregate/groupby specification '{spec}'.\n- Valid aggregate specification looks \
         like \"<field_name>:<agg>\" example: \"quantity:sum\".\n- Valid groupby specification \
         looks like \"<no_datish_field_name>\" or \"<datish_field_name>:<granularity>\" example: \
         \"date:month\" or \"<properties_field_name>.<property>:<granularity>\"."
    )
}

pub(crate) fn bad(external: bool, message: impl Into<String>) -> Error {
    if external {
        Error::validation(message)
    } else {
        Error::programming(message)
    }
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// The parts of a spec: field name, `.path` and `:function` (or granularity).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SpecParts<'a> {
    pub(crate) field: &'a str,
    pub(crate) path: Option<&'a str>,
    pub(crate) func: Option<&'a str>,
}

/// `parse_read_group_spec`: `(\w+)(\.([\w\.]+))?(?::(\w+))?$` matched from the start.
pub(crate) fn parse_spec(spec: &str) -> Option<SpecParts<'_>> {
    let end = spec.find(|c: char| !is_word(c)).unwrap_or(spec.len());
    if end == 0 {
        return None;
    }
    let field = &spec[..end];
    let mut rest = &spec[end..];
    let mut path = None;
    if let Some(after) = rest.strip_prefix('.') {
        let n = after
            .find(|c: char| !(is_word(c) || c == '.'))
            .unwrap_or(after.len());
        if n == 0 {
            return None;
        }
        // the path is greedy, like the regex: `a.b.` keeps the trailing dot in the path
        path = Some(&after[..n]);
        rest = &after[n..];
    }
    let mut func = None;
    if let Some(after) = rest.strip_prefix(':') {
        let n = after.find(|c: char| !is_word(c)).unwrap_or(after.len());
        if n == 0 {
            return None;
        }
        func = Some(&after[..n]);
        rest = &after[n..];
    }
    rest.is_empty().then_some(SpecParts { field, path, func })
}

pub(crate) fn parse_spec_or_err(spec: &str, external: bool) -> Result<SpecParts<'_>> {
    parse_spec(spec).ok_or_else(|| bad(external, invalid_spec(spec)))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Dir {
    Asc,
    Desc,
}

impl Dir {
    pub(crate) fn sql(self) -> &'static str {
        match self {
            Self::Asc => "ASC",
            Self::Desc => "DESC",
        }
    }
}

/// NULLS clause of a read-group order term.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NullsPos {
    Default,
    First,
    Last,
}

impl NullsPos {
    pub(crate) fn sql(self) -> &'static str {
        match self {
            Self::Default => "",
            Self::First => " NULLS FIRST",
            Self::Last => " NULLS LAST",
        }
    }
}

/// One term of a read-group order: a group-by spec or an aggregate spec with its direction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OrderPart {
    pub(crate) term: String,
    pub(crate) dir: Dir,
    pub(crate) nulls: NullsPos,
}

fn strip_ci<'a>(s: &'a str, word: &str) -> Option<&'a str> {
    let head = s.get(..word.len())?;
    head.eq_ignore_ascii_case(word).then(|| &s[word.len()..])
}

/// `regex_order_part_read_group` (full match, case-insensitive):
/// `\s*(field[.path][:func])(\s+(asc|desc))?(\s+(nulls first|nulls last))?\s*`.
pub(crate) fn parse_order_part(part: &str) -> Option<OrderPart> {
    let s = part.trim_start();
    let n = s
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .unwrap_or(s.len());
    if n == 0 {
        return None;
    }
    let mut end = n;
    let mut rest = &s[n..];
    if let Some(after) = rest.strip_prefix('.') {
        let m = after
            .find(|c: char| !(is_word(c) || c == '.'))
            .unwrap_or(after.len());
        if m == 0 {
            return None;
        }
        end += 1 + m;
        rest = &after[m..];
    }
    if let Some(after) = rest.strip_prefix(':') {
        let m = after
            .find(|c: char| !(c.is_ascii_alphabetic() || c == '_'))
            .unwrap_or(after.len());
        if m == 0 {
            return None;
        }
        end += 1 + m;
        rest = &after[m..];
    }
    let term = s[..end].to_owned();
    let mut dir = Dir::Asc;
    let mut nulls = NullsPos::Default;
    let trimmed = rest.trim_start();
    if trimmed.len() < rest.len() {
        if let Some(r) = strip_ci(trimmed, "desc").filter(|r| !r.starts_with(is_word)) {
            dir = Dir::Desc;
            rest = r;
        } else if let Some(r) = strip_ci(trimmed, "asc").filter(|r| !r.starts_with(is_word)) {
            rest = r;
        }
    }
    let trimmed = rest.trim_start();
    if trimmed.len() < rest.len() {
        if let Some(r) = strip_ci(trimmed, "nulls first").filter(|r| !r.starts_with(is_word)) {
            nulls = NullsPos::First;
            rest = r;
        } else if let Some(r) = strip_ci(trimmed, "nulls last").filter(|r| !r.starts_with(is_word))
        {
            nulls = NullsPos::Last;
            rest = r;
        }
    }
    rest.trim()
        .is_empty()
        .then_some(OrderPart { term, dir, nulls })
}

pub(crate) fn parse_order(order: &str, external: bool) -> Result<Vec<OrderPart>> {
    order
        .split(',')
        .map(|part| {
            parse_order_part(part).ok_or_else(|| {
                bad(
                    external,
                    format!("Invalid order '{order}' for _read_group()"),
                )
            })
        })
        .collect()
}

/// The aggregate functions of `READ_GROUP_AGGREGATE` (plus `sum_currency`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum AggFunc {
    Sum,
    Avg,
    Max,
    Min,
    BoolAnd,
    BoolOr,
    ArrayAgg,
    ArrayAggDistinct,
    Recordset,
    Count,
    CountDistinct,
    SumCurrency,
}

impl AggFunc {
    pub(crate) fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "sum" => Self::Sum,
            "avg" => Self::Avg,
            "max" => Self::Max,
            "min" => Self::Min,
            "bool_and" => Self::BoolAnd,
            "bool_or" => Self::BoolOr,
            "array_agg" => Self::ArrayAgg,
            "array_agg_distinct" => Self::ArrayAggDistinct,
            "recordset" => Self::Recordset,
            "count" => Self::Count,
            "count_distinct" => Self::CountDistinct,
            "sum_currency" => Self::SumCurrency,
            _ => return None,
        })
    }
}

/// The comparison operators a `having` clause accepts.
pub(crate) const HAVING_OPERATORS: [&str; 8] = ["in", "not in", "<", ">", "<=", ">=", "=", "!="];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_grammar_matches_reference() {
        assert_eq!(
            parse_spec("date:month"),
            Some(SpecParts {
                field: "date",
                path: None,
                func: Some("month")
            })
        );
        assert_eq!(
            parse_spec("foo_id.bar_id.name"),
            Some(SpecParts {
                field: "foo_id",
                path: Some("bar_id.name"),
                func: None
            })
        );
        assert_eq!(parse_spec("__count").map(|p| p.field), Some("__count"));
        for bad in [
            "Other stuff create_date:week",
            "\"create_date:week",
            "value:array_agg OR",
            "label:sum(value)",
            "",
            ":sum",
            "a:",
        ] {
            assert!(parse_spec(bad).is_none(), "{bad:?}");
        }
    }

    #[test]
    fn order_grammar_matches_reference() {
        let p = parse_order_part(" partner_id DESC nulls last ").unwrap();
        assert_eq!(p.term, "partner_id");
        assert_eq!(p.dir, Dir::Desc);
        assert_eq!(p.nulls, NullsPos::Last);
        assert_eq!(
            parse_order_part("date:year desc").unwrap().term,
            "date:year"
        );
        assert_eq!(parse_order_part("value:sum").unwrap().dir, Dir::Asc);
        for bad in [
            "__count DESC other",
            "value\" DESC",
            "value ASCCC",
            "",
            "a b",
        ] {
            assert!(parse_order_part(bad).is_none(), "{bad:?}");
        }
    }
}
