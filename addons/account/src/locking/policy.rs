// SPDX-License-Identifier: MIT OR Apache-2.0
use super::LockField;
use crate::fiscal::PeriodVerdict;
use erp_sdk::prelude::*;

/// Journal class used by the date policy. Derive it from an authorized journal, not RPC input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JournalClass {
    /// Sales journal.
    Sale,
    /// Purchase journal.
    Purchase,
    /// Any other accounting journal.
    Other,
}
/// Whether protected changes affect the tax report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaxImpact {
    /// No tax-report impact.
    None,
    /// Tax lines, tax tags, or another tax-report contribution.
    Report,
}
/// Validated input for one accounting date check.
#[derive(Debug, Clone, Copy)]
pub struct LockCheck {
    pub(crate) company: i64,
    pub(crate) date: Date,
    pub(crate) journal: JournalClass,
    pub(crate) tax: TaxImpact,
}
impl LockCheck {
    /// Build a check from already authorized business records.
    /// # Errors
    /// `Validation` for a nonpositive company or a date PostgreSQL cannot store.
    pub fn new(company: i64, date: Date, journal: JournalClass, tax: TaxImpact) -> Result<Self> {
        if company <= 0 {
            return Err(Error::validation(
                "A lock check requires a positive company id.",
            ));
        }
        if !valid_date(date) {
            return Err(Error::validation(
                "The accounting date is outside PostgreSQL's date range.",
            ));
        }
        Ok(Self {
            company,
            date,
            journal,
            tax,
        })
    }
}
// PostgreSQL's DATE wire domain, as in erp-fields/src/pg.rs, expressed relative to 1970.
// This is a storage invariant, not an administrator choice.
pub(crate) fn valid_date(date: Date) -> bool {
    (-2_440_588..=2_145_042_905).contains(&date.days())
}
/// One inclusive lock-date restriction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockViolation {
    field: LockField,
    through: Date,
}
impl LockViolation {
    /// The company field imposing this restriction.
    pub fn field(&self) -> LockField {
        self.field
    }
    /// Last date included in the restriction.
    pub fn through(&self) -> Date {
        self.through
    }
}
/// Typed policy refusal; period failures preserve the fiscal API's distinction.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum LockRefusal {
    /// All scalar date restrictions violated by this input.
    Dates {
        date: Date,
        violations: Vec<LockViolation>,
    },
    /// A missing or non-open fiscal period.
    Period(PeriodVerdict),
}
impl std::fmt::Display for LockRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Dates { date, violations } => {
                write!(f, "The accounting date {date} is locked: ")?;
                for (i, violation) in violations.iter().enumerate() {
                    if i != 0 {
                        f.write_str("; ")?;
                    }
                    write!(
                        f,
                        "{} through {}",
                        violation.field.as_str(),
                        violation.through
                    )?;
                }
                f.write_str(".")
            }
            Self::Period(period) => match period.clone().into_result() {
                Err(error) => f.write_str(error.message()),
                Ok(_) => f.write_str("A period refusal unexpectedly contains an open period."),
            },
        }
    }
}
impl std::error::Error for LockRefusal {}
/// Per-input decision. Recheck in the transaction that performs the business write.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum LockVerdict {
    /// No applicable lock refused the date.
    Allowed,
    /// Typed policy refusal.
    Refused(LockRefusal),
}
impl LockVerdict {
    /// Convert the refusal to its precise error kind/message, retaining its typed source.
    /// # Errors
    /// `Missing` for a missing period, `Validation` for a lock.
    pub fn into_result(self) -> Result<()> {
        match self {
            Self::Allowed => Ok(()),
            Self::Refused(refusal) => {
                let kind = match &refusal {
                    LockRefusal::Period(PeriodVerdict::NoPeriod { .. }) => {
                        erp_sdk::ErrorKind::Missing
                    }
                    _ => erp_sdk::ErrorKind::Validation,
                };
                Err(Error::new(kind, refusal.to_string()).with_source(refusal))
            }
        }
    }
}
/// Private immutable effective dates; never kept in transaction extensions.
#[derive(Default)]
pub(crate) struct EffectiveDates(pub [Option<Date>; 5]);
pub(crate) const FIELDS: [LockField; 5] = [
    LockField::FiscalYear,
    LockField::Sale,
    LockField::Purchase,
    LockField::Tax,
    LockField::Hard,
];
impl EffectiveDates {
    pub(crate) fn check(&self, input: &LockCheck) -> LockVerdict {
        let mut violations = Vec::new();
        for (i, field) in FIELDS.iter().enumerate() {
            let applies = match field {
                LockField::FiscalYear | LockField::Hard => true,
                LockField::Sale => input.journal == JournalClass::Sale,
                LockField::Purchase => input.journal == JournalClass::Purchase,
                LockField::Tax => input.tax == TaxImpact::Report,
            };
            if applies
                && let Some(through) = self.0[i]
                && input.date <= through
            {
                violations.push(LockViolation {
                    field: *field,
                    through,
                });
            }
        }
        violations.sort_by_key(|v| (v.through, v.field));
        if violations.is_empty() {
            LockVerdict::Allowed
        } else {
            LockVerdict::Refused(LockRefusal::Dates {
                date: input.date,
                violations,
            })
        }
    }
}
