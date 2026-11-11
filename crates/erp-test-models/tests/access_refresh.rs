//! Policy and membership changes inside a transaction (design note §3.4)
//! and principal resolution (§3.2), with a test policy provider and a test resolver standing in
//! for `erp-security`'s `PolicySource` and resolver: the engine flushes buffered
//! writes before it rebuilds or re-resolves, rebuilds privately, re-resolves every bound subject
//! in one statement and decides with the newest resolution for older `Env` clones too.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use std::num::NonZeroU64;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};

use erp_orm::{
    AccessPolicy, BeginOptions, BoxFuture, CompanyId, GroupId, Operation, PolicyProvider,
    PolicySourceWrite, Principal, PrincipalBuilder, PrincipalHint, PrincipalResolver, Resolution,
    ResolverStatement, SourceKind, UserId,
};
use erp_sdk::{Domain, ErrorKind, Operator, Records, Transaction, Value};
use erp_sql::param::{Param, Scalar, ScalarKind};
use erp_test_models::{base_users, test_access};
use erp_testkit::tokio_postgres::{self, Row};
use erp_testkit::{DomainPolicy, Harness, Otherwise};

const OBJ: &str = "test_access.some_obj";
const DEMO: i64 = 2;
const OTHER: i64 = 3;
/// Marks the resolver's statements in captures.
const RESOLVER_MARK: &str = "/* test resolver */";

/// `{name: value}`.
fn one(name: &str, value: Value) -> std::collections::BTreeMap<String, Value> {
    [(name.to_owned(), value)].into_iter().collect()
}

fn group(id: i64) -> GroupId {
    GroupId::new(id).unwrap()
}

fn companies() -> Vec<CompanyId> {
    (1..=3).map(|c| CompanyId::new(c).unwrap()).collect()
}

/// Reads `res_users.access_group_id` as the subject's one group.
#[derive(Default, Clone)]
struct TestResolver {
    re_resolutions: Arc<AtomicUsize>,
}

fn group_of(row: &Row) -> Vec<GroupId> {
    row.get::<_, Option<i64>>(1)
        .map(|g| vec![group(g)])
        .unwrap_or_default()
}

impl PrincipalResolver for TestResolver {
    fn statement(&self, hint: &PrincipalHint) -> erp_sdk::Result<ResolverStatement> {
        Ok(ResolverStatement::new(
            format!(
                "SELECT id, access_group_id FROM res_users WHERE id = $1::int8 {RESOLVER_MARK}"
            ),
            vec![Param::Scalar(Scalar::Int8(hint.subject().get()))],
        ))
    }

    fn finish(
        &self,
        hint: &PrincipalHint,
        rows: &[Row],
        policy: &dyn AccessPolicy,
    ) -> erp_sdk::Result<Principal> {
        let row = rows.first().ok_or_else(|| {
            erp_sdk::Error::access(format!("User {} does not exist", hint.subject()))
        })?;
        let groups = policy.groups(&group_of(row))?;
        let resolution = Resolution::in_id_order(groups, companies(), NonZeroU64::MIN)?;
        Ok(PrincipalBuilder::user(hint.subject())
            .resolution(resolution)
            .companies(vec![CompanyId::new(1).unwrap()])
            .build()?)
    }

    fn re_resolve<'a>(
        &'a self,
        tx: &'a mut Transaction,
        subjects: &'a [UserId],
    ) -> BoxFuture<'a, erp_sdk::Result<Vec<Resolution>>> {
        Box::pin(async move {
            self.re_resolutions.fetch_add(1, Ordering::SeqCst);
            let ids: Vec<i64> = subjects.iter().map(|s| s.get()).collect();
            let rows = tx
                .query_typed(
                    &format!(
                        "SELECT id, access_group_id FROM res_users WHERE id = ANY($1::int8[]) \
                         {RESOLVER_MARK}"
                    ),
                    &[(&ids, tokio_postgres::types::Type::INT8_ARRAY)],
                )
                .await?;
            subjects
                .iter()
                .map(|s| {
                    let row = rows
                        .iter()
                        .find(|r| r.get::<_, i64>(0) == s.get())
                        .ok_or_else(|| {
                            erp_sdk::Error::access(format!("User {s} does not exist"))
                        })?;
                    Ok(Resolution::in_id_order(
                        erp_orm::GroupSet::new(group_of(row)),
                        companies(),
                        NonZeroU64::MIN,
                    )?)
                })
                .collect()
        })
    }
}

/// The objects' read predicate: members of group 1 read everything, of group 2 `val > 3`;
/// privately rebuilt from `test_access_rule_row`: members of group 1 read `val > max(val_min)`.
#[derive(Clone)]
struct TestProvider {
    shared: Arc<dyn AccessPolicy>,
    rebuilds: Arc<AtomicUsize>,
    seen_min: Arc<AtomicI64>,
}

fn policy(group1: Domain) -> DomainPolicy {
    DomainPolicy::builder(Otherwise::Allow)
        .group_rule(OBJ, Operation::Read, group(1), group1)
        .group_rule(
            OBJ,
            Operation::Read,
            group(2),
            Domain::condition("val", Operator::Gt, 3).unwrap(),
        )
        .needs_resolved_principals()
        .build()
        .unwrap()
}

impl PolicyProvider for TestProvider {
    fn current(&self) -> Arc<dyn AccessPolicy> {
        Arc::clone(&self.shared)
    }

    fn rebuild_private<'a>(
        &'a self,
        tx: &'a mut Transaction,
    ) -> BoxFuture<'a, erp_sdk::Result<Arc<dyn AccessPolicy>>> {
        Box::pin(async move {
            self.rebuilds.fetch_add(1, Ordering::SeqCst);
            let rows = tx
                .query(
                    "SELECT coalesce(max(val_min), 0)::int8 FROM test_access_rule_row",
                    &[],
                )
                .await?;
            let min: i64 = rows[0].get(0);
            self.seen_min.store(min, Ordering::SeqCst);
            let private = DomainPolicy::builder(Otherwise::Allow)
                .group_rule(
                    OBJ,
                    Operation::Read,
                    group(1),
                    Domain::condition("val", Operator::Gt, min).unwrap(),
                )
                .needs_resolved_principals()
                .private()
                .build()?;
            Ok(Arc::new(private) as Arc<dyn AccessPolicy>)
        })
    }
}

static RULE_ROWS: PolicySourceWrite =
    PolicySourceWrite::new("test_access.rule_row", SourceKind::Policy);
static USERS: PolicySourceWrite =
    PolicySourceWrite::new("res.users", SourceKind::Membership).fields(&["access_group_id"]);

struct Fixture {
    h: Harness,
    resolver: TestResolver,
    provider: TestProvider,
    objs: Vec<i64>,
    rule_row: i64,
}

async fn fixture() -> Fixture {
    let resolver = TestResolver::default();
    let provider = TestProvider {
        shared: Arc::new(policy(Domain::TRUE)),
        rebuilds: Arc::new(AtomicUsize::new(0)),
        seen_min: Arc::new(AtomicI64::new(-1)),
    };
    let h = Harness::builder([&base_users::ADDON, &test_access::ADDON])
        .policy(Arc::new(provider.clone()) as Arc<dyn PolicyProvider>)
        .resolver(resolver.clone())
        .policy_sources([&RULE_ROWS, &USERS])
        .build()
        .await
        .expect("harness");
    let mut tx = h.begin().await.unwrap();
    tx.batch(
        "INSERT INTO test_access_group (id, name) VALUES (1, 'g1'), (2, 'g2'); \
         INSERT INTO res_users (id, name, login, active, access_group_id) VALUES \
         (2, 'demo', 'demo', true, 1), (3, 'other', 'other', true, 2); \
         SELECT setval(pg_get_serial_sequence('res_users', 'id'), 3)",
    )
    .await
    .unwrap();
    let objs = tx
        .env()
        .records(OBJ)
        .unwrap()
        .create_values(
            &mut tx,
            &(1..=6)
                .map(|v| [("val".to_owned(), Value::Int(v))].into_iter().collect())
                .collect::<Vec<_>>(),
        )
        .await
        .unwrap()
        .ids();
    let rule_row = tx
        .env()
        .records("test_access.rule_row")
        .unwrap()
        .create_values(
            &mut tx,
            &[[("val_min".to_owned(), Value::Int(0))]
                .into_iter()
                .collect()],
        )
        .await
        .unwrap()
        .ids()[0];
    tx.commit().await.unwrap();
    Fixture {
        h,
        resolver,
        provider,
        objs,
        rule_row,
    }
}

async fn visible(tx: &mut Transaction, records: &Records) -> Vec<i64> {
    records.search(tx, &Domain::TRUE).await.unwrap().ids()
}

fn index_of(captured: &[String], needle: &str) -> usize {
    captured
        .iter()
        .position(|s| s.contains(needle))
        .unwrap_or_else(|| panic!("no statement with {needle:?} in {captured:#?}"))
}

/// with a test resolver; the `res.users`/`res.groups` sources and their resolver are
/// 's)
#[tokio::test]
async fn a_buffered_membership_change_is_flushed_before_re_resolution() {
    let f = fixture().await;
    // resolved at the start: one statement after BEGIN
    let mut tx =
        f.h.orm()
            .begin_opts(BeginOptions::new(UserId::new(DEMO).unwrap()))
            .await
            .unwrap();
    assert_eq!(tx.query_count(), 1, "the resolver's statement");
    let env = tx.env();
    let objs = env.records(OBJ).unwrap();
    assert_eq!(visible(&mut tx, &objs).await, f.objs);
    // an older clone of the environment, kept across the change
    let older = objs.clone();
    // the user moves from group 1 to group 2: a buffered write, no flush
    let me = env
        .records("res.users")
        .unwrap()
        .browse([DEMO])
        .unwrap()
        .sudo(true);
    me.write_values(&mut tx, &one("access_group_id", Value::Int(2)))
        .await
        .unwrap();
    tx.capture_statements(true);
    assert_eq!(visible(&mut tx, &older).await, f.objs[3..]);
    let captured = tx.captured().to_vec();
    tx.capture_statements(false);
    let update = index_of(&captured, "UPDATE \"res_users\"");
    let resolve = index_of(&captured, RESOLVER_MARK);
    let search = index_of(&captured, "FROM \"test_access_some_obj\"");
    assert!(
        update < resolve && resolve < search,
        "flush, then re-resolution, then the search: {captured:#?}"
    );
    assert_eq!(f.resolver.re_resolutions.load(Ordering::SeqCst), 1);
    // no further re-resolution once current
    assert_eq!(visible(&mut tx, &objs).await, f.objs[3..]);
    assert_eq!(f.resolver.re_resolutions.load(Ordering::SeqCst), 1);
    drop(tx);
    f.h.cleanup().await.unwrap();
}

/// write, with a test provider; the `ir.access` source and its rebuild are's)
#[tokio::test]
async fn a_buffered_policy_write_is_flushed_before_a_private_rebuild() {
    let f = fixture().await;
    let mut tx =
        f.h.orm()
            .begin_opts(BeginOptions::new(UserId::new(DEMO).unwrap()))
            .await
            .unwrap();
    let objs = tx.env().records(OBJ).unwrap();
    assert_eq!(visible(&mut tx, &objs).await, f.objs);
    let stats = f.h.orm().search_memo_stats();
    // a buffered write of the policy source
    let row = tx
        .env()
        .records("test_access.rule_row")
        .unwrap()
        .browse([f.rule_row])
        .unwrap()
        .sudo(true);
    row.write_values(&mut tx, &one("val_min", Value::Int(4)))
        .await
        .unwrap();
    assert_eq!(visible(&mut tx, &objs).await, f.objs[4..]);
    assert_eq!(f.provider.rebuilds.load(Ordering::SeqCst), 1);
    assert_eq!(
        f.provider.seen_min.load(Ordering::SeqCst),
        4,
        "the rebuild read the flushed row"
    );
    // the private snapshot's texts never reach the shared memo
    assert_eq!(f.h.orm().search_memo_stats().entries, stats.entries);
    assert_eq!(visible(&mut tx, &objs).await, f.objs[4..]);
    assert_eq!(f.provider.rebuilds.load(Ordering::SeqCst), 1, "built once");
    tx.rollback().await.unwrap();
    // the next transaction decides with the shared snapshot again
    let mut tx =
        f.h.orm()
            .begin_opts(BeginOptions::new(UserId::new(DEMO).unwrap()))
            .await
            .unwrap();
    let objs = tx.env().records(OBJ).unwrap();
    assert_eq!(visible(&mut tx, &objs).await, f.objs);
    drop(tx);
    f.h.cleanup().await.unwrap();
}

/// A savepoint rolled back after a private rebuild: the next decision rebuilds again (never a
/// decision on the undone change, design note §3.4).
#[tokio::test]
async fn a_savepoint_rollback_rebuilds_again() {
    let f = fixture().await;
    let mut tx =
        f.h.orm()
            .begin_opts(BeginOptions::new(UserId::new(DEMO).unwrap()))
            .await
            .unwrap();
    let objs = tx.env().records(OBJ).unwrap();
    let rule_row = f.rule_row;
    let inside = tx
        .savepoint(async |tx: &mut Transaction| -> erp_sdk::Result<Vec<i64>> {
            let row = tx
                .env()
                .records("test_access.rule_row")?
                .browse([rule_row])?
                .sudo(true);
            row.write_values(tx, &one("val_min", Value::Int(5))).await?;
            let found = tx
                .env()
                .records(OBJ)?
                .search(tx, &Domain::TRUE)
                .await?
                .ids();
            Err(erp_sdk::Error::validation(format!("undo {found:?}")))
        })
        .await
        .unwrap_err();
    assert_eq!(inside.message(), format!("undo {:?}", &f.objs[5..]));
    assert_eq!(visible(&mut tx, &objs).await, f.objs);
    assert_eq!(f.provider.rebuilds.load(Ordering::SeqCst), 2);
    assert_eq!(f.provider.seen_min.load(Ordering::SeqCst), 0);
    drop(tx);
    f.h.cleanup().await.unwrap();
}

/// An environment of another user (`with_user`) under a policy that decides on groups: resolved
/// at its first use, one statement.
#[tokio::test]
async fn another_users_environment_is_resolved_at_first_use() {
    let f = fixture().await;
    let mut tx = f.h.begin().await.unwrap();
    let as_other = tx.env().records(OBJ).unwrap().with_user(OTHER);
    let before = f.resolver.re_resolutions.load(Ordering::SeqCst);
    assert_eq!(visible(&mut tx, &as_other).await, f.objs[3..]);
    assert_eq!(f.resolver.re_resolutions.load(Ordering::SeqCst), before + 1);
    assert_eq!(visible(&mut tx, &as_other).await, f.objs[3..]);
    assert_eq!(f.resolver.re_resolutions.load(Ordering::SeqCst), before + 1);
    drop(tx);
    f.h.cleanup().await.unwrap();
}

/// A principal no resolver read is refused under a policy that decides on groups (BASE-0
/// follow-up): never decided with an empty group set.
#[tokio::test]
async fn an_unresolved_principal_is_refused_under_a_compiled_policy() {
    let h = Harness::builder([&base_users::ADDON, &test_access::ADDON])
        .policy(policy(Domain::TRUE))
        .build()
        .await
        .unwrap();
    let mut tx = h.begin_as(DEMO).await.unwrap();
    let err = tx
        .env()
        .records(OBJ)
        .unwrap()
        .search(&mut tx, &Domain::TRUE)
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "the user principal of user 2 was never resolved against the database: the installed \
         access policy decides on the subject's groups and companies, so the principal must come \
         from the policy's resolver"
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Snapshot identity (Revision 3 M1): every policy installation is its own snapshot, and a
/// private rebuild has its own id and origin.
#[tokio::test]
async fn snapshot_ids_are_unique_across_shared_and_private_builds() {
    let a = policy(Domain::TRUE);
    let b = policy(Domain::TRUE);
    let private = DomainPolicy::builder(Otherwise::Allow)
        .private()
        .build()
        .unwrap();
    let ids = [a.snapshot(), b.snapshot(), private.snapshot()];
    assert_ne!(ids[0].id(), ids[1].id());
    assert_ne!(ids[1].id(), ids[2].id());
    assert!(ids[0].is_shared() && !ids[2].is_shared());
}

/// `ScalarKind` is what the resolver's statement binds (kept here so the test resolver's
/// parameter types match the engine's).
#[test]
fn resolver_statements_bind_typed_parameters() {
    let statement = ResolverStatement::new("SELECT 1", vec![Param::Scalar(Scalar::Int8(2))]);
    assert_eq!(statement.params()[0].sql_type(), ScalarKind::Int8.sql());
}

/// A read served from the cache by a superuser (inside a compute or a constraint, say) starts no
/// flush or re-resolution while a membership change is pending (review S1): it needs
/// no decision.
#[tokio::test]
async fn a_superuser_cache_read_does_not_refresh_the_policy() {
    let f = fixture().await;
    let mut tx =
        f.h.orm()
            .begin_opts(BeginOptions::new(UserId::new(DEMO).unwrap()))
            .await
            .unwrap();
    let users = tx.env().records("res.users").unwrap().sudo(true);
    let me = users.browse([DEMO]).unwrap();
    assert_eq!(me.value(&mut tx, "name").await.unwrap(), Value::str("demo"));
    me.write_values(&mut tx, &one("access_group_id", Value::Int(2)))
        .await
        .unwrap();
    let resolutions = f.resolver.re_resolutions.load(Ordering::SeqCst);
    tx.capture_statements(true);
    assert_eq!(me.value(&mut tx, "name").await.unwrap(), Value::str("demo"));
    let captured = tx.captured().to_vec();
    tx.capture_statements(false);
    assert!(
        captured.is_empty(),
        "a cached superuser read runs no statement: {captured:#?}"
    );
    assert_eq!(
        f.resolver.re_resolutions.load(Ordering::SeqCst),
        resolutions,
        "and re-resolves nobody"
    );
    drop(tx);
    f.h.cleanup().await.unwrap();
}

/// A cached read under `with_user` resolves the subject the same way whether or not a change is
/// pending: it used to fail with the unresolved-principal error when nothing was pending (S1).
#[tokio::test]
async fn a_cached_read_of_another_user_resolves_the_subject() {
    let f = fixture().await;
    let mut tx = f.h.begin().await.unwrap();
    let objs = tx.env().records(OBJ).unwrap().browse([f.objs[4]]).unwrap();
    assert_eq!(objs.value(&mut tx, "val").await.unwrap(), Value::Int(5));
    let before = f.resolver.re_resolutions.load(Ordering::SeqCst);
    let as_other = objs.with_user(OTHER);
    assert_eq!(as_other.value(&mut tx, "val").await.unwrap(), Value::Int(5));
    assert_eq!(f.resolver.re_resolutions.load(Ordering::SeqCst), before + 1);
    drop(tx);
    f.h.cleanup().await.unwrap();
}

/// SM-1 of the merge review: the remembered visibility of an x2many's targets is not
/// served before the policy is brought up to date. The user reads a container's objects (decided
/// and remembered), then loses the group that let them through in the same transaction (a
/// buffered membership write): the next read of the same field hides the objects the new group
/// does not allow, with the same older environment, warm and cold.
#[tokio::test]
async fn a_membership_change_ends_the_remembered_x2many_visibility() {
    let f = fixture().await;
    let mut tx =
        f.h.orm()
            .begin_opts(BeginOptions::new(UserId::new(DEMO).unwrap()))
            .await
            .unwrap();
    let env = tx.env();
    let ids = Value::list(f.objs.iter().map(|id| Value::Int(*id)));
    let container = env
        .records("test_access.container")
        .unwrap()
        .sudo(true)
        .create_values(
            &mut tx,
            &[[("some_ids".to_owned(), ids)].into_iter().collect()],
        )
        .await
        .unwrap();
    let mine = container.sudo(false);
    // cold, then warm: the group 1 rule lets every object through
    for _ in 0..2 {
        assert_eq!(
            mine.related(&mut tx, "some_ids").await.unwrap().ids(),
            f.objs
        );
    }
    let me = env
        .records("res.users")
        .unwrap()
        .browse([DEMO])
        .unwrap()
        .sudo(true);
    me.write_values(&mut tx, &one("access_group_id", Value::Int(2)))
        .await
        .unwrap();
    // group 2 allows `val > 3` only
    assert_eq!(
        mine.related(&mut tx, "some_ids").await.unwrap().ids(),
        f.objs[3..]
    );
    drop(tx);
    f.h.cleanup().await.unwrap();
}
