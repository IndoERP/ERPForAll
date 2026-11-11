//! Company consistency failures (design note).

use std::fmt;

use erp_fields::ErrorKind;

/// A company consistency failure. Converts into [`erp_fields::Error`] with the
/// kind of [`CompanyError::kind`], the same message and itself as source.
///
/// ```
/// use erp_fields::ErrorKind;
/// use erp_orm::CompanyError;
/// let e = CompanyError::NoCurrentCompany { model: "my.doc", field: "tax_id" };
/// assert_eq!(e.kind(), ErrorKind::UserError);
/// assert_eq!(
///     e.to_string(),
///     "my.doc.tax_id is company-dependent and was written without a current company"
/// );
/// let converted: erp_fields::Error = e.into();
/// assert_eq!(converted.kind(), ErrorKind::UserError);
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum CompanyError {
    /// `total` counts all inconsistencies.
    Inconsistent {
        /// The first inconsistencies (at most five), names resolved: what the message lists.
        items: Vec<CompanyInconsistency>,
        /// Every inconsistency found, also those the message does not list.
        total: usize,
    },
    /// `Missing`: a linked record does not exist (rule 7.2).
    MissingTarget {
        /// The linked record's model.
        comodel: &'static str,
        /// Its id.
        id: i64,
        /// The linking model.
        model: &'static str,
        /// The linking field.
        field: &'static str,
    },
    /// `Missing`: a record being checked is gone from the database (deleted by another
    /// transaction between its write and the check).
    MissingRecord {
        /// The record's model.
        model: &'static str,
        /// Its id.
        id: i64,
    },
    /// `UserError`: a company-dependent field was written, with a value, without a current
    /// company (open question 1).
    NoCurrentCompany {
        /// The model.
        model: &'static str,
        /// The company-dependent field.
        field: &'static str,
    },
    /// `Database`: a company's `parent_path` is empty or not a `/`-separated list of ids.
    MalformedParentPath {
        /// The company.
        company: i64,
    },
}

impl CompanyError {
    /// The error kind the failure converts to.
    pub fn kind(&self) -> ErrorKind {
        match self {
            Self::Inconsistent { .. } | Self::NoCurrentCompany { .. } => ErrorKind::UserError,
            Self::MissingTarget { .. } | Self::MissingRecord { .. } => ErrorKind::Missing,
            Self::MalformedParentPath { .. } => ErrorKind::Database,
        }
    }
}

impl fmt::Display for CompanyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Inconsistent { items, .. } => f.write_str(&message(items)),
            Self::MissingTarget {
                comodel,
                id,
                model,
                field,
            } => write!(
                f,
                "Record does not exist or has been deleted: {comodel},{id} (linked by \
                 {model}.{field})"
            ),
            Self::MissingRecord { model, id } => {
                write!(f, "Record does not exist or has been deleted: {model},{id}")
            }
            Self::NoCurrentCompany { model, field } => write!(
                f,
                "{model}.{field} is company-dependent and was written without a current company"
            ),
            Self::MalformedParentPath { company } => write!(
                f,
                "res.company,{company} has an empty or malformed parent_path; the company \
                 hierarchy cannot be checked"
            ),
        }
    }
}

impl std::error::Error for CompanyError {}

impl From<CompanyError> for erp_fields::Error {
    fn from(e: CompanyError) -> Self {
        erp_fields::Error::new(e.kind(), e.to_string()).with_source(e)
    }
}

pub(super) const LISTED: usize = 5;

/// One listed inconsistency of a [`CompanyError::Inconsistent`], names already resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct CompanyInconsistency {
    /// `res.company` records are their own company (`Record is company …`).
    pub itself: bool,
    /// The record's display name (unused for `itself`).
    pub record: String,
    /// The record's companies by name (empty when it has none).
    pub companies: Vec<String>,
    /// The field's label.
    pub label: String,
    /// The field's name.
    pub field: &'static str,
    /// The targets, each a Python `repr` of its name (or `model(id,)` when the caller cannot
    /// read it).
    pub values: Vec<String>,
}

impl CompanyInconsistency {
    /// An inconsistency of `record` (see the fields).
    pub(crate) fn new(
        itself: bool,
        record: String,
        companies: Vec<String>,
        label: String,
        field: &'static str,
        values: Vec<String>,
    ) -> Self {
        Self {
            itself,
            record,
            companies,
            label,
            field,
            values,
        }
    }
}

fn message(items: &[CompanyInconsistency]) -> String {
    let mut lines = Vec::with_capacity(items.len().saturating_add(2));
    lines.push("Uh-oh! You’ve got some company inconsistencies here:".to_owned());
    for i in items {
        let companies = i.companies.join(", ");
        let values = i.values.join(", ");
        lines.push(if i.itself {
            format!(
                "- Record is company “{companies}” while “{}” ({}: {values}) belongs to another \
                 company.",
                i.label, i.field
            )
        } else {
            format!(
                "- “{}” belongs to company “{companies}” while “{}” ({}: {values}) belongs to \
                 another company.",
                i.record, i.label, i.field
            )
        });
    }
    lines.push("To avoid a mess, no company crossover is allowed!".to_owned());
    lines.join("\n")
}

/// Python's `repr()` of a `str`: single quotes unless the text has a `'` and no `"`, backslash,
/// quote, `\n`, `\r`, `\t` and control or invisible characters escaped, everything else (non-ASCII
/// letters included) kept as is.
///
/// "Invisible" is the common set Python's `str.isprintable` refuses: C0/C1 controls, no-break and
/// other space separators, soft hyphen, zero-width and bidi marks, line and paragraph separators,
/// BOM.
pub(super) fn py_repr(text: &str) -> String {
    let quote = if text.contains('\'') && !text.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::with_capacity(text.len().saturating_add(2));
    out.push(quote);
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if c.is_control() || is_invisible(c) => {
                let n = u32::from(c);
                if n <= 0xff {
                    out.push_str(&format!("\\x{n:02x}"));
                } else if n <= 0xffff {
                    out.push_str(&format!("\\u{n:04x}"));
                } else {
                    out.push_str(&format!("\\U{n:08x}"));
                }
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

fn is_invisible(c: char) -> bool {
    matches!(
        u32::from(c),
        0xa0 | 0xad | 0x1680 | 0x2000..=0x200f | 0x2028..=0x202f | 0x205f..=0x2064 | 0x3000 | 0xfeff
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repr_follows_python() {
        assert_eq!(py_repr("x"), "'x'");
        assert_eq!(py_repr("it's"), "\"it's\"");
        assert_eq!(py_repr("say \"hi\""), "'say \"hi\"'");
        assert_eq!(py_repr("O'Neil \"Jr\""), "'O\\'Neil \"Jr\"'");
        assert_eq!(py_repr("Société d’Ébène"), "'Société d’Ébène'");
        assert_eq!(py_repr("標籤"), "'標籤'");
        assert_eq!(py_repr("a\r\nb\tc\\d"), "'a\\r\\nb\\tc\\\\d'");
        assert_eq!(py_repr("\u{7f}\u{a0}\u{200b}"), "'\\x7f\\xa0\\u200b'");
        assert_eq!(py_repr(""), "''");
    }

    #[test]
    fn message_has_header_lines_and_footer() {
        let items = vec![
            CompanyInconsistency::new(
                false,
                "C0".to_owned(),
                vec!["Company B".to_owned()],
                "Parent".to_owned(),
                "parent_id",
                vec!["'PA'".to_owned()],
            ),
            CompanyInconsistency::new(
                true,
                "ignored".to_owned(),
                vec!["Company A".to_owned()],
                "Partner".to_owned(),
                "partner_id",
                vec!["'x'".to_owned(), "\"it's\"".to_owned()],
            ),
        ];
        let error = CompanyError::Inconsistent { items, total: 9 };
        assert_eq!(
            error.to_string(),
            "Uh-oh! You’ve got some company inconsistencies here:\n\
             - “C0” belongs to company “Company B” while “Parent” (parent_id: 'PA') belongs to \
             another company.\n\
             - Record is company “Company A” while “Partner” (partner_id: 'x', \"it's\") belongs \
             to another company.\n\
             To avoid a mess, no company crossover is allowed!"
        );
    }

    #[test]
    fn kinds_and_conversion_keep_the_message() {
        let e = CompanyError::MissingTarget {
            comodel: "a.b",
            id: 7,
            model: "c.d",
            field: "e_id",
        };
        assert_eq!(e.kind(), ErrorKind::Missing);
        let converted: erp_fields::Error = e.clone().into();
        assert_eq!(converted.kind(), ErrorKind::Missing);
        assert_eq!(
            converted.message(),
            "Record does not exist or has been deleted: a.b,7 (linked by c.d.e_id)"
        );
        assert_eq!(
            CompanyError::MalformedParentPath { company: 3 }.kind(),
            ErrorKind::Database
        );
    }
}
