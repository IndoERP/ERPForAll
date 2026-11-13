//! One validated company context in `Env` (
//! ): `env.companies()`, `env.company()`, `with_company`, the
//! resolver's default order, record rules and company-dependent cache keys that follow the
//! validated companies, and the refusals (also under `sudo()`).
//!
//! Two kinds of harness: the first decides with a `DomainPolicy` and principals built by the test
//! (companies are plain ids there); the second installs the real `Resolver` over `res.company`
//! rows, so the default order and the archived filter are read from the database.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

#[path = "support/currency_fixture.rs"]
mod currency_fixture;

use std::collections::BTreeMap;
use std::num::NonZeroU64;

use erp_orm::{
    BeginOptions, CompanyId, Context, GroupSet, Operation, Principal, PrincipalBuilder, Resolution,
    UserId,
};
use erp_registry::{FieldDecl, ModelDecl};
use erp_sdk::domain::PrincipalParam;
use erp_sdk::{Command, Domain, ErrorKind, Operator, RawVals, Transaction, Value};
use erp_security::resolver::Resolver;
use erp_sql as _;
use erp_test_models::{base_users, test_access, test_compute, test_orm};
use erp_testkit::{DomainPolicy, Harness, Otherwise, assert_query_count};

const OBJ: &str = "test_access.some_obj";
const DEMO: i64 = 2;
const UNAUTHORIZED: &str = "Access to unauthorized or invalid companies.";

fn company(id: i64) -> CompanyId {
    CompanyId::new(id).unwrap()
}

fn ids(list: &[i64]) -> Value {
    Value::list(list.iter().map(|i| Value::Int(*i)))
}

fn ctx(list: &[i64]) -> Context {
    Context::from_pairs([("allowed_company_ids", ids(list))])
}

#[track_caller]
fn assert_error(error: &erp_sdk::Error, kind: ErrorKind, message: &str) {
    assert_eq!(error.kind(), kind, "{error}");
    assert_eq!(error.message(), message);
}

#[track_caller]
fn assert_unauthorized(error: &erp_sdk::Error) {
    assert_error(error, ErrorKind::Access, UNAUTHORIZED);
}

fn raw(companies: &[CompanyId]) -> Vec<i64> {
    companies.iter().map(|c| c.get()).collect()
}

// ---------------------------------------------------------------------------------------------
// harness 1: principals built by the test
// ---------------------------------------------------------------------------------------------

/// `company_id in company_ids` on the fixture's objects, everything else allowed.
async fn policy_harness() -> Harness {
    let rule = Domain::condition(
        "company_id",
        Operator::In,
        erp_sdk::domain::Value::Param(PrincipalParam::CompanyIds),
    )
    .unwrap();
    let policy = DomainPolicy::builder(Otherwise::Allow)
        .rule(OBJ, Operation::Read, rule)
        .build()
        .unwrap();
    let h = Harness::builder([&base_users::ADDON, &test_access::ADDON])
        .policy(policy)
        .build()
        .await
        .expect("harness");
    let mut tx = h.begin().await.unwrap();
    tx.batch(
        "INSERT INTO res_users (id, name, login, active, share) VALUES (2, 'demo', 'demo', true, false), \
         (3, 'other', 'other', true, false); \
         SELECT setval(pg_get_serial_sequence('res_users', 'id'), 3)",
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    h
}

/// A user principal of `uid` whose companies are `subject_companies`, default context
/// `default_order` (a permutation of them).
fn principal(uid: i64, subject_companies: &[i64], default_order: &[i64]) -> Principal {
    let resolution = Resolution::in_id_order(
        GroupSet::default(),
        subject_companies.iter().map(|c| company(*c)),
        NonZeroU64::MIN,
    )
    .unwrap()
    .with_default_order(default_order.iter().map(|c| company(*c)).collect())
    .unwrap();
    PrincipalBuilder::user(UserId::new(uid).unwrap())
        .resolution(resolution)
        .companies(default_order.iter().map(|c| company(*c)).collect())
        .build()
        .unwrap()
}

async fn begin(h: &Harness, principal: Principal, context: Context) -> Transaction {
    h.orm()
        .begin_opts(BeginOptions::for_principal(principal).context(context))
        .await
        .unwrap()
}

const A: i64 = 1;
const B: i64 = 2;
const C: i64 = 3;

/// Review R7: the baseline rule. A principal built with its own companies keeps them when the
/// context has no `allowed_company_ids`; an empty list asks for the subject's default order
/// instead. The two differ exactly when the builder named other companies than the default.
#[tokio::test]
async fn absent_key_keeps_the_built_companies_and_empty_list_means_default() {
    let h = policy_harness().await;
    let resolution = Resolution::in_id_order(
        GroupSet::default(),
        [company(A), company(C)],
        NonZeroU64::MIN,
    )
    .unwrap();
    let built = PrincipalBuilder::user(UserId::new(DEMO).unwrap())
        .resolution(resolution)
        .companies(vec![company(C)])
        .build()
        .unwrap();
    let absent = begin(&h, built.clone(), Context::new()).await;
    assert_eq!(raw(absent.env().companies().unwrap()), [C]);
    drop(absent);
    let empty = begin(&h, built, ctx(&[])).await;
    assert_eq!(raw(empty.env().companies().unwrap()), [A, C]);
    drop(empty);
    h.cleanup().await.unwrap();
}

/// `with_company` moves the company to the front of the active companies and `0` changes
/// nothing; `allowed_company_ids=[]`, `False` and an absent key all give the subject's default
/// order, main company first; `[A]` restricts the companies to A and `with_company(C)` on it
/// adds C in front.
#[tokio::test]
async fn spec_env_007_with_company_reorders() {
    let h = policy_harness().await;
    let user = principal(DEMO, &[A, C], &[A, C]);
    let default: Vec<i64> = raw(user.resolution().default_companies());
    let tx = begin(&h, user.clone(), ctx(&[])).await;
    let env = tx.env();
    assert_eq!(raw(env.companies().unwrap()), default);
    assert_eq!(env.company_id().unwrap(), Some(default[0]));

    let u1 = env.with_company(A).unwrap();
    assert_eq!(raw(u1.companies().unwrap()), [A, C]);
    let u2 = u1.with_company(C).unwrap();
    assert_eq!(raw(u2.companies().unwrap()), [C, A]);
    assert_eq!(u2.company().unwrap(), Some(company(C)));
    assert_eq!(
        u2.context().get("allowed_company_ids"),
        Some(ids(&[C, A])),
        "the context records the new order"
    );
    let u3 = u2.with_company(A).unwrap();
    assert_eq!(raw(u3.companies().unwrap()), [A, C]);
    assert_eq!(u3.context().get("allowed_company_ids"), Some(ids(&[A, C])));

    // a falsy company keeps the very same environment and context
    let same = u2.with_company(0).unwrap();
    assert_eq!(same, u2);
    assert_eq!(same.context(), u2.context());

    // a list restricts the companies; with_company adds in front; again it reorders
    let only_a = env.with_context([("allowed_company_ids", ids(&[A]))]);
    assert_eq!(raw(only_a.companies().unwrap()), [A]);
    assert_eq!(
        raw(only_a.with_company(C).unwrap().companies().unwrap()),
        [C, A]
    );
    assert_eq!(
        raw(only_a
            .with_company(C)
            .unwrap()
            .with_company(A)
            .unwrap()
            .companies()
            .unwrap()),
        [A, C]
    );

    // `None` / `False` behaves like an absent key: back to the default order
    let falsy = only_a.with_context([("allowed_company_ids", Value::False)]);
    assert_eq!(raw(falsy.companies().unwrap()), default);
    let empty = only_a.with_context([("allowed_company_ids", ids(&[]))]);
    assert_eq!(raw(empty.companies().unwrap()), default);
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_env_008_company_environment() {
    let h = policy_harness().await;
    let user = principal(DEMO, &[A], &[A]);
    let bad_lists = [
        ids(&[A, B, C]),
        ids(&[B]),
        Value::list([Value::str("x")]),
        Value::str("1"),
        ids(&[i64::MAX]),
    ];
    // the error every use of a refused environment reports
    let expected = |env: &erp_orm::Env| match env.context().allowed_company_ids() {
        Err(malformed) => malformed,
        Ok(_) => erp_sdk::Error::access(UNAUTHORIZED),
    };
    for bad in bad_lists {
        let tx = begin(&h, user.clone(), Context::new()).await;
        for env in [
            tx.env()
                .with_context([("allowed_company_ids", bad.clone())]),
            tx.env()
                .with_context([("allowed_company_ids", bad.clone())])
                .sudo(true),
            tx.env()
                .sudo(true)
                .with_context([("allowed_company_ids", bad.clone())]),
        ] {
            let want = expected(&env);
            assert_error(&env.companies().unwrap_err(), want.kind(), want.message());
            assert_error(&env.company().unwrap_err(), want.kind(), want.message());
            assert_error(&env.company_id().unwrap_err(), want.kind(), want.message());
            assert_error(
                &env.with_company(A).unwrap_err(),
                want.kind(),
                want.message(),
            );
        }
        drop(tx);
        let mut tx = begin(&h, user.clone(), Context::new()).await;
        for env in [
            tx.env()
                .with_context([("allowed_company_ids", bad.clone())]),
            tx.env()
                .with_context([("allowed_company_ids", bad.clone())])
                .sudo(true),
        ] {
            let want = expected(&env);
            let err = env
                .records(OBJ)
                .unwrap()
                .search(&mut tx, &Domain::TRUE)
                .await
                .unwrap_err();
            assert_error(&err, want.kind(), want.message());
        }
        drop(tx);
    }
    let tx = begin(&h, user.clone(), Context::new()).await;
    let malformed = tx
        .env()
        .with_context([("allowed_company_ids", Value::str("1"))]);
    assert_eq!(
        malformed.companies().unwrap_err().kind(),
        ErrorKind::Validation
    );
    drop(tx);

    // without the key: the main company and all the subject's companies
    let tx = begin(&h, user.clone(), Context::new()).await;
    let default = raw(user.resolution().default_companies());
    assert_eq!(raw(tx.env().companies().unwrap()), default);
    assert_eq!(tx.env().company_id().unwrap(), Some(default[0]));
    assert_eq!(raw(tx.env().sudo(true).companies().unwrap()), default);
    drop(tx);

    // System takes the ids as given
    let tx = h.begin().await.unwrap();
    let env = tx
        .env()
        .with_context([("allowed_company_ids", ids(&[C, B, i64::MAX]))]);
    assert_eq!(raw(env.companies().unwrap()), [C, B, i64::MAX]);
    assert_eq!(env.company_id().unwrap(), Some(C));
    assert_eq!(tx.env().company().unwrap(), None, "no list: no company");
    assert!(tx.env().companies().unwrap().is_empty());
    drop(tx);
    h.cleanup().await.unwrap();
}

/// A company listed twice is a `Validation` error with the text the RPC layer gives for a call's
/// context, for users, under `sudo()` and for `System`; reported by the first use, not by
/// building the environment.
#[tokio::test]
async fn duplicate_company_in_context() {
    let h = policy_harness().await;
    let user = principal(DEMO, &[A, C], &[A, C]);
    let message = erp_orm::PrincipalError::DuplicateContextCompany { company: A }.to_string();
    assert_eq!(
        message,
        format!("context key 'allowed_company_ids': company {A} is listed twice")
    );
    let tx = begin(&h, user, Context::new()).await;
    let env = tx
        .env()
        .with_context([("allowed_company_ids", ids(&[A, A]))]);
    assert_error(
        &env.companies().unwrap_err(),
        ErrorKind::Validation,
        &message,
    );
    assert_error(
        &env.sudo(true).company().unwrap_err(),
        ErrorKind::Validation,
        &message,
    );
    drop(tx);
    let tx = h.begin().await.unwrap();
    let env = tx
        .env()
        .with_context([("allowed_company_ids", ids(&[C, A, C]))]);
    let message = erp_orm::PrincipalError::DuplicateContextCompany { company: C }.to_string();
    assert_error(
        &env.companies().unwrap_err(),
        ErrorKind::Validation,
        &message,
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn foreign_and_negative_with_company() {
    let h = policy_harness().await;
    let tx = begin(&h, principal(DEMO, &[A, C], &[A, C]), Context::new()).await;
    let env = tx.env().with_company(B).unwrap();
    assert_unauthorized(&env.companies().unwrap_err());
    assert_unauthorized(&env.company().unwrap_err());
    // moving a valid company in front of the refused list is refused with the same error
    assert_unauthorized(&env.with_company(A).unwrap_err());

    let err = tx.env().with_company(-1).unwrap_err();
    assert_error(
        &err,
        ErrorKind::Programming,
        "with_company: invalid company id -1",
    );
    let err = tx.env().with_company(i64::MIN).unwrap_err();
    assert_error(
        &err,
        ErrorKind::Programming,
        &format!("with_company: invalid company id {}", i64::MIN),
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Reading the companies costs no statement, however often.
#[tokio::test]
async fn company_reads_cost_nothing() {
    let h = policy_harness().await;
    let tx = begin(&h, principal(DEMO, &[A, C], &[A, C]), ctx(&[C, A])).await;
    let env = tx.env();
    let (companies, current) = assert_query_count!(tx, <= 0, {
        let moved = env.with_company(A).unwrap();
        (
            raw(moved.companies().unwrap()),
            (moved.company().unwrap(), moved.company_id().unwrap()),
        )
    });
    assert_eq!(companies, [A, C]);
    assert_eq!(current, (Some(company(A)), Some(A)));
    drop(tx);
    h.cleanup().await.unwrap();
}

/// the companies record rules see are the environment's validated companies, so
/// `with_company` and `with_context(allowed_company_ids=…)` change what a search finds. On the
/// code before this slice the principal kept its own companies and the restricted searches found
/// both records.
#[tokio::test]
async fn rules_follow_with_company() {
    let h = policy_harness().await;
    let mut tx = h.begin().await.unwrap();
    let companies = tx
        .env()
        .records("test_access.company")
        .unwrap()
        .create_values(&mut tx, &[named("a"), named("b"), named("c")])
        .await
        .unwrap()
        .ids();
    let (a, b, c) = (companies[0], companies[1], companies[2]);
    let objs = tx
        .env()
        .records(OBJ)
        .unwrap()
        .create_values(
            &mut tx,
            &[
                BTreeMap::from([("company_id".to_owned(), Value::Int(a))]),
                BTreeMap::from([("company_id".to_owned(), Value::Int(b))]),
                BTreeMap::from([("company_id".to_owned(), Value::Int(c))]),
            ],
        )
        .await
        .unwrap()
        .ids();
    tx.commit().await.unwrap();
    let (obj_a, obj_b, obj_c) = (objs[0], objs[1], objs[2]);

    // the subject is in a and c, never in b
    let mut tx = begin(&h, principal(DEMO, &[a, c], &[a, c]), ctx(&[a, c])).await;
    let base = tx.env();
    assert_eq!(visible(&mut tx, &base).await, [obj_a, obj_c]);
    let only_a = base.with_context([("allowed_company_ids", ids(&[a]))]);
    assert_eq!(visible(&mut tx, &only_a).await, [obj_a]);
    // with_company puts c in front of [a]: both are active again
    let c_then_a = only_a.with_company(c).unwrap();
    assert_eq!(visible(&mut tx, &c_then_a).await, [obj_a, obj_c]);
    // and a context list that drops a hides its record again
    let only_c = c_then_a.with_context([("allowed_company_ids", ids(&[c]))]);
    assert_eq!(visible(&mut tx, &only_c).await, [obj_c]);
    // b was never the subject's: its record is never found, and naming it is refused
    let named_b = base.with_company(b).unwrap();
    let err = named_b
        .records(OBJ)
        .unwrap()
        .search(&mut tx, &Domain::TRUE)
        .await
        .unwrap_err();
    assert_unauthorized(&err);
    assert!(!visible(&mut tx, &base).await.contains(&obj_b));
    drop(tx);
    h.cleanup().await.unwrap();
}

fn named(name: &str) -> BTreeMap<String, Value> {
    BTreeMap::from([("name".to_owned(), Value::str(name))])
}

async fn visible(tx: &mut Transaction, env: &erp_orm::Env) -> Vec<i64> {
    let mut found = env
        .records(OBJ)
        .unwrap()
        .search(tx, &Domain::TRUE)
        .await
        .unwrap()
        .ids();
    found.sort_unstable();
    found
}

// ---------------------------------------------------------------------------------------------
// harness 2: the real resolver over res.company rows
// ---------------------------------------------------------------------------------------------

fn company_columns() -> ModelDecl {
    ModelDecl::extension("test_access", "res.company", &["res.company"])
        .fields(const { &[FieldDecl::boolean("active"), FieldDecl::integer("sequence")] })
}

async fn resolver_harness() -> Harness {
    let addons = || {
        [
            &erp_addon_base::ADDON,
            &base_users::ADDON,
            &test_access::ADDON,
        ]
    };
    let registry = Harness::builder(addons())
        .with_decls([company_columns()])
        .registry_only()
        .unwrap();
    let resolver = Resolver::for_registry(&registry).unwrap();
    let policy = DomainPolicy::builder(Otherwise::Allow).build().unwrap();
    Harness::builder(addons())
        .with_decls([company_columns()])
        .policy(policy)
        // the cases switch from one user to another (ann to dora): the ORM names the
        // impersonation capability and this policy grants every capability (A4)
        .impersonate_capability(&erp_security::principal::PRINCIPAL_IMPERSONATE)
        .resolver(resolver)
        .build()
        .await
        .expect("harness")
}

/// Ids of the rows `seed` committed.
struct Seeded {
    /// Sorted by `(sequence, name, id)` among the active ones: `first_tie` and `second_tie` share
    /// sequence 10 and name "Alpha", `beta` has sequence 10, `main` sequence 20, `old` is
    /// archived with sequence 1.
    first_tie: i64,
    second_tie: i64,
    beta: i64,
    main: i64,
    old: i64,
    /// Main company `main`, member of all five.
    ann: i64,
    /// Main company `old` (archived), member of `old` and `beta`.
    dora: i64,
}

async fn seed(h: &Harness) -> Seeded {
    let mut tx = h.begin().await.unwrap();
    let currency = erp_testkit::fixtures::seed_currency_in(&mut tx)
        .await
        .unwrap();
    let mut make = async |name: &str, sequence: i32, active: bool| -> i64 {
        tx.query(
            "INSERT INTO res_company (name, sequence, active, currency_id) VALUES ($1, $2, $3, $4) \
             RETURNING id::int8",
            &[&name, &sequence, &active, &currency],
        )
        .await
        .unwrap()[0]
            .get(0)
    };
    let first_tie = make("Alpha", 10, true).await;
    let beta = make("Beta", 10, true).await;
    let second_tie = make("Alpha", 10, true).await;
    let main = make("Main", 20, true).await;
    let old = make("Old", 1, false).await;
    let mut user = async |login: &str, main_company: i64, member_of: &[i64]| -> i64 {
        let uid: i64 = tx
            .query(
                "INSERT INTO res_users (name, login, active, company_id, share) \
                 VALUES ($1, $1, true, $2, false) RETURNING id::int8",
                &[&login, &main_company],
            )
            .await
            .unwrap()[0]
            .get(0);
        for cid in member_of {
            tx.execute(
                "INSERT INTO res_company_users_rel (user_id, cid) VALUES ($1, $2)",
                &[&uid, cid],
            )
            .await
            .unwrap();
        }
        uid
    };
    let ann = user("ann", main, &[beta, old, main, second_tie, first_tie]).await;
    let dora = user("dora", old, &[beta, old]).await;
    tx.commit().await.unwrap();
    Seeded {
        first_tie,
        second_tie,
        beta,
        main,
        old,
        ann,
        dora,
    }
}

/// The same default serves a system-level `sudo()` of the user.
#[tokio::test]
async fn spec_env_008_companies_order() {
    let h = resolver_harness().await;
    let s = seed(&h).await;
    let tx = h.begin_as(s.ann).await.unwrap();
    let env = tx.env();
    assert_eq!(
        raw(env.companies().unwrap()),
        [s.main, s.first_tie, s.second_tie, s.beta]
    );
    assert_eq!(env.company_id().unwrap(), Some(s.main));
    assert_eq!(
        raw(env.principal().resolution().all_companies()),
        {
            let mut sorted = vec![s.main, s.first_tie, s.second_tie, s.beta];
            sorted.sort_unstable();
            sorted
        },
        "the archived company is not one of the subject's companies"
    );
    assert_eq!(
        raw(env.sudo(true).companies().unwrap()),
        raw(env.companies().unwrap())
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn archived_company_not_default() {
    let h = resolver_harness().await;
    let s = seed(&h).await;
    let tx = h.begin_as(s.ann).await.unwrap();
    let named_old = tx
        .env()
        .with_context([("allowed_company_ids", ids(&[s.old]))]);
    assert_unauthorized(&named_old.companies().unwrap_err());
    let with_old = tx.env().with_company(s.old).unwrap();
    assert_unauthorized(&with_old.company().unwrap_err());
    drop(tx);

    let tx = h.begin_as(s.dora).await.unwrap();
    let env = tx.env();
    assert_eq!(raw(env.companies().unwrap()), [s.old, s.beta]);
    assert_eq!(env.company_id().unwrap(), Some(s.old));
    let narrowed = env.with_context([("allowed_company_ids", ids(&[s.beta, s.old]))]);
    assert_eq!(raw(narrowed.companies().unwrap()), [s.beta, s.old]);
    drop(tx);
    h.cleanup().await.unwrap();
}

/// After the user loses a company in the transaction, an environment still holding it is refused
/// at its next decision (the engine re-reads the subject), with the same error as any
/// foreign company.
#[tokio::test]
async fn membership_change_invalidates_env() {
    let h = resolver_harness().await;
    let s = seed(&h).await;
    let mut tx = h.begin_as(s.ann).await.unwrap();
    let before = tx
        .env()
        .with_context([("allowed_company_ids", ids(&[s.main, s.beta]))]);
    assert_eq!(raw(before.companies().unwrap()), [s.main, s.beta]);
    before
        .records("test_access.company")
        .unwrap()
        .search(&mut tx, &Domain::TRUE)
        .await
        .unwrap();
    let rid = |id: i64| erp_sdk::RecId::from_real(id).unwrap();
    tx.env()
        .sudo(true)
        .records("res.users")
        .unwrap()
        .browse([s.ann])
        .unwrap()
        .write(
            &mut tx,
            RawVals::new().set_commands("company_ids", [Command::unlink(rid(s.beta))]),
        )
        .await
        .unwrap();
    let err = before
        .records("test_access.company")
        .unwrap()
        .search(&mut tx, &Domain::TRUE)
        .await
        .unwrap_err();
    assert_unauthorized(&err);
    drop(tx);
    h.cleanup().await.unwrap();
}

/// and sanitising for)
/// The company part of the cache key of a context-dependent field is the validated current
/// company: users whose current company is 0 (none), 1 and 2 each get their own key, and a
/// refused company list is an error rather than the "no company" key. Per-company storage of
/// company-dependent values (/085) is not on `main` yet, so the value half of the
/// case is not covered here.
#[tokio::test]
async fn spec_field_086_company_dependent_html() {
    let h = Harness::builder([&base_users::ADDON, &test_orm::ADDON, &test_compute::ADDON])
        .with_static_decls(crate::currency_fixture::decls())
        .build()
        .await
        .expect("harness");
    let tx = h.begin().await.unwrap();
    let key = |env: &erp_orm::Env| {
        tx.cache_key(env, "test_orm.ctx_key", "all")
            .map(|k| k[0].clone())
    };
    let none = tx.env();
    assert_eq!(key(&none).unwrap(), Value::False);
    for company in [1, 2] {
        let env = tx.env().with_company(company).unwrap();
        assert_eq!(key(&env).unwrap(), Value::Int(company));
    }
    // a user whose list holds a company outside its own is refused, never keyed as "no company"
    let user = principal(DEMO, &[A], &[A]);
    let user_tx = h
        .orm()
        .begin_opts(BeginOptions::for_principal(user).context(ctx(&[B])))
        .await
        .unwrap();
    let err = user_tx
        .cache_key(&user_tx.env(), "test_orm.ctx_key", "all")
        .unwrap_err();
    assert_unauthorized(&err);
    drop(user_tx);
    drop(tx);
    h.cleanup().await.unwrap();
}

/// with the registry's columns, the resolver's one statement reads the subject's
/// active companies (plus its main company, even archived) ordered by `sequence, name, id`, and
/// `finish` and the re-resolution of a transaction agree on the default context (main first).
#[tokio::test]
async fn resolver_default_order_and_active() {
    let h = resolver_harness().await;
    let s = seed(&h).await;
    let resolver = Resolver::for_registry(h.registry()).unwrap();
    let mut tx = h.begin().await.unwrap();
    let policy = std::sync::Arc::clone(tx.policy());
    let hint = erp_orm::PrincipalHint::user(UserId::new(s.ann).unwrap());
    let statement = erp_orm::PrincipalResolver::statement(&resolver, &hint).unwrap();
    assert!(
        statement
            .sql()
            .contains("ORDER BY rc.sequence, rc.name, rc.id)"),
        "{}",
        statement.sql()
    );
    assert!(
        statement
            .sql()
            .contains("rc.active IS TRUE OR rc.id = u.company_id"),
        "{}",
        statement.sql()
    );
    let [erp_sql::param::Param::Scalar(erp_sql::param::Scalar::Int8(id))] = statement.params()
    else {
        panic!("one int8 parameter: {:?}", statement.params());
    };
    let rows = assert_query_count!(tx, >= 1, <= 1, {
        tx.query(statement.sql(), &[id]).await.unwrap()
    });
    let principal =
        erp_orm::PrincipalResolver::finish(&resolver, &hint, &rows, policy.as_ref()).unwrap();
    let expected = [s.main, s.first_tie, s.second_tie, s.beta];
    assert_eq!(raw(principal.companies().allowed()), expected);
    assert_eq!(raw(principal.resolution().default_companies()), expected);

    let resolutions = resolver
        .resolve_subjects(
            &mut tx,
            &[UserId::new(s.ann).unwrap()],
            policy.as_ref(),
            NonZeroU64::MIN,
        )
        .await
        .unwrap();
    assert_eq!(raw(resolutions[0].default_companies()), expected);

    // an archived main company stays, first, and the active others follow in order
    tx.execute(
        "UPDATE res_users SET company_id = $1 WHERE id = $2",
        &[&s.old, &s.ann],
    )
    .await
    .unwrap();
    let rows = tx.query(statement.sql(), &[id]).await.unwrap();
    let principal =
        erp_orm::PrincipalResolver::finish(&resolver, &hint, &rows, policy.as_ref()).unwrap();
    assert_eq!(
        raw(principal.companies().allowed()),
        [s.old, s.first_tie, s.second_tie, s.beta, s.main]
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// A `res.company` column of the wrong type is refused when the resolver is built, naming it.
#[tokio::test]
async fn resolver_refuses_a_company_column_of_the_wrong_type() {
    let wrong = ModelDecl::extension("test_access", "res.company", &["res.company"])
        .fields(const { &[FieldDecl::char("sequence")] });
    let registry = Harness::builder([
        &erp_addon_base::ADDON,
        &base_users::ADDON,
        &test_access::ADDON,
    ])
    .with_decls([wrong])
    .registry_only()
    .unwrap();
    let err = Resolver::for_registry(&registry).unwrap_err();
    assert_error(
        &err,
        ErrorKind::Programming,
        "res.company.sequence must be a stored integer for the principal resolver, found Char { size: None } (store = true)",
    );
}

// ---------------------------------------------------------------------------------------------
// review S1, S2, S4: the validated company read
// ---------------------------------------------------------------------------------------------

/// Review S1: an environment from `with_user` is bounded by that subject's companies, under
/// `sudo()` too, at the transaction-level read the decisions use (`Transaction::companies`) and at
/// a site that used `env.company_id()` (the sequence draw). Fails on the old code: the unresolved
/// principal accepted any list and the draw ran as if the company were valid.
#[tokio::test]
async fn with_user_and_sudo_are_bounded_by_the_subjects_companies() {
    let h = resolver_harness().await;
    let s = seed(&h).await;
    let mut tx = h.begin_as(s.ann).await.unwrap();
    // dora is in old and beta; main is foreign to her
    let foreign = tx
        .env()
        .with_user(s.dora)
        .with_context([("allowed_company_ids", ids(&[s.main]))]);
    for env in [
        foreign.clone(),
        foreign.sudo(true),
        tx.env()
            .sudo(true)
            .with_user(s.dora)
            .with_context([("allowed_company_ids", ids(&[s.main]))]),
    ] {
        assert_unauthorized(&tx.companies(&env).await.unwrap_err());
        let err = erp_orm::sequence::next_by_code(&mut tx, &env, "no.such.code", None)
            .await
            .unwrap_err();
        assert_unauthorized(&err);
    }
    // her own companies pass, with the current one first
    let own = tx
        .env()
        .with_user(s.dora)
        .sudo(true)
        .with_context([("allowed_company_ids", ids(&[s.beta, s.old]))]);
    assert_eq!(raw(&tx.companies(&own).await.unwrap()), [s.beta, s.old]);
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Review R2 (c): an environment from `with_user` that names no companies acts with the
/// subject's default companies (main company first), not with none and not with the actor's.
/// Dora's main company is archived but stays first, Ann's default order is the one of
/// `spec_env_008_companies_order`.
#[tokio::test]
async fn with_user_without_the_key_gets_the_subjects_default_companies() {
    let h = resolver_harness().await;
    let s = seed(&h).await;
    let mut tx = h.begin_as(s.ann).await.unwrap();
    let dora = tx.env().with_user(s.dora);
    assert_eq!(raw(&tx.companies(&dora).await.unwrap()), [s.old, s.beta]);
    assert_eq!(
        raw(&tx.companies(&dora.sudo(true)).await.unwrap()),
        [s.old, s.beta]
    );
    drop(tx);
    let mut tx = h.begin_as(s.dora).await.unwrap();
    let ann = tx.env().with_user(s.ann);
    assert_eq!(
        raw(&tx.companies(&ann).await.unwrap()),
        [s.main, s.first_tie, s.second_tie, s.beta]
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Before the
/// fix the search bound `[]` (no rows) and `company_id` failed with "no current company".
#[tokio::test]
async fn sudo_with_user_binds_the_subjects_default_companies_in_a_search() {
    let h = resolver_harness().await;
    let s = seed(&h).await;
    let mut tx = h.begin_as(s.ann).await.unwrap();
    for (company, name) in [(s.old, "old"), (s.beta, "beta"), (s.main, "main")] {
        tx.execute(
            "INSERT INTO test_access_company (id, name) VALUES ($1, $2)",
            &[&company, &name],
        )
        .await
        .unwrap();
        tx.execute(
            "INSERT INTO test_access_some_obj (name, company_id) VALUES ($1, $2)",
            &[&name, &company],
        )
        .await
        .unwrap();
    }
    let env = tx.env().with_user(s.dora).sudo(true);
    let objs = env.records(OBJ).unwrap();
    let names = async |tx: &mut Transaction, domain: Domain| -> Vec<String> {
        let found = objs.search(tx, &domain).await.unwrap();
        let mut names: Vec<String> = found
            .cells(tx, "name")
            .await
            .unwrap()
            .into_iter()
            .map(|c| format!("{c:?}"))
            .collect();
        names.sort();
        names
    };
    let in_companies = Domain::condition(
        "company_id",
        Operator::In,
        erp_sdk::domain::Value::Param(PrincipalParam::CompanyIds),
    )
    .unwrap();
    // dora's default companies are old and beta: main is not hers
    let got = names(&mut tx, in_companies).await;
    assert_eq!(got.len(), 2, "{got:?}");
    assert!(got[0].contains("beta") && got[1].contains("old"), "{got:?}");
    let current = Domain::condition(
        "company_id",
        Operator::Eq,
        erp_sdk::domain::Value::Param(PrincipalParam::CompanyId),
    )
    .unwrap();
    let got = names(&mut tx, current).await;
    assert_eq!(got.len(), 1, "{got:?}");
    assert!(got[0].contains("old"), "the first default company: {got:?}");
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Review S4: a membership change made in the transaction is seen by the elevated read too: the
/// environment that still names the company is refused after the user lost it. Fails on the old
/// code (the read did not consult the newest resolution under `sudo()`).
#[tokio::test]
async fn a_membership_change_is_seen_under_sudo() {
    let h = resolver_harness().await;
    let s = seed(&h).await;
    let mut tx = h.begin_as(s.ann).await.unwrap();
    let before = tx
        .env()
        .with_context([("allowed_company_ids", ids(&[s.main, s.beta]))]);
    assert_eq!(
        raw(&tx.companies(&before.sudo(true)).await.unwrap()),
        [s.main, s.beta]
    );
    let rid = |id: i64| erp_sdk::RecId::from_real(id).unwrap();
    tx.env()
        .sudo(true)
        .records("res.users")
        .unwrap()
        .browse([s.ann])
        .unwrap()
        .write(
            &mut tx,
            RawVals::new().set_commands("company_ids", [Command::unlink(rid(s.beta))]),
        )
        .await
        .unwrap();
    assert_unauthorized(&tx.companies(&before.sudo(true)).await.unwrap_err());
    assert_unauthorized(&tx.companies(&before).await.unwrap_err());
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Review S2: `with_user(SUPERUSER_ID)` from a user keeps the user's companies as its bound; only
/// the `System` principal the core created takes any list. Fails on the old code (the foreign
/// company was accepted). Since A4 the switch itself is refused: the
/// environment keeps the user's own principal, so its companies are the user's.
#[tokio::test]
async fn with_user_superuser_keeps_the_actors_companies() {
    let h = policy_harness().await;
    let tx = begin(&h, principal(DEMO, &[A, C], &[A, C]), Context::new()).await;
    let su = tx.env().with_user(erp_orm::SUPERUSER_ID.get());
    // refused, never superuser mode (A4)
    assert!(!su.su());
    assert_eq!(su.uid(), DEMO);
    let foreign = su.with_context([("allowed_company_ids", ids(&[B]))]);
    assert_unauthorized(&foreign.companies().unwrap_err());
    assert_unauthorized(&su.with_company(B).unwrap().company().unwrap_err());
    // its actor's companies pass
    let own = su.with_context([("allowed_company_ids", ids(&[C, A]))]);
    assert_eq!(raw(own.companies().unwrap()), [C, A]);
    drop(tx);
    // the core's own System principal still takes the ids as given
    let tx = h.begin().await.unwrap();
    let env = tx.env().with_context([("allowed_company_ids", ids(&[B]))]);
    assert_eq!(raw(env.companies().unwrap()), [B]);
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Review F1/S5: a refused company context is reported by the boolean and filtering forms of the
/// access API too, never read as "no access". Fails on the old code (`Ok(false)`, an empty set,
/// read-only fields).
#[tokio::test]
async fn refused_companies_are_not_read_as_no_access() {
    let h = policy_harness().await;
    let mut tx = begin(&h, principal(DEMO, &[A, C], &[A, C]), Context::new()).await;
    let refused = tx.env().with_company(B).unwrap();
    let objs = refused.records(OBJ).unwrap();
    assert_unauthorized(&objs.has_access(&mut tx, Operation::Read).await.unwrap_err());
    assert_unauthorized(
        &objs
            .filtered_access(&mut tx, Operation::Read)
            .await
            .unwrap_err(),
    );
    assert_unauthorized(&objs.fields_get(&mut tx, None, None).await.unwrap_err());
    // the same environment without the foreign company is answered normally
    let fine = tx.env().with_company(A).unwrap().records(OBJ).unwrap();
    assert!(fine.has_access(&mut tx, Operation::Read).await.unwrap());
    drop(tx);
    h.cleanup().await.unwrap();
}
