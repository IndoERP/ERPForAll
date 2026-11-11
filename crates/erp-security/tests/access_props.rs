//! - P1: the compiled predicate, run by the engine, selects what the in-memory evaluator selects
//!   with the same predicate's parameters bound (`Domain::bind_params`, the test oracle).
//! - P1b: what the engine shows equals the in-memory evaluation of the rules combined
//!   independently from their raw definitions (group and operation filters, OR across the
//!   principal's permissions, AND of the restrictions), not of the engine's own `AccessDomain`.
//! - P2: the predicate's SQL value partitions the table into `TRUE`, `FALSE` and `NULL` rows;
//!   only `TRUE` rows are searched or pass a record check.
//! - P3: a record check of one id passes exactly when the list forms (search, `filtered_access`)
//!   hold the id, for every operation.
//! - P4: adding a group to a principal never hides a record; adding a restriction never shows one.
//! - P7: an `_inherits` child is visible exactly when its own rules and its parent's full
//!   predicate allow it, and its predicate holds `delegate any! <parent predicate>`.
//!
//! Each property runs a fixed-seed batch in CI; the `#[ignore]`d `*_full` variants run the design
//! note's sizes with more cases (`cargo test -p erp-security --test access_props -- --ignored`).
//! Committed seeds under `tests/seeds/` are read-only fixtures. Generated failures are saved
//! under workspace `target/proptest-regressions/erp-security/`; both are replayed first.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod props;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use erp_domain::{Domain, DomainKind, Operator, Value};
use erp_orm::{AccessPolicy, BeginOptions, Operation, Principal, Transaction};
use erp_security::snapshot::PolicySnapshot;
use erp_testkit::Harness;
use props::generate::{self, Case, On, PRINCIPAL_GROUPS, RuleSpec, Sizes};
use props::oracle::{
    EngineAnswer, IdSet, Oracle, classify, diff, engine_allowed, is_access, visible,
};
use props::{CATEG, CHILD, OBJ, SwapPolicy, fail};
use proptest::prelude::*;
use proptest::test_runner::{TestCaseError, TestRunner};

/// Every model with generated rows.
const MODELS: [&str; 3] = [OBJ, CHILD, CATEG];

/// Run `property` on the cases of `strategy` with `runner`, on one harness.
fn check<S, F>(mut runner: TestRunner, strategy: S, property: F)
where
    S: Strategy,
    F: AsyncFn(&Harness, &SwapPolicy, S::Value) -> Result<(), TestCaseError>,
{
    let rt = props::runtime();
    let (h, swap) = rt.block_on(props::swap_harness());
    let result = runner.run(&strategy, |value| rt.block_on(property(&h, &swap, value)));
    rt.block_on(h.cleanup()).unwrap();
    if let Err(error) = result {
        panic!("{error}");
    }
}

/// Install the snapshot of `rules` for the next transactions.
fn install(h: &Harness, swap: &SwapPolicy, rules: &[RuleSpec]) -> Arc<PolicySnapshot> {
    let snapshot = Arc::new(props::compile(h.registry(), rules, 1));
    swap.set(snapshot.clone());
    snapshot
}

async fn begin(h: &Harness, principal: &Principal) -> Transaction {
    h.orm()
        .begin_opts(BeginOptions::for_principal(principal.clone()))
        .await
        .unwrap()
}

/// The engine's answer for the transaction's own principal.
async fn own_answer(tx: &mut Transaction, model: &str, ids: &[i64], op: Operation) -> EngineAnswer {
    let env = tx.env();
    engine_allowed(tx, &env, model, ids, op).await
}

/// What the transaction's own principal may `op` (see [`visible`]).
async fn own_visible(tx: &mut Transaction, model: &str, ids: &[i64], op: Operation) -> IdSet {
    let env = tx.env();
    visible(tx, &env, model, ids, op).await
}

/// The policy's own model-level refusal of `op` on `model` (the SEC-008 text), which `search`
/// raises when the predicate is `FALSE`.
fn model_refusal(
    snapshot: &PolicySnapshot,
    principal: &Principal,
    tx: &Transaction,
    model: &str,
    op: Operation,
) -> erp_fields::Error {
    let spec = tx.registry().model(model).unwrap();
    snapshot
        .check_model(&props::bind(snapshot, principal), spec, op)
        .expect_err("a FALSE predicate is a model-level refusal")
}

/// `actual` is `expected` (kind and message).
fn same_error(actual: &erp_fields::Error, expected: &erp_fields::Error) -> bool {
    (actual.kind(), actual.message()) == (expected.kind(), expected.message())
}

// ----- P1 ----------------------------------------------------------------------------------------

/// What a P1 run compared: (model, operation) pairs; pairs whose predicate kept some rows and hid
/// others; pairs with principal parameters; pairs the policy refused with an error.
static P1_PAIRS: AtomicUsize = AtomicUsize::new(0);
static P1_SPLIT: AtomicUsize = AtomicUsize::new(0);
static P1_PARAMS: AtomicUsize = AtomicUsize::new(0);
static P1_ERRORS: AtomicUsize = AtomicUsize::new(0);

async fn p1(h: &Harness, swap: &SwapPolicy, case: Case) -> Result<(), TestCaseError> {
    let ids = props::seed(h, &case).await;
    let snapshot = install(h, swap, &case.rules);
    for spec in &case.principals {
        let principal = props::principal(&*snapshot, spec);
        let mut tx = begin(h, &principal).await;
        let oracle = Oracle::load(&mut tx, &principal).await;
        for model in MODELS {
            for op in Operation::ALL {
                let expected = oracle
                    .allowed(&mut tx, &*snapshot, &principal, model, ids.of(model), op)
                    .await;
                let actual = own_answer(&mut tx, model, ids.of(model), op).await;
                let at = format!("{model} {op} for {spec:?}");
                P1_PAIRS.fetch_add(1, Ordering::Relaxed);
                match &expected {
                    Ok((kept, domain)) => {
                        if !kept.is_empty() && kept.len() < ids.of(model).len() {
                            P1_SPLIT.fetch_add(1, Ordering::Relaxed);
                        }
                        if domain.has_params() {
                            P1_PARAMS.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    Err(_) => {
                        P1_ERRORS.fetch_add(1, Ordering::Relaxed);
                    }
                }
                match expected {
                    Ok((expected, domain)) => {
                        match &actual.filtered {
                            Ok(filtered) if *filtered == expected => {}
                            Ok(filtered) => {
                                return Err(fail(format!(
                                    "{at}: filtered_access differs from the oracle for \
                                     {domain:?}: {}",
                                    diff(filtered, &expected)
                                )));
                            }
                            Err(error) => {
                                return Err(fail(format!(
                                    "{at}: filtered_access failed on {domain:?}: {error} ({:?})",
                                    error.kind()
                                )));
                            }
                        }
                        match &actual.searched {
                            None => {}
                            Some(Ok(searched)) if *searched == expected => {}
                            // `FALSE`: the model check refuses before any statement
                            Some(Err(error))
                                if domain.is_false()
                                    && same_error(
                                        error,
                                        &model_refusal(&snapshot, &principal, &tx, model, op),
                                    ) => {}
                            Some(other) => {
                                return Err(fail(format!(
                                    "{at}: search is not the oracle's {expected:?} for \
                                     {domain:?}: {other:?}"
                                )));
                            }
                        }
                    }
                    // the policy has no predicate (a rule that does not compile): the model check
                    // refuses with its error, which `filtered_access` turns into "nothing"
                    Err(policy) => {
                        let filtered_ok = match &actual.filtered {
                            Ok(filtered) => is_access(&policy) && filtered.is_empty(),
                            Err(error) => same_error(error, &policy),
                        };
                        let searched_ok = match &actual.searched {
                            None => true,
                            Some(Ok(_)) => false,
                            Some(Err(error)) => same_error(error, &policy),
                        };
                        if !filtered_ok || !searched_ok {
                            return Err(fail(format!(
                                "{at}: the policy failed with {policy} ({:?}) but the engine \
                                 answered {actual:?}",
                                policy.kind()
                            )));
                        }
                    }
                }
            }
        }
        drop(tx);
    }
    Ok(())
}

/// P1 (a): compiled predicate = in-memory evaluator, on every model and operation.
/// The run must compare predicates that split the rows and that bind principal parameters, or it
/// proves little.
#[test]
fn p1_compiled_predicates_select_what_the_oracle_selects() {
    check(
        props::runner(
            48,
            0x5031_0001,
            concat!(env!("CARGO_MANIFEST_DIR"), "/tests/seeds/p1.txt"),
        ),
        generate::case(&Sizes::ci()),
        p1,
    );
    let (pairs, split, params, errors) = (
        P1_PAIRS.load(Ordering::Relaxed),
        P1_SPLIT.load(Ordering::Relaxed),
        P1_PARAMS.load(Ordering::Relaxed),
        P1_ERRORS.load(Ordering::Relaxed),
    );
    eprintln!(
        "p1: {pairs} pairs, {split} split the rows, {params} with parameters, {errors} errors"
    );
    assert!(
        split * 4 >= pairs && params * 4 >= pairs,
        "the generated predicates are too trivial: {pairs} pairs, {split} split the rows, \
         {params} bind principal parameters"
    );
}

/// P1 at the design note's sizes (50–500 rows), more cases, a random seed.
#[test]
#[ignore = "heavy: design-note sizes; run with --ignored on a build host"]
fn p1_full() {
    check(
        props::heavy_runner(
            128,
            concat!(env!("CARGO_MANIFEST_DIR"), "/tests/seeds/p1.txt"),
        ),
        generate::case(&Sizes::full()),
        p1,
    );
}

// ----- P1b: the rules combined from their definitions ----------------------------------------------

/// The groups `direct` imply, from the fixture's own implication rows (both user types imply
/// everyone, group 3 implies group 1), not from the engine's group graph.
fn implied_groups(direct: &[i64]) -> BTreeSet<i64> {
    const IMPLIES: [(i64, i64); 3] = [
        (props::USER, props::EVERYONE),
        (props::PORTAL, props::EVERYONE),
        (props::G3, props::G1),
    ];
    let mut groups: BTreeSet<i64> = direct.iter().copied().collect();
    loop {
        let before = groups.len();
        for (from, to) in IMPLIES {
            if groups.contains(&from) {
                groups.insert(to);
            }
        }
        if groups.len() == before {
            return groups;
        }
    }
}

/// The letter of `op` in an `ir.access` operation column (`c`, `r`, `u`, `d`).
fn op_letter(op: Operation) -> char {
    match op {
        Operation::Create => 'c',
        Operation::Read => 'r',
        Operation::Write => 'u',
        Operation::Unlink => 'd',
    }
}

/// The compiled domain of each rule alone (one snapshot of just that rule), `None` when it does
/// not compile. Only the domain text comes from the compiler; which rules apply and how they
/// combine is decided below from the raw definitions.
fn domains_of(h: &Harness, rules: &[RuleSpec]) -> Vec<Option<Domain>> {
    rules
        .iter()
        .map(|rule| {
            let one = props::compile(h.registry(), std::slice::from_ref(rule), 1);
            let model = one.registry().model(rule.on.model()).unwrap().id;
            let mut compiled = one.rules_of(model);
            let found = compiled.next().expect("the rule is compiled");
            assert!(compiled.next().is_none(), "one rule, one compiled rule");
            found.domain().ok().cloned()
        })
        .collect()
}

fn combined(
    rules: &[RuleSpec],
    domains: &[Option<Domain>],
    groups: &BTreeSet<i64>,
    on: On,
    op: Operation,
) -> Option<Domain> {
    let mut permissions = Vec::new();
    let mut restrictions = Vec::new();
    for (rule, domain) in rules.iter().zip(domains) {
        if rule.on != on || !rule.operation().contains(op_letter(op)) {
            continue;
        }
        let domain = domain.clone()?;
        match rule.group_id() {
            Some(group) if groups.contains(&group) => permissions.push(domain),
            Some(_) => {}
            None => restrictions.push(domain),
        }
    }
    Some(Domain::and([
        Domain::or(permissions),
        Domain::and(restrictions),
    ]))
}

/// Pairs compared, and those that kept some rows and hid others.
static P1B_PAIRS: AtomicUsize = AtomicUsize::new(0);
static P1B_SPLIT: AtomicUsize = AtomicUsize::new(0);

async fn p1b(h: &Harness, swap: &SwapPolicy, case: Case) -> Result<(), TestCaseError> {
    // rules the compiler accepts, and no `access` condition (the in-memory evaluation reads as
    // superuser; P1 covers `access`)
    let rules: Vec<RuleSpec> = case
        .rules
        .iter()
        .map(|r| strip_access(&r.without_errors()))
        .collect();
    let ids = props::seed(h, &case).await;
    let snapshot = install(h, swap, &rules);
    let domains = domains_of(h, &rules);
    for spec in &case.principals {
        let principal = props::principal(&*snapshot, spec);
        let mut tx = begin(h, &principal).await;
        let oracle = Oracle::load(&mut tx, &principal).await;
        let groups = implied_groups(&spec.direct_groups());
        for on in [On::Obj, On::Categ] {
            let model = on.model();
            for op in Operation::ALL {
                let at = format!("{model} {op} for {spec:?}");
                let Some(domain) = combined(&rules, &domains, &groups, on, op) else {
                    return Err(fail(format!("{at}: a cleaned rule does not compile")));
                };
                let bound = domain.bind_params(&|p| oracle.value(p)).unwrap();
                let all = tx
                    .env()
                    .sudo(true)
                    .records(model)
                    .unwrap()
                    .browse(ids.of(model).iter().copied())
                    .unwrap();
                let expected: IdSet = all
                    .filtered_domain(&mut tx, &bound)
                    .await
                    .unwrap_or_else(|e| panic!("{at}: the oracle failed on {bound:?}: {e}"))
                    .ids()
                    .into_iter()
                    .collect();
                let actual = own_visible(&mut tx, model, ids.of(model), op).await;
                P1B_PAIRS.fetch_add(1, Ordering::Relaxed);
                if !expected.is_empty() && expected.len() < ids.of(model).len() {
                    P1B_SPLIT.fetch_add(1, Ordering::Relaxed);
                }
                if actual != expected {
                    return Err(fail(format!(
                        "{at}: the engine shows what the rules combined from their definitions \
                         ({domain:?}) do not: {}",
                        diff(&actual, &expected)
                    )));
                }
            }
        }
    }
    Ok(())
}

/// P1b (a): what the engine shows equals the in-memory evaluation of the rules
/// combined independently from their raw definitions (group filter, operation filter, OR across
/// the principal's permissions, AND of the restrictions), not of the engine's own `AccessDomain`.
#[test]
fn p1b_rules_combine_as_their_definitions_say() {
    check(
        props::runner(
            P1B_CASES,
            0x5031_0002,
            concat!(env!("CARGO_MANIFEST_DIR"), "/tests/seeds/p1b.txt"),
        ),
        generate::case(&Sizes::ci()),
        p1b,
    );
    let (pairs, split) = (
        P1B_PAIRS.load(Ordering::Relaxed),
        P1B_SPLIT.load(Ordering::Relaxed),
    );
    eprintln!("p1b: {pairs} pairs, {split} split the rows");
    assert!(
        split * 8 >= pairs,
        "the generated rules are too trivial: {pairs} pairs, {split} split the rows"
    );
}

/// Cases of the CI run of P1b.
const P1B_CASES: u32 = 32;

/// P1b at the design note's sizes.
#[test]
#[ignore = "heavy: design-note sizes; run with --ignored on a build host"]
fn p1b_full() {
    check(
        props::heavy_runner(
            128,
            concat!(env!("CARGO_MANIFEST_DIR"), "/tests/seeds/p1b.txt"),
        ),
        generate::case(&Sizes::full()),
        p1b,
    );
}

// ----- P2 ----------------------------------------------------------------------------------------

/// Rows whose predicate was `NULL` in SQL, over the whole run (the generator must produce some).
static NULL_ROWS: AtomicUsize = AtomicUsize::new(0);

/// The first line of a record-level refusal (SEC-009, `explain::RecordMessage`).
const RECORD_HEADER: &str = "Uh-oh! Looks like you have stumbled upon some top-secret records.";

/// Whether `error` is a refusal of `op` on `model`: the SEC-009 record message, or the model-level
/// message when no ACL row of the principal's groups grants the operation (the model check comes
/// first). The property only needs the refusal, not the body under it.
fn is_refusal(tx: &Transaction, model: &str, op: Operation, error: &erp_fields::Error) -> bool {
    let spec = tx.registry().model(model).unwrap();
    let verb = match op {
        Operation::Read => "access",
        Operation::Write => "modify",
        Operation::Create => "create",
        Operation::Unlink => "delete",
    };
    let model_header = format!(
        "You are not allowed to {verb} '{}' ({}) records.",
        spec.description, spec.name
    );
    is_access(error)
        && (error.message().starts_with(RECORD_HEADER)
            || error.message().starts_with(&model_header))
}

/// Cases of the CI run of P2.
const P2_CASES: u32 = 32;

async fn p2(h: &Harness, swap: &SwapPolicy, case: Case) -> Result<(), TestCaseError> {
    let ids = props::seed(h, &case).await;
    let snapshot = install(h, swap, &case.rules);
    for spec in &case.principals {
        let principal = props::principal(&*snapshot, spec);
        let mut tx = begin(h, &principal).await;
        for model in [OBJ, CHILD] {
            for op in [Operation::Read, Operation::Write] {
                let at = format!("{model} {op} for {spec:?}");
                let Ok(access) = snapshot.access_domain(
                    &props::bind(&*snapshot, &principal),
                    tx.registry().model(model).unwrap(),
                    op,
                ) else {
                    continue;
                };
                let classes = classify(&mut tx, &principal, model, access.domain())
                    .await
                    .unwrap_or_else(|e| panic!("{at}: classification failed: {e}"));
                let all: IdSet = ids.of(model).iter().copied().collect();
                let union: IdSet = classes
                    .true_
                    .iter()
                    .chain(&classes.false_)
                    .chain(&classes.null)
                    .copied()
                    .collect();
                if union != all {
                    return Err(fail(format!(
                        "{at}: TRUE/FALSE/NULL do not cover the table: {}",
                        diff(&union, &all)
                    )));
                }
                NULL_ROWS.fetch_add(classes.null.len(), Ordering::Relaxed);
                let listed = own_visible(&mut tx, model, ids.of(model), op).await;
                if listed != classes.true_ {
                    return Err(fail(format!(
                        "{at}: the engine's list is not the TRUE rows of {:?}: {}",
                        access.domain(),
                        diff(&listed, &classes.true_)
                    )));
                }
                // point checks: every NULL row and a few FALSE rows are refused, a few TRUE rows
                // pass (each its own statement: refusals are never memoized)
                let refused = classes
                    .null
                    .iter()
                    .chain(classes.false_.iter().take(4))
                    .copied();
                for id in refused {
                    let one = tx.env().records(model).unwrap().browse([id]).unwrap();
                    match one.check_access(&mut tx, op).await {
                        Err(error) if is_refusal(&tx, model, op, &error) => {}
                        other => {
                            return Err(fail(format!(
                                "{at}: record {id} (predicate {}) passed or failed otherwise: \
                                 {other:?}",
                                if classes.null.contains(&id) {
                                    "NULL"
                                } else {
                                    "FALSE"
                                }
                            )));
                        }
                    }
                }
                for id in classes.true_.iter().take(4) {
                    let one = tx.env().records(model).unwrap().browse([*id]).unwrap();
                    if !one.has_access(&mut tx, op).await.unwrap() {
                        return Err(fail(format!("{at}: TRUE record {id} refused")));
                    }
                }
            }
        }
    }
    Ok(())
}

/// P2 (b): ternary partition; `NULL` rows are neither searched nor allowed by a
/// record check. The run must meet `NULL` rows, or it proves nothing.
#[test]
fn p2_null_predicates_deny() {
    NULL_ROWS.store(0, Ordering::Relaxed);
    check(
        props::runner(
            P2_CASES,
            0x5032_0001,
            concat!(env!("CARGO_MANIFEST_DIR"), "/tests/seeds/p2.txt"),
        ),
        generate::case(&Sizes::ci()),
        p2,
    );
    let nulls = NULL_ROWS.load(Ordering::Relaxed);
    eprintln!("p2: {nulls} rows with a NULL predicate");
    assert!(
        nulls >= P2_CASES as usize,
        "too few NULL rows: {nulls} over {P2_CASES} cases (measured 392 over 32)"
    );
}

/// P2 at the design note's sizes.
#[test]
#[ignore = "heavy: design-note sizes; run with --ignored on a build host"]
fn p2_full() {
    check(
        props::heavy_runner(
            128,
            concat!(env!("CARGO_MANIFEST_DIR"), "/tests/seeds/p2.txt"),
        ),
        generate::case(&Sizes::full()),
        p2,
    );
}

// ----- P3 ----------------------------------------------------------------------------------------

/// Point checks run, and lists that kept some ids and dropped others.
static P3_POINTS: AtomicUsize = AtomicUsize::new(0);
static P3_SPLIT: AtomicUsize = AtomicUsize::new(0);

/// Cases of the CI run of P3.
const P3_CASES: u32 = 16;

async fn p3(h: &Harness, swap: &SwapPolicy, case: Case) -> Result<(), TestCaseError> {
    let ids = props::seed(h, &case).await;
    let snapshot = install(h, swap, &case.rules);
    for spec in &case.principals {
        let principal = props::principal(&*snapshot, spec);
        // the point forms first, each id alone in a fresh transaction's memo
        let mut point_tx = begin(h, &principal).await;
        let mut list_tx = begin(h, &principal).await;
        for model in MODELS {
            for op in Operation::ALL {
                let at = format!("{model} {op} for {spec:?}");
                let mut point = IdSet::new();
                for id in ids.of(model) {
                    let one = point_tx
                        .env()
                        .records(model)
                        .unwrap()
                        .browse([*id])
                        .unwrap();
                    if one
                        .has_access(&mut point_tx, op)
                        .await
                        .unwrap_or_else(|e| panic!("{at}: record {id}: {e}"))
                    {
                        point.insert(*id);
                    }
                }
                let list = own_visible(&mut list_tx, model, ids.of(model), op).await;
                P3_POINTS.fetch_add(ids.of(model).len(), Ordering::Relaxed);
                if !list.is_empty() && list.len() < ids.of(model).len() {
                    P3_SPLIT.fetch_add(1, Ordering::Relaxed);
                }
                if point != list {
                    return Err(fail(format!(
                        "{at}: point checks and the list differ: {}",
                        diff(&point, &list)
                    )));
                }
                // the same list again, now answered partly from the memo
                let again = own_visible(&mut point_tx, model, ids.of(model), op).await;
                if again != list {
                    return Err(fail(format!(
                        "{at}: the memoized list differs: {}",
                        diff(&again, &list)
                    )));
                }
            }
        }
    }
    Ok(())
}

/// P3 (c): `check(id)` ⇔ `id ∈ search`, for every id and operation.
#[test]
fn p3_point_checks_agree_with_the_list() {
    check(
        props::runner(
            P3_CASES,
            0x5033_0001,
            concat!(env!("CARGO_MANIFEST_DIR"), "/tests/seeds/p3.txt"),
        ),
        generate::case(&Sizes::point()),
        p3,
    );
    let (points, split) = (
        P3_POINTS.load(Ordering::Relaxed),
        P3_SPLIT.load(Ordering::Relaxed),
    );
    eprintln!("p3: {points} point checks, {split} lists split the rows");
    assert!(
        split >= P3_CASES as usize,
        "too few lists kept some ids and dropped others: {split} over {P3_CASES} cases (measured 115 over 16)"
    );
}

/// P3 with the CI sizes and more cases.
#[test]
#[ignore = "heavy: one statement per record and operation; run with --ignored on a build host"]
fn p3_full() {
    check(
        props::heavy_runner(
            64,
            concat!(env!("CARGO_MANIFEST_DIR"), "/tests/seeds/p3.txt"),
        ),
        generate::case(&Sizes::ci()),
        p3,
    );
}

// ----- P4 ----------------------------------------------------------------------------------------

/// What P4 adds to a case.
#[derive(Debug, Clone)]
enum Delta {
    /// Principal `.0` (modulo the count) gets the direct group `PRINCIPAL_GROUPS[.1]`.
    Group(usize, usize),
    /// A restriction rule.
    Restriction(RuleSpec),
}

fn delta() -> impl Strategy<Value = Delta> {
    prop_oneof![
        (0usize..4, 0usize..PRINCIPAL_GROUPS.len()).prop_map(|(p, g)| Delta::Group(p, g)),
        prop_oneof![
            3 => Just(On::Obj),
            1 => Just(On::Categ),
            1 => Just(On::Child)
        ]
        .prop_flat_map(generate::restriction)
        .prop_map(Delta::Restriction),
    ]
}

/// What every principal of `case` sees under `rules` (one transaction each).
async fn sees(
    h: &Harness,
    swap: &SwapPolicy,
    rules: &[RuleSpec],
    principals: &[Principal],
    ids: &props::Ids,
) -> Vec<Vec<(String, Operation, IdSet)>> {
    let _snapshot = install(h, swap, rules);
    let mut out = Vec::new();
    for principal in principals {
        let mut tx = begin(h, principal).await;
        let mut seen = Vec::new();
        for model in MODELS {
            for op in Operation::ALL {
                seen.push((
                    model.to_owned(),
                    op,
                    own_visible(&mut tx, model, ids.of(model), op).await,
                ));
            }
        }
        out.push(seen);
    }
    out
}

/// (principal, model, operation) answers the delta changed.
static P4_CHANGED: AtomicUsize = AtomicUsize::new(0);

/// Cases of the CI run of P4.
const P4_CASES: u32 = 32;

async fn p4(h: &Harness, swap: &SwapPolicy, input: (Case, Delta)) -> Result<(), TestCaseError> {
    let (case, delta) = input;
    // a negated `access` condition shrinks when the target's access grows: excluded by design
    let rules: Vec<RuleSpec> = case
        .rules
        .iter()
        .map(RuleSpec::without_negated_access)
        .collect();
    let ids = props::seed(h, &case).await;
    let snapshot = props::compile(h.registry(), &rules, 1);
    let principals: Vec<Principal> = case
        .principals
        .iter()
        .map(|p| props::principal(&snapshot, p))
        .collect();
    let before = sees(h, swap, &rules, &principals, &ids).await;
    let (after, grows) = match &delta {
        Delta::Group(index, group) => {
            let index = index % case.principals.len();
            let spec = &case.principals[index];
            let mut direct = spec.direct_groups();
            direct.push(PRINCIPAL_GROUPS[*group]);
            let mut widened = principals.clone();
            widened[index] = props::principal_with(&snapshot, spec.uid, &direct, &spec.allowed());
            (sees(h, swap, &rules, &widened, &ids).await, true)
        }
        Delta::Restriction(rule) => {
            let mut more = rules.clone();
            more.push(rule.without_negated_access());
            (sees(h, swap, &more, &principals, &ids).await, false)
        }
    };
    for (p, (old, new)) in before.iter().zip(&after).enumerate() {
        for ((model, op, old), (_, _, new)) in old.iter().zip(new) {
            let (small, large) = if grows { (old, new) } else { (new, old) };
            if small != large {
                P4_CHANGED.fetch_add(1, Ordering::Relaxed);
            }
            if !small.is_subset(large) {
                return Err(fail(format!(
                    "principal {p}, {model} {op}: {delta:?} {} the visible set: {}",
                    if grows { "shrank" } else { "grew" },
                    diff(old, new)
                )));
            }
        }
    }
    Ok(())
}

/// P4 (d): a group never removes rows, a restriction never adds rows.
#[test]
fn p4_groups_widen_and_restrictions_narrow() {
    check(
        props::runner(
            P4_CASES,
            0x5034_0001,
            concat!(env!("CARGO_MANIFEST_DIR"), "/tests/seeds/p4.txt"),
        ),
        (generate::case(&Sizes::ci()), delta()),
        p4,
    );
    let changed = P4_CHANGED.load(Ordering::Relaxed);
    eprintln!("p4: {changed} answers changed by a delta");
    assert!(
        changed >= P4_CASES as usize / 2,
        "too few answers changed by a delta: {changed} over {P4_CASES} cases (measured 93 over 32)"
    );
}

/// P4 at the design note's sizes.
#[test]
#[ignore = "heavy: design-note sizes; run with --ignored on a build host"]
fn p4_full() {
    check(
        props::heavy_runner(
            128,
            concat!(env!("CARGO_MANIFEST_DIR"), "/tests/seeds/p4.txt"),
        ),
        (generate::case(&Sizes::full()), delta()),
        p4,
    );
}

// ----- P7 ----------------------------------------------------------------------------------------

fn own_child_domain(
    snapshot: &PolicySnapshot,
    principal: &Principal,
    op: Operation,
) -> Option<Domain> {
    let model = snapshot.registry().model(CHILD).unwrap().id;
    let groups = principal.resolution().groups();
    let mut permissions = Vec::new();
    let mut restrictions = Vec::new();
    for rule in snapshot.rules_of(model) {
        if !rule.ops().contains(op) {
            continue;
        }
        let domain = rule.domain().ok()?.clone();
        match rule.group() {
            Some(group) if groups.contains(group) => permissions.push(domain),
            Some(_) => {}
            None => restrictions.push(domain),
        }
    }
    Some(Domain::and([
        Domain::or(permissions),
        Domain::and(restrictions),
    ]))
}

/// The conjuncts of `domain`.
fn conjuncts(domain: &Domain) -> Vec<Domain> {
    match domain.kind() {
        DomainKind::And(children) => children.to_vec(),
        _ => vec![domain.clone()],
    }
}

/// Child answers compared, and those that kept some children and dropped others.
static P7_PAIRS: AtomicUsize = AtomicUsize::new(0);
static P7_SPLIT: AtomicUsize = AtomicUsize::new(0);

async fn p7(h: &Harness, swap: &SwapPolicy, case: Case) -> Result<(), TestCaseError> {
    // the child's own rules are evaluated in memory: no `access` condition (P1 covers them)
    let rules: Vec<RuleSpec> = case
        .rules
        .iter()
        .map(|r| {
            if r.on == On::Child {
                strip_access(r)
            } else {
                r.clone()
            }
        })
        .collect();
    let ids = props::seed(h, &case).await;
    let snapshot = install(h, swap, &rules);
    let child_spec = snapshot.registry().model(CHILD).unwrap();
    let obj_spec = snapshot.registry().model(OBJ).unwrap();
    for spec in &case.principals {
        let principal = props::principal(&*snapshot, spec);
        let binding = props::bind(&*snapshot, &principal);
        let mut tx = begin(h, &principal).await;
        let oracle = Oracle::load(&mut tx, &principal).await;
        for op in Operation::ALL {
            let at = format!("{op} for {spec:?}");
            // a parent or child rule that does not compile denies with its error: P1 compares
            // those errors with the engine's; this property is about the predicates' shape
            let (Ok(parent), Ok(child), Some(own)) = (
                snapshot.access_domain(&binding, obj_spec, op),
                snapshot.access_domain(&binding, child_spec, op),
                own_child_domain(&snapshot, &principal, op),
            ) else {
                continue;
            };
            // shape (SEC-013/045): a parent that hides everything hides every child; otherwise
            // the child's predicate holds `obj_id any! <parent predicate>`, unless its own rules
            // already hide everything
            let delegate = Domain::condition(
                "obj_id",
                Operator::AnyBypass,
                Value::Domain(parent.domain().clone()),
            )
            .unwrap();
            let shape_ok = if parent.is_false() || own.is_false() {
                child.is_false()
            } else {
                conjuncts(child.domain()).contains(&delegate)
            };
            if !shape_ok {
                return Err(fail(format!(
                    "{at}: the child predicate {:?} is not its own rules AND {delegate:?}",
                    child.domain()
                )));
            }
            // results: own rules (in memory) and the parent visible through the engine
            let parents = own_visible(&mut tx, OBJ, &ids.objs, op).await;
            let bound = own.bind_params(&|p| oracle.value(p)).unwrap();
            let all = tx
                .env()
                .sudo(true)
                .records(CHILD)
                .unwrap()
                .browse(ids.children.iter().copied())
                .unwrap();
            let own_ok = all.filtered_domain(&mut tx, &bound).await.unwrap();
            // the parent of each child as seeded, read back from the table
            let parent_of: BTreeMap<i64, i64> = tx
                .query("SELECT id, obj_id FROM x_child", &[])
                .await
                .unwrap()
                .iter()
                .map(|row| (row.get::<_, i64>(0), row.get::<_, i64>(1)))
                .collect();
            let expected: IdSet = own_ok
                .ids()
                .into_iter()
                .filter(|c| parents.contains(&parent_of[c]))
                .collect();
            let actual = own_visible(&mut tx, CHILD, &ids.children, op).await;
            P7_PAIRS.fetch_add(1, Ordering::Relaxed);
            if !expected.is_empty() && expected.len() < ids.children.len() {
                P7_SPLIT.fetch_add(1, Ordering::Relaxed);
            }
            if actual != expected {
                return Err(fail(format!(
                    "{at}: children differ from own rules {own:?} AND visible parents: {}",
                    diff(&actual, &expected)
                )));
            }
        }
    }
    Ok(())
}

/// `r` with its `access` conditions replaced by a plain one.
fn strip_access(r: &RuleSpec) -> RuleSpec {
    let mut out = r.clone();
    if let Some(terms) = &mut out.domain {
        for term in terms {
            *term = replace_access(term, r.on.leaves());
        }
    }
    out
}

fn replace_access(term: &generate::Term, leaves: &[&str]) -> generate::Term {
    match term {
        generate::Term::Leaf(i) if leaves[usize::from(*i) % leaves.len()].contains("'access'") => {
            generate::Term::Leaf(0)
        }
        generate::Term::Leaf(_) => term.clone(),
        generate::Term::And(a, b) => generate::Term::And(
            Box::new(replace_access(a, leaves)),
            Box::new(replace_access(b, leaves)),
        ),
        generate::Term::Or(a, b) => generate::Term::Or(
            Box::new(replace_access(a, leaves)),
            Box::new(replace_access(b, leaves)),
        ),
        generate::Term::Not(a) => generate::Term::Not(Box::new(replace_access(a, leaves))),
    }
}

/// Cases with rules on both the parent and the child.
fn inherits_case(sizes: &Sizes) -> impl Strategy<Value = Case> + use<> {
    (
        generate::case(sizes),
        prop::collection::vec(generate::rule(), 1..=3),
        prop::collection::vec(generate::rule(), 1..=3),
    )
        .prop_map(|(mut case, parents, children)| {
            case.rules
                .extend(parents.into_iter().map(|r| RuleSpec { on: On::Obj, ..r }));
            case.rules.extend(
                children
                    .into_iter()
                    .map(|r| RuleSpec { on: On::Child, ..r }),
            );
            case
        })
}

/// P7: `_inherits` parents (SEC-013/045 shape and results for generated parents).
///
/// access for generated rules and rows; the change of a parent's value is the case's own test)
#[test]
fn p7_inherits_children_follow_their_parents() {
    check(
        props::runner(
            32,
            0x5037_0001,
            concat!(env!("CARGO_MANIFEST_DIR"), "/tests/seeds/p7.txt"),
        ),
        inherits_case(&Sizes::ci()),
        p7,
    );
    let (pairs, split) = (
        P7_PAIRS.load(Ordering::Relaxed),
        P7_SPLIT.load(Ordering::Relaxed),
    );
    eprintln!("p7: {pairs} child answers, {split} split the children");
    assert!(
        split * 4 >= pairs,
        "the generated child predicates are too trivial"
    );
}

/// P7 at the design note's sizes.
#[test]
#[ignore = "heavy: design-note sizes; run with --ignored on a build host"]
fn p7_full() {
    check(
        props::heavy_runner(
            128,
            concat!(env!("CARGO_MANIFEST_DIR"), "/tests/seeds/p7.txt"),
        ),
        inherits_case(&Sizes::full()),
        p7,
    );
}
