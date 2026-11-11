//! A spec is a comma-separated list of group external ids (`module.name`), each optionally
//! preceded by `!`, or the single character `.` (nobody):
//!
//! | spec | satisfied when the principal is |
//! |---|---|
//! | `a,b` | in `a` or in `b` |
//! | `a,!b` | in `a` and not in `b` |
//! | `!b` | not in `b` (no plain group means nothing is required) |
//! | `a,!a` | never: the negation is checked first |
//! | `.` | never |
//!
//! An external id that names
//! no group of the snapshot is an error, never an ignored term: a typo must not silently open or
//! close a field.
//!
//! ```
//! use erp_security::group_spec::GroupSpec;
//! # use erp_security::rows::{GroupRow, PolicyRows};
//! # use erp_security::groups::GroupGraph;
//! # use erp_orm::{GroupId, GroupSet, QualifiedXmlId};
//! # let id = |n| GroupId::new(n).unwrap();
//! # let rows = PolicyRows::builder(0)
//! #     .group(GroupRow::new(id(1), "Sales")?.with_xmlid(QualifiedXmlId::parse("sale.group")?))
//! #     .build()?;
//! # let graph = GroupGraph::build(&rows)?;
//! let spec = GroupSpec::parse("sale.group,!sale.group", &graph)?;
//! let member = GroupSet::new([id(1)]);
//! assert!(!spec.satisfied_by(&member, false));
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

use std::fmt;

use erp_fields::{Error, ErrorKind};
use erp_orm::{GroupId, GroupSet, PrincipalError, QualifiedXmlId};

use crate::groups::GroupGraph;
use crate::snapshot::DEBUG_GROUP;

pub const NEVER: &str = ".";

/// Why a group spec cannot be used. Every variant is a configuration or programming error: the
/// spec is never partly applied.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum GroupSpecError {
    /// The spec is empty (or only blanks).
    Empty,
    /// A comma-separated term is empty (`a,,b`, a trailing comma, a lone `!`).
    EmptyTerm {
        /// The whole spec.
        spec: String,
    },
    /// A term is not a `module.name` external id.
    InvalidXmlId {
        /// The term as written.
        term: String,
        /// What is wrong with it.
        problem: String,
    },
    /// An external id names no group of the policy snapshot.
    UnknownGroup {
        /// The external id.
        xmlid: String,
    },
    /// A group id (from the field-groups overlay) is not in the policy snapshot.
    UnknownGroupId {
        /// The group id.
        group: GroupId,
    },
}

impl fmt::Display for GroupSpecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("the group spec is empty"),
            Self::EmptyTerm { spec } => write!(
                f,
                "the group spec '{spec}' has an empty term (a doubled or trailing comma, or a \
                 lone '!')"
            ),
            Self::InvalidXmlId { term, problem } => write!(
                f,
                "the group spec term '{term}' is not a group external id (module.name): {problem}"
            ),
            Self::UnknownGroup { xmlid } => {
                write!(f, "the group spec names the unknown group '{xmlid}'")
            }
            Self::UnknownGroupId { group } => {
                write!(f, "the group spec names the unknown group {group}")
            }
        }
    }
}

impl std::error::Error for GroupSpecError {}

impl From<GroupSpecError> for Error {
    fn from(error: GroupSpecError) -> Self {
        Error::new(ErrorKind::Programming, error.to_string()).with_source(error)
    }
}

/// The technical-features group of `graph`, when it has one.
fn debug_group(graph: &GroupGraph) -> Result<Option<GroupId>, GroupSpecError> {
    let xmlid = QualifiedXmlId::parse(DEBUG_GROUP).map_err(|e| invalid(DEBUG_GROUP, &e))?;
    Ok(graph.by_xmlid(&xmlid))
}

fn invalid(term: &str, error: &PrincipalError) -> GroupSpecError {
    let problem = match error {
        PrincipalError::InvalidXmlId { problem, .. } => (*problem).to_owned(),
        other => other.to_string(),
    };
    GroupSpecError::InvalidXmlId {
        term: term.to_owned(),
        problem,
    }
}

/// One group of a spec, resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct Term {
    group: GroupId,
    /// `base.group_no_one`: counts only in debug mode.
    debug_only: bool,
}

impl Term {
    fn held(self, groups: &GroupSet, debug: bool) -> bool {
        (debug || !self.debug_only) && groups.contains(self.group)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Shape {
    /// `.`
    Never,
    Terms {
        positives: Box<[Term]>,
        negatives: Box<[Term]>,
    },
}

/// A parsed group spec, with every group resolved against one snapshot's group graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupSpec {
    shape: Shape,
}

impl GroupSpec {
    /// Parse `text` against `graph`.
    ///
    /// # Errors
    ///
    /// [`GroupSpecError`]: an empty spec or term, a term that is not `module.name`, an external id
    /// the graph does not know.
    pub fn parse(text: &str, graph: &GroupGraph) -> Result<Self, GroupSpecError> {
        if text == NEVER {
            return Ok(Self::never());
        }
        if text.trim().is_empty() {
            return Err(GroupSpecError::Empty);
        }
        let debug_group = debug_group(graph)?;
        let mut positives = Vec::new();
        let mut negatives = Vec::new();
        for raw in text.split(',') {
            let term = raw.trim();
            let (negated, xmlid) = match term.strip_prefix('!') {
                Some(rest) => (true, rest),
                None => (false, term),
            };
            if xmlid.is_empty() {
                return Err(GroupSpecError::EmptyTerm {
                    spec: text.to_owned(),
                });
            }
            let parsed = QualifiedXmlId::parse(xmlid).map_err(|e| invalid(term, &e))?;
            let group = graph
                .by_xmlid(&parsed)
                .ok_or_else(|| GroupSpecError::UnknownGroup {
                    xmlid: xmlid.to_owned(),
                })?;
            let term = Term {
                group,
                debug_only: Some(group) == debug_group,
            };
            if negated {
                negatives.push(term);
            } else {
                positives.push(term);
            }
        }
        Ok(Self::from_terms(positives, negatives))
    }

    /// The spec satisfied by members of any of `groups` (the field-groups overlay of a custom
    /// field). An empty list requires nothing, as a field without groups.
    ///
    /// # Errors
    ///
    /// [`GroupSpecError::UnknownGroupId`] for a group the graph does not have.
    pub fn any_of(
        groups: impl IntoIterator<Item = GroupId>,
        graph: &GroupGraph,
    ) -> Result<Self, GroupSpecError> {
        let debug_group = debug_group(graph)?;
        let mut positives = Vec::new();
        for group in groups {
            if !graph.contains(group) {
                return Err(GroupSpecError::UnknownGroupId { group });
            }
            positives.push(Term {
                group,
                debug_only: Some(group) == debug_group,
            });
        }
        Ok(Self::from_terms(positives, Vec::new()))
    }

    /// The spec `.`: nobody.
    pub fn never() -> Self {
        Self {
            shape: Shape::Never,
        }
    }

    fn from_terms(mut positives: Vec<Term>, mut negatives: Vec<Term>) -> Self {
        positives.sort_unstable();
        positives.dedup();
        negatives.sort_unstable();
        negatives.dedup();
        Self {
            shape: Shape::Terms {
                positives: positives.into(),
                negatives: negatives.into(),
            },
        }
    }

    /// Whether this is the spec `.` (nobody, not even in debug mode).
    pub fn is_never(&self) -> bool {
        matches!(self.shape, Shape::Never)
    }

    /// The plain groups, ascending.
    pub fn positives(&self) -> impl Iterator<Item = GroupId> + '_ {
        match &self.shape {
            Shape::Never => [].iter(),
            Shape::Terms { positives, .. } => positives.iter(),
        }
        .map(|t| t.group)
    }

    /// The negated groups, ascending.
    pub fn negatives(&self) -> impl Iterator<Item = GroupId> + '_ {
        match &self.shape {
            Shape::Never => [].iter(),
            Shape::Terms { negatives, .. } => negatives.iter(),
        }
        .map(|t| t.group)
    }

    /// Whether a principal in `groups` (its closed set) satisfies the spec; `debug` says whether
    /// it is in debug mode (`base.group_no_one` counts only then).
    pub fn satisfied_by(&self, groups: &GroupSet, debug: bool) -> bool {
        match &self.shape {
            Shape::Never => false,
            Shape::Terms {
                positives,
                negatives,
            } => {
                if negatives.iter().any(|t| t.held(groups, debug)) {
                    return false;
                }
                positives.is_empty() || positives.iter().any(|t| t.held(groups, debug))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use erp_fields::ErrorKind;

    use super::*;
    use crate::rows::{Implication, PolicyRows};
    use crate::testing::{g, group, xid};

    const A: i64 = 1;
    const B: i64 = 2;
    const NO_ONE: i64 = 3;
    const TOP: i64 = 4;

    /// `a`, `b`, `base.group_no_one`, and `top` which `a` implies through a chain of 40 groups.
    fn graph() -> GroupGraph {
        let chain_first = 100;
        let mut rows = PolicyRows::builder(0).groups([
            group(A, "Group A").with_xmlid(xid("t.a")),
            group(B, "Group B").with_xmlid(xid("t.b")),
            group(NO_ONE, "Technical").with_xmlid(xid("base.group_no_one")),
            group(TOP, "Top")
                .with_xmlid(xid("t.top"))
                .with_xmlid(xid("t.alias")),
        ]);
        let mut implications = vec![Implication::new(g(A), g(chain_first))];
        for i in 0..40 {
            rows = rows.group(group(chain_first + i, "chain"));
            let next = if i == 39 { TOP } else { chain_first + i + 1 };
            implications.push(Implication::new(g(chain_first + i), g(next)));
        }
        GroupGraph::build(&rows.implications(implications).build().unwrap()).unwrap()
    }

    fn members(graph: &GroupGraph, direct: &[i64]) -> GroupSet {
        let direct: Vec<GroupId> = direct.iter().map(|i| g(*i)).collect();
        graph.closure(&direct).unwrap()
    }

    fn holds(graph: &GroupGraph, spec: &str, groups: &GroupSet, debug: bool) -> bool {
        GroupSpec::parse(spec, graph)
            .unwrap()
            .satisfied_by(groups, debug)
    }

    #[test]
    fn spec_sec_025_group_spec_semantics() {
        let graph = graph();
        let user = members(&graph, &[A]);
        for (spec, expected) in [
            ("t.a", true),
            ("t.b", false),
            ("t.a,t.b", true),
            ("!t.b", true),
            ("t.a,!t.a", false),
            (".", false),
            ("base.group_no_one", false),
        ] {
            assert_eq!(holds(&graph, spec, &user, false), expected, "{spec}");
        }
        // a member of the technical group counts there in debug mode only
        let tech = members(&graph, &[A, NO_ONE]);
        assert!(!holds(&graph, "base.group_no_one", &tech, false));
        assert!(holds(&graph, "base.group_no_one", &tech, true));
        // as a negation too: outside debug mode the group does not count, so nothing is excluded
        assert!(holds(&graph, "t.a,!base.group_no_one", &tech, false));
        assert!(!holds(&graph, "t.a,!base.group_no_one", &tech, true));
        // a non-member never gets it in debug mode
        assert!(!holds(&graph, "base.group_no_one", &user, true));
        // `.` stays closed in debug mode
        assert!(!holds(&graph, ".", &tech, true));
    }

    #[test]
    fn negations_only_need_the_absence() {
        let graph = graph();
        let (a, none) = (members(&graph, &[A]), members(&graph, &[]));
        assert!(!holds(&graph, "!t.a", &a, false));
        assert!(holds(&graph, "!t.a", &none, false));
        assert!(holds(&graph, "!t.a,!t.b", &none, false));
        assert!(!holds(&graph, "!t.a,!t.b", &members(&graph, &[B]), false));
    }

    #[test]
    fn terms_are_trimmed_and_duplicates_collapse() {
        let graph = graph();
        let spec = GroupSpec::parse(" t.b , t.a,t.a ,t.b,!t.b", &graph).unwrap();
        assert_eq!(spec.positives().collect::<Vec<_>>(), [g(A), g(B)]);
        assert_eq!(spec.negatives().collect::<Vec<_>>(), [g(B)]);
        assert_eq!(
            GroupSpec::parse("t.a,t.a", &graph).unwrap(),
            GroupSpec::parse("t.a", &graph).unwrap()
        );
    }

    #[test]
    fn deep_implied_chains_and_aliases() {
        let graph = graph();
        // `a` implies `top` through forty groups; the alias names the same group
        let user = members(&graph, &[A]);
        assert!(holds(&graph, "t.top", &user, false));
        assert!(holds(&graph, "t.alias", &user, false));
        assert!(!holds(&graph, "t.top", &members(&graph, &[B]), false));
        assert_eq!(
            GroupSpec::parse("t.alias", &graph).unwrap(),
            GroupSpec::parse("t.top", &graph).unwrap()
        );
    }

    #[test]
    fn what_does_not_compile_is_an_error_naming_the_text() {
        let graph = graph();
        let cases = [
            ("", "the group spec is empty"),
            ("  ", "the group spec is empty"),
            (
                "t.a,,t.b",
                "the group spec 't.a,,t.b' has an empty term (a doubled or trailing comma, or a \
                 lone '!')",
            ),
            (
                "t.a,",
                "the group spec 't.a,' has an empty term (a doubled or trailing comma, or a lone \
                 '!')",
            ),
            (
                "!",
                "the group spec '!' has an empty term (a doubled or trailing comma, or a lone '!')",
            ),
            (
                "t.nobody",
                "the group spec names the unknown group 't.nobody'",
            ),
            (
                "t.a,!t.nobody",
                "the group spec names the unknown group 't.nobody'",
            ),
            (
                "nodot",
                "the group spec term 'nodot' is not a group external id (module.name): has no \
                 module part",
            ),
            (
                "!!t.a",
                "the group spec term '!!t.a' is not a group external id (module.name): has a module \
                 part that is not ASCII letters, digits and '_'",
            ),
            (
                "t. a",
                "the group spec term 't. a' is not a group external id (module.name): contains \
                 whitespace or control characters",
            ),
            (
                ". ",
                "the group spec term '.' is not a group external id (module.name): has an empty \
                 module part",
            ),
        ];
        for (text, message) in cases {
            let error = GroupSpec::parse(text, &graph).unwrap_err();
            assert_eq!(error.to_string(), message, "{text:?}");
            let error: Error = error.into();
            assert_eq!(error.kind(), ErrorKind::Programming, "{text:?}");
            assert_eq!(error.message(), message, "{text:?}");
        }
    }

    #[test]
    fn any_of_lists_and_unknown_ids() {
        let graph = graph();
        let spec = GroupSpec::any_of([g(B), g(A), g(B)], &graph).unwrap();
        assert_eq!(spec.positives().collect::<Vec<_>>(), [g(A), g(B)]);
        assert!(spec.satisfied_by(&members(&graph, &[B]), false));
        assert!(!spec.satisfied_by(&members(&graph, &[TOP]), false));
        // no group: no restriction
        let open = GroupSpec::any_of([], &graph).unwrap();
        assert!(open.satisfied_by(&members(&graph, &[]), false));
        let error = GroupSpec::any_of([g(9999)], &graph).unwrap_err();
        assert_eq!(error, GroupSpecError::UnknownGroupId { group: g(9999) });
        assert_eq!(
            error.to_string(),
            "the group spec names the unknown group 9999"
        );
        // the technical group in an overlay counts in debug mode only
        let tech = GroupSpec::any_of([g(NO_ONE)], &graph).unwrap();
        let member = members(&graph, &[NO_ONE]);
        assert!(!tech.satisfied_by(&member, false));
        assert!(tech.satisfied_by(&member, true));
    }

    #[test]
    fn never_is_never() {
        let graph = graph();
        let never = GroupSpec::parse(NEVER, &graph).unwrap();
        assert!(never.is_never());
        assert_eq!(never, GroupSpec::never());
        assert_eq!(never.positives().count() + never.negatives().count(), 0);
        assert!(!never.satisfied_by(&members(&graph, &[A, B, NO_ONE]), true));
    }
}
