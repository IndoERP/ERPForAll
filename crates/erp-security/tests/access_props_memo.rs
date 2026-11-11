//! Property test P8 of the access engine (g, design note §8.2): memo
//! isolation. Principals with the **same groups** but a different subject, allowed companies,
//! API-key scope or elevation are interleaved in one transaction through `Env` clones taken from
//! other transactions; every answer must equal what a fresh transaction gives that principal.
//!
//! The record memo of a transaction is keyed by the full binding by value (DN §3.3); a memo keyed
//! by the group set (or by anything less than the binding) would hand one principal another's
//! answer, which these interleavings detect because the generated rules depend on the subject
//! (`user.id`, user paths) and the companies (`company_ids`, `company_id`). The snapshot's
//! combined-predicate memo is shared by such principals by design: its predicates hold no
//! principal value (parameters are bound per statement), which the same comparison checks.
//! Grant bindings and field masks do not exist yet.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod props;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use erp_orm::{
    AccessPolicy, ApiKeyId, BeginOptions, Env, MethodAllow, ModelAllow, Operation, Principal, Scope,
};
use erp_testkit::Harness;
use props::generate::{self, Case, Sizes};
use props::oracle::{IdSet, diff, visible};
use props::{CATEG, CHILD, OBJ, SwapPolicy, fail};
use proptest::prelude::*;
use proptest::test_runner::{TestCaseError, TestRunner};

const MODELS: [&str; 3] = [OBJ, CHILD, CATEG];

/// One principal of the interleaving; every variant of a case has the same direct groups.
#[derive(Debug, Clone)]
struct Variant {
    uid: i64,
    companies: Vec<u8>,
    /// A read-only API-key scope.
    read_only: bool,
    /// Superuser mode (`sudo()`, elevation `Full`).
    full: bool,
}

/// One step: variant, model, operation, and a record (`None`: the list forms).
#[derive(Debug, Clone)]
struct Step {
    variant: usize,
    model: usize,
    op: usize,
    record: Option<u16>,
}

#[derive(Debug, Clone)]
struct Input {
    case: Case,
    groups: u8,
    variants: Vec<Variant>,
    steps: Vec<Step>,
}

fn variant() -> impl Strategy<Value = Variant> {
    (
        2i64..=5,
        prop::collection::vec(0u8..3, 1..=3),
        prop::bool::weighted(0.2),
        prop::bool::weighted(0.15),
    )
        .prop_map(|(uid, companies, read_only, full)| Variant {
            uid,
            companies,
            read_only,
            full,
        })
}

fn input(sizes: &Sizes) -> impl Strategy<Value = Input> + use<> {
    (
        generate::case(sizes),
        prop_oneof![4 => (0u8..32).prop_map(|g| g | 1), 1 => 0u8..32],
        prop::collection::vec(variant(), 2..=5),
        prop::collection::vec(
            (
                0usize..5,
                0usize..MODELS.len(),
                0usize..4,
                prop::option::weighted(0.5, any::<u16>()),
            )
                .prop_map(|(variant, model, op, record)| Step {
                    variant,
                    model,
                    op,
                    record,
                }),
            6..=30,
        ),
    )
        .prop_map(|(case, groups, variants, steps)| Input {
            case,
            groups,
            variants,
            steps,
        })
}

/// The principal of `v` with the case's direct `groups`.
fn principal(policy: &dyn AccessPolicy, groups: u8, v: &Variant) -> Principal {
    let spec = generate::PrincipalSpec {
        uid: v.uid,
        groups,
        companies: v.companies.clone(),
    };
    let base = props::principal(policy, &spec);
    if !v.read_only {
        return base;
    }
    let scope = Scope::builder(ApiKeyId::new(7).unwrap(), "props read-only key")
        .models(ModelAllow::All)
        .methods(MethodAllow::All)
        .read_only(true)
        .build()
        .unwrap();
    erp_orm::PrincipalBuilder::user(base.subject())
        .resolution((**base.resolution()).clone())
        .companies(base.companies().allowed().to_vec())
        .scope(scope)
        .build()
        .unwrap()
}

/// The environment of `v`, taken from a transaction of its own (then dropped): what an `Env`
/// clone held by a recordset from elsewhere looks like.
async fn env_of(h: &Harness, principal: &Principal, v: &Variant) -> Env {
    let tx = h
        .orm()
        .begin_opts(BeginOptions::for_principal(principal.clone()))
        .await
        .unwrap();
    let env = tx.env();
    drop(tx);
    if v.full { env.sudo(true) } else { env }
}

/// Steps whose expected answer differs from another variant's for the same (model, operation):
/// those a shared memo entry would have answered wrongly.
static SENSITIVE: AtomicUsize = AtomicUsize::new(0);
static STEPS: AtomicUsize = AtomicUsize::new(0);
/// Steps by a plain user variant (neither superuser nor read-only), and those among them whose
/// expected answer differs from another plain user variant's: the hazard the property is about
/// (a memo keyed by the group set, answering one user with another's result). The superuser
/// variant differs from nearly everyone, so it is left out of this count.
static USER_STEPS: AtomicUsize = AtomicUsize::new(0);
static USER_SENSITIVE: AtomicUsize = AtomicUsize::new(0);

async fn p8(h: &Harness, swap: &SwapPolicy, input: Input) -> Result<(), TestCaseError> {
    let Input {
        case,
        groups,
        variants,
        steps,
    } = input;
    let ids = props::seed(h, &case).await;
    let snapshot = Arc::new(props::compile(h.registry(), &case.rules, 1));
    swap.set(snapshot.clone());
    let principals: Vec<Principal> = variants
        .iter()
        .map(|v| principal(&*snapshot, groups, v))
        .collect();
    let mut envs = Vec::new();
    for (p, v) in principals.iter().zip(&variants) {
        envs.push(env_of(h, p, v).await);
    }
    // what a fresh transaction gives each variant
    let mut expected: Vec<BTreeMap<(usize, usize), IdSet>> = Vec::new();
    for (p, env) in principals.iter().zip(&envs) {
        let mut tx = h
            .orm()
            .begin_opts(BeginOptions::for_principal(p.clone()))
            .await
            .unwrap();
        let mut answers = BTreeMap::new();
        for (m, model) in MODELS.iter().enumerate() {
            for (o, op) in Operation::ALL.iter().enumerate() {
                answers.insert(
                    (m, o),
                    visible(&mut tx, env, model, ids.of(model), *op).await,
                );
            }
        }
        expected.push(answers);
    }
    // the interleaving, in one transaction of the first variant
    let mut tx = h
        .orm()
        .begin_opts(BeginOptions::for_principal(principals[0].clone()))
        .await
        .unwrap();
    for step in &steps {
        let v = step.variant % variants.len();
        let (model, op) = (MODELS[step.model], Operation::ALL[step.op]);
        let want = &expected[v][&(step.model, step.op)];
        STEPS.fetch_add(1, Ordering::Relaxed);
        if expected
            .iter()
            .any(|other| other[&(step.model, step.op)] != *want)
        {
            SENSITIVE.fetch_add(1, Ordering::Relaxed);
        }
        let plain = |i: usize| {
            !variants[i % variants.len()].full && !variants[i % variants.len()].read_only
        };
        if plain(v) {
            USER_STEPS.fetch_add(1, Ordering::Relaxed);
            if expected
                .iter()
                .enumerate()
                .any(|(w, other)| plain(w) && other[&(step.model, step.op)] != *want)
            {
                USER_SENSITIVE.fetch_add(1, Ordering::Relaxed);
            }
        }
        let at = format!("{model} {op} as {:?}", variants[v]);
        let all = ids.of(model);
        match step.record {
            None => {
                let got = visible(&mut tx, &envs[v], model, all, op).await;
                if got != *want {
                    return Err(fail(format!(
                        "{at}: the interleaved list differs from a fresh transaction's: {}",
                        diff(&got, want)
                    )));
                }
            }
            Some(_) if all.is_empty() => {}
            Some(record) => {
                let id = all[usize::from(record) % all.len()];
                let one = envs[v].records(model).unwrap().browse([id]).unwrap();
                let got = one.has_access(&mut tx, op).await.unwrap();
                if got != want.contains(&id) {
                    return Err(fail(format!(
                        "{at}: record {id} answered {got}, a fresh transaction {}",
                        want.contains(&id)
                    )));
                }
            }
        }
    }
    Ok(())
}

fn check(mut runner: TestRunner, sizes: &Sizes) {
    let rt = props::runtime();
    let (h, swap) = rt.block_on(props::swap_harness());
    let result = runner.run(&input(sizes), |value| rt.block_on(p8(&h, &swap, value)));
    rt.block_on(h.cleanup()).unwrap();
    if let Err(error) = result {
        panic!("{error}");
    }
}

/// P8 (g): no memo entry is shared by principals that differ in anything but
/// their groups; every interleaved answer equals a fresh transaction's. The run must contain
/// steps where sharing would have answered wrongly.
#[test]
fn p8_memos_are_isolated_between_principals_with_the_same_groups() {
    check(
        props::runner(
            48,
            0x5038_0001,
            concat!(env!("CARGO_MANIFEST_DIR"), "/tests/seeds/p8.txt"),
        ),
        &Sizes::ci(),
    );
    let (steps, sensitive) = (
        STEPS.load(Ordering::Relaxed),
        SENSITIVE.load(Ordering::Relaxed),
    );
    eprintln!("p8: {steps} steps, {sensitive} where a shared memo entry would answer wrongly");
    assert!(
        sensitive * 4 >= steps,
        "too few steps tell the principals apart: {sensitive} of {steps}"
    );
    let (user_steps, user_sensitive) = (
        USER_STEPS.load(Ordering::Relaxed),
        USER_SENSITIVE.load(Ordering::Relaxed),
    );
    eprintln!(
        "p8: {user_steps} steps by plain users, {user_sensitive} where another plain user's \
         answer differs"
    );
    assert!(
        user_sensitive * 10 >= user_steps,
        "too few steps tell plain users apart: {user_sensitive} of {user_steps}"
    );
}

/// P8 at the design note's sizes.
#[test]
#[ignore = "heavy: design-note sizes; run with --ignored on a build host"]
fn p8_full() {
    check(
        props::heavy_runner(
            128,
            concat!(env!("CARGO_MANIFEST_DIR"), "/tests/seeds/p8.txt"),
        ),
        &Sizes::full(),
    );
}
