//! The pure part of the company check: which fields are checked, and whether a linked record's
//! companies fit the linking record's (3). No I/O.

use erp_fields::{Error, Result};
use erp_registry::{CompanyMatch, FieldId, ModelId};
use rustc_hash::FxHashSet;

/// How the targets of a checked field are read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum LinkKind {
    /// A many2one column of the model's table.
    Many2one,
    /// A many2many stored in a relation table with no field domain: its rows are read directly
    /// when the cache lacks the value.
    Many2many,
    /// Anything else (one2many, many2many with a domain, fields without a column): read through
    /// the x2many cache, which may cost statements of its own.
    Other,
}

/// How the companies of a target are read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TargetScope {
    /// A `company_id` many2one of the comodel.
    Single(FieldId),
    /// A `company_ids` many2many of the comodel.
    Many(FieldId),
    /// The comodel's inherited `company_id`, which comes by `_inherits` delegation from the model
    /// that stores it (A2). The batched statement reads nothing for it: the companies
    /// are read through the ORM's delegated read once the targets are known.
    Delegated(FieldId),
}

impl TargetScope {
    /// The comodel's company field (for [`Self::Delegated`], the comodel's inherited copy).
    pub(super) fn field(self) -> FieldId {
        match self {
            Self::Single(f) | Self::Many(f) | Self::Delegated(f) => f,
        }
    }
}

/// One checked field.
#[derive(Debug, Clone)]
pub(super) struct PlanField {
    pub field: FieldId,
    pub comodel: ModelId,
    pub kind: LinkKind,
    /// The comodel's company field and its form.
    pub target: TargetScope,
    /// The comodel's match.
    pub matching: CompanyMatch,
    /// Checked against `env.company` rather than the record's companies.
    pub company_dependent: bool,
}

/// The companies of a target record.
#[derive(Debug, Clone, Copy)]
pub(super) enum TargetCompanies<'a> {
    /// `company_id` (None when the record has no company).
    One(Option<i64>),
    /// `company_ids`.
    Many(&'a [i64]),
}

/// The companies `r` (a record's, or the current one) and the closure of their ancestors,
/// self included (needed only by the parent forms).
pub(super) struct Linking<'a> {
    pub companies: &'a [i64],
    pub closure: &'a FxHashSet<i64>,
}

/// Whether `target` may be linked by a record of `linking` under `matching`.
///
/// # Errors
///
/// `Programming` for a match whose form does not fit the target's shape (`Exact` against
/// `company_ids`): the registry refuses those, so reaching one is a broken invariant, never
/// answered as "not allowed" (review R8).
pub(super) fn allowed(
    matching: CompanyMatch,
    linking: &Linking<'_>,
    target: TargetCompanies<'_>,
) -> Result<bool> {
    Ok(match (matching, target) {
        (CompanyMatch::Exact, TargetCompanies::One(c)) => {
            c.is_none_or(|c| linking.companies.contains(&c))
        }
        (CompanyMatch::ParentOf, TargetCompanies::One(c)) => {
            c.is_none_or(|c| linking.closure.contains(&c))
        }
        (CompanyMatch::AnyOf, TargetCompanies::Many(cs)) => {
            linking.companies.is_empty() || cs.iter().any(|c| linking.companies.contains(c))
        }
        (CompanyMatch::ParentOfAny, TargetCompanies::Many(cs)) => {
            linking.companies.is_empty() || cs.iter().any(|c| linking.closure.contains(c))
        }
        (matching, _) => {
            return Err(Error::programming(format!(
                "the company match {} does not fit the shape of the target's company field \
                 (registry invariant)",
                matching.as_str()
            )));
        }
    })
}

/// The ids of a `parent_path` (`"1/5/9/"` gives `[1, 5, 9]`): the company and its ancestors.
/// `None` for an empty path or a part that is not a positive integer.
pub(super) fn path_ids(path: &str) -> Option<Vec<i64>> {
    let body = path.strip_suffix('/')?;
    if body.is_empty() {
        return None;
    }
    body.split('/')
        .map(|part| part.parse::<i64>().ok().filter(|n| *n > 0))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn linking(companies: &[i64], closure: &[i64]) -> (Vec<i64>, FxHashSet<i64>) {
        (companies.to_vec(), closure.iter().copied().collect())
    }

    fn check(
        matching: CompanyMatch,
        companies: &[i64],
        closure: &[i64],
        target: TargetCompanies<'_>,
    ) -> bool {
        let (c, s) = linking(companies, closure);
        allowed(
            matching,
            &Linking {
                companies: &c,
                closure: &s,
            },
            target,
        )
        .unwrap()
    }

    #[test]
    fn exact_follows_the_null_rule() {
        use CompanyMatch::Exact;
        assert!(check(Exact, &[1], &[], TargetCompanies::One(None)));
        assert!(check(Exact, &[], &[], TargetCompanies::One(None)));
        assert!(check(Exact, &[1], &[], TargetCompanies::One(Some(1))));
        assert!(!check(Exact, &[1], &[], TargetCompanies::One(Some(2))));
        assert!(!check(Exact, &[], &[], TargetCompanies::One(Some(1))));
        assert!(check(Exact, &[1, 2], &[], TargetCompanies::One(Some(2))));
    }

    #[test]
    fn parent_of_accepts_ancestors() {
        use CompanyMatch::ParentOf;
        // company 9 is a child of 5, a child of 1
        assert!(check(
            ParentOf,
            &[9],
            &[1, 5, 9],
            TargetCompanies::One(Some(5))
        ));
        assert!(check(
            ParentOf,
            &[9],
            &[1, 5, 9],
            TargetCompanies::One(Some(1))
        ));
        assert!(check(
            ParentOf,
            &[9],
            &[1, 5, 9],
            TargetCompanies::One(None)
        ));
        assert!(!check(
            ParentOf,
            &[9],
            &[1, 5, 9],
            TargetCompanies::One(Some(6))
        ));
        // the parent does not accept its child
        assert!(!check(
            ParentOf,
            &[5],
            &[1, 5],
            TargetCompanies::One(Some(9))
        ));
        assert!(!check(ParentOf, &[], &[], TargetCompanies::One(Some(1))));
    }

    #[test]
    fn any_of_and_parent_of_any_accept_no_company() {
        use CompanyMatch::{AnyOf, ParentOfAny};
        assert!(check(AnyOf, &[], &[], TargetCompanies::Many(&[7])));
        assert!(check(AnyOf, &[1], &[], TargetCompanies::Many(&[1, 2])));
        assert!(!check(AnyOf, &[1], &[], TargetCompanies::Many(&[2])));
        assert!(!check(AnyOf, &[1], &[], TargetCompanies::Many(&[])));
        assert!(check(ParentOfAny, &[], &[], TargetCompanies::Many(&[7])));
        assert!(check(
            ParentOfAny,
            &[9],
            &[1, 9],
            TargetCompanies::Many(&[3, 1])
        ));
        assert!(!check(
            ParentOfAny,
            &[9],
            &[1, 9],
            TargetCompanies::Many(&[3])
        ));
    }

    #[test]
    fn mismatched_forms_are_refused() {
        let (c, s) = linking(&[1], &[]);
        let linking = Linking {
            companies: &c,
            closure: &s,
        };
        for (matching, target) in [
            (CompanyMatch::Exact, TargetCompanies::Many(&[1])),
            (CompanyMatch::AnyOf, TargetCompanies::One(Some(1))),
        ] {
            let err = allowed(matching, &linking, target).unwrap_err();
            assert_eq!(err.kind(), erp_fields::ErrorKind::Programming);
            assert_eq!(
                err.message(),
                format!(
                    "the company match {} does not fit the shape of the target's company field \
                     (registry invariant)",
                    matching.as_str()
                )
            );
        }
    }

    #[test]
    fn parent_paths() {
        assert_eq!(path_ids("1/5/9/"), Some(vec![1, 5, 9]));
        assert_eq!(path_ids("7/"), Some(vec![7]));
        for bad in [
            "",
            "/",
            "1/5",
            "1//5/",
            "a/",
            "1/-5/",
            "0/",
            "1/ 5/",
            "99999999999999999999/",
        ] {
            assert_eq!(path_ids(bad), None, "{bad:?}");
        }
    }
}
