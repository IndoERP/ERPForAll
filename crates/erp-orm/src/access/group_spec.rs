//! Group specs for view architecture guards.
//!
//! A group spec represents the group restrictions of an XML element or field definition,
//! parsed once and evaluated per user without statements or allocations.

use std::fmt;

use crate::principal::{PrincipalError, QualifiedXmlId};

/// One clause of a group spec: positive alternatives and negated exclusions.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct GroupClause {
    positives: Box<[QualifiedXmlId]>,
    negatives: Box<[QualifiedXmlId]>,
}

impl GroupClause {
    /// Creates a clause with positive and negative group external ids.
    #[must_use]
    pub fn new(
        positives: impl IntoIterator<Item = QualifiedXmlId>,
        negatives: impl IntoIterator<Item = QualifiedXmlId>,
    ) -> Self {
        let mut pos: Vec<_> = positives.into_iter().collect();
        let mut neg: Vec<_> = negatives.into_iter().collect();
        pos.sort();
        pos.dedup();
        neg.sort();
        neg.dedup();
        Self {
            positives: pos.into_boxed_slice(),
            negatives: neg.into_boxed_slice(),
        }
    }

    /// The positive group external ids (at least one must be held if non-empty).
    #[must_use]
    pub fn positives(&self) -> &[QualifiedXmlId] {
        &self.positives
    }

    /// The negated group external ids (none must be held).
    #[must_use]
    pub fn negatives(&self) -> &[QualifiedXmlId] {
        &self.negatives
    }
}

/// A parsed group specification for view architectures.
///
/// Holds parsed and interned group external ids (`QualifiedXmlId`) and their negations.
/// Multiple clauses represent the conjunction (intersection) of group requirements
/// (e.g. an element's guard intersected with the enclosing guard and field definition's groups).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct GroupSpec {
    clauses: Box<[GroupClause]>,
    is_never: bool,
}

impl GroupSpec {
    /// Parses a comma-separated list of group references with optional `!` negation
    /// (e.g. `"base.group_user,!base.group_portal"`).
    ///
    /// The special value `.` represents `never` (nobody matches).
    /// Empty string represents no restrictions.
    ///
    /// # Errors
    ///
    /// `PrincipalError::InvalidXmlId` when a term is not a valid `module.name` external id.
    pub fn parse(text: &str) -> Result<Self, PrincipalError> {
        let trimmed = text.trim();
        if trimmed == "." {
            return Ok(Self::never());
        }
        if trimmed.is_empty() {
            return Ok(Self::default());
        }
        let mut positives = Vec::new();
        let mut negatives = Vec::new();
        for item in trimmed.split(',') {
            let item = item.trim();
            if item.is_empty() {
                continue;
            }
            if let Some(rest) = item.strip_prefix('!') {
                let xmlid = QualifiedXmlId::parse(rest.trim())?;
                negatives.push(xmlid);
            } else {
                let xmlid = QualifiedXmlId::parse(item)?;
                positives.push(xmlid);
            }
        }
        positives.sort();
        positives.dedup();
        negatives.sort();
        negatives.dedup();
        Ok(Self {
            clauses: Box::new([GroupClause {
                positives: positives.into_boxed_slice(),
                negatives: negatives.into_boxed_slice(),
            }]),
            is_never: false,
        })
    }

    /// A spec that matches nobody.
    #[must_use]
    pub fn never() -> Self {
        Self {
            clauses: Box::new([]),
            is_never: true,
        }
    }

    /// Whether this spec matches nobody.
    #[must_use]
    pub fn is_never(&self) -> bool {
        self.is_never
    }

    /// Whether this spec has no restrictions (matches all users).
    #[must_use]
    pub fn is_open(&self) -> bool {
        !self.is_never && self.clauses.is_empty()
    }

    /// The clauses of this spec.
    #[must_use]
    pub fn clauses(&self) -> &[GroupClause] {
        &self.clauses
    }

    /// A spec from already-built clauses (the plan side of a view bundle): clauses are sorted and
    /// deduplicated, so equal clause sets give equal specs. No clause and `never == false` is the
    /// open spec; `never == true` ignores the clauses.
    #[must_use]
    pub fn from_clauses(clauses: impl IntoIterator<Item = GroupClause>, never: bool) -> Self {
        if never {
            return Self::never();
        }
        let mut clauses: Vec<GroupClause> = clauses.into_iter().collect();
        clauses.sort();
        clauses.dedup();
        Self {
            clauses: clauses.into_boxed_slice(),
            is_never: false,
        }
    }

    /// Total number of group references (positive and negated) in all clauses.
    #[must_use]
    pub fn ref_count(&self) -> usize {
        self.clauses
            .iter()
            .map(|c| c.positives.len() + c.negatives.len())
            .sum()
    }

    /// Bytes this spec retains (struct, boxed slices and external id texts), for cache accounting.
    #[must_use]
    pub fn retained_bytes(&self) -> usize {
        let clause = std::mem::size_of::<GroupClause>();
        let id = std::mem::size_of::<QualifiedXmlId>();
        let mut total = std::mem::size_of::<Self>();
        for c in &self.clauses {
            total = total.saturating_add(clause);
            for x in c.positives.iter().chain(c.negatives.iter()) {
                total = total.saturating_add(id).saturating_add(x.as_str().len());
            }
        }
        total
    }

    /// Intersects this spec with `other` (conjunction).
    #[must_use]
    pub fn intersect(&self, other: &Self) -> Self {
        if self.is_never || other.is_never {
            return Self::never();
        }
        if self.is_open() {
            return other.clone();
        }
        if other.is_open() {
            return self.clone();
        }
        let mut combined = Vec::with_capacity(self.clauses.len() + other.clauses.len());
        combined.extend_from_slice(&self.clauses);
        combined.extend_from_slice(&other.clauses);
        combined.sort();
        combined.dedup();
        Self {
            clauses: combined.into_boxed_slice(),
            is_never: false,
        }
    }
}

impl fmt::Display for GroupSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_never {
            return f.write_str(".");
        }
        if self.is_open() {
            return f.write_str("<open>");
        }
        for (i, clause) in self.clauses.iter().enumerate() {
            if i > 0 {
                f.write_str(" & ")?;
            }
            f.write_str("(")?;
            let mut first = true;
            for pos in clause.positives() {
                if !first {
                    f.write_str(",")?;
                }
                first = false;
                write!(f, "{pos}")?;
            }
            for neg in clause.negatives() {
                if !first {
                    f.write_str(",")?;
                }
                first = false;
                write!(f, "!{neg}")?;
            }
            f.write_str(")")?;
        }
        Ok(())
    }
}
