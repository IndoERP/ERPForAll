//! Every generated value is a small index; it is mapped onto the fixture's value tables when the
//! case is seeded or rendered, so proptest shrinks towards `NULL` values, fewer rows, fewer rules
//! and simpler domains.

use std::fmt;
use std::ops::RangeInclusive;

use proptest::prelude::*;

use super::{EVERYONE, G1, G2, G3, PORTAL, USER};

/// The model a rule is written for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum On {
    /// `x.obj`.
    Obj,
    /// `x.categ`.
    Categ,
    /// `x.child` (`_inherits` `x.obj`).
    Child,
}

impl On {
    /// The model name.
    pub fn model(self) -> &'static str {
        match self {
            Self::Obj => super::OBJ,
            Self::Categ => super::CATEG,
            Self::Child => super::CHILD,
        }
    }

    /// The table of condition texts rules on this model draw from.
    pub fn leaves(self) -> &'static [&'static str] {
        match self {
            Self::Obj => OBJ_LEAVES,
            Self::Categ => CATEG_LEAVES,
            Self::Child => CHILD_LEAVES,
        }
    }
}

/// Conditions of `x.obj` rules: every field type, `NULL`-sensitive operators, principal symbols
/// (`user.id`, user paths, `company_ids`, `company_id`, typed "now"), relational paths (`any!`),
/// `any`/`not any` and the `access` operator.
pub const OBJ_LEAVES: &[&str] = &[
    "('val', '=', 0)",
    "('val', '!=', 0)",
    "('val', '>', 2)",
    "('val', '<', 2)",
    "('val', '>=', 3)",
    "('val', '<=', -1)",
    "('val', 'in', [1, 2, 3])",
    "('val', 'not in', [0, 5])",
    "('val', 'in', [False, 1])",
    "('val', '=', False)",
    "('qty', '!=', False)",
    "('qty', '>', 0)",
    "('name', '=', 'a')",
    "('name', '!=', 'a')",
    "('name', 'in', ['a', 'b'])",
    "('name', '=', False)",
    "('name', '=', '')",
    "('name', 'ilike', 'A')",
    "('name', 'not ilike', 'b')",
    "('name', 'like', 'a')",
    "('code', '=', False)",
    "('code', 'in', ['x'])",
    "('amount', '>', 1.0)",
    "('amount', '<=', 0.5)",
    "('amount', '=', False)",
    "('flag', '=', True)",
    "('flag', '=', False)",
    "('flag', '!=', True)",
    "('day', '<=', time.strftime('%Y-%m-%d'))",
    "('day', '>', context_today())",
    "('day', '=', fields.Date.today())",
    "('day', '=', False)",
    "('moment', '<', datetime.now())",
    "('moment', '>=', fields.Datetime.now())",
    "('moment', '!=', False)",
    "('company_id', 'in', company_ids)",
    "('company_id', 'in', company_ids + [False])",
    "('company_id', '=', company_id)",
    "('company_id', '=', False)",
    "('company_id', 'not in', company_ids)",
    "('user_id', '=', user.id)",
    "('user_id', 'in', [user.id, False])",
    "('user_id', '!=', user.id)",
    "('partner_id', '=', user.partner_id.id)",
    "('partner_id', '!=', user.partner_id.id)",
    "('partner_id.commercial_partner_id', '=', user.partner_id.commercial_partner_id.id)",
    "('partner_id', 'in', user.partner_id.ids)",
    "('categ_id', 'any', [('seq', '>', 2)])",
    "('categ_id', 'not any', [('name', '=', 'internal')])",
    "('categ_id.seq', '<', 3)",
    "('categ_id.name', '=', 'c1')",
    "('categ_id.company_id', 'in', company_ids)",
    "('categ_id', 'access', 'read')",
    "('categ_id', 'access', 'write')",
    "('categ_id', '=', False)",
    "('id', 'in', [1, 2, 3, 5, 8, 13])",
    "('id', 'not in', [2, 4])",
    // rules that do not compile: they deny their (model, operation) with the rule's error
    "('val', '>', user.id + 1)",
    "('nope', '=', 1)",
];

/// Whether `leaf` is one of the conditions above that do not compile.
pub fn is_error_leaf(leaf: &str) -> bool {
    leaf.contains("user.id + 1") || leaf.contains("'nope'")
}

/// Conditions of `x.categ` rules (the target of the `access` and `any` conditions above).
pub const CATEG_LEAVES: &[&str] = &[
    "('seq', '>', 1)",
    "('seq', '<=', 2)",
    "('seq', '=', False)",
    "('name', '!=', 'internal')",
    "('name', 'in', ['c1', 'c2'])",
    "('name', '=', False)",
    "('company_id', 'in', company_ids)",
    "('company_id', 'in', company_ids + [False])",
];

/// Conditions of `x.child` rules: its own fields, fields inherited from `x.obj` and the
/// delegation field.
pub const CHILD_LEAVES: &[&str] = &[
    "('label', '=', 'l1')",
    "('label', '!=', False)",
    "('size', '>', 1)",
    "('size', '<', 1)",
    "('size', '=', False)",
    "('obj_id.val', '>', 2)",
    "('val', '<', 3)",
    "('name', '=', 'a')",
    "('company_id', 'in', company_ids)",
    "('user_id', '=', user.id)",
    "('obj_id', 'access', 'read')",
];

#[derive(Clone, PartialEq, Eq)]
pub enum Term {
    /// A condition: an index into the model's leaf table (modulo its length).
    Leaf(u8),
    /// `'&', a, b`.
    And(Box<Term>, Box<Term>),
    /// `'|', a, b`.
    Or(Box<Term>, Box<Term>),
    /// `'!', a`.
    Not(Box<Term>),
}

impl fmt::Debug for Term {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Leaf(i) => write!(f, "L{i}"),
            Self::And(a, b) => write!(f, "&({a:?}, {b:?})"),
            Self::Or(a, b) => write!(f, "|({a:?}, {b:?})"),
            Self::Not(a) => write!(f, "!({a:?})"),
        }
    }
}

impl Term {
    fn tokens(&self, leaves: &[&str], out: &mut Vec<String>) {
        match self {
            Self::Leaf(i) => out.push(leaves[usize::from(*i) % leaves.len()].to_owned()),
            Self::And(a, b) => {
                out.push("'&'".to_owned());
                a.tokens(leaves, out);
                b.tokens(leaves, out);
            }
            Self::Or(a, b) => {
                out.push("'|'".to_owned());
                a.tokens(leaves, out);
                b.tokens(leaves, out);
            }
            Self::Not(a) => {
                out.push("'!'".to_owned());
                a.tokens(leaves, out);
            }
        }
    }

    /// The same term with every `access` condition under a negation replaced by a condition
    /// without one (property P4: a negated `access` condition makes a policy shrink when a group
    /// widens the target's access, so monotonicity does not hold for it by design).
    pub fn without_negated_access(&self, leaves: &[&str], negated: bool) -> Term {
        match self {
            Self::Leaf(i) => {
                let text = leaves[usize::from(*i) % leaves.len()];
                if negated && text.contains("'access'") {
                    Self::Leaf(0)
                } else {
                    self.clone()
                }
            }
            Self::And(a, b) => Self::And(
                Box::new(a.without_negated_access(leaves, negated)),
                Box::new(b.without_negated_access(leaves, negated)),
            ),
            Self::Or(a, b) => Self::Or(
                Box::new(a.without_negated_access(leaves, negated)),
                Box::new(b.without_negated_access(leaves, negated)),
            ),
            Self::Not(a) => Self::Not(Box::new(a.without_negated_access(leaves, !negated))),
        }
    }
}

/// The groups a rule may name: `None` makes it a restriction.
pub const RULE_GROUPS: [i64; 6] = [USER, G1, G2, G3, EVERYONE, PORTAL];

/// One `ir.access` row.
#[derive(Clone, PartialEq, Eq)]
pub struct RuleSpec {
    /// Its model.
    pub on: On,
    /// Index into [`RULE_GROUPS`]; `None` for a restriction.
    pub group: Option<usize>,
    /// Operations as a bit set over `r`, `w`, `c`, `d` (1..=15).
    pub ops: u8,
    /// The domain: the terms of an implicit AND, or no domain (`TRUE`).
    pub domain: Option<Vec<Term>>,
}

impl fmt::Debug for RuleSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} {} {} {}",
            self.on.model(),
            self.group_id()
                .map_or_else(|| "restriction".to_owned(), |g| format!("group {g}")),
            self.operation(),
            self.text().unwrap_or_else(|| "(no domain)".to_owned())
        )
    }
}

impl RuleSpec {
    /// The group id, `None` for a restriction.
    pub fn group_id(&self) -> Option<i64> {
        self.group.map(|g| RULE_GROUPS[g % RULE_GROUPS.len()])
    }

    pub fn operation(&self) -> String {
        let ops = if self.ops & 0x0F == 0 { 1 } else { self.ops };
        [(4, 'c'), (1, 'r'), (2, 'u'), (8, 'd')]
            .into_iter()
            .filter(|(bit, _)| ops & bit != 0)
            .map(|(_, c)| c)
            .collect()
    }

    /// The domain string, `None` without a domain.
    pub fn text(&self) -> Option<String> {
        let terms = self.domain.as_ref()?;
        let mut tokens = Vec::new();
        for term in terms {
            term.tokens(self.on.leaves(), &mut tokens);
        }
        Some(format!("[{}]", tokens.join(", ")))
    }

    /// The rule with every condition that does not compile replaced by one that does (rules
    /// written through the ORM are validated at write time, SEC-016's validation).
    pub fn without_errors(&self) -> RuleSpec {
        fn clean(term: &Term, leaves: &[&str]) -> Term {
            match term {
                Term::Leaf(i) if is_error_leaf(leaves[usize::from(*i) % leaves.len()]) => {
                    Term::Leaf(0)
                }
                Term::Leaf(_) => term.clone(),
                Term::And(a, b) => {
                    Term::And(Box::new(clean(a, leaves)), Box::new(clean(b, leaves)))
                }
                Term::Or(a, b) => Term::Or(Box::new(clean(a, leaves)), Box::new(clean(b, leaves))),
                Term::Not(a) => Term::Not(Box::new(clean(a, leaves))),
            }
        }
        RuleSpec {
            domain: self
                .domain
                .as_ref()
                .map(|terms| terms.iter().map(|t| clean(t, self.on.leaves())).collect()),
            ..self.clone()
        }
    }

    /// The rule with no `access` condition under a negation (see [`Term::without_negated_access`]).
    pub fn without_negated_access(&self) -> RuleSpec {
        RuleSpec {
            domain: self.domain.as_ref().map(|terms| {
                terms
                    .iter()
                    .map(|t| t.without_negated_access(self.on.leaves(), false))
                    .collect()
            }),
            ..self.clone()
        }
    }
}

/// The direct groups a principal may hold.
pub const PRINCIPAL_GROUPS: [i64; 5] = [USER, G1, G2, G3, PORTAL];

/// A user principal: subject, direct groups, allowed companies.
#[derive(Clone, PartialEq, Eq)]
pub struct PrincipalSpec {
    /// The subject (2..=5, seeded users).
    pub uid: i64,
    /// Direct groups, a bit set over [`PRINCIPAL_GROUPS`].
    pub groups: u8,
    /// Allowed companies, indexes into the subject's companies (mapped and deduplicated, never
    /// empty).
    pub companies: Vec<u8>,
}

impl fmt::Debug for PrincipalSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "uid {} groups {:?} companies {:?}",
            self.uid,
            self.direct_groups(),
            self.allowed()
        )
    }
}

impl PrincipalSpec {
    /// The direct group ids.
    pub fn direct_groups(&self) -> Vec<i64> {
        PRINCIPAL_GROUPS
            .iter()
            .enumerate()
            .filter(|(i, _)| self.groups & (1 << i) != 0)
            .map(|(_, g)| *g)
            .collect()
    }

    /// The allowed companies, current first.
    pub fn allowed(&self) -> Vec<i64> {
        let all = super::user_companies(self.uid);
        let mut out: Vec<i64> = Vec::new();
        for i in &self.companies {
            let company = all[usize::from(*i) % all.len()];
            if !out.contains(&company) {
                out.push(company);
            }
        }
        if out.is_empty() {
            out.push(all[0]);
        }
        out
    }
}

/// One generated scenario: rows of every fixture model, rules, principals.
#[derive(Clone)]
pub struct Case {
    /// `x.partner` rows: (name, commercial partner) indexes.
    pub partners: Vec<(u8, u8)>,
    /// The partner index of users 2..=5 (0: none).
    pub user_partners: Vec<u8>,
    /// `x.categ` rows: (name, seq, company) indexes.
    pub categs: Vec<(u8, u8, u8)>,
    /// `x.obj` rows (see [`ObjRow`]).
    pub objs: Vec<ObjRow>,
    /// `x.child` rows: (obj, label, size) indexes.
    pub children: Vec<(u16, u8, u8)>,
    /// The rules.
    pub rules: Vec<RuleSpec>,
    /// The principals.
    pub principals: Vec<PrincipalSpec>,
}

impl fmt::Debug for Case {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Case")
            .field("rules", &self.rules)
            .field("principals", &self.principals)
            .field("partners", &self.partners)
            .field("user_partners", &self.user_partners)
            .field("categs", &self.categs)
            .field("objs", &self.objs)
            .field("children", &self.children)
            .finish()
    }
}

/// The value indexes of one `x.obj` row: name, code, val, qty, amount, flag, day, moment,
/// categ, partner, company, user.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct ObjRow(pub [u8; 12]);

impl fmt::Debug for ObjRow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self.0)
    }
}

/// Row and rule counts of a run.
#[derive(Debug, Clone)]
pub struct Sizes {
    /// `x.obj` rows.
    pub objs: RangeInclusive<usize>,
    /// `x.child` rows.
    pub children: RangeInclusive<usize>,
    /// Rules.
    pub rules: RangeInclusive<usize>,
    /// Principals.
    pub principals: RangeInclusive<usize>,
}

impl Sizes {
    /// The design note's sizes (§8.2): 50–500 rows, 1–6 rules, 1–4 principals.
    pub fn full() -> Self {
        Self {
            objs: 50..=500,
            children: 10..=80,
            rules: 1..=6,
            principals: 1..=4,
        }
    }

    /// Smaller tables for the runs in every CI (same shapes, fewer rows).
    pub fn ci() -> Self {
        Self {
            objs: 20..=80,
            children: 5..=25,
            rules: 1..=6,
            principals: 1..=4,
        }
    }

    /// Few rows: per-record statements (point checks) stay affordable.
    pub fn point() -> Self {
        Self {
            objs: 8..=30,
            children: 3..=10,
            rules: 1..=5,
            principals: 1..=3,
        }
    }
}

/// A domain term, at most three levels deep.
pub fn term() -> impl Strategy<Value = Term> {
    any::<u8>()
        .prop_map(Term::Leaf)
        .prop_recursive(3, 8, 2, |inner| {
            prop_oneof![
                (inner.clone(), inner.clone())
                    .prop_map(|(a, b)| Term::And(Box::new(a), Box::new(b))),
                (inner.clone(), inner.clone())
                    .prop_map(|(a, b)| Term::Or(Box::new(a), Box::new(b))),
                inner.prop_map(|a| Term::Not(Box::new(a))),
            ]
        })
}

fn on() -> impl Strategy<Value = On> {
    prop_oneof![3 => Just(On::Obj), 1 => Just(On::Categ), 1 => Just(On::Child)]
}

/// A rule of any kind.
pub fn rule() -> impl Strategy<Value = RuleSpec> {
    (
        on(),
        prop::option::of(0usize..RULE_GROUPS.len()),
        1u8..16,
        prop::option::weighted(0.85, prop::collection::vec(term(), 1..=2)),
    )
        .prop_map(|(on, group, ops, domain)| RuleSpec {
            on,
            group,
            ops,
            domain,
        })
}

/// The index of `base.group_everyone` in [`RULE_GROUPS`].
const EVERYONE_RULE: usize = 4;

/// A permission of `base.group_everyone` on `on` (every principal with a user type holds it):
/// without one, most (model, operation) pairs would have no permission at all and every
/// predicate would be `FALSE`.
pub fn base_permission(on: On) -> impl Strategy<Value = RuleSpec> {
    (
        prop_oneof![2 => Just(15u8), 1 => 1u8..16],
        prop::option::weighted(0.6, prop::collection::vec(term(), 1..=2)),
    )
        .prop_map(move |(ops, domain)| RuleSpec {
            on,
            group: Some(EVERYONE_RULE),
            ops,
            domain,
        })
}

/// A restriction (a rule without group) on `on`.
pub fn restriction(on: On) -> impl Strategy<Value = RuleSpec> {
    (1u8..16, prop::collection::vec(term(), 1..=2)).prop_map(move |(ops, terms)| RuleSpec {
        on,
        group: None,
        ops,
        domain: Some(terms),
    })
}

/// A principal: mostly an internal user, sometimes a portal user, rarely neither.
pub fn principal() -> impl Strategy<Value = PrincipalSpec> {
    let groups = prop_oneof![
        7 => (0u8..32).prop_map(|g| g | 0b0_0001),
        2 => (0u8..32).prop_map(|g| (g & 0b0_1110) | 0b1_0000),
        1 => 0u8..32,
    ];
    (2i64..=5, groups, prop::collection::vec(0u8..3, 1..=3)).prop_map(|(uid, groups, companies)| {
        PrincipalSpec {
            uid,
            groups,
            companies,
        }
    })
}

/// A scenario of `sizes`.
pub fn case(sizes: &Sizes) -> impl Strategy<Value = Case> + use<> {
    (
        prop::collection::vec((0u8..7, 0u8..8), 2..=6),
        prop::collection::vec(0u8..8, 4),
        prop::collection::vec((0u8..4, 0u8..6, 0u8..4), 3..=8),
        prop::collection::vec(
            prop::array::uniform12(any::<u8>()).prop_map(ObjRow),
            sizes.objs.clone(),
        ),
        prop::collection::vec((any::<u16>(), 0u8..3, 0u8..5), sizes.children.clone()),
        (
            base_permission(On::Obj),
            base_permission(On::Categ),
            base_permission(On::Child),
            prop::collection::vec(rule(), sizes.rules.clone()),
        ),
        prop::collection::vec(principal(), sizes.principals.clone()),
    )
        .prop_map(
            |(
                partners,
                user_partners,
                categs,
                objs,
                children,
                (obj, categ, child, more),
                principals,
            )| {
                let mut rules = vec![obj, categ, child];
                rules.extend(more);
                Case {
                    partners,
                    user_partners,
                    categs,
                    objs,
                    children,
                    rules,
                    principals,
                }
            },
        )
}
