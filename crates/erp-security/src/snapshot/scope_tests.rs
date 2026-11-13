// SPDX-License-Identifier: MIT OR Apache-2.0
//! Decisions for a scoped binding and the lift checks of
//! [`AccessPolicy::check_scope`], on the compiled policy with the in-memory oracle rows. The
//! bindings carry a scope without a block (`with_scope_for_tests`): the block lifetime is the
//! engine's (`crates/erp-test-models/tests/principal_block.rs`), what it grants through a real
//! block is `crates/erp-test-models/tests/principal_policy.rs` and property P8.

use std::sync::Arc;

use erp_domain::Operation;
use erp_fields::ErrorKind;
use erp_orm::{
    AccessPolicy, ElevationScope, IntoPolicyProvider, OperationSet, Principal, PrincipalBinding,
    RuleKind, ScopeItem, StaticPolicy,
};
use erp_registry::{
    AddonDescriptor, CompanyAccessDecl, CompanyBranch, EmptyCompany, FieldDecl, ModelDecl,
    RegistryBuilder,
};

use super::*;
use crate::rows::RuleRow;
use crate::testing::{
    ALL, BOB, CATEG, G1, G2, SOME_OBJ, USER, bind, model, objs, principal, rows, rule, xid,
};

/// A rule of [`rule`] with the external id `t.<name>`.
fn named(
    id: i64,
    name: &str,
    model: &str,
    ops: &str,
    group: Option<i64>,
    domain: Option<&str>,
) -> RuleRow {
    rule(id, name, model, ops, group, domain).with_xmlid(xid(&format!("t.{name}")))
}

/// Permission of Group 1 to read Mario, Luigi and Peach; the ordinary restriction `val > 1`
/// (every operation); a write restriction naming the company through a relational path, a write
/// restriction with an `access` condition, a create restriction that does not compile, an
/// unlink-only restriction, a permission and a restriction of the category model.
fn policy() -> PolicySnapshot {
    let compiled = Compiler::new(crate::testing::registry_arc())
        .compile(&rows(vec![
            named(
                10,
                "perm",
                SOME_OBJ,
                "r",
                Some(G1),
                Some("[('id', 'in', [1, 2, 3])]"),
            ),
            named(
                11,
                "above_one",
                SOME_OBJ,
                "crud",
                None,
                Some("[('val', '>', 1)]"),
            ),
            named(
                12,
                "company",
                SOME_OBJ,
                "u",
                None,
                Some("[('parent_id.company_id', 'in', company_ids)]"),
            ),
            named(
                13,
                "categ_access",
                SOME_OBJ,
                "u",
                None,
                Some("[('categ_id', 'access', 'read')]"),
            ),
            named(
                14,
                "broken",
                SOME_OBJ,
                "c",
                None,
                Some("[('no_such_field', '=', 1)]"),
            ),
            named(
                15,
                "unlink_only",
                SOME_OBJ,
                "d",
                None,
                Some("[('val', '<', 100)]"),
            ),
            named(
                16,
                "categ_name",
                CATEG,
                "r",
                None,
                Some("[('name', '!=', 'x')]"),
            ),
            named(17, "categ_perm", CATEG, "r", Some(USER), None),
        ]))
        .unwrap();
    assert_eq!(
        compiled.report().rule_errors().len(),
        1,
        "only the broken rule"
    );
    compiled
}

fn bob(policy: &PolicySnapshot) -> Principal {
    principal(policy, BOB, &[USER, G1, G2])
}

fn item(model_name: &str, ops: &[Operation], lift: &[&str]) -> ScopeItem {
    ScopeItem::new(model(model_name).id, OperationSet::of(ops.iter().copied()))
        .unwrap()
        .lift(lift.iter().map(|x| xid(x)))
        .unwrap()
}

fn scope(items: impl IntoIterator<Item = ScopeItem>) -> ElevationScope {
    ElevationScope::new(items).unwrap()
}

fn scoped<'p>(
    policy: &PolicySnapshot,
    p: &'p Principal,
    s: ElevationScope,
) -> PrincipalBinding<'p> {
    bind(policy, p).with_scope_for_tests(Arc::new(s))
}

/// The objects the binding may `op`, by the in-memory oracle over its compiled predicate.
fn allowed(policy: &PolicySnapshot, b: &PrincipalBinding<'_>, op: Operation) -> Vec<i64> {
    let domain = policy.access_domain(b, model(SOME_OBJ), op).unwrap();
    objs().filter(SOME_OBJ, &ALL, domain.domain(), BOB)
}

#[track_caller]
fn assert_programming(error: &erp_fields::Error, message: &str) {
    assert_eq!(error.kind(), ErrorKind::Programming, "{error:?}");
    assert_eq!(error.message(), message);
}

/// ordinary restriction of the item's model that applies to a named operation is accepted; a
/// permission, a rule of another model, an unknown id, a rule that does not compile, one that
/// names the company in a nested relational path, one with an `access` condition and one of no
/// named operation are refused with the rule and the reason. The default of a policy without
/// restriction rules refuses every lift.
#[test]
fn lift_checks_accept_ordinary_restrictions_only() {
    let policy = policy();
    let read = [Operation::Read];
    policy
        .check_scope(&scope([item(SOME_OBJ, &read, &["t.above_one"])]))
        .unwrap();
    policy
        .check_scope(&scope([item(SOME_OBJ, &read, &[])]))
        .unwrap();
    let obj = SOME_OBJ;
    let cases: [(ScopeItem, String); 7] = [
        (
            item(SOME_OBJ, &read, &["t.perm"]),
            format!(
                "cannot lift restriction rule 't.perm' (perm) on {obj}: it is a permission of a \
                 group, not a restriction (a scope already grants the permission of the \
                 operations it names)"
            ),
        ),
        (
            item(SOME_OBJ, &read, &["t.categ_name"]),
            format!(
                "cannot lift restriction rule 't.categ_name' (categ_name) on {obj}: it is a rule \
                 of {CATEG}, not of the item's model"
            ),
        ),
        (
            item(SOME_OBJ, &read, &["t.nothing"]),
            format!(
                "cannot lift restriction rule 't.nothing' on {obj}: no access rule of this \
                 policy has that external id"
            ),
        ),
        (
            item(SOME_OBJ, &read, &["t.broken"]),
            format!(
                "cannot lift restriction rule 't.broken' (broken) on {obj}: the rule does not \
                 compile, and lifting it would turn its denial into an allow"
            ),
        ),
        (
            item(SOME_OBJ, &read, &["t.company"]),
            format!(
                "cannot lift restriction rule 't.company' (company) on {obj}: it bears the \
                 company (its domain names the company), and a scope never lifts a company \
                 restriction"
            ),
        ),
        (
            item(SOME_OBJ, &[Operation::Write], &["t.categ_access"]),
            format!(
                "cannot lift restriction rule 't.categ_access' (categ_access) on {obj}: its \
                 domain holds an 'access' condition or a declared rule operator, which brings in \
                 another model's whole predicate, company restriction included"
            ),
        ),
        (
            item(
                SOME_OBJ,
                &[Operation::Read, Operation::Write],
                &["t.unlink_only"],
            ),
            format!(
                "cannot lift restriction rule 't.unlink_only' (unlink_only) on {obj}: it applies \
                 to none of the operations the item names (read, write), so lifting it changes \
                 nothing"
            ),
        ),
    ];
    for (item, message) in cases {
        let error = policy.check_scope(&scope([item])).unwrap_err();
        assert_programming(&error, &message);
    }
    // a policy without restriction rules (the trait's default) refuses any lift, also through
    // the `Arc` the transaction holds
    let allow = StaticPolicy::AllowAll
        .into_policy_provider()
        .unwrap()
        .current();
    allow
        .check_scope(&scope([item(SOME_OBJ, &read, &[])]))
        .unwrap();
    assert_programming(
        &allow
            .check_scope(&scope([item(SOME_OBJ, &read, &["t.above_one"])]))
            .unwrap_err(),
        "cannot lift restriction rule 't.above_one': this access policy has no restriction rules \
         to lift",
    );
}

/// not name is decided exactly as the base binding: the same predicate, rules, grants, model
/// refusal and evidence columns; the named pair drops the permission filter and keeps the
/// restrictions.
#[test]
fn unnamed_pairs_decide_as_the_base_binding() {
    let policy = policy();
    let bob = bob(&policy);
    let base = bind(&policy, &bob);
    // the scope names Read on the category only
    let s = scoped(&policy, &bob, scope([item(CATEG, &[Operation::Read], &[])]));
    for op in Operation::ALL {
        let spec = model(SOME_OBJ);
        let want = policy.access_domain(&base, spec, op);
        let got = policy.access_domain(&s, spec, op);
        match (want, got) {
            (Ok(want), Ok(got)) => {
                assert_eq!(got.domain(), want.domain(), "{op}");
                assert_eq!(got.rules(), want.rules(), "{op}");
                assert_eq!(got.grants(), want.grants(), "{op}");
            }
            (Err(want), Err(got)) => {
                assert_eq!(
                    (got.kind(), got.message()),
                    (want.kind(), want.message()),
                    "{op}"
                );
            }
            (want, got) => panic!("{op}: base {want:?}, scoped {got:?}"),
        }
        let want = policy
            .check_model(&base, spec, op)
            .map_err(|e| (e.kind(), e.message().to_owned()));
        let got = policy
            .check_model(&s, spec, op)
            .map_err(|e| (e.kind(), e.message().to_owned()));
        assert_eq!(got, want, "{op}");
        if let (Ok(want), Ok(got)) = (
            policy.evidence_columns(&base, spec, op),
            policy.evidence_columns(&s, spec, op),
        ) {
            assert_eq!(got, want, "{op}");
        }
    }
    // the write is refused for the base (no permission of Bob's groups) and for the scope
    let refusal = policy
        .check_model(&s, model(SOME_OBJ), Operation::Write)
        .unwrap_err();
    assert_eq!(refusal.kind(), ErrorKind::Access);
    // the category read the scope names keeps its restriction, and lists no permission rule
    let categ = policy
        .access_domain(&s, model(CATEG), Operation::Read)
        .unwrap();
    let names: Vec<&str> = categ.rules().iter().map(|r| r.name()).collect();
    assert_eq!(names, ["categ_name"]);
}

/// every restriction and grants the permission: Bob reads the objects `val > 1` allows, not only
/// Group 1's three; a valid lift removes the lifted rule for the named operations only; the
/// evidence columns of the named pair hold no permission and no lifted rule.
#[test]
fn named_pairs_grant_permission_and_keep_restrictions() {
    let policy = policy();
    let bob = bob(&policy);
    let base = bind(&policy, &bob);
    let read = [Operation::Read];
    assert_eq!(allowed(&policy, &base, Operation::Read), [2, 3]);
    let s = scoped(&policy, &bob, scope([item(SOME_OBJ, &read, &[])]));
    // `val > 1` holds, Group 1's permission no longer filters
    assert_eq!(allowed(&policy, &s, Operation::Read), [2, 3, 4, 5, 6]);
    let columns = policy
        .evidence_columns(&s, model(SOME_OBJ), Operation::Read)
        .unwrap();
    assert!(columns.iter().all(|c| c.kind() == RuleKind::Restriction));
    let lifted = scoped(
        &policy,
        &bob,
        scope([item(SOME_OBJ, &read, &["t.above_one"])]),
    );
    assert_eq!(allowed(&policy, &lifted, Operation::Read), ALL);
    let columns = policy
        .evidence_columns(&lifted, model(SOME_OBJ), Operation::Read)
        .unwrap();
    assert!(columns.iter().all(|c| c.rule().name() != "above_one"));
    // the lift is for the read: the unnamed write is the base decision (no permission)
    assert_eq!(
        policy
            .check_model(&lifted, model(SOME_OBJ), Operation::Write)
            .unwrap_err()
            .message(),
        policy
            .check_model(&base, model(SOME_OBJ), Operation::Write)
            .unwrap_err()
            .message()
    );
}

/// lift one snapshot accepts is refused, not skipped, by a snapshot in which the same external
/// id names a company restriction (the transaction's policy rebuilt after the block opened).
#[test]
fn a_lift_the_deciding_snapshot_refuses_is_an_error() {
    let first = Compiler::new(crate::testing::registry_arc())
        .compile(&rows(vec![
            named(10, "perm", SOME_OBJ, "r", Some(G1), None),
            named(
                11,
                "swapped",
                SOME_OBJ,
                "r",
                None,
                Some("[('val', '>', 1)]"),
            ),
        ]))
        .unwrap();
    let second = Compiler::new(crate::testing::registry_arc())
        .compile(&rows(vec![
            named(10, "perm", SOME_OBJ, "r", Some(G1), None),
            named(
                11,
                "swapped",
                SOME_OBJ,
                "r",
                None,
                Some("[('company_id', 'in', company_ids)]"),
            ),
        ]))
        .unwrap();
    let s = scope([item(SOME_OBJ, &[Operation::Read], &["t.swapped"])]);
    first.check_scope(&s).unwrap();
    let bob = bob(&second);
    let b = scoped(&second, &bob, s);
    let error = second
        .access_domain(&b, model(SOME_OBJ), Operation::Read)
        .unwrap_err();
    assert_programming(
        &error,
        &format!(
            "cannot lift restriction rule 't.swapped' (swapped) on {SOME_OBJ}: it bears the \
             company (its domain names the company), and a scope never lifts a company \
             restriction"
        ),
    );
}

// ----- company-bearing rules (and A6) ----------------------------------------------

static SCOPE_ADDON: AddonDescriptor = AddonDescriptor {
    name: "test_scope",
    depends: &["base"],
    exact_only: false,
};

const SCOPE_DOC: &str = "test.scope.doc";
const SCOPE_OWNER: &str = "test.scope.owner";
const SCOPE_JOB: &str = "test.scope.job";
const COMPANY: &str = "res.company";
const SCOPE_LEDGER: &str = "test.scope.ledger";
const SCOPE_BOOK: &str = "test.scope.book";
const SCOPE_LINE: &str = "test.scope.line";

/// Company relations under other names than `company_id(s)`: a document with a direct one, a
/// nested one through its owner, reference fields, a user with a home company, and a model shaped
/// like `ir.job` (`run_as_company`, `addons/base/src/ir_job.rs`). The registry knows a company
/// field by its name only, so these models declare no `company_access` (it refuses one). Two
/// models with a `company_id`: a ledger without a generated company restriction (`unrestricted`)
/// and a book with one (`direct`), and a line of the book without a company field of its own.
static SCOPE_DECLS: [ModelDecl; 9] = [
    ModelDecl::new("test_scope", COMPANY)
        .description("Companies")
        .parent_name("parent_id")
        .fields(
            const {
                &[
                    FieldDecl::char("name"),
                    FieldDecl::many2one("parent_id", COMPANY),
                    FieldDecl::many2one("owner_id", SCOPE_OWNER),
                ]
            },
        ),
    ModelDecl::extension("test_scope", "res.users", &["res.users"])
        .company_access(CompanyAccessDecl::unrestricted(
            "scope fixture tests handwritten rules",
        ))
        .fields(
            const {
                &[
                    FieldDecl::many2one("company_id", COMPANY),
                    FieldDecl::many2many("company_ids", COMPANY),
                    FieldDecl::many2one("x_home_company", COMPANY),
                ]
            },
        ),
    ModelDecl::new("test_scope", SCOPE_OWNER)
        .description("Owner")
        .fields(
            const {
                &[
                    FieldDecl::char("name"),
                    FieldDecl::many2one("home_company", COMPANY),
                ]
            },
        ),
    ModelDecl::new("test_scope", SCOPE_DOC)
        .description("Scope Doc")
        .fields(
            const {
                &[
                    FieldDecl::char("name"),
                    FieldDecl::integer("val"),
                    FieldDecl::many2one("owner_company", COMPANY),
                    FieldDecl::many2one("owner_id", SCOPE_OWNER),
                    FieldDecl::reference("ref", &[(SCOPE_OWNER, "Owner")]),
                    FieldDecl::char("res_model"),
                    FieldDecl::many2one_reference("res_id", "res_model"),
                ]
            },
        ),
    ModelDecl::new("test_scope", SCOPE_JOB)
        .description("Scope Job")
        .fields(
            const {
                &[
                    FieldDecl::char("name"),
                    FieldDecl::many2one("run_as_company", COMPANY),
                ]
            },
        ),
    ModelDecl::new("test_scope", "ir.access")
        .description("Access")
        .fields(const { &[FieldDecl::char("name")] }),
    ModelDecl::new("test_scope", SCOPE_LEDGER)
        .description("Scope Ledger")
        .company_access(CompanyAccessDecl::unrestricted(
            "scope fixture: its company filters are handwritten rules",
        ))
        .fields(
            const {
                &[
                    FieldDecl::char("name"),
                    FieldDecl::boolean("public"),
                    FieldDecl::many2one("company_id", COMPANY),
                ]
            },
        ),
    ModelDecl::new("test_scope", SCOPE_BOOK)
        .description("Scope Book")
        .company_access(CompanyAccessDecl::direct(
            "company_id",
            CompanyBranch::Exact,
            EmptyCompany::Deny,
        ))
        .fields(
            const {
                &[
                    FieldDecl::char("name"),
                    FieldDecl::many2one("company_id", COMPANY),
                ]
            },
        ),
    ModelDecl::new("test_scope", SCOPE_LINE)
        .description("Scope Line")
        .fields(
            const {
                &[
                    FieldDecl::char("name"),
                    FieldDecl::many2one("book_id", SCOPE_BOOK),
                ]
            },
        ),
];

fn scope_registry() -> Arc<erp_registry::Registry> {
    Arc::new(
        RegistryBuilder::new([&SCOPE_ADDON])
            .linked(false)
            .with_decls(SCOPE_DECLS.iter())
            .build()
            .unwrap(),
    )
}

fn scope_item(
    policy: &PolicySnapshot,
    model_name: &str,
    ops: &[Operation],
    lift: &[&str],
) -> ScopeItem {
    let model = policy.registry.model(model_name).unwrap().id;
    ScopeItem::new(model, OperationSet::of(ops.iter().copied()))
        .unwrap()
        .lift(lift.iter().map(|x| xid(x)))
        .unwrap()
}

/// `company_id(s)` names) a restriction is refused as company-bearing when it is a rule of
/// `res.company`, reads a many2one to `res.company` under another name, reaches the company
/// through a nested relational path, an `any` sub-domain, a literal search or a `user.<path>`
/// through a company field of another name, reads a field shaped like `ir.job`'s
/// `run_as_company`, or reads a reference or many2one_reference field; restrictions whose paths
/// do not reach the company are still accepted.
#[test]
fn lifts_of_rules_reaching_the_company_under_any_name_are_refused() {
    let restriction =
        |id, name: &str, model: &str, domain: &str| named(id, name, model, "r", None, Some(domain));
    let policy = Compiler::new(scope_registry())
        .compile(&rows(vec![
            restriction(1, "co_parent", COMPANY, "[('parent_id', '=', False)]"),
            restriction(
                2,
                "owner_company",
                SCOPE_DOC,
                "[('owner_company', 'in', [1])]",
            ),
            restriction(
                3,
                "owner_home",
                SCOPE_DOC,
                "[('owner_id.home_company', '=', 1)]",
            ),
            restriction(
                4,
                "any_owner",
                SCOPE_DOC,
                "[('owner_id', 'any', [('home_company', 'in', [1])])]",
            ),
            restriction(
                5,
                "searched_owner",
                SCOPE_DOC,
                "[('owner_id', 'in', user.env['test.scope.owner'].search([('home_company', '=', \
                 1)]).ids)]",
            ),
            restriction(
                6,
                "user_home",
                SCOPE_OWNER,
                "[('id', '=', user.x_home_company.owner_id.id)]",
            ),
            restriction(
                7,
                "job_company",
                SCOPE_JOB,
                "[('run_as_company', 'in', [1])]",
            ),
            restriction(8, "ref_set", SCOPE_DOC, "[('ref', '!=', False)]"),
            restriction(9, "res_id_set", SCOPE_DOC, "[('res_id', '=', 1)]"),
            restriction(10, "plain", SCOPE_DOC, "[('val', '>', 1)]"),
            restriction(11, "owner_name", SCOPE_DOC, "[('owner_id.name', '=', 'x')]"),
        ]))
        .unwrap();
    assert!(
        policy.report().rule_errors().is_empty(),
        "{:?}",
        policy.report().rule_errors()
    );
    let read = [Operation::Read];
    // paths that never reach the company stay liftable
    policy
        .check_scope(&scope([scope_item(
            &policy,
            SCOPE_DOC,
            &read,
            &["t.plain", "t.owner_name"],
        )]))
        .unwrap();
    let bears = |rule: &str, model: &str, why: &str| {
        format!(
            "cannot lift restriction rule 't.{rule}' ({rule}) on {model}: it bears the company \
             ({why}), and a scope never lifts a company restriction"
        )
    };
    let cases = [
        (
            COMPANY,
            "co_parent",
            bears("co_parent", COMPANY, "it is a rule of res.company"),
        ),
        (
            SCOPE_DOC,
            "owner_company",
            bears(
                "owner_company",
                SCOPE_DOC,
                "it reads test.scope.doc.owner_company, a relation to res.company",
            ),
        ),
        (
            SCOPE_DOC,
            "owner_home",
            bears(
                "owner_home",
                SCOPE_DOC,
                "it reads test.scope.owner.home_company, a relation to res.company",
            ),
        ),
        (
            SCOPE_DOC,
            "any_owner",
            bears(
                "any_owner",
                SCOPE_DOC,
                "it reads test.scope.owner.home_company, a relation to res.company",
            ),
        ),
        (
            SCOPE_DOC,
            "searched_owner",
            bears(
                "searched_owner",
                SCOPE_DOC,
                "it reads test.scope.owner.home_company, a relation to res.company",
            ),
        ),
        (
            SCOPE_OWNER,
            "user_home",
            bears(
                "user_home",
                SCOPE_OWNER,
                "it reads res.users.x_home_company, a relation to res.company",
            ),
        ),
        (
            SCOPE_JOB,
            "job_company",
            bears(
                "job_company",
                SCOPE_JOB,
                "it reads test.scope.job.run_as_company, a relation to res.company",
            ),
        ),
        (
            SCOPE_DOC,
            "ref_set",
            bears(
                "ref_set",
                SCOPE_DOC,
                "it reads test.scope.doc.ref, a reference field that may point at a company",
            ),
        ),
        (
            SCOPE_DOC,
            "res_id_set",
            bears(
                "res_id_set",
                SCOPE_DOC,
                "it reads test.scope.doc.res_id, a reference field that may point at a company",
            ),
        ),
    ];
    for (model, rule, message) in cases {
        let lift = format!("t.{rule}");
        let error = policy
            .check_scope(&scope([scope_item(&policy, model, &read, &[&lift])]))
            .unwrap_err();
        assert_programming(&error, &message);
    }
}

/// A6; a fail-closed rule of this slice, accepted) naming Read or
/// Write on a `res.company`-like model whose only company filter is a permission
/// (`[('id', 'in', company_ids)]`, as `base.access_company_internal`) is `Programming` at entry
/// and in a decision of a binding that carries the scope; whichever groups the user holds. Only
/// the generated company restriction keeps the confinement: a handwritten restriction does not,
/// whether it bears the company only by the wide test (a rule of `res.company`, a reference read)
/// or names the company without confining to the user's companies (`company_id != False`, an OR
/// with a public flag) or even with confining (`owner_company in company_ids`). The same
/// permission next to a generated company restriction of the operation is admitted, and a
/// permission that does not bear the company never blocks.
#[test]
fn named_pairs_never_drop_a_company_confinement_held_only_by_permissions() {
    let registry = scope_registry();
    let generated = crate::company_access::generate(&registry).unwrap();
    let book_restriction = match generated.as_slice() {
        [g] if g.model_name() == SCOPE_BOOK => {
            rule(20, g.name(), g.model_name(), "crud", None, Some(g.text())).with_generated()
        }
        other => panic!("the fixture generates for the book only: {other:?}"),
    };
    let company_permission = |id, name: &str, model: &str, ops: &str| {
        named(
            id,
            name,
            model,
            ops,
            Some(G1),
            Some("[('company_id', 'in', company_ids)]"),
        )
    };
    let policy = Compiler::new(registry)
        .compile(&rows(vec![
            named(
                1,
                "co_allowed",
                COMPANY,
                "r",
                Some(G1),
                Some("[('id', 'in', company_ids)]"),
            ),
            named(2, "co_manage", COMPANY, "ru", Some(G2), None),
            // restrictions that bear the company only by the wide test do not keep a company
            // confinement: a rule of res.company and a reference read
            named(
                6,
                "co_named",
                COMPANY,
                "r",
                None,
                Some("[('name', '!=', 'x')]"),
            ),
            named(
                7,
                "doc_ref",
                SCOPE_DOC,
                "u",
                None,
                Some("[('ref', '!=', False)]"),
            ),
            named(
                3,
                "doc_perm",
                SCOPE_DOC,
                "ru",
                Some(G1),
                Some("[('owner_company', 'in', company_ids)]"),
            ),
            // a handwritten restriction naming the company and the user's companies is not the
            // generated one either
            named(
                4,
                "doc_company",
                SCOPE_DOC,
                "r",
                None,
                Some("[('owner_company', 'in', company_ids)]"),
            ),
            named(
                5,
                "job_perm",
                SCOPE_JOB,
                "r",
                Some(G1),
                Some("[('name', '!=', 'x')]"),
            ),
            // the ledger's company filter is a permission; its restrictions name the company
            // without confining to the user's companies (orphan rows hidden on read, public rows
            // or the user's companies on write)
            company_permission(8, "ledger_perm", SCOPE_LEDGER, "ru"),
            named(
                9,
                "ledger_set",
                SCOPE_LEDGER,
                "r",
                None,
                Some("[('company_id', '!=', False)]"),
            ),
            named(
                10,
                "ledger_public",
                SCOPE_LEDGER,
                "u",
                None,
                Some("['|', ('public', '=', True), ('company_id', 'in', company_ids)]"),
            ),
            // the book keeps its generated company restriction
            company_permission(11, "book_perm", SCOPE_BOOK, "ru"),
            book_restriction,
        ]))
        .unwrap();
    assert!(
        policy.report().rule_errors().is_empty(),
        "{:?}",
        policy.report().rule_errors()
    );
    assert!(
        policy.report().generated_issues().is_empty(),
        "{:?}",
        policy.report().generated_issues()
    );
    let refused = |op: &str, model: &str, rules: &str| {
        format!(
            "the scope names {op} on '{model}', which keeps no generated company restriction and \
             whose permission rules ({rules}) may bear the company: scoped elevation cannot open \
             it"
        )
    };
    let refusal = |model: &str, op: Operation| {
        policy
            .check_scope(&scope([scope_item(&policy, model, &[op], &[])]))
            .unwrap_err()
    };
    assert_programming(
        &refusal(COMPANY, Operation::Read),
        &refused("read", COMPANY, "t.co_allowed, t.co_manage"),
    );
    assert_programming(
        &refusal(COMPANY, Operation::Write),
        &refused("write", COMPANY, "t.co_manage"),
    );
    // neither the handwritten company restriction (read) nor the reference read (write) keeps
    // the doc's confinement
    assert_programming(
        &refusal(SCOPE_DOC, Operation::Read),
        &refused("read", SCOPE_DOC, "t.doc_perm"),
    );
    assert_programming(
        &refusal(SCOPE_DOC, Operation::Write),
        &refused("write", SCOPE_DOC, "t.doc_perm"),
    );
    // `company_id != False` and `public OR company_id in company_ids` confine nothing to the
    // user's companies
    assert_programming(
        &refusal(SCOPE_LEDGER, Operation::Read),
        &refused("read", SCOPE_LEDGER, "t.ledger_perm"),
    );
    assert_programming(
        &refusal(SCOPE_LEDGER, Operation::Write),
        &refused("write", SCOPE_LEDGER, "t.ledger_perm"),
    );
    // the generated company restriction is kept, and a permission that bears no company never
    // blocks
    let read = [Operation::Read];
    policy
        .check_scope(&scope([
            scope_item(&policy, SCOPE_JOB, &read, &[]),
            scope_item(
                &policy,
                SCOPE_BOOK,
                &[Operation::Read, Operation::Write],
                &[],
            ),
        ]))
        .unwrap();
    // the deciding snapshot refuses it too, whatever groups the binding holds (here none of the
    // permissions' groups)
    let p = principal(&policy, BOB, &[USER]);
    let b = bind(&policy, &p).with_scope_for_tests(Arc::new(scope([scope_item(
        &policy,
        COMPANY,
        &read,
        &[],
    )])));
    let company = policy.registry.model(COMPANY).unwrap();
    assert_programming(
        &policy
            .access_domain(&b, company, Operation::Read)
            .unwrap_err(),
        &refused("read", COMPANY, "t.co_allowed, t.co_manage"),
    );
}

/// holds through an `access` condition, and A6) a line without a company field of
/// its own, confined only by the permission `[('book_id', 'access', 'read')]` on its book (which
/// keeps its generated company restriction), is `Programming` at entry and in a decision of a
/// binding that carries the scope: the permission brings in the book's whole predicate, company
/// restriction included, so counting it as granted would open the line across companies.
#[test]
fn named_pairs_never_drop_a_company_confinement_held_through_an_access_condition() {
    let registry = scope_registry();
    let generated = crate::company_access::generate(&registry).unwrap();
    let book_restriction = match generated.as_slice() {
        [g] if g.model_name() == SCOPE_BOOK => {
            rule(20, g.name(), g.model_name(), "crud", None, Some(g.text())).with_generated()
        }
        other => panic!("the fixture generates for the book only: {other:?}"),
    };
    let policy = Compiler::new(registry)
        .compile(&rows(vec![
            named(
                1,
                "book_perm",
                SCOPE_BOOK,
                "r",
                Some(G1),
                Some("[('company_id', 'in', company_ids)]"),
            ),
            book_restriction,
            named(
                2,
                "line_perm",
                SCOPE_LINE,
                "r",
                Some(G1),
                Some("[('book_id', 'access', 'read')]"),
            ),
        ]))
        .unwrap();
    assert!(
        policy.report().rule_errors().is_empty(),
        "{:?}",
        policy.report().rule_errors()
    );
    let message = "the scope names read on 'test.scope.line', which keeps no generated company \
                   restriction and whose permission rules (t.line_perm) may bear the company: \
                   scoped elevation cannot open it";
    let read = [Operation::Read];
    assert_programming(
        &policy
            .check_scope(&scope([scope_item(&policy, SCOPE_LINE, &read, &[])]))
            .unwrap_err(),
        message,
    );
    // the deciding snapshot refuses it too, whatever groups the binding holds
    let p = principal(&policy, BOB, &[USER]);
    let b = bind(&policy, &p).with_scope_for_tests(Arc::new(scope([scope_item(
        &policy,
        SCOPE_LINE,
        &read,
        &[],
    )])));
    let line = policy.registry.model(SCOPE_LINE).unwrap();
    assert_programming(
        &policy.access_domain(&b, line, Operation::Read).unwrap_err(),
        message,
    );
}
