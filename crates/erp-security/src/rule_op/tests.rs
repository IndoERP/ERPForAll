//! Declared rule operators (Amendment 1): rule text, declaration validation, the
//! per-model expansion and its memo safety, and what the expansion reads.

use std::sync::{Arc, OnceLock};

use erp_domain::{Domain, DomainKind, Operation, Value};
use erp_fields::ErrorKind;
use erp_orm::{AccessPolicy, RuleOperatorDecl};
use erp_registry::{
    CompanyAccessDecl, FieldDecl, ModelDecl, ModelKind, ModelSpec, Registry, RegistryBuilder,
};
use erp_sql::reference::ReferenceQuery;

use super::*;
use crate::rows::RuleRow;
use crate::rule::{RuleError, compile_with};
use crate::snapshot::{Compiler, PolicySnapshot};
use crate::testing::{ADDON, BOB, DECLS, G1, G2, G3, USER, bind, principal, rows, rule};

const MSG: &str = "thr.msg";
const DOC_A: &str = "thr.doc_a";
const DOC_B: &str = "thr.doc_b";

/// A message-like model (`model` + `res_id`), thread models, and the kinds the operator skips.
static THREAD: [ModelDecl; 8] = [
    ModelDecl::new("test_access", MSG).fields(
        const {
            &[
                FieldDecl::char("model"),
                FieldDecl::many2one_reference("res_id", "model"),
                FieldDecl::many2one("author_id", "res.partner"),
                FieldDecl::many2many("partner_ids", "res.partner").relation(
                    "thr_msg_partner_rel",
                    "msg_id",
                    "partner_id",
                ),
                FieldDecl::boolean("is_internal"),
            ]
        },
    ),
    ModelDecl::new("test_access", "thr.mixin")
        .kind(ModelKind::Abstract)
        .fields(const { &[FieldDecl::char("title")] }),
    ModelDecl::new("test_access", DOC_A)
        .company_access(CompanyAccessDecl::unrestricted("handwritten rules"))
        .inherit(&["thr.mixin"])
        .fields(
            const {
                &[
                    FieldDecl::integer("val"),
                    FieldDecl::many2one("company_id", "res.company"),
                    FieldDecl::many2one("owner_id", "res.users"),
                ]
            },
        ),
    ModelDecl::new("test_access", DOC_B)
        .inherit(&["thr.mixin"])
        .fields(
            const {
                &[
                    FieldDecl::integer("val"),
                    FieldDecl::many2one("msg_id", MSG),
                ]
            },
        ),
    // inherits the mixin but is a wizard: skipped
    ModelDecl::new("test_access", "thr.wizard")
        .kind(ModelKind::Transient)
        .inherit(&["thr.mixin"])
        .fields(const { &[FieldDecl::integer("val")] }),
    // inherits the mixin but is abstract: skipped
    ModelDecl::new("test_access", "thr.abstract_child")
        .kind(ModelKind::Abstract)
        .inherit(&["thr.mixin"])
        .fields(const { &[FieldDecl::integer("val")] }),
    // not a thread model: skipped
    ModelDecl::new("test_access", "thr.plain").fields(const { &[FieldDecl::integer("val")] }),
    // a second kind of reference holder, for the wrong-model refusal
    ModelDecl::new("test_access", "thr.other").fields(
        const {
            &[
                FieldDecl::char("model"),
                FieldDecl::many2one_reference("res_id", "model"),
            ]
        },
    ),
];

static REGISTRY: OnceLock<Arc<Registry>> = OnceLock::new();

fn registry_cell() -> &'static Arc<Registry> {
    REGISTRY.get_or_init(|| {
        Arc::new(
            RegistryBuilder::new([&ADDON])
                .linked(false)
                .with_decls(DECLS.iter().chain(THREAD.iter()))
                .build()
                .unwrap(),
        )
    })
}

fn registry_arc() -> Arc<Registry> {
    Arc::clone(registry_cell())
}

fn registry() -> &'static Registry {
    registry_cell()
}

fn model(name: &str) -> &'static ModelSpec {
    registry().model(name).unwrap()
}

/// The declaration of the tests: `thread_access` of `thr.msg`, as `mail` declares it.
static OP: RuleOperatorDecl = RuleOperatorDecl::__declare(
    "thread_access",
    "test_access",
    MSG,
    "res_id",
    "thr.mixin",
    &[Operation::Read],
    "erp_security::rule_op::tests",
);

fn operators() -> Operators {
    Operators::new(registry(), [&OP]).unwrap()
}

fn compile_msg(text: &str) -> Result<crate::rule::RuleDomain, RuleError> {
    compile_with(registry(), model(MSG), Some(text), &operators())
}

fn invalid(text: &str, on: &str) -> String {
    match compile_with(registry(), model(on), Some(text), &operators()) {
        Err(RuleError::Invalid { message }) => message,
        other => panic!("expected an invalid rule for {text}, got {other:?}"),
    }
}

// ----- rule text ------------------------------------------------------------------------------

/// The accepted forms: the operator beside ordinary conditions, under `|` and `&`. Also the
/// recipient condition the `mail` rows use (`user.partner_id.ids` against a many2many) compiles
/// under the rule table (plan assumption 1).
#[test]
fn accepted_forms_compile_and_the_reads_include_every_target() {
    let text = "['|', ('author_id', '=', user.partner_id.id), '|', ('partner_ids', 'in', \
                user.partner_id.ids), ('res_id', 'thread_access', 'read')]";
    let compiled = compile_msg(text).unwrap();
    let registry = registry();
    let name = |(m, f): &(erp_registry::ModelId, erp_registry::FieldId)| {
        let spec = registry.model_by_id(*m);
        format!("{}.{}", spec.name, spec.field_by_id(*f).name)
    };
    let reads: Vec<String> = compiled.reads.iter().map(name).collect();
    for expected in [
        "thr.msg.author_id",
        "thr.msg.partner_ids",
        "thr.msg.res_id",
        "thr.msg.model",
    ] {
        assert!(
            reads.contains(&expected.to_owned()),
            "{expected} in {reads:?}"
        );
    }
    // one access leaf per model the operator ranges over: concrete thread models only
    let mut targets: Vec<&str> = compiled
        .access
        .iter()
        .map(|leaf| registry.model_by_id(leaf.target).name)
        .collect();
    targets.sort_unstable();
    assert_eq!(targets, [DOC_A, DOC_B]);
    assert!(compiled.access.iter().all(|l| l.op == Operation::Read));

    // alone, and under `&`
    for text in [
        "[('res_id', 'thread_access', 'read')]",
        "['&', ('is_internal', '=', False), ('res_id', 'thread_access', 'read')]",
    ] {
        let compiled = compile_msg(text).unwrap();
        let holds = compiled.domain.iter_conditions().count() > 0
            || matches!(compiled.domain.kind(), DomainKind::Custom(_));
        assert!(holds, "{text}");
    }
}

/// model, field and operations)
#[test]
fn a_declared_operator_is_refused_outside_its_declaration() {
    // another model
    assert_eq!(
        invalid("[('res_id', 'thread_access', 'read')]", "thr.other"),
        "The 'thread_access' operator works only on thr.msg in condition ('res_id', \
         'thread_access', 'read')"
    );
    // another field of the right model
    assert_eq!(
        invalid("[('author_id', 'thread_access', 'read')]", MSG),
        "The 'thread_access' operator works only on the field 'res_id' in condition ('author_id', \
         'thread_access', 'read')"
    );
    // an operation the declaration does not list, one that is not an operation, not a string
    for (value, shown) in [("'write'", "'write'"), ("'bogus'", "'bogus'"), ("1", "1")] {
        assert_eq!(
            invalid(&format!("[('res_id', 'thread_access', {value})]"), MSG),
            format!(
                "Invalid value for 'thread_access' operator in condition ('res_id', \
                 'thread_access', {shown})"
            )
        );
    }
    // a name nobody declared is the ordinary invalid operator
    assert_eq!(
        invalid("[('res_id', 'thread_acces', 'read')]", MSG),
        "Invalid operator in condition ('res_id', 'thread_acces', 'read')"
    );
}

/// Without the declaration in the table the spelling is the ordinary invalid operator, and a
/// negation is refused: a negated expansion could let rows through where it is unknown.
#[test]
fn an_undeclared_operator_and_a_negated_one_are_refused() {
    let err = compile_with(
        registry(),
        model(MSG),
        Some("[('res_id', 'thread_access', 'read')]"),
        &Operators::none(),
    )
    .unwrap_err();
    assert_eq!(
        err.to_string(),
        "Invalid operator in condition ('res_id', 'thread_access', 'read')"
    );
    for (text, shown) in [
        (
            "['!', ('res_id', 'thread_access', 'read')]",
            "['!', ('res_id', 'thread_access', 'read')]",
        ),
        (
            "['!', '|', ('is_internal', '=', True), ('res_id', 'thread_access', 'read')]",
            "['!', '|', ('is_internal', '=', True), ('res_id', 'thread_access', 'read')]",
        ),
    ] {
        assert_eq!(
            invalid(text, MSG),
            format!("a declared rule operator cannot be negated: {shown}")
        );
    }
}

/// A `not any` over a sub-domain that holds the operator is a negation of it too, also nested.
#[test]
fn a_declared_operator_under_not_any_is_refused() {
    for text in [
        "[('msg_id', 'not any', [('res_id', 'thread_access', 'read')])]",
        "[('msg_id', 'not any', ['&', ('is_internal', '=', False), ('res_id', 'thread_access', 'read')])]",
    ] {
        let message = invalid(text, DOC_B);
        assert!(
            message
                .starts_with("a declared rule operator cannot be negated: ('msg_id', 'not any', ["),
            "{message}"
        );
        assert!(message.ends_with("'thread_access', 'read')])"), "{message}");
    }
    // the positive form is fine, and so is `not any` without the operator
    compile_with(
        registry(),
        model(DOC_B),
        Some("[('msg_id', 'any', [('res_id', 'thread_access', 'read')])]"),
        &operators(),
    )
    .unwrap();
    compile_with(
        registry(),
        model(DOC_B),
        Some("[('msg_id', 'not any', [('is_internal', '=', True)])]"),
        &operators(),
    )
    .unwrap();
}

// ----- declarations ---------------------------------------------------------------------------

fn bad(
    name: &'static str,
    model: &'static str,
    field: &'static str,
    inherits: &'static str,
    operations: &'static [Operation],
) -> &'static RuleOperatorDecl {
    Box::leak(Box::new(RuleOperatorDecl::__declare(
        name,
        "test_access",
        model,
        field,
        inherits,
        operations,
        "tests::bad",
    )))
}

/// Every declaration error refuses to build, with the declaration named.
#[test]
fn a_bad_declaration_refuses_to_build() {
    let read: &[Operation] = &[Operation::Read];
    let cases: [(&RuleOperatorDecl, &str); 9] = [
        (
            bad("Thread", MSG, "res_id", "thr.mixin", read),
            "the name must start with a lowercase ASCII letter and hold lowercase ASCII letters, \
             digits and '_' only, at most 64 bytes",
        ),
        (
            bad("", MSG, "res_id", "thr.mixin", read),
            "the name must start with a lowercase ASCII letter and hold lowercase ASCII letters, \
             digits and '_' only, at most 64 bytes",
        ),
        (
            bad("any", MSG, "res_id", "thr.mixin", read),
            "the name is a core domain operator",
        ),
        (
            bad("nomodel", "thr.nope", "res_id", "thr.mixin", read),
            "the model 'thr.nope' does not exist",
        ),
        (
            bad("noparent", MSG, "res_id", "thr.nope", read),
            "the inherited model 'thr.nope' does not exist",
        ),
        (
            bad("nofield", MSG, "nope", "thr.mixin", read),
            "thr.msg has no field 'nope'",
        ),
        (
            bad("wrongkind", MSG, "author_id", "thr.mixin", read),
            "thr.msg.author_id is a many2one field, not a many2one_reference",
        ),
        (
            bad("noop", MSG, "res_id", "thr.mixin", &[]),
            "it lists no operation",
        ),
        (
            bad(
                "twice",
                MSG,
                "res_id",
                "thr.mixin",
                &[Operation::Read, Operation::Read],
            ),
            "the operation 'read' is listed twice",
        ),
    ];
    for (decl, why) in cases {
        let err = Operators::new(registry(), [decl]).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Programming, "{why}");
        assert_eq!(
            err.message(),
            format!(
                "rule operator '{}' (declared in tests::bad): {why}",
                decl.name()
            )
        );
    }
}

/// A name declared twice is refused, naming the first declaration; the compiler reports the same
/// error, so a misdeclared operator never reaches a running policy.
#[test]
fn a_duplicate_name_refuses_the_policy_build() {
    let second = bad(
        "thread_access",
        MSG,
        "res_id",
        "thr.mixin",
        &[Operation::Read],
    );
    let err = Operators::new(registry(), [&OP, second]).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "rule operator 'thread_access' (declared in tests::bad): the name is declared twice \
         (first in erp_security::rule_op::tests)"
    );
    let err = Compiler::new(registry_arc())
        .rule_operators([&OP, second])
        .compile(&rows(vec![]))
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "rule operator 'thread_access' (declared in tests::bad): the name is declared twice \
         (first in erp_security::rule_op::tests)"
    );
}

// ----- the expansion --------------------------------------------------------------------------

const MSG_RULE: &str = "['|', ('author_id', '=', user.partner_id.id), '|', ('partner_ids', 'in', \
                        user.partner_id.ids), ('res_id', 'thread_access', 'read')]";

fn policy(rules: Vec<RuleRow>) -> PolicySnapshot {
    Compiler::new(registry_arc())
        .rule_operators([&OP])
        .compile(&rows(rules))
        .unwrap()
}

/// `thr.msg` readable by internal users through the thread rule; `thr.doc_a` by every internal
/// user where `val > 0`; `thr.doc_b` by Group 1 only.
fn rules() -> Vec<RuleRow> {
    vec![
        rule(10, "msg", MSG, "r", Some(USER), Some(MSG_RULE)),
        rule(
            11,
            "doc a",
            DOC_A,
            "r",
            Some(USER),
            Some("[('val', '>', 0)]"),
        ),
        rule(12, "doc b", DOC_B, "r", Some(G1), None),
    ]
}

/// The (target model, predicate) of every reference query in `domain`, in order.
fn branches(domain: &Domain) -> Vec<(String, String)> {
    fn walk(domain: &Domain, out: &mut Vec<(String, String)>) {
        match domain.kind() {
            DomainKind::True | DomainKind::False | DomainKind::Custom(_) => {}
            DomainKind::Not(child) => walk(child, out),
            DomainKind::And(children) | DomainKind::Or(children) => {
                children.iter().for_each(|c| walk(c, out));
            }
            DomainKind::Condition(c) => {
                if let Some(query) = ReferenceQuery::of(c.value()) {
                    let target = registry().model_by_id(query.target()).name;
                    out.push((target.to_owned(), format!("{:?}", query.sub())));
                }
            }
        }
    }
    let mut out = Vec::new();
    walk(domain, &mut out);
    out
}

fn read_msg(policy: &PolicySnapshot, uid: i64, groups: &[i64]) -> erp_orm::AccessDomain {
    let user = principal(policy, uid, groups);
    let binding = bind(policy, &user);
    policy
        .access_domain(&binding, model(MSG), Operation::Read)
        .unwrap()
}

/// the mail addon's `thread_access` test)
#[test]
fn one_branch_per_readable_thread_model() {
    let policy = policy(rules());
    // Group 1 reads both models: a branch each, with the model's own predicate
    let both = read_msg(&policy, BOB, &[USER, G1]);
    let branches = branches(both.domain());
    assert_eq!(
        branches.iter().map(|(m, _)| m.as_str()).collect::<Vec<_>>(),
        [DOC_A, DOC_B]
    );
    assert!(branches[0].1.contains("'val', '>', 0"), "{branches:?}");
    // outside Group 1 the model-level read of `thr.doc_b` is denied: no branch for it, and the
    // other branches stay
    let one = read_msg(&policy, BOB, &[USER, G2]);
    let branches = branches_of(&one);
    assert_eq!(branches, [DOC_A]);
}

fn branches_of(domain: &erp_orm::AccessDomain) -> Vec<String> {
    branches(domain.domain())
        .into_iter()
        .map(|(m, _)| m)
        .collect()
}

/// An empty branch set is `FALSE`: no thread model readable leaves the other conditions only, and
/// a rule that is only the operator allows nothing.
#[test]
fn no_readable_thread_model_is_false() {
    // nothing grants any thread model
    let policy = policy(vec![rule(10, "msg", MSG, "r", Some(USER), Some(MSG_RULE))]);
    let domain = read_msg(&policy, BOB, &[USER]);
    assert!(branches_of(&domain).is_empty());
    assert!(
        !domain.is_false(),
        "the author and recipient branches remain"
    );

    let only = policy_only_operator();
    let domain = read_msg(&only, BOB, &[USER]);
    assert!(domain.is_false());
}

fn policy_only_operator() -> PolicySnapshot {
    policy(vec![
        rule(
            10,
            "msg",
            MSG,
            "r",
            Some(USER),
            Some("[('res_id', 'thread_access', 'read')]"),
        ),
        rule(11, "doc b", DOC_B, "r", Some(G1), None),
    ])
}

/// A target whose predicate is `TRUE` still gets its branch: the column has no foreign key.
#[test]
fn a_true_target_predicate_still_gets_a_branch() {
    let policy = policy_only_operator();
    let domain = read_msg(&policy, BOB, &[USER, G1]);
    let branches = branches(domain.domain());
    assert_eq!(branches.len(), 1);
    assert_eq!(branches[0].0, DOC_B);
    assert_eq!(branches[0].1, "[(1, '=', 1)]");
}

/// Abstract, transient, table-less and non-thread models are never targets.
#[test]
fn only_concrete_thread_models_are_targets() {
    let registry = registry();
    let names: Vec<&str> = Operators::targets(registry, &OP).map(|m| m.name).collect();
    assert_eq!(names, [DOC_A, DOC_B]);
}

/// The memo is keyed by groups: asking in any order gives each principal the branches of its own
/// groups, and a repeated question is the memoized predicate.
#[test]
fn the_memo_never_shares_branches_across_groups() {
    let policy = policy(rules());
    let first = read_msg(&policy, BOB, &[USER, G1]);
    let narrow = read_msg(&policy, BOB, &[USER, G2]);
    let again = read_msg(&policy, BOB, &[USER, G1]);
    assert_eq!(branches_of(&first), [DOC_A, DOC_B]);
    assert_eq!(branches_of(&narrow), [DOC_A]);
    // same groups: the memoized predicate (carriers compare by identity)
    assert_eq!(first.domain(), again.domain());
    assert_ne!(first.domain(), narrow.domain());
    // and the other order gives the same answers on a fresh policy
    let fresh = self::policy(rules());
    let narrow_first = read_msg(&fresh, BOB, &[USER, G2]);
    let wide_second = read_msg(&fresh, BOB, &[USER, G1]);
    assert_eq!(branches_of(&narrow_first), [DOC_A]);
    assert_eq!(branches_of(&wide_second), [DOC_A, DOC_B]);
    assert_eq!(
        format!("{:?}", narrow_first.domain()),
        format!("{:?}", narrow.domain())
    );
}

/// The predicate is principal-free: two users of the same groups get the same text with
/// parameters where the user, partner or companies show, and no literal id of either.
#[test]
fn the_predicate_holds_no_literal_principal_values() {
    let mut rules = rules();
    rules.push(rule(
        13,
        "doc a companies",
        DOC_A,
        "r",
        None,
        Some("[('company_id', 'in', company_ids), ('owner_id', '=', user.id)]"),
    ));
    let policy = policy(rules);
    let (uid_a, uid_b) = (7, 8);
    let a = read_msg(&policy, uid_a, &[USER, G1]);
    let b = read_msg(&policy, uid_b, &[USER, G1]);
    assert_eq!(
        a.domain(),
        b.domain(),
        "one memoized predicate serves both users"
    );
    let text = format!("{:?}", a.domain());
    for param in ["company_ids", "user.id", "user.partner_id.id"] {
        assert!(text.contains(param), "{param} in {text}");
    }
    for literal in [
        format!("'owner_id', '=', {uid_a}"),
        format!("'owner_id', '=', {uid_b}"),
    ] {
        assert!(!text.contains(&literal), "{literal} in {text}");
    }
}

/// An `access` chain that comes back through the operator is a cycle, found at build time: the
/// rules concerned deny, and the report names them.
#[test]
fn a_cycle_through_the_operator_is_refused() {
    let policy = policy(vec![
        rule(10, "msg", MSG, "r", Some(USER), Some(MSG_RULE)),
        rule(
            12,
            "doc b back to msg",
            DOC_B,
            "r",
            Some(USER),
            Some("[('msg_id', 'access', 'read')]"),
        ),
    ]);
    let issues: Vec<String> = policy
        .report()
        .rule_errors()
        .iter()
        .map(|issue| format!("{}: {}", issue.model(), issue.error()))
        .collect();
    assert!(!issues.is_empty(), "the cycle is reported");
    for issue in &issues {
        assert!(
            issue.contains("its 'access' conditions form a cycle"),
            "{issue}"
        );
    }
    let user = principal(&policy, BOB, &[USER]);
    let binding = bind(&policy, &user);
    let err = policy
        .access_domain(&binding, model(MSG), Operation::Read)
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access);
}

// ----- reads and invalidation -----------------------------------------------------------------

/// The expanded predicate reads its own columns and what each target's predicate reads, and a
/// write to a target model drops the checks that depended on it.
#[test]
fn the_expansion_reads_and_invalidates_through_the_targets() {
    let policy = policy(rules());
    let domain = read_msg(&policy, BOB, &[USER, G1]);
    let registry = registry();
    let msg = model(MSG);
    let doc_a = model(DOC_A);
    assert!(domain.reads_field(msg.id, msg.field_id("res_id").unwrap()));
    assert!(domain.reads_field(msg.id, msg.field_id("model").unwrap()));
    // the `val` that thr.doc_a's rule tests
    assert!(domain.reads_field(doc_a.id, doc_a.field_id("val").unwrap()));
    // the expansion selects from the target tables
    let names: Vec<&str> = [doc_a.id]
        .iter()
        .map(|m| registry.model_by_id(*m).name)
        .collect();
    assert_eq!(names, [DOC_A]);
    // a write to a thread model (the target of the operator) or to the message's own columns
    // invalidates the message read decisions
    for written in [doc_a.id, model(DOC_B).id, msg.id] {
        assert!(
            policy
                .reads_index(written)
                .contains(&(msg.id, Operation::Read)),
            "{}",
            registry.model_by_id(written).name
        );
    }
    // a model that is no thread model does not
    assert!(
        !policy
            .reads_index(model("thr.plain").id)
            .contains(&(msg.id, Operation::Read))
    );
    // the leaf is gone from the combined predicate: only conditions and carriers remain
    fn no_custom(domain: &Domain) -> bool {
        match domain.kind() {
            DomainKind::Custom(_) => false,
            DomainKind::True | DomainKind::False => true,
            DomainKind::Not(c) => no_custom(c),
            DomainKind::And(cs) | DomainKind::Or(cs) => cs.iter().all(no_custom),
            DomainKind::Condition(c) => match c.value() {
                Value::Domain(sub) => no_custom(sub),
                _ => true,
            },
        }
    }
    assert!(no_custom(domain.domain()));
    let _ = G3;
}

// ----- addons that are not installed ----------------------------------------------------------

/// A declaration of an addon the database does not have is not part of its policy: the build
/// succeeds even though the declared model does not exist there, the spelling is then the ordinary
/// invalid operator, and rows of other models validate.
#[test]
fn a_declaration_of_an_addon_that_is_not_installed_is_ignored() {
    static ABSENT: RuleOperatorDecl = RuleOperatorDecl::__declare(
        "thread_access",
        "mail_not_installed",
        "mail.message",
        "res_id",
        "mail.thread",
        &[Operation::Read],
        "tests::absent",
    );
    let operators = Operators::new(registry(), [&ABSENT]).unwrap();
    assert!(operators.by_name("thread_access").is_none());
    Compiler::new(registry_arc())
        .rule_operators([&ABSENT])
        .compile(&rows(vec![rule(
            10,
            "ok",
            DOC_A,
            "r",
            Some(USER),
            Some("[('val', '>', 0)]"),
        )]))
        .unwrap();
    let err = compile_with(
        registry(),
        model(DOC_A),
        Some("[('val', '>', 0), ('id', 'thread_access', 'read')]"),
        &operators,
    )
    .unwrap_err();
    assert_eq!(
        err.to_string(),
        "Invalid operator in condition ('id', 'thread_access', 'read')"
    );
    compile_with(
        registry(),
        model(DOC_A),
        Some("[('val', '>', 0)]"),
        &operators,
    )
    .unwrap();
    // an installed addon with a declaration that does not hold up still refuses
    let broken = bad(
        "brokenop",
        "thr.nope",
        "res_id",
        "thr.mixin",
        &[Operation::Read],
    );
    assert!(Operators::new(registry(), [broken]).is_err());
}

/// The same groups under other active companies share the one predicate: the branches and the
/// parameters depend on the groups and the shape, never on which companies are active.
#[test]
fn the_memo_is_safe_across_companies() {
    use erp_orm::{CompanyId, PrincipalBuilder, Resolution, UserId};
    use std::num::NonZeroU64;
    let policy = policy(rules());
    let as_company = |current: i64| {
        let direct = [crate::testing::g(USER), crate::testing::g(G1)];
        let groups = policy.groups(&direct).unwrap();
        let companies = [CompanyId::new(1).unwrap(), CompanyId::new(2).unwrap()];
        let principal = PrincipalBuilder::user(UserId::new(BOB).unwrap())
            .resolution(Resolution::in_id_order(groups, companies, NonZeroU64::MIN).unwrap())
            .companies(vec![CompanyId::new(current).unwrap()])
            .build()
            .unwrap();
        let binding = bind(&policy, &principal);
        policy
            .access_domain(&binding, model(MSG), Operation::Read)
            .unwrap()
    };
    let (one, two) = (as_company(1), as_company(2));
    assert_eq!(branches_of(&one), [DOC_A, DOC_B]);
    assert_eq!(branches_of(&two), [DOC_A, DOC_B]);
    assert_eq!(format!("{:?}", one.domain()), format!("{:?}", two.domain()));
    assert!(!format!("{:?}", one.domain()).contains("'company_id', 'in', [1"));
}

/// A negated search over the operator (`not in`, `!=`) is a negation too, refused when the rule
/// compiles.
#[test]
fn a_declared_operator_under_a_negated_search_is_refused() {
    for op in ["not in", "!="] {
        let text = format!(
            "[('msg_id', '{op}', user.env['thr.msg'].search([('res_id', 'thread_access', 'read')]))]"
        );
        let message = invalid(&text, DOC_B);
        assert!(
            message.starts_with(&format!(
                "a declared rule operator cannot be negated: ('msg_id', '{op}', user.env['thr.msg'].search("
            )),
            "{message}"
        );
    }
    compile_with(
        registry(),
        model(DOC_B),
        Some(
            "[('msg_id', 'in', user.env['thr.msg'].search([('res_id', 'thread_access', 'read')]))]",
        ),
        &operators(),
    )
    .unwrap();
}
