//! The compiled policy: combination (SEC-001..005), `_inherits` (SEC-013), the `access`
//! operator and its cycles (DOM-035), grants and overrides, memos, errors and the build report.

use std::sync::Arc;

use erp_domain::{Domain, Operation, Operator, PrincipalParam, Value};
use erp_fields::ErrorKind;
use erp_orm::{
    AccessCtx, AccessDecision, AccessDomainImpl, AccessPolicy, AccessSuper, Capability,
    EvidenceColumn, EvidenceRow, ExplainDetail, ExplainRequest, GrantDomain, GrantRef, GrantSource,
    GroupId, GroupSet, KindClass, Principal, PrincipalBinding, RecordEvidence, Scope,
    SnapshotOrigin,
};
use erp_registry::ModelSpec;

use super::*;
use crate::rows::{CapabilityRow, Implication, PolicyRows};
use crate::testing::{
    ALL, BOB, CATEG, CHILD, Db, EVERYONE, G1, G2, G3, GRANDCHILD, NO_ONE, PORTAL, SOME_OBJ,
    UNCHECKED, USER, V, base_groups, bind, g, model, objs, on, principal, principal_in,
    registry_arc, rows, rule, snapshot,
};

const OPS: [Operation; 4] = [
    Operation::Read,
    Operation::Write,
    Operation::Create,
    Operation::Unlink,
];

/// Bob: `base.group_user`, Group 1 and Group 2, not Group 3.
fn bob(policy: &PolicySnapshot) -> Principal {
    principal(policy, BOB, &[USER, G1, G2])
}

/// The ids of `objs()` Bob may `op` (the oracle over the compiled predicate).
fn allowed(policy: &PolicySnapshot, op: Operation) -> Vec<i64> {
    let bob = bob(policy);
    let b = bind(policy, &bob);
    let domain = policy.access_domain(&b, model(SOME_OBJ), op).unwrap();
    objs().filter(SOME_OBJ, &ALL, domain.domain(), BOB)
}

fn model_error(policy: &PolicySnapshot, b: &PrincipalBinding<'_>, op: Operation) -> String {
    let err = policy.check_model(b, model(SOME_OBJ), op).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access);
    err.message().to_owned()
}

fn not_allowed(verb: &str, groups: &[&str]) -> String {
    let info = if groups.is_empty() {
        "No group currently allows this operation.".to_owned()
    } else {
        let list: Vec<String> = groups.iter().map(|g| format!("\t- {g}")).collect();
        format!(
            "This operation is allowed for the following groups:\n{}",
            list.join("\n")
        )
    };
    format!(
        "You are not allowed to {verb} 'Object For Test Access Right' \
         (test_access_right.some_obj) records.\n\n{info}\n\nContact your administrator to request \
         access if necessary."
    )
}

/// oracle rows; the ORM entry points are not exercised here)
#[test]
fn spec_sec_001_no_access_record_means_no_access() {
    let policy = snapshot(vec![]);
    let bob = bob(&policy);
    let b = bind(&policy, &bob);
    for (op, verb) in OPS.iter().zip(["access", "modify", "create", "delete"]) {
        let domain = policy.access_domain(&b, model(SOME_OBJ), *op).unwrap();
        assert!(domain.is_false(), "{op}");
        assert!(allowed(&policy, *op).is_empty());
        assert_eq!(model_error(&policy, &b, *op), not_allowed(verb, &[]));
    }
}

/// oracle rows; the ORM entry points are not exercised here)
#[test]
fn spec_sec_002_restrictions_alone_grant_nothing() {
    let policy = snapshot(vec![on(10, "all", Some(&ALL), None, "r")]);
    let bob = bob(&policy);
    let b = bind(&policy, &bob);
    for op in [Operation::Read, Operation::Write] {
        assert!(
            policy
                .access_domain(&b, model(SOME_OBJ), op)
                .unwrap()
                .is_false()
        );
        assert!(allowed(&policy, op).is_empty());
    }
    assert_eq!(
        model_error(&policy, &b, Operation::Read),
        not_allowed("access", &[])
    );
}

/// oracle rows; the ORM entry points are not exercised here)
#[test]
fn spec_sec_003_permissions_are_ored() {
    // two permissions in one group
    let policy = snapshot(vec![
        on(10, "a", Some(&[1, 2]), Some(G1), "r"),
        on(11, "b", Some(&[1, 3]), Some(G1), "r"),
    ]);
    assert_eq!(allowed(&policy, Operation::Read), [1, 2, 3]);
    // permissions in distinct groups; Bob is not in Group 3
    let policy = snapshot(vec![
        on(10, "a", Some(&[1, 2]), Some(G1), "r"),
        on(11, "b", Some(&[1, 3]), Some(G2), "r"),
        on(12, "c", Some(&[1, 5]), Some(G3), "r"),
    ]);
    assert_eq!(allowed(&policy, Operation::Read), [1, 2, 3]);
    // the same rule twice changes nothing
    let policy = snapshot(vec![
        on(10, "a", Some(&[1, 2]), Some(G1), "r"),
        on(11, "a again", Some(&[1, 2]), Some(G1), "r"),
    ]);
    assert_eq!(allowed(&policy, Operation::Read), [1, 2]);
}

/// oracle rows; the ORM entry points are not exercised here)
#[test]
fn spec_sec_004_restrictions_are_anded() {
    let not_bowser: Vec<i64> = ALL[..5].to_vec();
    let not_peach: Vec<i64> = ALL.iter().copied().filter(|i| *i != 3).collect();
    // one permission, one restriction
    let policy = snapshot(vec![
        on(10, "p", Some(&[1, 3, 6]), Some(G1), "r"),
        on(11, "r", Some(&not_bowser), None, "r"),
    ]);
    assert_eq!(allowed(&policy, Operation::Read), [1, 3]);
    // two permissions, one restriction
    let two = || {
        vec![
            on(10, "p1", Some(&[1, 2, 6]), Some(G1), "r"),
            on(11, "p2", Some(&[1, 3, 6]), Some(G2), "r"),
            on(12, "r1", Some(&not_bowser), None, "r"),
        ]
    };
    assert_eq!(allowed(&snapshot(two()), Operation::Read), [1, 2, 3]);
    // two permissions, two restrictions
    let mut rules = two();
    rules.push(on(13, "r2", Some(&not_peach), None, "r"));
    assert_eq!(allowed(&snapshot(rules), Operation::Read), [1, 2]);
    // one partial and one full permission, one restriction ("special user manager")
    let policy = snapshot(vec![
        on(10, "p1", Some(&[1, 2, 6]), Some(G1), "r"),
        on(11, "p2", None, Some(G2), "r"),
        on(12, "r1", Some(&not_bowser), None, "r"),
    ]);
    assert_eq!(allowed(&policy, Operation::Read), not_bowser);
}

/// oracle rows; the ORM entry points are not exercised here)
#[test]
fn spec_sec_005_operation_subsets() {
    let not_bowser: Vec<i64> = ALL[..5].to_vec();
    let policy = snapshot(vec![
        on(10, "read all", None, Some(G1), "r"),
        on(11, "everything", None, Some(G2), "crud"),
        on(12, "keep bowser", Some(&not_bowser), None, "cud"),
    ]);
    assert_eq!(allowed(&policy, Operation::Read), ALL);
    for op in [Operation::Write, Operation::Create, Operation::Unlink] {
        assert_eq!(allowed(&policy, op), not_bowser, "{op}");
    }
    // read everything, update the humans
    let policy = snapshot(vec![
        on(10, "read", None, Some(G1), "r"),
        on(11, "humans", Some(&[1, 2, 3]), Some(G1), "u"),
    ]);
    assert_eq!(allowed(&policy, Operation::Read), ALL);
    assert_eq!(allowed(&policy, Operation::Write), [1, 2, 3]);
    assert!(allowed(&policy, Operation::Create).is_empty());
}

/// snapshot's group graph, names in one language; the ORM raising them is)
#[test]
fn spec_sec_008_model_level_error_message() {
    let policy = snapshot(vec![on(10, "g3", None, Some(G3), "cd")]);
    let bob = bob(&policy);
    let b = bind(&policy, &bob);
    assert_eq!(
        model_error(&policy, &b, Operation::Read),
        not_allowed("access", &[])
    );
    assert_eq!(
        model_error(&policy, &b, Operation::Write),
        not_allowed("modify", &[])
    );
    assert_eq!(
        model_error(&policy, &b, Operation::Create),
        not_allowed("create", &["Group 3"])
    );
    assert_eq!(
        model_error(&policy, &b, Operation::Unlink),
        not_allowed("delete", &["Group 3"])
    );
    // test_access_feedback: Group 1 `r`, "Group 0" `cr` (an implying group, non-ASCII names)
    let rows = PolicyRows::builder(1)
        .groups(base_groups())
        .group(crate::testing::group(8, "Group 0"))
        .group(crate::testing::group(9, "Gérant 経理"))
        .implications([
            Implication::new(g(USER), g(EVERYONE)),
            Implication::new(g(9), g(G1)),
        ])
        .rules([
            on(10, "g1", None, Some(G1), "r"),
            on(11, "g0", None, Some(8), "cr"),
        ])
        .build()
        .unwrap();
    let policy = Compiler::new(registry_arc()).compile(&rows).unwrap();
    let portal = principal(&policy, 20, &[PORTAL]);
    let b = bind(&policy, &portal);
    assert_eq!(
        model_error(&policy, &b, Operation::Read),
        not_allowed("access", &["Group 0", "Group 1", "Gérant 経理"])
    );
    assert_eq!(
        model_error(&policy, &b, Operation::Create),
        not_allowed("create", &["Group 0"])
    );
}

/// implying groups, from hand-built rows; the `res.groups` records are)
#[test]
fn spec_sec_011_groups_that_would_allow_an_operation() {
    let objs = model(SOME_OBJ);
    // Group 2 implies the internal user type, which implies everyone; the comodel is readable by
    // internal users and writable by Group 2
    let policy_of = |extra: Vec<RuleRow>| {
        let mut rules = vec![
            rule(20, "categ read", CATEG, "r", Some(USER), None),
            rule(21, "categ write", CATEG, "u", Some(G2), None),
        ];
        rules.extend(extra);
        let rows = PolicyRows::builder(1)
            .groups(base_groups())
            .implications([
                Implication::new(g(USER), g(EVERYONE)),
                Implication::new(g(PORTAL), g(EVERYONE)),
                Implication::new(g(G2), g(USER)),
            ])
            .rules(rules)
            .build()
            .unwrap();
        Compiler::new(registry_arc()).compile(&rows).unwrap()
    };
    let ids = |policy: &PolicySnapshot, model: &ModelSpec, op: Operation| -> Vec<i64> {
        policy
            .allowing_groups(model, op)
            .unwrap()
            .iter()
            .map(|g| g.get())
            .collect()
    };
    let sorted = |mut v: Vec<i64>| {
        v.sort_unstable();
        v
    };
    assert!(ids(&policy_of(vec![]), objs, Operation::Read).is_empty());
    let good = || on(10, "Good guys", Some(&[1, 2, 3, 4, 5]), Some(G1), "r");
    assert_eq!(ids(&policy_of(vec![good()]), objs, Operation::Read), [G1]);
    let base = policy_of(vec![]);
    assert_eq!(ids(&base, model(CATEG), Operation::Read), [G2, USER]);
    assert_eq!(ids(&base, model(CATEG), Operation::Write), [G2]);
    let delegated = |group: i64, domain: &str| {
        policy_of(vec![
            good(),
            rule(11, "Delegated", SOME_OBJ, "r", Some(group), Some(domain)),
        ])
    };
    // everyone, through the access operator: the comodel's readers, not everyone
    let policy = delegated(EVERYONE, "[('categ_id', 'access', 'read')]");
    assert_eq!(sorted(ids(&policy, objs, Operation::Read)), [USER, G1, G2]);
    // a restricted group: its implying groups that also read the comodel
    let policy = delegated(G2, "[('categ_id', 'access', 'read')]");
    assert_eq!(sorted(ids(&policy, objs, Operation::Read)), [G1, G2]);
    // the access condition is necessary
    let policy = delegated(
        EVERYONE,
        "['&', ('categ_id', 'access', 'read'), ('id', '>', 5)]",
    );
    assert_eq!(sorted(ids(&policy, objs, Operation::Read)), [USER, G1, G2]);
    // not necessary: every group implying everyone
    let policy = delegated(
        EVERYONE,
        "['|', ('categ_id', 'access', 'read'), ('id', '>', 5)]",
    );
    assert_eq!(
        sorted(ids(&policy, objs, Operation::Read)),
        [USER, G1, G2, EVERYONE, PORTAL]
    );
    // two necessary conditions intersect
    let policy = delegated(
        EVERYONE,
        "['&', ('categ_id', 'access', 'read'), ('categ_id', 'access', 'write')]",
    );
    assert_eq!(sorted(ids(&policy, objs, Operation::Read)), [G1, G2]);
    // a negated condition subtracts
    let policy = delegated(
        EVERYONE,
        "['&', ('categ_id', 'access', 'read'), '!', ('categ_id', 'access', 'write')]",
    );
    assert_eq!(sorted(ids(&policy, objs, Operation::Read)), [USER, G1]);
    // `_inherits`: the child's groups are intersected with the parent's
    let policy = policy_of(vec![
        good(),
        rule(30, "child", CHILD, "r", Some(G2), None),
        rule(31, "child too", CHILD, "r", Some(G1), None),
    ]);
    assert_eq!(ids(&policy, model(CHILD), Operation::Read), [G1]);
}

/// The records Bob may read of a model with `_inherits` children.
fn children_db() -> Db {
    objs()
        .with(SOME_OBJ, 7, &[("val", V::Int(-1))])
        .with(CHILD, 1, &[("some_id", V::Ref(1))])
        .with(CHILD, 2, &[("some_id", V::Ref(7))])
        .with(GRANDCHILD, 1, &[("child_id", V::Ref(1))])
        .with(GRANDCHILD, 2, &[("child_id", V::Ref(2))])
}

/// `_filtered_access` through the ORM are in tests/access_props.rs)
#[test]
fn spec_sec_013_inherits_parent_access() {
    let policy = snapshot(vec![
        on(10, "See all", None, Some(G1), "r"),
        rule(
            11,
            "Forbid negatives",
            SOME_OBJ,
            "r",
            None,
            Some("[('val', '>', 0)]"),
        ),
        rule(12, "children", CHILD, "r", Some(G1), None),
        rule(13, "grandchildren", GRANDCHILD, "r", Some(G1), None),
    ]);
    let bob = bob(&policy);
    let b = bind(&policy, &bob);
    let child = policy
        .access_domain(&b, model(CHILD), Operation::Read)
        .unwrap();
    let parent = policy
        .access_domain(&b, model(SOME_OBJ), Operation::Read)
        .unwrap();
    assert_eq!(
        child.domain(),
        &Domain::condition(
            "some_id",
            Operator::AnyBypass,
            Value::Domain(parent.domain().clone())
        )
        .unwrap()
    );
    let mut db = children_db();
    assert_eq!(db.filter(CHILD, &[1, 2], child.domain(), BOB), [1]);
    let grandchild = policy
        .access_domain(&b, model(GRANDCHILD), Operation::Read)
        .unwrap();
    assert_eq!(
        db.filter(GRANDCHILD, &[1, 2], grandchild.domain(), BOB),
        [1]
    );
    // the parent's value changes: the child disappears
    db.set(SOME_OBJ, 1, "val", V::Int(-5));
    assert!(db.filter(CHILD, &[1, 2], child.domain(), BOB).is_empty());
    assert!(
        db.filter(GRANDCHILD, &[1, 2], grandchild.domain(), BOB)
            .is_empty()
    );
    // the child reads the delegate and the parent's fields; it does not sub-select its own table
    let reads = child.reads();
    let some_id = model(CHILD).field_id("some_id").unwrap();
    let val = model(SOME_OBJ).field_id("val").unwrap();
    assert!(
        reads.contains(&(model(CHILD).id, some_id)) && reads.contains(&(model(SOME_OBJ).id, val))
    );
    assert!(!child.reads_own_table());
    // a parent that allows nothing denies the child, whatever the child's rules
    let stranger = principal(&policy, 21, &[USER, G2]);
    let b = bind(&policy, &stranger);
    assert!(
        policy
            .access_domain(&b, model(SOME_OBJ), Operation::Read)
            .unwrap()
            .is_false()
    );
    assert!(
        policy
            .access_domain(&b, model(CHILD), Operation::Read)
            .unwrap()
            .is_false()
    );
}

#[test]
fn inherits_access_false_leaves_the_parent_out() {
    let policy = snapshot(vec![
        rule(
            10,
            "state a",
            SOME_OBJ,
            "r",
            Some(G1),
            Some("[('name', '=', 'a')]"),
        ),
        rule(11, "boxes", CHILD, "ru", Some(G1), None),
        rule(12, "unchecked", UNCHECKED, "ru", Some(G1), None),
    ]);
    assert!(model(CHILD).inherits_access);
    assert!(!model(UNCHECKED).inherits_access);
    let bob = bob(&policy);
    let b = bind(&policy, &bob);
    let child = policy
        .access_domain(&b, model(CHILD), Operation::Read)
        .unwrap();
    assert_eq!(
        child.domain(),
        &Domain::condition(
            "some_id",
            Operator::AnyBypass,
            Value::Domain(Domain::condition("name", Operator::Eq, "a").unwrap()),
        )
        .unwrap()
    );
    let unchecked = policy
        .access_domain(&b, model(UNCHECKED), Operation::Read)
        .unwrap();
    assert!(unchecked.domain().is_true(), "{:?}", unchecked.domain());
    // the parent allows no write: the child is denied, the unchecked model is not
    let child_write = policy
        .access_domain(&b, model(CHILD), Operation::Write)
        .unwrap();
    assert!(child_write.domain().is_false());
    let unchecked_write = policy
        .access_domain(&b, model(UNCHECKED), Operation::Write)
        .unwrap();
    assert!(unchecked_write.domain().is_true());
}

/// Compile half: the child's predicate is `delegate any! <parent predicate>`,
/// the shape the SQL builder joins once for ordering and filtering. The statement and its
/// results are `spec_sec_045_parent_rule_in_a_child_search_ordered_by_an_inherited_field`
/// (`tests/spec_whole.rs`), the whole case.
#[test]
fn spec_sec_045_parent_rule_in_the_child_predicate() {
    let policy = snapshot(vec![
        rule(
            10,
            "state a",
            SOME_OBJ,
            "r",
            Some(G1),
            Some("[('name', '=', 'a')]"),
        ),
        rule(11, "boxes", CHILD, "r", Some(G1), None),
    ]);
    let bob = bob(&policy);
    let b = bind(&policy, &bob);
    let child = policy
        .access_domain(&b, model(CHILD), Operation::Read)
        .unwrap();
    assert_eq!(
        child.domain(),
        &Domain::condition(
            "some_id",
            Operator::AnyBypass,
            Value::Domain(Domain::condition("name", Operator::Eq, "a").unwrap()),
        )
        .unwrap()
    );
}

/// Compile half: the compiled read predicate, relational paths as `any!`
/// sub-domains. The SQL and its results are
/// `spec_sec_046_read_access_domain_is_part_of_the_search_statement` (`tests/spec_whole.rs`),
/// the whole case.
#[test]
fn spec_sec_046_read_predicate_with_paths() {
    let policy = snapshot(vec![rule(
        10,
        "categ x",
        SOME_OBJ,
        "r",
        Some(G1),
        Some("[('categ_id.name', '=', 'x'), ('user_id', '=', user.id)]"),
    )]);
    let bob = bob(&policy);
    let b = bind(&policy, &bob);
    let read = policy
        .access_domain(&b, model(SOME_OBJ), Operation::Read)
        .unwrap();
    assert_eq!(
        read.domain(),
        &Domain::and([
            Domain::condition(
                "categ_id",
                Operator::AnyBypass,
                Value::Domain(Domain::condition("name", Operator::Eq, "x").unwrap()),
            )
            .unwrap(),
            Domain::condition("user_id", Operator::Eq, Value::Param(PrincipalParam::Uid)).unwrap(),
        ])
    );
    assert_eq!(
        read.rules().iter().map(|r| r.id()).collect::<Vec<_>>(),
        [10]
    );
}

/// Snapshot half: the model-level decision is memoized per (model, operation,
/// group set, shape). The statement counts of the model-level and record-level checks are
/// `spec_sec_047_model_level_versus_record_level_evaluation` (`tests/spec_whole.rs`), the whole
/// case.
#[test]
fn spec_sec_047_model_level_decisions_are_memoized() {
    let policy = snapshot(vec![on(10, "humans", Some(&[1, 2, 3]), Some(G1), "r")]);
    let bob = bob(&policy);
    let b = bind(&policy, &bob);
    let first = policy
        .access_domain(&b, model(SOME_OBJ), Operation::Read)
        .unwrap();
    assert_eq!(policy.combined.len(), 1);
    let again = policy
        .access_domain(&b, model(SOME_OBJ), Operation::Read)
        .unwrap();
    assert_eq!(policy.combined.len(), 1, "served from the memo");
    assert_eq!(first.domain(), again.domain());
    // another principal with the same groups shares the entry (no principal values in it)
    let other = principal(&policy, 99, &[USER, G1, G2]);
    let b2 = bind(&policy, &other);
    assert_eq!(b2.groups(), b.groups());
    policy
        .access_domain(&b2, model(SOME_OBJ), Operation::Read)
        .unwrap();
    assert_eq!(policy.combined.len(), 1);
    // another group set does not
    let g1_only = principal(&policy, 99, &[USER, G1]);
    let b3 = bind(&policy, &g1_only);
    policy
        .access_domain(&b3, model(SOME_OBJ), Operation::Read)
        .unwrap();
    assert_eq!(policy.combined.len(), 2);
}

/// The engine's evidence statement, played by the oracle: each column on each denied record.
fn evidence_of(columns: &[EvidenceColumn], denied: &[i64]) -> RecordEvidence {
    let names = ["Mario", "Luigi", "Peach", "Toad", "Yoshi", "Bowser"];
    let rows = denied
        .iter()
        .map(|id| {
            let results = columns
                .iter()
                .map(|c| objs().matches(SOME_OBJ, *id, c.domain(), BOB))
                .collect();
            let name = names[usize::try_from(*id - 1).unwrap()];
            EvidenceRow::new(*id, Some(name.to_owned()), results)
        })
        .collect();
    RecordEvidence::new(columns.to_vec(), rows, "Bob Bobman").unwrap()
}

/// statement and the engine error path are in tests/sec009.rs)
#[test]
fn spec_sec_009_record_level_error_message() {
    let humans = [1, 2, 3];
    let cases: [(Vec<RuleRow>, &[&str]); 3] = [
        (
            vec![on(10, "Restrict to humans", Some(&humans), Some(G1), "r")],
            &["Restrict to humans"],
        ),
        (
            vec![
                on(10, "See all", None, Some(G1), "r"),
                on(11, "Restrict to humans", Some(&humans), None, "r"),
            ],
            &["Restrict to humans"],
        ),
        (
            vec![
                on(10, "See good guys", Some(&ALL[..5]), Some(G1), "r"),
                on(11, "Restrict to humans", Some(&humans), None, "r"),
            ],
            &["See good guys", "Restrict to humans"],
        ),
    ];
    let head = format!(
        "Uh-oh! Looks like you have stumbled upon some top-secret records.\n\nSorry, Bob Bobman \
         (id={BOB}) doesn't have 'read' access to:"
    );
    let tail = "If you really, really need access, perhaps you can win over your friendly \
                administrator with a batch of freshly baked cookies.";
    let denied = [4, 5, 6];
    for (rules, blamed) in cases {
        let policy = snapshot(rules);
        assert_eq!(allowed(&policy, Operation::Read), humans);
        for debug in [false, true] {
            // Bob, and in debug mode with the technical features group
            let bob = principal_in(&policy, BOB, &[USER, G1, G2, NO_ONE], debug);
            let b = bind(&policy, &bob);
            let request = ExplainRequest::new(&b, model(SOME_OBJ), Operation::Read)
                .ids(&denied)
                .unwrap()
                .detail(ExplainDetail::Records);
            // step 1: the columns the engine evaluates
            let asked = policy.explain(&request).unwrap();
            assert_eq!(asked.decision(), AccessDecision::Deny);
            assert!(asked.message().is_none() && asked.blamed().is_empty());
            let evidence = evidence_of(asked.columns(), &denied);
            // step 2: blame and text
            let explained = policy.explain(&request.evidence(&evidence)).unwrap();
            assert_eq!(
                explained
                    .blamed()
                    .iter()
                    .map(|r| r.name())
                    .collect::<Vec<_>>(),
                blamed
            );
            let expected = if debug {
                let listing = [
                    "- Object For Test Access Right, Toad (test_access_right.some_obj: 4)",
                    "- Object For Test Access Right, Yoshi (test_access_right.some_obj: 5)",
                    "- Object For Test Access Right, Bowser (test_access_right.some_obj: 6)",
                ]
                .join("\n");
                let blame: Vec<String> = blamed.iter().map(|n| format!("- {n}")).collect();
                format!(
                    "{head}\n{listing}\n\nBlame the following accesses:\n{}\n\n{tail}",
                    blame.join("\n")
                )
            } else {
                format!(
                    "{head}\n- Object For Test Access Right (test_access_right.some_obj)\n\n{tail}"
                )
            };
            assert_eq!(explained.message(), Some(expected.as_str()));
        }
    }
}

#[test]
fn debug_listing_needs_debug_mode_internal_and_technical_features() {
    let policy = snapshot(vec![on(10, "humans", Some(&[1, 2, 3]), Some(G1), "r")]);
    let denied = [4];
    let message = |direct: &[i64], debug: bool| {
        let p = principal_in(&policy, BOB, direct, debug);
        let b = bind(&policy, &p);
        let request = ExplainRequest::new(&b, model(SOME_OBJ), Operation::Read)
            .ids(&denied)
            .unwrap();
        let asked = policy.explain(&request).unwrap();
        let evidence = evidence_of(asked.columns(), &denied);
        let explained = policy.explain(&request.evidence(&evidence)).unwrap();
        explained
            .message()
            .unwrap()
            .contains("Blame the following accesses")
    };
    assert!(message(&[USER, G1, NO_ONE], true));
    assert!(!message(&[USER, G1, NO_ONE], false), "debug mode off");
    assert!(!message(&[USER, G1], true), "no technical features");
    assert!(!message(&[PORTAL, G1, NO_ONE], true), "a portal user");
}

#[test]
fn explain_answers_the_model_level_decision_with_its_text() {
    let policy = snapshot(vec![on(10, "humans", Some(&[1, 2, 3]), Some(G1), "r")]);
    let bob = bob(&policy);
    let b = bind(&policy, &bob);
    let allowed = policy
        .explain(&ExplainRequest::new(&b, model(SOME_OBJ), Operation::Read))
        .unwrap();
    assert_eq!(allowed.decision(), AccessDecision::Allow);
    assert!(allowed.message().is_none());
    let denied = policy
        .explain(&ExplainRequest::new(&b, model(SOME_OBJ), Operation::Unlink))
        .unwrap();
    assert_eq!(denied.decision(), AccessDecision::Deny);
    assert_eq!(denied.message(), Some(not_allowed("delete", &[]).as_str()));
    // evidence about another record than asked is refused
    let ids = [4];
    let request = ExplainRequest::new(&b, model(SOME_OBJ), Operation::Read)
        .ids(&ids)
        .unwrap();
    let columns = policy.explain(&request).unwrap().columns().to_vec();
    let evidence = evidence_of(&columns, &[5]);
    let err = policy.explain(&request.evidence(&evidence)).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "access evidence names record 5, which the explanation was not asked about"
    );
}

#[test]
fn evidence_is_bounded_and_consistent() {
    let policy = snapshot(vec![on(10, "humans", Some(&[1, 2, 3]), Some(G1), "r")]);
    let bob = bob(&policy);
    let b = bind(&policy, &bob);
    let columns = policy
        .evidence_columns(&b, model(SOME_OBJ), Operation::Read)
        .unwrap();
    let seven: Vec<EvidenceRow> = (1..=7)
        .map(|id| EvidenceRow::new(id, None, vec![false]))
        .collect();
    let err = RecordEvidence::new(columns.clone(), seven, "Bob").unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "access evidence covers at most 6 records, 7 were given"
    );
    let err =
        RecordEvidence::new(columns, vec![EvidenceRow::new(4, None, vec![])], "Bob").unwrap_err();
    assert_eq!(
        err.message(),
        "access evidence of record 4 has 0 results for 1 rules"
    );
    // explain refuses 7 ids before any evidence is gathered (erp-orm's request bound)
    let ids = [1, 2, 3, 4, 5, 6, 7];
    let err = ExplainRequest::new(&b, model(SOME_OBJ), Operation::Read)
        .ids(&ids)
        .unwrap_err();
    assert_eq!(
        err.message(),
        "an explanation covers at most 6 records, 7 were asked"
    );
}

/// Compile half: `access` conditions resolve against the comodel's predicate for
/// the same principal, `FALSE`, `!= False` or `any!`. The optimization levels, `filtered_domain`
/// and the searches over `ir_access` rows are `spec_dom_035_access_operator`
/// (`tests/spec_whole.rs`), the whole case.
#[test]
fn spec_dom_035_access_conditions_resolve_per_principal() {
    let rules = vec![
        // internal users read records of `val > 20`, Group 2 all, portal none
        rule(
            10,
            "internal",
            SOME_OBJ,
            "r",
            Some(USER),
            Some("[('val', '>', 20)]"),
        ),
        rule(11, "elevated", SOME_OBJ, "r", Some(G2), None),
        // the comodel: write limited by name for internal users, wider for Group 2
        rule(20, "categ read", CATEG, "r", Some(USER), None),
        rule(
            21,
            "categ write",
            CATEG,
            "u",
            Some(USER),
            Some("[('name', '=', 'foo')]"),
        ),
        rule(22, "categ write elevated", CATEG, "u", Some(G2), None),
        // the delegating rules
        rule(
            30,
            "by categ",
            CHILD,
            "r",
            Some(USER),
            Some("[('some_id', 'access', 'read')]"),
        ),
        rule(
            31,
            "own",
            SOME_OBJ,
            "u",
            Some(USER),
            Some("[('id', 'access', 'read')]"),
        ),
        rule(
            32,
            "categ ok",
            SOME_OBJ,
            "c",
            Some(USER),
            Some("[('categ_id', 'access', 'write')]"),
        ),
    ];
    let policy = snapshot(rules);
    let internal = principal(&policy, 30, &[USER]);
    let elevated = principal(&policy, 31, &[USER, G2]);
    let portal = principal(&policy, 32, &[PORTAL]);
    let domain = |p: &Principal, m: &str, op: Operation| {
        let b = bind(&policy, p);
        policy
            .access_domain(&b, model(m), op)
            .unwrap()
            .domain()
            .clone()
    };
    let val_gt_20 = Domain::condition("val", Operator::Gt, 20).unwrap();
    // `id access read` is the model's own read predicate
    assert_eq!(domain(&internal, SOME_OBJ, Operation::Write), val_gt_20);
    // many2one: any! of the comodel's write predicate for the same principal
    assert_eq!(
        domain(&internal, SOME_OBJ, Operation::Create),
        Domain::condition(
            "categ_id",
            Operator::AnyBypass,
            Value::Domain(Domain::condition("name", Operator::Eq, "foo").unwrap()),
        )
        .unwrap()
    );
    // full comodel access: the link must be set
    assert_eq!(
        domain(&elevated, SOME_OBJ, Operation::Create),
        Domain::condition("categ_id", Operator::Ne, false).unwrap()
    );
    // no access on the comodel: FALSE (portal users have no rule at all)
    assert!(domain(&portal, CHILD, Operation::Read).is_false());
    // inside a child's rule: inline, no extra query
    let child = domain(&internal, CHILD, Operation::Read);
    let parent_read = Domain::condition(
        "some_id",
        Operator::AnyBypass,
        Value::Domain(val_gt_20.clone()),
    )
    .unwrap();
    assert_eq!(child, Domain::and([parent_read.clone(), parent_read]));
}

#[test]
fn access_cycles_are_rule_errors() {
    // self: a read rule asking for read on the model itself
    let policy = snapshot(vec![rule(
        10,
        "self",
        SOME_OBJ,
        "r",
        Some(G1),
        Some("[('id', 'access', 'read')]"),
    )]);
    let issues = policy.report().rule_errors();
    assert_eq!(issues.len(), 1);
    assert_eq!(
        issues[0].to_string(),
        "Access rule 'self' (10) on test_access_right.some_obj cannot be compiled: its 'access' \
         conditions form a cycle: test_access_right.some_obj (read) -> test_access_right.some_obj \
         (read) [denies r]"
    );
    let bob = bob(&policy);
    let b = bind(&policy, &bob);
    let err = policy
        .access_domain(&b, model(SOME_OBJ), Operation::Read)
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access);
    assert_eq!(
        err.message(),
        "Access rule 'self' (10) on test_access_right.some_obj cannot be compiled: its 'access' \
         conditions form a cycle: test_access_right.some_obj (read) -> test_access_right.some_obj \
         (read)"
    );
    // two models: some_obj -> categ... -> some_obj, through the child's parent link
    let policy = snapshot(vec![
        rule(
            10,
            "a",
            CHILD,
            "r",
            Some(G1),
            Some("[('some_id', 'access', 'write')]"),
        ),
        rule(
            11,
            "b",
            SOME_OBJ,
            "u",
            Some(G1),
            Some("[('parent_id', 'access', 'read')]"),
        ),
        rule(
            12,
            "c",
            SOME_OBJ,
            "r",
            Some(G1),
            Some("[('id', 'access', 'write')]"),
        ),
    ]);
    let ids: Vec<i64> = policy
        .report()
        .rule_errors()
        .iter()
        .map(|i| i.rule().id())
        .collect();
    assert_eq!(
        ids,
        [11, 12],
        "the rules on the cycle; the tail (10) is not on it"
    );
    let message = policy.report().rule_errors()[0].error().to_string();
    assert_eq!(
        message,
        "its 'access' conditions form a cycle: test_access_right.some_obj (write) -> \
         test_access_right.some_obj (read) -> test_access_right.some_obj (write)"
    );
    // the tail is denied through the cycle's error when it is reached
    let b = bind(&policy, &bob);
    let err = policy
        .access_domain(&b, model(CHILD), Operation::Read)
        .unwrap_err();
    assert_eq!(
        err.message(),
        "Access rule 'c' (12) on test_access_right.some_obj cannot be compiled: its 'access' \
         conditions form a cycle: test_access_right.some_obj (read) -> test_access_right.some_obj \
         (write) -> test_access_right.some_obj (read)"
    );
    // a deep cycle with a tail: child -> obj(read) -> obj(write) -> obj(create) -> obj(read)
    let policy = snapshot(vec![
        rule(
            10,
            "tail",
            CHILD,
            "r",
            Some(G1),
            Some("[('some_id', 'access', 'read')]"),
        ),
        rule(
            11,
            "r",
            SOME_OBJ,
            "r",
            Some(G1),
            Some("[('id', 'access', 'write')]"),
        ),
        rule(
            12,
            "w",
            SOME_OBJ,
            "u",
            Some(G1),
            Some("[('id', 'access', 'create')]"),
        ),
        rule(
            13,
            "c",
            SOME_OBJ,
            "c",
            Some(G1),
            Some("[('id', 'access', 'read')]"),
        ),
    ]);
    let ids: Vec<i64> = policy
        .report()
        .rule_errors()
        .iter()
        .map(|i| i.rule().id())
        .collect();
    assert_eq!(ids, [11, 12, 13]);
    assert_eq!(
        policy.report().errors_by_model(),
        [("test_access_right.some_obj", 3)]
    );
}

#[test]
fn rule_errors_deny_only_where_they_apply() {
    let policy = snapshot(vec![
        on(10, "read", None, Some(G1), "r"),
        rule(
            11,
            "broken g3",
            SOME_OBJ,
            "r",
            Some(G3),
            Some("[('nope', '=', 1)]"),
        ),
        rule(
            12,
            "broken restriction",
            SOME_OBJ,
            "u",
            None,
            Some("[('val', '=', user.name)]"),
        ),
        on(13, "write", None, Some(G1), "u"),
    ]);
    // Bob is not in Group 3: its broken permission does not apply to him
    assert_eq!(allowed(&policy, Operation::Read), ALL);
    let bob = bob(&policy);
    let b = bind(&policy, &bob);
    let err = policy
        .access_domain(&b, model(SOME_OBJ), Operation::Write)
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access);
    assert_eq!(
        err.message(),
        "Access rule 'broken restriction' (12) on test_access_right.some_obj cannot be compiled: \
         'user.name' is not supported in access rules: user.name is a value of the user's record, \
         not a record id: rules compare ids (user.<path>.id or .ids)"
    );
    assert!(std::error::Error::source(&err).is_some());
    let g3 = principal(&policy, 40, &[USER, G3]);
    let b = bind(&policy, &g3);
    let err = policy
        .access_domain(&b, model(SOME_OBJ), Operation::Read)
        .unwrap_err();
    assert_eq!(
        err.message(),
        "Access rule 'broken g3' (11) on test_access_right.some_obj cannot be compiled: Invalid \
         field test_access_right.some_obj.nope in condition ('nope', '=', 1)"
    );
    // a broken permission allows nobody in the error message's group list
    assert_eq!(
        policy
            .allowing_groups(model(SOME_OBJ), Operation::Read)
            .unwrap(),
        [g(G1)]
    );
}

#[test]
fn non_ascii_names_pass_through_reports_and_messages() {
    let policy = snapshot(vec![
        rule(10, "règle 経理", "modèle.経理", "cu", Some(G1), None),
        rule(
            11,
            "lire tout · 全部",
            SOME_OBJ,
            "r",
            Some(G1),
            Some("[('name', '!=', 'Ünïcode 名前')]"),
        ),
    ]);
    let issues = policy.report().rule_errors();
    assert_eq!(issues.len(), 1);
    assert_eq!(
        issues[0].to_string(),
        "Access rule 'règle 経理' (10) on modèle.経理 cannot be compiled: the model 'modèle.経理' \
         does not exist [denies cu]"
    );
    assert_eq!(policy.report().errors_by_model(), [("modèle.経理", 1)]);
    let bob = bob(&policy);
    let b = bind(&policy, &bob);
    let read = policy
        .access_domain(&b, model(SOME_OBJ), Operation::Read)
        .unwrap();
    assert_eq!(
        read.rules().iter().map(|r| r.name()).collect::<Vec<_>>(),
        ["lire tout · 全部"]
    );
}

#[test]
fn rules_on_unknown_models_are_reported_and_inert() {
    let policy = snapshot(vec![
        rule(10, "gone", "x.uninstalled", "r", Some(G1), None),
        on(11, "read", None, Some(G1), "r"),
    ]);
    let issues = policy.report().rule_errors();
    assert_eq!(issues.len(), 1);
    assert_eq!(
        issues[0].to_string(),
        "Access rule 'gone' (10) on x.uninstalled cannot be compiled: the model 'x.uninstalled' \
         does not exist [denies r]"
    );
    assert_eq!(allowed(&policy, Operation::Read), ALL);
    assert_eq!(policy.report().counts().rules, 2);
    assert_eq!(policy.report().counts().compiled, 1);
}

/// A grant source for the tests: `user_id = user.id` on some_obj reads.
struct OwnRecords;

impl GrantSource for OwnRecords {
    fn name(&self) -> &'static str {
        "own"
    }

    fn applies(
        &self,
        _ctx: &AccessCtx<'_>,
        model: &ModelSpec,
        op: Operation,
    ) -> erp_fields::Result<bool> {
        Ok(model.name == SOME_OBJ && op == Operation::Read)
    }

    fn grant_domain(
        &self,
        ctx: &AccessCtx<'_>,
        _model: &ModelSpec,
        _op: Operation,
    ) -> erp_fields::Result<GrantDomain> {
        let mine = Domain::condition("user_id", Operator::Eq, ctx.param(PrincipalParam::Uid))?;
        GrantDomain::new(mine, GrantRef::new("own"))
    }
}

struct Broken;

impl GrantSource for Broken {
    fn name(&self) -> &'static str {
        "broken"
    }

    fn applies(
        &self,
        ctx: &AccessCtx<'_>,
        _model: &ModelSpec,
        _op: Operation,
    ) -> erp_fields::Result<bool> {
        ctx.has_group("base.no_such_group")
    }

    fn grant_domain(
        &self,
        _ctx: &AccessCtx<'_>,
        _model: &ModelSpec,
        _op: Operation,
    ) -> erp_fields::Result<GrantDomain> {
        unreachable!("applies fails first")
    }
}

static OWN: OwnRecords = OwnRecords;
static BROKEN: Broken = Broken;

#[test]
fn grants_widen_permissions_and_restrictions_still_apply() {
    let rows = rows(vec![
        on(10, "humans", Some(&[1, 2, 3]), Some(G1), "r"),
        on(11, "not bowser", Some(&ALL[..5]), None, "r"),
    ]);
    let policy = Compiler::new(registry_arc())
        .grant_sources([&OWN as &dyn GrantSource])
        .compile(&rows)
        .unwrap();
    let bob = bob(&policy);
    let b = bind(&policy, &bob);
    let read = policy
        .access_domain(&b, model(SOME_OBJ), Operation::Read)
        .unwrap();
    assert_eq!(read.grants(), [GrantRef::new("own")]);
    let db = objs().with(SOME_OBJ, 8, &[("user_id", V::Ref(BOB))]).with(
        SOME_OBJ,
        9,
        &[("user_id", V::Ref(BOB))],
    );
    let mut db = db;
    db.set(SOME_OBJ, 6, "user_id", V::Ref(BOB));
    db.set(SOME_OBJ, 5, "user_id", V::Ref(BOB));
    // humans or own records, but never Bowser (the restriction ANDs over the grant)
    assert_eq!(
        db.filter(SOME_OBJ, &[1, 2, 3, 4, 5, 6], read.domain(), BOB),
        [1, 2, 3, 5]
    );
    // a principal without any permission still gets the grant: grants widen
    let portal = principal(&policy, BOB, &[PORTAL]);
    let b = bind(&policy, &portal);
    let read = policy
        .access_domain(&b, model(SOME_OBJ), Operation::Read)
        .unwrap();
    assert_eq!(db.filter(SOME_OBJ, &ALL, read.domain(), BOB), [5]);
    // a source that errors denies with its error
    let policy = Compiler::new(registry_arc())
        .grant_sources([&BROKEN as &dyn GrantSource])
        .compile(&rows)
        .unwrap();
    let bob = self::bob(&policy);
    let b = bind(&policy, &bob);
    let err = policy
        .access_domain(&b, model(SOME_OBJ), Operation::Read)
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "has_group('base.no_such_group') in an access override of 'test_access_right.some_obj': \
         no such group"
    );
    // two sources with one name are refused at build
    let err = Compiler::new(registry_arc())
        .grant_sources([&OWN as &dyn GrantSource, &OWN as &dyn GrantSource])
        .compile(&rows)
        .unwrap_err();
    assert_eq!(
        err.message(),
        "two grant sources are named 'own': grant source names are unique"
    );
}

fn only_positive(
    ctx: &AccessCtx<'_>,
    op: Operation,
    sup: AccessSuper<'_>,
) -> erp_fields::Result<Domain> {
    let base = sup.call(ctx, op)?;
    Ok(Domain::and([
        base,
        Domain::condition("val", Operator::Gt, 0)?,
    ]))
}

fn back_to_itself(
    ctx: &AccessCtx<'_>,
    op: Operation,
    sup: AccessSuper<'_>,
) -> erp_fields::Result<Domain> {
    let base = sup.call(ctx, op)?;
    Ok(Domain::and([
        base,
        Domain::condition("parent_id", Operator::Access, op.as_str())?,
    ]))
}

static NARROW: AccessDomainImpl = AccessDomainImpl {
    addon: "test_access",
    model: SOME_OBJ,
    func: only_positive,
    file: file!(),
    line: line!(),
};

static CYCLIC: AccessDomainImpl = AccessDomainImpl {
    addon: "test_access",
    model: SOME_OBJ,
    func: back_to_itself,
    file: file!(),
    line: line!(),
};

#[test]
fn overrides_run_on_the_combined_domain() {
    let rows = rows(vec![on(10, "humans", Some(&[1, 2, 3]), Some(G1), "r")]);
    let policy = Compiler::new(registry_arc())
        .overrides([&NARROW])
        .compile(&rows)
        .unwrap();
    let bob = bob(&policy);
    let b = bind(&policy, &bob);
    let read = policy
        .access_domain(&b, model(SOME_OBJ), Operation::Read)
        .unwrap();
    let mut db = objs();
    db.set(SOME_OBJ, 2, "val", V::Int(-2));
    assert_eq!(db.filter(SOME_OBJ, &ALL, read.domain(), BOB), [1, 3]);
    // the override's `access` condition is resolved too, and a cycle through it is refused
    let policy = Compiler::new(registry_arc())
        .overrides([&CYCLIC])
        .compile(&rows)
        .unwrap();
    let b = bind(&policy, &bob);
    let err = policy
        .access_domain(&b, model(SOME_OBJ), Operation::Read)
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access);
    assert_eq!(
        err.message(),
        "The access domain of test_access_right.some_obj (read) cannot be computed: its 'access' \
         conditions form a cycle: test_access_right.some_obj (read) -> test_access_right.some_obj \
         (read)"
    );
}

#[test]
fn interner_and_memo_eviction_never_change_results() {
    let rows = rows(vec![
        on(10, "humans", Some(&[1, 2, 3]), Some(G1), "r"),
        on(11, "toad", Some(&[4]), Some(G2), "r"),
    ]);
    let policy = Compiler::new(registry_arc())
        .interner_capacity(1)
        .memo_capacity(1)
        .compile(&rows)
        .unwrap();
    let sets = [[USER, G1], [USER, G2], [USER, G1]];
    let mut ids: Vec<GroupSetId> = Vec::new();
    let mut results = Vec::new();
    for direct in sets {
        let p = principal(&policy, BOB, &direct);
        let b = bind(&policy, &p);
        ids.push(b.groups());
        let read = policy
            .access_domain(&b, model(SOME_OBJ), Operation::Read)
            .unwrap();
        results.push(objs().filter(SOME_OBJ, &ALL, read.domain(), BOB));
    }
    assert_eq!(results, [vec![1, 2, 3], vec![4], vec![1, 2, 3]]);
    assert!(
        ids[0] != ids[1] && ids[1] != ids[2] && ids[0] != ids[2],
        "ids are never reused"
    );
    let (memo, interner) = policy.memo_evictions();
    assert!(memo >= 2 && interner >= 2, "{memo} {interner}");
    let err = Compiler::new(registry_arc())
        .memo_capacity(0)
        .compile(&rows)
        .unwrap_err();
    assert_eq!(
        err.message(),
        "the combined access domain memo capacity must be non-zero (zero never means unlimited)"
    );
}

#[test]
fn interning_checks_groups_and_tells_share_sets() {
    let policy = snapshot(vec![]);
    let err = policy
        .intern_groups(&GroupSet::new([g(USER), g(77)]))
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access);
    assert_eq!(
        err.message(),
        "The group 77 does not exist (any more): the user's groups cannot be resolved."
    );
    let err = policy.groups(&[g(77)]).unwrap_err();
    assert_eq!(
        err.message(),
        "The group 77 does not exist (any more): the user's groups cannot be resolved."
    );
    assert!(
        !policy
            .intern_groups(&policy.groups(&[g(USER)]).unwrap())
            .unwrap()
            .share()
    );
    assert!(
        policy
            .intern_groups(&policy.groups(&[g(PORTAL)]).unwrap())
            .unwrap()
            .share()
    );
    let portal = principal(&policy, 50, &[PORTAL]);
    assert_eq!(bind(&policy, &portal).shape().class(), KindClass::Portal);
    assert!(policy.needs_resolved_principals());
}

#[test]
fn read_only_api_keys_refuse_every_other_operation() {
    let policy = snapshot(vec![on(10, "all", None, Some(G1), "crud")]);
    let scope = Scope::builder(erp_orm::ApiKeyId::new(3).unwrap(), "reporting")
        .models(erp_orm::ModelAllow::All)
        .methods(erp_orm::MethodAllow::All)
        .read_only(true)
        .build()
        .unwrap();
    let groups = policy.groups(&[g(USER), g(G1)]).unwrap();
    let companies = [erp_orm::CompanyId::new(1).unwrap()];
    let p = erp_orm::PrincipalBuilder::user(erp_orm::UserId::new(BOB).unwrap())
        .resolution(
            erp_orm::Resolution::in_id_order(groups, companies, std::num::NonZeroU64::MIN).unwrap(),
        )
        .companies(companies.to_vec())
        .scope(scope)
        .build()
        .unwrap();
    let b = bind(&policy, &p);
    policy
        .check_model(&b, model(SOME_OBJ), Operation::Read)
        .unwrap();
    let err = policy
        .check_model(&b, model(SOME_OBJ), Operation::Write)
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access);
    assert_eq!(
        err.message(),
        "The API key 'reporting' is read-only: it may not write 'Object For Test Access Right' \
         (test_access_right.some_obj)."
    );
}

erp_orm::capability!(TEST_CAP = "test_access.explain");

fn mapped_linked_capabilities_except(excluded: &str) -> Vec<CapabilityRow> {
    // The GC coordinator fixtures link TestDb and its RPC dependency. Map every unrelated
    // linked capability so these tests exercise only their deliberately missing mapping.
    // this crate's own capabilities are linked twice into its unit-test binary (the crate under
    // test and the copy its dev-dependencies use), so a name is mapped once
    let mut names: Vec<&str> = erp_orm::CAPABILITIES
        .iter()
        .map(|capability| capability.name())
        .filter(|name| *name != excluded)
        .collect();
    names.sort_unstable();
    names.dedup();
    names
        .into_iter()
        .map(|name| CapabilityRow::new(name, "base.group_user").unwrap())
        .collect()
}

#[test]
fn capabilities_without_a_mapping_are_reported_and_refused() {
    static EXTRA: Capability = Capability::__declare("test_access.extra", "tests");
    let rows = PolicyRows::builder(1)
        .groups(base_groups())
        .capabilities(
            mapped_linked_capabilities_except("test_access.explain")
                .into_iter()
                .chain([
                    CapabilityRow::new("test_access.explain", "base.group_user").unwrap(),
                    CapabilityRow::new("gone.addon", "base.group_user").unwrap(),
                ]),
        )
        .build()
        .unwrap();
    let policy = Compiler::new(registry_arc())
        .capabilities([&EXTRA])
        .compile(&rows)
        .unwrap();
    assert_eq!(
        policy.report().missing_capabilities(),
        ["test_access.extra"]
    );
    assert_eq!(policy.report().unknown_capabilities(), ["gone.addon"]);
    assert!(policy.report().has_errors());
    let bob = principal(&policy, BOB, &[USER]);
    let b = bind(&policy, &bob);
    let err = policy.has_capability(&b, &EXTRA).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "the capability 'test_access.extra' has no ir_capability row in this policy: map it to \
         groups (the build report lists it)"
    );
    assert!(policy.has_capability(&b, &TEST_CAP).unwrap());
    let portal = principal(&policy, BOB + 1, &[PORTAL]);
    assert!(
        !policy
            .has_capability(&bind(&policy, &portal), &TEST_CAP)
            .unwrap()
    );
    assert_eq!(
        policy.capability_spec("test_access.explain"),
        Some("base.group_user")
    );
}

#[test]
fn snapshots_have_their_own_identity_and_generation() {
    let rows = rows(vec![]);
    let shared = Compiler::new(registry_arc()).compile(&rows).unwrap();
    let private = Compiler::new(registry_arc())
        .origin(SnapshotOrigin::Private)
        .compile(&rows)
        .unwrap();
    assert!(shared.snapshot().is_shared());
    assert_eq!(private.snapshot().origin(), SnapshotOrigin::Private);
    assert!(private.snapshot().id() > shared.snapshot().id());
    assert_eq!(shared.generation(), private.generation());
    assert_eq!(shared.generation().policy(), 1);
    assert_eq!(shared.generation().registry(), registry_arc().generation());
}

#[test]
fn reads_index_maps_written_models_to_predicates() {
    let policy = snapshot(vec![
        rule(
            10,
            "partner",
            SOME_OBJ,
            "r",
            Some(G1),
            Some("[('partner_id', 'child_of', user.partner_id.id)]"),
        ),
        rule(11, "children", CHILD, "r", Some(G1), None),
    ]);
    let users = model("res.users").id;
    let index = policy.reads_index(users);
    assert!(index.contains(&(model(SOME_OBJ).id, Operation::Read)));
    assert!(
        index.contains(&(model(CHILD).id, Operation::Read)),
        "through _inherits"
    );
    assert!(policy.reads_index(model(CATEG).id).is_empty());
}

#[test]
fn group_cycles_are_reported() {
    let rows = PolicyRows::builder(1)
        .groups(base_groups())
        .capabilities(mapped_linked_capabilities_except("test_access.explain"))
        .implications([
            Implication::new(g(G1), g(G2)),
            Implication::new(g(G2), g(G1)),
            Implication::new(g(USER), g(EVERYONE)),
        ])
        .build()
        .unwrap();
    let policy = Compiler::new(registry_arc()).compile(&rows).unwrap();
    assert_eq!(policy.report().group_cycles(), [vec![g(G1), g(G2)]]);
    // a cycle is not an error (the only error here: this test binary registers a capability
    // the rows do not map)
    assert!(policy.report().rule_errors().is_empty());
    assert_eq!(
        policy.report().missing_capabilities(),
        ["test_access.explain"]
    );
    let closure: Vec<GroupId> = policy.groups(&[g(G1)]).unwrap().ids().to_vec();
    assert_eq!(closure, [g(G1), g(G2)]);
    let text = policy.report().to_string();
    assert!(
        text.contains("note: groups 2, 3 imply each other (a cycle)"),
        "{text}"
    );
}

fn _assert_send_sync() {
    fn check<T: Send + Sync>() {}
    check::<PolicySnapshot>();
    let _: Option<Arc<dyn AccessPolicy>> = None;
}
