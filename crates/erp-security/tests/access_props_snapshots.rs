//! Properties P6 and P9 of the access engine (f and h, design note §8.2,
//! Revision 3 M1) with the policy read from the database: `ir_access` rows (whose statement
//! triggers bump the `policy` generation), `erp-security`'s `PolicySource` refreshed through
//! SIGNAL, principals resolved by its `Resolver`.
//!
//! - P6, snapshot pinning: while two other transactions change the rules and commit in the
//!   opposite order of their start, a running transaction keeps deciding with the snapshot it
//!   pinned (its answers equal a fresh process's at its start); a transaction begun between the
//!   two commits sees exactly the first committed change, one begun after both sees both (the sum
//!   of weights is not fooled by the commit order).
//! - P9, snapshot identity: a transaction's private snapshot (after its own uncommitted rule) and
//!   a later shared snapshot built from another committed rule have the same `policy` generation;
//!   the shared one's answers and rule ids equal a fresh process's, the private one never touches
//!   the shared search memo or the prepared statements. Second form: two shared builds of the same
//!   generation whose fresh interners give one `GroupSetId` to different group sets answer each
//!   principal as a fresh process does.
//!
//! The deterministic statement test of P9 is `p9_deterministic_snapshot_identity` below.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod props;

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};

use erp_fields::Value;
use erp_orm::{
    AccessPolicy, IntoPolicyProvider, Operation, SnapshotOrigin, StaticPolicy, Transaction,
};
use erp_security::source::PolicySource;
use erp_testkit::Harness;
use props::db::{self, ModelIds};
use props::generate::{self, Case, On, RuleSpec, Sizes};
use props::oracle::{IdSet, diff};
use props::{OBJ, fail};
use proptest::prelude::*;
use proptest::test_runner::{TestCaseError, TestRunner};

/// A database with the fixture's groups and models in the policy tables.
struct Fixture {
    h: Harness,
    models: ModelIds,
}

fn check<S, F>(mut runner: TestRunner, strategy: S, property: F)
where
    S: Strategy,
    F: AsyncFn(&Fixture, S::Value) -> Result<(), TestCaseError>,
{
    let rt = props::runtime();
    let fixture = rt.block_on(async {
        let (h, _swap) = props::swap_harness().await;
        let models = db::seed_policy_base(&h).await;
        Fixture { h, models }
    });
    let result = runner.run(&strategy, |value| rt.block_on(property(&fixture, value)));
    rt.block_on(fixture.h.cleanup()).unwrap();
    if let Err(error) = result {
        panic!("{error}");
    }
}

/// `answers` equal `expected`, or a failure naming the first difference.
fn same(at: &str, answers: &[IdSet], expected: &[IdSet]) -> Result<(), TestCaseError> {
    for (i, (got, want)) in answers.iter().zip(expected).enumerate() {
        if got != want {
            let (model, op) = db::pair(i);
            return Err(fail(format!("{at}: {model} {op}: {}", diff(got, want))));
        }
    }
    Ok(())
}

/// Rules any principal of the case can be affected by; the valid ones only (rules written by the
/// ORM are validated, raw rows are not, and the bumpers mix both).
fn valid(rules: &[RuleSpec]) -> Vec<RuleSpec> {
    rules.iter().map(RuleSpec::without_errors).collect()
}

// ----- P6 ----------------------------------------------------------------------------------------

/// A concurrent writer of `ir_access`: rules it adds, and whether it deletes one of the case's
/// rules (from its own half, so the two writers never wait on each other's row locks).
#[derive(Debug, Clone)]
struct Bumper {
    adds: Vec<RuleSpec>,
    delete: Option<u8>,
}

fn bumper() -> impl Strategy<Value = Bumper> {
    (
        prop::collection::vec(generate::rule(), 1..=2),
        prop::option::of(any::<u8>()),
    )
        .prop_map(|(adds, delete)| Bumper { adds, delete })
}

/// Run `bumper`'s statements in an open transaction of `client`; `half` 0 or 1 picks the rules
/// it may delete (even or odd positions of `rule_ids`).
async fn apply(
    client: &erp_testkit::tokio_postgres::Client,
    models: &ModelIds,
    bumper: &Bumper,
    rule_ids: &[i64],
    half: usize,
    name: &str,
) {
    for (i, rule) in valid(&bumper.adds).iter().enumerate() {
        db::insert_rule(client, models, rule, &format!("{name} {i}")).await;
    }
    let mine: Vec<i64> = rule_ids.iter().skip(half).step_by(2).copied().collect();
    if let Some(pick) = bumper.delete
        && !mine.is_empty()
    {
        let id = mine[usize::from(pick) % mine.len()];
        client
            .execute("DELETE FROM ir_access WHERE id = $1", &[&id])
            .await
            .unwrap();
    }
}

/// P6 cases where the committed writes changed what the principal sees (between and after the
/// commits): the run must contain some, or pinning was never put to the test.
static P6_CHANGED: AtomicUsize = AtomicUsize::new(0);
/// Cases of the CI run of P6.
const P6_CASES: u32 = 12;

async fn p6(
    f: &Fixture,
    (case, first, second, uid): (Case, Bumper, Bumper, i64),
) -> Result<(), TestCaseError> {
    let ids = props::seed(&f.h, &case).await;
    let rule_ids = db::set_policy(&f.h, &f.models, &case, &valid(&case.rules)).await;
    let source = db::source(&f.h).await;
    let orm = db::orm(&f.h, &source, true).await;

    let mut running = db::begin(&orm, uid).await;
    let pinned = running.policy().snapshot();
    let generation = running.policy().generation().policy();
    let before = db::answers(&mut running, &ids).await;
    same(
        "at its start",
        &before,
        &db::fresh_answers(&f.h, uid, &ids).await,
    )?;

    // two writers; the second commits first
    let a = db::connect(&f.h).await;
    let b = db::connect(&f.h).await;
    a.batch_execute("BEGIN").await.unwrap();
    apply(&a, &f.models, &first, &rule_ids, 0, "first").await;
    b.batch_execute("BEGIN").await.unwrap();
    apply(&b, &f.models, &second, &rule_ids, 1, "second").await;
    b.batch_execute("COMMIT").await.unwrap();

    same(
        "running, after the second commit",
        &db::answers(&mut running, &ids).await,
        &before,
    )?;
    let mut between = db::begin(&orm, uid).await;
    if between.policy().snapshot() == pinned
        || between.policy().generation().policy() != generation + 1
    {
        return Err(fail(format!(
            "begun after one commit: snapshot {:?} generation {} (pinned {pinned:?} at {generation})",
            between.policy().snapshot(),
            between.policy().generation().policy()
        )));
    }
    let mid = db::answers(&mut between, &ids).await;
    same(
        "begun between the commits",
        &mid,
        &db::fresh_answers(&f.h, uid, &ids).await,
    )?;

    a.batch_execute("COMMIT").await.unwrap();
    same(
        "running, after both commits",
        &db::answers(&mut running, &ids).await,
        &before,
    )?;
    if running.policy().snapshot() != pinned {
        return Err(fail(
            "the running transaction changed its snapshot".to_owned(),
        ));
    }
    let mut after = db::begin(&orm, uid).await;
    if after.policy().generation().policy() != generation + 2
        || after.policy().snapshot() == between.policy().snapshot()
    {
        return Err(fail(format!(
            "begun after both commits: snapshot {:?} generation {} (expected {})",
            after.policy().snapshot(),
            after.policy().generation().policy(),
            generation + 2
        )));
    }
    let last = db::answers(&mut after, &ids).await;
    same(
        "begun after both commits",
        &last,
        &db::fresh_answers(&f.h, uid, &ids).await,
    )?;
    if mid != before || last != before {
        P6_CHANGED.fetch_add(1, Ordering::Relaxed);
    }
    Ok(())
}

/// P6 (f): snapshot pinning under concurrent policy writes committed out of order.
#[test]
fn p6_snapshots_stay_pinned_while_others_commit() {
    check(
        props::runner(
            P6_CASES,
            0x5036_0001,
            concat!(env!("CARGO_MANIFEST_DIR"), "/tests/seeds/p6.txt"),
        ),
        (generate::case(&Sizes::ci()), bumper(), bumper(), 2i64..=5),
        p6,
    );
    let changed = P6_CHANGED.load(Ordering::Relaxed);
    eprintln!("p6: {changed} cases where the concurrent writes changed the answers");
    assert!(
        changed * 2 >= P6_CASES as usize,
        "too few cases where a concurrent write changed an answer: {changed} of {P6_CASES} \
         (measured 12 of 12)"
    );
}

/// P6 with the design note's sizes and more cases.
#[test]
#[ignore = "heavy: design-note sizes; run with --ignored on a build host"]
fn p6_full() {
    check(
        props::heavy_runner(
            64,
            concat!(env!("CARGO_MANIFEST_DIR"), "/tests/seeds/p6.txt"),
        ),
        (generate::case(&Sizes::full()), bumper(), bumper(), 2i64..=5),
        p6,
    );
}

// ----- P9 ----------------------------------------------------------------------------------------

/// The ids of the rules behind each (model, operation) predicate of `tx`'s principal (the
/// policy's error message where it has no predicate).
fn rule_ids(tx: &Transaction) -> Vec<Result<Vec<i64>, String>> {
    let policy = tx.policy();
    let principal = tx.env().principal().clone();
    let binding = props::bind(&**policy, &principal);
    let mut out = Vec::new();
    for model in db::MODELS {
        for op in Operation::ALL {
            let spec = tx.registry().model(model).unwrap();
            out.push(match policy.access_domain(&binding, spec, op) {
                Ok(domain) => {
                    let mut ids: Vec<i64> = domain.rules().iter().map(|r| r.id()).collect();
                    ids.sort_unstable();
                    Ok(ids)
                }
                Err(error) => Err(format!("{:?}: {}", error.kind(), error.message())),
            });
        }
    }
    out
}

/// The `ir.access` row of `rule` as ORM values.
fn access_values(models: &ModelIds, rule: &RuleSpec, name: &str) -> BTreeMap<String, Value> {
    let mut row = BTreeMap::new();
    row.insert("name".to_owned(), Value::str(name));
    row.insert("model_id".to_owned(), Value::Int(models[rule.on.model()]));
    row.insert(
        "group_id".to_owned(),
        rule.group_id().map_or(Value::False, Value::Int),
    );
    row.insert("operation".to_owned(), Value::str(rule.operation()));
    row.insert(
        "domain".to_owned(),
        rule.text().map_or(Value::False, Value::str),
    );
    row
}

/// Cases where the private and the later shared snapshot had the same `policy` generation (the
/// hazard of Revision 3 M1), and the second form's `GroupSetId` collisions.
/// Cases of the CI runs of P9 (first form) and of its interner form.
const P9_CASES: u32 = 12;
const P9I_CASES: u32 = 24;
static SAME_GENERATION: AtomicUsize = AtomicUsize::new(0);
static ID_COLLISIONS: AtomicUsize = AtomicUsize::new(0);

async fn p9(
    f: &Fixture,
    (case, mine, theirs, uid): (Case, RuleSpec, RuleSpec, i64),
) -> Result<(), TestCaseError> {
    let ids = props::seed(&f.h, &case).await;
    db::set_policy(&f.h, &f.models, &case, &valid(&case.rules)).await;
    let source = db::source(&f.h).await;
    let orm = db::orm(&f.h, &source, true).await;

    // T1 writes its own rule through the ORM: it decides on a private snapshot from then on
    let mut t1 = db::begin(&orm, uid).await;
    let sudo = t1.env().sudo(true);
    sudo.records("ir.access")
        .unwrap()
        .create_values(
            &mut t1,
            &[access_values(&f.models, &mine.without_errors(), "mine")],
        )
        .await
        .unwrap();
    db::answers(&mut t1, &ids).await;
    if t1.policy().snapshot().origin() != SnapshotOrigin::Private {
        return Err(fail(
            "the writing transaction did not rebuild privately".to_owned(),
        ));
    }
    let memo = orm.search_memo_stats();
    let prepared = t1.prepared_statements();
    db::answers(&mut t1, &ids).await;
    if orm.search_memo_stats() != memo || t1.prepared_statements() != prepared {
        return Err(fail(format!(
            "searches on a private snapshot touched shared caches: memo {memo:?} -> {:?}, \
             prepared statements {prepared} -> {}",
            orm.search_memo_stats(),
            t1.prepared_statements()
        )));
    }

    // T2 commits another rule (one bump, as T1's)
    let client = db::connect(&f.h).await;
    db::insert_rule(&client, &f.models, &theirs.without_errors(), "theirs").await;

    // T3 pins the refreshed shared snapshot and asks the same questions with the same groups
    let mut t3 = db::begin(&orm, uid).await;
    if t3.policy().snapshot().origin() != SnapshotOrigin::Shared
        || t3.policy().snapshot().id() == t1.policy().snapshot().id()
    {
        return Err(fail(format!(
            "T3 pinned {:?}, T1 decides on {:?}",
            t3.policy().snapshot(),
            t1.policy().snapshot()
        )));
    }
    if t3.policy().generation() == t1.policy().generation() {
        SAME_GENERATION.fetch_add(1, Ordering::Relaxed);
    }
    let answers = db::answers(&mut t3, &ids).await;
    let rules = rule_ids(&t3);
    let fresh_source = db::source(&f.h).await;
    let fresh_orm = db::orm(&f.h, &fresh_source, false).await;
    let mut fresh = db::begin(&fresh_orm, uid).await;
    same(
        "T3 against a fresh process",
        &answers,
        &db::answers(&mut fresh, &ids).await,
    )?;
    let fresh_rules = rule_ids(&fresh);
    if rules != fresh_rules {
        return Err(fail(format!(
            "T3 decides with the rules {rules:?}, a fresh process with {fresh_rules:?}"
        )));
    }
    t1.rollback().await.unwrap();
    Ok(())
}

/// P9 (h, Revision 3 M1): a private snapshot and a later shared snapshot with the
/// same `policy` generation never share decisions or caches.
#[test]
fn p9_private_and_shared_snapshots_of_one_generation_stay_apart() {
    SAME_GENERATION.store(0, Ordering::Relaxed);
    check(
        props::runner(
            P9_CASES,
            0x5039_0001,
            concat!(env!("CARGO_MANIFEST_DIR"), "/tests/seeds/p9.txt"),
        ),
        (
            generate::case(&Sizes::ci()),
            generate::rule(),
            generate::rule(),
            2i64..=5,
        ),
        p9,
    );
    let same = SAME_GENERATION.load(Ordering::Relaxed);
    eprintln!("p9: {same} cases with one generation for the private and the shared snapshot");
    assert!(
        same * 2 >= P9_CASES as usize,
        "the hazard (one generation, two snapshots) occurred in {same} of {P9_CASES} cases \
         (measured 12 of 12)"
    );
}

async fn p9_interners(
    f: &Fixture,
    (case, first, second): (Case, i64, i64),
) -> Result<(), TestCaseError> {
    let ids = props::seed(&f.h, &case).await;
    db::set_policy(&f.h, &f.models, &case, &valid(&case.rules)).await;
    let source: PolicySource = db::source(&f.h).await;
    let orm = db::orm(&f.h, &source, true).await;
    // two shared builds of the same generation, each with a fresh interner, each pinned by a
    // transaction whose principal interns its group set first
    source.refresh_shared().await.unwrap();
    let mut a = db::begin(&orm, first).await;
    let answers_a = db::answers(&mut a, &ids).await;
    source.refresh_shared().await.unwrap();
    let mut b = db::begin(&orm, second).await;
    let answers_b = db::answers(&mut b, &ids).await;
    let set = |tx: &Transaction| tx.env().principal().resolution().groups().clone();
    let id = |tx: &Transaction| tx.policy().intern_groups(&set(tx)).unwrap().id();
    if a.policy().snapshot().id() == b.policy().snapshot().id()
        || a.policy().generation() != b.policy().generation()
    {
        return Err(fail(format!(
            "expected two snapshots of one generation: {:?} {:?} / {:?} {:?}",
            a.policy().snapshot(),
            a.policy().generation(),
            b.policy().snapshot(),
            b.policy().generation()
        )));
    }
    if set(&a) != set(&b) && id(&a) == id(&b) {
        ID_COLLISIONS.fetch_add(1, Ordering::Relaxed);
    }
    same(
        "first principal",
        &answers_a,
        &db::fresh_answers(&f.h, first, &ids).await,
    )?;
    same(
        "second principal",
        &answers_b,
        &db::fresh_answers(&f.h, second, &ids).await,
    )?;
    Ok(())
}

/// P9, second form: one `GroupSetId` for different group sets in two shared snapshots of one
/// generation never mixes their decisions.
#[test]
fn p9_interner_ids_never_cross_snapshots() {
    ID_COLLISIONS.store(0, Ordering::Relaxed);
    check(
        props::runner(
            P9I_CASES,
            0x5039_0002,
            concat!(env!("CARGO_MANIFEST_DIR"), "/tests/seeds/p9i.txt"),
        ),
        (generate::case(&Sizes::ci()), 2i64..=5, 2i64..=5),
        p9_interners,
    );
    let collisions = ID_COLLISIONS.load(Ordering::Relaxed);
    eprintln!("p9: {collisions} cases where one GroupSetId named two group sets");
    assert!(
        collisions * 8 >= P9I_CASES as usize,
        "too few cases gave one GroupSetId to two group sets: {collisions} of {P9I_CASES} \
         (measured 3 of 12)"
    );
}

/// P9 with the design note's sizes and more cases (both forms).
#[test]
#[ignore = "heavy: design-note sizes; run with --ignored on a build host"]
fn p9_full() {
    check(
        props::heavy_runner(
            64,
            concat!(env!("CARGO_MANIFEST_DIR"), "/tests/seeds/p9.txt"),
        ),
        (
            generate::case(&Sizes::full()),
            generate::rule(),
            generate::rule(),
            2i64..=5,
        ),
        p9,
    );
    check(
        props::heavy_runner(
            64,
            concat!(env!("CARGO_MANIFEST_DIR"), "/tests/seeds/p9i.txt"),
        ),
        (generate::case(&Sizes::full()), 2i64..=5, 2i64..=5),
        p9_interners,
    );
}

// ----- P9, the deterministic statement test --------------------------------------------------------

/// The objects the transaction's principal reads.
async fn vals(tx: &mut Transaction) -> Vec<i64> {
    let env = tx.env();
    env.records(OBJ)
        .unwrap()
        .search(tx, &erp_domain::Domain::TRUE)
        .await
        .unwrap()
        .ids()
}

/// The deterministic case of P9 (DN §8.3): internal users read every object; T1 adds the
/// restriction `val > 3` through the ORM (private snapshot), T2 commits `val < 3` (same weight);
/// T3 pins the shared snapshot: it sees `val < 3` only, decides with T2's rule, and T1's searches
/// left the shared memo and the prepared statements alone. Snapshot ids are unique across the
/// shared builds, the private build, a forced refresh of the same generation and static policies.
#[tokio::test]
async fn p9_deterministic_snapshot_identity() {
    let (h, _swap) = props::swap_harness().await;
    let models = db::seed_policy_base(&h).await;
    let case = Case {
        partners: Vec::new(),
        user_partners: vec![0; 4],
        categs: Vec::new(),
        // val 1, 3, 5 (the value table: index 3 is 1, 5 is 3, 6 is 5)
        objs: [3u8, 5, 6]
            .into_iter()
            .map(|v| {
                let mut row = [0u8; 12];
                row[2] = v;
                generate::ObjRow(row)
            })
            .collect(),
        children: Vec::new(),
        rules: Vec::new(),
        principals: vec![generate::PrincipalSpec {
            uid: 2,
            groups: 0b1,
            companies: vec![0],
        }],
    };
    let ids = props::seed(&h, &case).await;
    let everyone = RuleSpec {
        on: On::Obj,
        group: Some(4),
        ops: 15,
        domain: None,
    };
    db::set_policy(&h, &models, &case, &[everyone]).await;
    let restriction = |op: &str| {
        let mut row = BTreeMap::new();
        row.insert("name".to_owned(), Value::str(format!("val {op} 3")));
        row.insert("model_id".to_owned(), Value::Int(models[OBJ]));
        row.insert("group_id".to_owned(), Value::False);
        row.insert("operation".to_owned(), Value::str("r"));
        row.insert(
            "domain".to_owned(),
            Value::str(format!("[('val', '{op}', 3)]")),
        );
        row
    };
    let source = db::source(&h).await;
    let orm = db::orm(&h, &source, true).await;
    let shared_before = source.current().snapshot();

    let mut t1 = db::begin(&orm, 2).await;
    assert_eq!(
        vals(&mut t1).await,
        ids.objs,
        "every object before the write"
    );
    let sudo = t1.env().sudo(true);
    sudo.records("ir.access")
        .unwrap()
        .create_values(&mut t1, &[restriction(">")])
        .await
        .unwrap();
    assert_eq!(
        vals(&mut t1).await,
        [ids.objs[2]],
        "T1 sees its own restriction"
    );
    let private = t1.policy().snapshot();
    assert_eq!(private.origin(), SnapshotOrigin::Private);
    let memo = orm.search_memo_stats();
    let prepared = t1.prepared_statements();
    assert_eq!(vals(&mut t1).await, [ids.objs[2]]);
    assert_eq!(
        orm.search_memo_stats(),
        memo,
        "no shared memo entry read or written"
    );
    assert_eq!(
        t1.prepared_statements(),
        prepared,
        "statements sent unnamed"
    );

    // one statement, one transaction: one bump, as T1's
    let client = db::connect(&h).await;
    let theirs_id: i64 = client
        .query_one(
            "INSERT INTO ir_access (name, model_id, group_id, operation, domain, active) \
             VALUES ('val < 3', $1, NULL, 'r', '[(''val'', ''<'', 3)]', true) RETURNING id::int8",
            &[&models[OBJ]],
        )
        .await
        .unwrap()
        .get(0);

    let mut t3 = db::begin(&orm, 2).await;
    let shared = t3.policy().snapshot();
    assert_eq!(shared.origin(), SnapshotOrigin::Shared);
    assert_eq!(
        t3.policy().generation().policy(),
        t1.policy().generation().policy(),
        "the hazard: one generation, two snapshots"
    );
    assert_eq!(vals(&mut t3).await, [ids.objs[0]], "T3 sees T2's rule only");
    assert!(
        rule_ids(&t3)[0]
            .as_ref()
            .is_ok_and(|ids| ids.contains(&theirs_id)),
        "T3's read predicate names T2's rule: {:?}",
        rule_ids(&t3)
    );
    assert_eq!(vals(&mut t1).await, [ids.objs[2]], "T1 still on its own");
    t1.rollback().await.unwrap();

    let forced = source.refresh_shared().await.unwrap().snapshot();
    let first_static = StaticPolicy::AllowAll
        .into_policy_provider()
        .unwrap()
        .current()
        .snapshot();
    let second_static = StaticPolicy::AllowAll
        .into_policy_provider()
        .unwrap()
        .current()
        .snapshot();
    let all = [
        shared_before.id(),
        private.id(),
        shared.id(),
        forced.id(),
        first_static.id(),
        second_static.id(),
    ];
    let mut unique = all.to_vec();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(unique.len(), all.len(), "snapshot ids repeat: {all:?}");
    drop((t3, client, orm, source));
    h.cleanup().await.unwrap();
}
