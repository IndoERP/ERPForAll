//! the principal resolver of `erp-security`, `base`'s registrations, the engine's
//! `ensure_policy_current` (design note §3.4, Revision 3 M2): a buffered group removal, company
//! removal or restriction is in force at the next decision of the same transaction, with no
//! explicit flush, and the rebuild or re-resolution reads the rows only after their writes ran.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod support;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use erp_domain::Domain;
use erp_fields::{ErrorKind, RecId, Value};
use erp_orm::{
    AccessPolicy, BeginOptions, BoxFuture, Command, CompanyId, GroupId, GroupSet, Orm,
    PolicyProvider, Principal, PrincipalBuilder, PrincipalHint, PrincipalResolver, RawVals,
    Resolution, ResolverStatement, Transaction, UserId,
};
use erp_security::loader::STATEMENTS;
use erp_security::resolver::{Resolver, SUBJECTS_SQL};
use erp_security::source::PolicySource;
use erp_testkit::Harness;
use erp_testkit::tokio_postgres::{Client, Row};
use support::{connect, harness, pool};

/// What `seed` committed.
struct Seeded {
    user: i64,
    secret_readers: i64,
    public: i64,
    secret: i64,
    company_a: i64,
    company_b: i64,
}

async fn one(
    client: &Client,
    sql: &str,
    params: &[&(dyn erp_testkit::tokio_postgres::types::ToSql + Sync)],
) -> i64 {
    client.query_one(sql, params).await.unwrap().get(0)
}

/// `base.group_user` (the internal user type the snapshot needs), a group `x.group_secret`, two
/// privileges (`public`, `secret`), two companies, a user in both groups and both companies, and
/// two permissions on `res.groups.privilege`: internal users read `public`, the secret readers
/// read everything.
async fn seed(client: &Client) -> Seeded {
    let internal = one(
        client,
        "INSERT INTO res_groups (name) VALUES ('Internal User') RETURNING id::int8",
        &[],
    )
    .await;
    let secret_readers = one(
        client,
        "INSERT INTO res_groups (name) VALUES ('Secret readers') RETURNING id::int8",
        &[],
    )
    .await;
    client
        .execute(
            "INSERT INTO ir_model_data (module, name, model, res_id) \
             VALUES ('base', 'group_user', 'res.groups', $1), \
                    ('x', 'group_secret', 'res.groups', $2)",
            &[&internal, &secret_readers],
        )
        .await
        .unwrap();
    // the last-administrator rule of `res.users` needs the administrator group and one member
    let system = one(
        client,
        "INSERT INTO res_groups (name) VALUES ('Administrator') RETURNING id::int8",
        &[],
    )
    .await;
    client
        .execute(
            "INSERT INTO ir_model_data (module, name, model, res_id) \
             VALUES ('base', 'group_system', 'res.groups', $1)",
            &[&system],
        )
        .await
        .unwrap();
    let admin = one(
        client,
        "INSERT INTO res_users (name, login, active, share) VALUES ('Root', 'root', true, false) \
         RETURNING id::int8",
        &[],
    )
    .await;
    client
        .execute(
            "INSERT INTO res_groups_users_rel (uid, gid) VALUES ($1, $2)",
            &[&admin, &system],
        )
        .await
        .unwrap();
    let model = one(
        client,
        "INSERT INTO ir_model (name, model, state, transient) \
         VALUES ('Privilege', 'res.groups.privilege', 'base', false) RETURNING id::int8",
        &[],
    )
    .await;
    client
        .execute(
            "INSERT INTO ir_access (name, model_id, group_id, operation, domain, active) VALUES \
             ('internal users read public', $1, $2, 'r', '[(''name'', ''='', ''public'')]', true), \
             ('secret readers read all', $1, $3, 'r', NULL, true)",
            &[&model, &internal, &secret_readers],
        )
        .await
        .unwrap();
    let public = one(
        client,
        "INSERT INTO res_groups_privilege (name, sequence) VALUES ('public', 1) RETURNING id::int8",
        &[],
    )
    .await;
    let secret = one(
        client,
        "INSERT INTO res_groups_privilege (name, sequence) VALUES ('secret', 2) RETURNING id::int8",
        &[],
    )
    .await;
    let currency = erp_testkit::fixtures::seed_currency(client).await.unwrap();
    let company_a = one(
        client,
        "INSERT INTO res_company (name, currency_id, active) VALUES ('A', $1, true) RETURNING id::int8",
        &[&currency],
    )
    .await;
    let company_b = one(
        client,
        "INSERT INTO res_company (name, currency_id, active) VALUES ('B', $1, true) RETURNING id::int8",
        &[&currency],
    )
    .await;
    let user = one(
        client,
        "INSERT INTO res_users (name, login, active, share, company_id) \
         VALUES ('Ann', 'ann', true, false, $1) RETURNING id::int8",
        &[&company_a],
    )
    .await;
    client
        .execute(
            "INSERT INTO res_groups_users_rel (uid, gid) VALUES ($1, $2), ($1, $3)",
            &[&user, &internal, &secret_readers],
        )
        .await
        .unwrap();
    client
        .execute(
            "INSERT INTO res_company_users_rel (user_id, cid) VALUES ($1, $2), ($1, $3)",
            &[&user, &company_a, &company_b],
        )
        .await
        .unwrap();
    Seeded {
        user,
        secret_readers,
        public,
        secret,
        company_a,
        company_b,
    }
}

/// An `Orm` on the harness database deciding with the policy source and resolving principals.
async fn policy_orm(h: &Harness) -> Orm {
    let source = PolicySource::build(Arc::clone(h.registry()), pool(h, 2))
        .await
        .unwrap();
    let provider: Arc<dyn PolicyProvider> = Arc::new(source);
    Orm::builder(Arc::clone(h.registry()), pool(h, 2))
        .policy(provider)
        .resolver(Resolver::id_order())
        .build()
        .unwrap()
}

fn rid(id: i64) -> RecId {
    RecId::from_real(id).unwrap()
}

/// The privileges `tx`'s own principal can see.
async fn visible(tx: &mut Transaction) -> Vec<i64> {
    let mut ids = tx
        .env()
        .records("res.groups.privilege")
        .unwrap()
        .search(tx, &Domain::constant(true))
        .await
        .unwrap()
        .ids();
    ids.sort_unstable();
    ids
}

fn position(statements: &[String], what: impl Fn(&str) -> bool) -> Vec<usize> {
    statements
        .iter()
        .enumerate()
        .filter(|(_, s)| what(s))
        .map(|(i, _)| i)
        .collect()
}

/// Removing the user from the group
/// that lets it read the secret record (an ORM write, buffered, done with superuser rights in the
/// same transaction) hides that record from the user's next search, with no explicit flush. The
/// relation rows are written first, then the subject is re-read with one statement, then the
/// search runs; a membership change rebuilds no policy.
#[tokio::test]
async fn a_group_removal_is_seen_by_the_next_search() {
    let h = harness().await;
    let client = connect(&h).await;
    let s = seed(&client).await;
    let orm = policy_orm(&h).await;
    let mut tx = orm.begin(UserId::new(s.user).unwrap()).await.unwrap();
    assert_eq!(visible(&mut tx).await, [s.public, s.secret]);

    tx.capture_statements(true);
    let sudo = tx.env().sudo(true);
    sudo.records("res.users")
        .unwrap()
        .browse([s.user])
        .unwrap()
        .write(
            &mut tx,
            RawVals::new().set_commands("group_ids", [Command::unlink(rid(s.secret_readers))]),
        )
        .await
        .unwrap();
    assert_eq!(visible(&mut tx).await, [s.public]);

    let captured = tx.captured().to_vec();
    let removed = position(&captured, |sql| {
        sql.starts_with("DELETE FROM \"res_groups_users_rel\"")
    });
    let reread = position(&captured, |sql| sql == SUBJECTS_SQL);
    assert_eq!(removed.len(), 1, "{captured:#?}");
    assert_eq!(
        reread.len(),
        1,
        "one re-resolution statement: {captured:#?}"
    );
    assert!(removed[0] < reread[0], "the write ran first: {captured:#?}");
    assert!(
        position(&captured, |sql| STATEMENTS.contains(&sql)).is_empty(),
        "a membership change rebuilds no policy"
    );
    tx.rollback().await.unwrap();
    drop(tx);
    // nothing committed: a new transaction sees both again
    let mut tx = orm.begin(UserId::new(s.user).unwrap()).await.unwrap();
    assert_eq!(visible(&mut tx).await, [s.public, s.secret]);
    drop(tx);
    drop(client);
    h.cleanup().await.unwrap();
}

/// A restriction created with the ORM and not flushed is in force at the next search: the INSERT runs first, then the private
/// rebuild (the loader's statements on the transaction), then the search.
#[tokio::test]
async fn a_new_restriction_is_seen_by_the_next_search() {
    let h = harness().await;
    let client = connect(&h).await;
    let s = seed(&client).await;
    let orm = policy_orm(&h).await;
    let mut tx = orm.begin(UserId::new(s.user).unwrap()).await.unwrap();
    assert_eq!(visible(&mut tx).await, [s.public, s.secret]);

    let model: i64 = client
        .query_one(
            "SELECT id::int8 FROM ir_model WHERE model = 'res.groups.privilege'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    tx.capture_statements(true);
    let sudo = tx.env().sudo(true);
    let vals: BTreeMap<String, Value> = [
        ("name", Value::str("never the secret")),
        ("model_id", Value::Int(model)),
        ("operation", Value::str("r")),
        ("domain", Value::str("[('name', '!=', 'secret')]")),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v))
    .collect();
    sudo.records("ir.access")
        .unwrap()
        .create_values(&mut tx, &[vals])
        .await
        .unwrap();
    assert_eq!(visible(&mut tx).await, [s.public]);

    let captured = tx.captured().to_vec();
    // the ORM inserts through a CTE (`WITH ... INSERT INTO "ir_access" ...`)
    let inserted = position(&captured, |sql| sql.contains("INSERT INTO \"ir_access\""));
    let loaded = position(&captured, |sql| STATEMENTS.contains(&sql));
    assert_eq!(inserted.len(), 1, "{captured:#?}");
    assert_eq!(
        loaded.len(),
        STATEMENTS.len(),
        "one private rebuild: {captured:#?}"
    );
    assert!(
        inserted[0] < loaded[0],
        "the restriction was written first: {captured:#?}"
    );
    tx.rollback().await.unwrap();
    drop(tx);
    drop(client);
    h.cleanup().await.unwrap();
}

/// After the user loses a company in the transaction, an older environment that still allows it
/// is refused at its next decision.
#[tokio::test]
async fn a_company_removal_is_seen_by_an_older_environment() {
    let h = harness().await;
    let client = connect(&h).await;
    let s = seed(&client).await;
    let orm = policy_orm(&h).await;
    let mut tx = orm.begin(UserId::new(s.user).unwrap()).await.unwrap();
    let before = tx.env();
    assert_eq!(
        before.principal().companies().allowed(),
        [
            erp_orm::CompanyId::new(s.company_a).unwrap(),
            erp_orm::CompanyId::new(s.company_b).unwrap()
        ]
    );
    let sudo = tx.env().sudo(true);
    sudo.records("res.users")
        .unwrap()
        .browse([s.user])
        .unwrap()
        .write(
            &mut tx,
            RawVals::new().set_commands("company_ids", [Command::unlink(rid(s.company_b))]),
        )
        .await
        .unwrap();
    let err = before
        .records("res.groups.privilege")
        .unwrap()
        .search(&mut tx, &Domain::constant(true))
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access);
    assert_eq!(
        err.message(),
        "One of the selected companies does not exist or is not one of yours."
    );
    drop(tx);
    drop(client);
    h.cleanup().await.unwrap();
}

/// The `ir.access` row granting the secret readers everything.
async fn secret_rule(client: &Client) -> i64 {
    client
        .query_one(
            "SELECT id::int8 FROM ir_access WHERE name = 'secret readers read all'",
            &[],
        )
        .await
        .unwrap()
        .get(0)
}

/// Every kind of change the case lists, each in its own transaction after a warm read, is in
/// force at the next check of that transaction: an `ir.access` row created, modified and deleted,
/// a group of the user removed, a company of the user removed. Nothing is flushed explicitly.
#[tokio::test]
async fn spec_sec_018_access_changes_take_effect_immediately() {
    let h = harness().await;
    let client = connect(&h).await;
    let s = seed(&client).await;
    let rule = secret_rule(&client).await;
    let orm = policy_orm(&h).await;
    let user = UserId::new(s.user).unwrap();
    let both = [s.public, s.secret];

    // modified: the secret readers' rule narrowed to the public record
    let mut tx = orm.begin(user).await.unwrap();
    assert_eq!(visible(&mut tx).await, both);
    let mut narrowed = BTreeMap::new();
    narrowed.insert("domain".to_owned(), Value::str("[('name', '=', 'public')]"));
    tx.env()
        .sudo(true)
        .records("ir.access")
        .unwrap()
        .browse([rule])
        .unwrap()
        .write_values(&mut tx, &narrowed)
        .await
        .unwrap();
    assert_eq!(visible(&mut tx).await, [s.public], "modified");
    tx.rollback().await.unwrap();
    drop(tx);

    // deleted: the secret readers' rule removed
    let mut tx = orm.begin(user).await.unwrap();
    assert_eq!(visible(&mut tx).await, both);
    tx.env()
        .sudo(true)
        .records("ir.access")
        .unwrap()
        .browse([rule])
        .unwrap()
        .unlink(&mut tx)
        .await
        .unwrap();
    assert_eq!(visible(&mut tx).await, [s.public], "deleted");
    tx.rollback().await.unwrap();
    drop(tx);

    // created: a restriction hiding the secret record
    let model: i64 = client
        .query_one(
            "SELECT id::int8 FROM ir_model WHERE model = 'res.groups.privilege'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    let mut tx = orm.begin(user).await.unwrap();
    assert_eq!(visible(&mut tx).await, both);
    let vals: BTreeMap<String, Value> = [
        ("name", Value::str("never the secret")),
        ("model_id", Value::Int(model)),
        ("operation", Value::str("r")),
        ("domain", Value::str("[('name', '!=', 'secret')]")),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v))
    .collect();
    tx.env()
        .sudo(true)
        .records("ir.access")
        .unwrap()
        .create_values(&mut tx, &[vals])
        .await
        .unwrap();
    assert_eq!(visible(&mut tx).await, [s.public], "created");
    tx.rollback().await.unwrap();
    drop(tx);

    // the user's groups: out of the secret readers
    let mut tx = orm.begin(user).await.unwrap();
    assert_eq!(visible(&mut tx).await, both);
    tx.env()
        .sudo(true)
        .records("res.users")
        .unwrap()
        .browse([s.user])
        .unwrap()
        .write(
            &mut tx,
            RawVals::new().set_commands("group_ids", [Command::unlink(rid(s.secret_readers))]),
        )
        .await
        .unwrap();
    assert_eq!(visible(&mut tx).await, [s.public], "group removed");
    tx.rollback().await.unwrap();
    drop(tx);

    // the user's allowed companies: company B taken away
    let mut tx = orm.begin(user).await.unwrap();
    assert_eq!(visible(&mut tx).await, both);
    let before = tx.env();
    tx.env()
        .sudo(true)
        .records("res.users")
        .unwrap()
        .browse([s.user])
        .unwrap()
        .write(
            &mut tx,
            RawVals::new().set_commands("company_ids", [Command::unlink(rid(s.company_b))]),
        )
        .await
        .unwrap();
    let err = before
        .records("res.groups.privilege")
        .unwrap()
        .search(&mut tx, &Domain::constant(true))
        .await
        .unwrap_err();
    assert_eq!(
        (err.kind(), err.message()),
        (
            ErrorKind::Access,
            "One of the selected companies does not exist or is not one of yours."
        ),
        "company removed"
    );
    drop(tx);

    // nothing was committed
    let mut tx = orm.begin(user).await.unwrap();
    assert_eq!(visible(&mut tx).await, both);
    drop(tx);
    drop(client);
    h.cleanup().await.unwrap();
}

// ----- caller-built resolutions are hints (review T-2/R-2/R-3/R-1/R-4) ---------------------------

async fn group_user(client: &Client) -> i64 {
    one(
        client,
        "SELECT res_id::int8 FROM ir_model_data WHERE module = 'base' AND name = 'group_user'",
        &[],
    )
    .await
}

/// A principal for `user` whose resolution the caller wrote: `groups` and `companies`.
fn built(user: i64, groups: &[i64], companies: &[i64]) -> erp_orm::Principal {
    let company = |c: &i64| CompanyId::new(*c).unwrap();
    PrincipalBuilder::user(UserId::new(user).unwrap())
        .resolution(
            Resolution::in_id_order(
                GroupSet::new(groups.iter().map(|g| GroupId::new(*g).unwrap())),
                companies.iter().map(company),
                std::num::NonZeroU64::MIN,
            )
            .unwrap(),
        )
        .companies(companies.iter().map(company).collect())
        .build()
        .unwrap()
}

/// Review T-2 (a): a resolution naming a group the user does not have grants nothing: the
/// subject is read from the database, the forged group is not in it.
#[tokio::test]
async fn a_forged_resolution_grants_nothing() {
    let h = harness().await;
    let client = connect(&h).await;
    let s = seed(&client).await;
    let internal = group_user(&client).await;
    let bob = one(
        &client,
        "INSERT INTO res_users (name, login, active, share, company_id) \
         VALUES ('Bob', 'bob', true, false, $1) RETURNING id::int8",
        &[&s.company_a],
    )
    .await;
    client
        .execute(
            "INSERT INTO res_groups_users_rel (uid, gid) VALUES ($1, $2)",
            &[&bob, &internal],
        )
        .await
        .unwrap();
    client
        .execute(
            "INSERT INTO res_company_users_rel (user_id, cid) VALUES ($1, $2)",
            &[&bob, &s.company_a],
        )
        .await
        .unwrap();
    let orm = policy_orm(&h).await;
    let forged = built(bob, &[internal, s.secret_readers], &[s.company_a]);
    let mut tx = orm
        .begin_opts(BeginOptions::for_principal(forged))
        .await
        .unwrap();
    assert_eq!(visible(&mut tx).await, [s.public]);
    drop(tx);

    // companies the user does not have are refused too
    let forged = built(bob, &[internal], &[s.company_a, s.company_b]);
    let mut tx = orm
        .begin_opts(BeginOptions::for_principal(forged))
        .await
        .unwrap();
    let err = tx
        .env()
        .records("res.groups.privilege")
        .unwrap()
        .search(&mut tx, &Domain::constant(true))
        .await
        .unwrap_err();
    assert_eq!(
        (err.kind(), err.message()),
        (
            ErrorKind::Access,
            "One of the selected companies does not exist or is not one of yours."
        )
    );
    drop(tx);
    drop(client);
    h.cleanup().await.unwrap();
}

/// Review T-2 (b): a principal built with a resolution (`for_principal`) is re-read after a
/// membership change in the same transaction.
#[tokio::test]
async fn a_pre_resolved_principal_sees_a_group_removal() {
    let h = harness().await;
    let client = connect(&h).await;
    let s = seed(&client).await;
    let internal = group_user(&client).await;
    let orm = policy_orm(&h).await;
    let ann = built(
        s.user,
        &[internal, s.secret_readers],
        &[s.company_a, s.company_b],
    );
    let mut tx = orm
        .begin_opts(BeginOptions::for_principal(ann))
        .await
        .unwrap();
    assert_eq!(visible(&mut tx).await, [s.public, s.secret]);
    tx.env()
        .sudo(true)
        .records("res.users")
        .unwrap()
        .browse([s.user])
        .unwrap()
        .write(
            &mut tx,
            RawVals::new().set_commands("group_ids", [Command::unlink(rid(s.secret_readers))]),
        )
        .await
        .unwrap();
    assert_eq!(visible(&mut tx).await, [s.public]);
    drop(tx);
    drop(client);
    h.cleanup().await.unwrap();
}

/// Review R-3: a transaction reused after `commit` re-reads the subject; the groups revoked by
/// the committed work are gone.
#[tokio::test]
async fn a_reused_transaction_forgets_revoked_groups() {
    let h = harness().await;
    let client = connect(&h).await;
    let s = seed(&client).await;
    let orm = policy_orm(&h).await;
    let mut tx = orm.begin(UserId::new(s.user).unwrap()).await.unwrap();
    assert_eq!(visible(&mut tx).await, [s.public, s.secret]);
    tx.env()
        .sudo(true)
        .records("res.users")
        .unwrap()
        .browse([s.user])
        .unwrap()
        .write(
            &mut tx,
            RawVals::new().set_commands("group_ids", [Command::unlink(rid(s.secret_readers))]),
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(visible(&mut tx).await, [s.public]);
    drop(tx);
    drop(client);
    h.cleanup().await.unwrap();
}

/// A resolver whose re-resolution can be made to wait for ever, so a test drops the future
/// while it is inside the refresh.
struct Gated {
    inner: Resolver,
    blocked: Arc<AtomicBool>,
}

impl PrincipalResolver for Gated {
    fn statement(&self, hint: &PrincipalHint) -> erp_fields::Result<ResolverStatement> {
        self.inner.statement(hint)
    }

    fn finish(
        &self,
        hint: &PrincipalHint,
        rows: &[Row],
        policy: &dyn AccessPolicy,
    ) -> erp_fields::Result<Principal> {
        self.inner.finish(hint, rows, policy)
    }

    fn re_resolve<'a>(
        &'a self,
        tx: &'a mut Transaction,
        subjects: &'a [UserId],
    ) -> BoxFuture<'a, erp_fields::Result<Vec<Resolution>>> {
        Box::pin(async move {
            if self.blocked.load(Ordering::SeqCst) {
                std::future::pending::<()>().await;
            }
            self.inner.re_resolve(tx, subjects).await
        })
    }
}

const CANCELLED: &str = "the transaction can no longer be used: an ORM operation was cancelled \
                         (its future was dropped) while it was changing the transaction's state; \
                         roll back";

/// Review R-1: a refresh whose future is dropped inside it (the resolver waits, a timeout drops
/// the call) poisons the transaction: the next call fails instead of deciding with the old policy.
#[tokio::test]
async fn a_cancelled_refresh_poisons_the_transaction() {
    let h = harness().await;
    let client = connect(&h).await;
    let s = seed(&client).await;
    let source = PolicySource::build(Arc::clone(h.registry()), pool(&h, 2))
        .await
        .unwrap();
    let provider: Arc<dyn PolicyProvider> = Arc::new(source);
    let blocked = Arc::new(AtomicBool::new(false));
    let orm = Orm::builder(Arc::clone(h.registry()), pool(&h, 2))
        .policy(provider)
        .resolver(Gated {
            inner: Resolver::id_order(),
            blocked: Arc::clone(&blocked),
        })
        .build()
        .unwrap();
    let mut tx = orm.begin(UserId::new(s.user).unwrap()).await.unwrap();
    assert_eq!(visible(&mut tx).await, [s.public, s.secret]);
    tx.env()
        .sudo(true)
        .records("res.users")
        .unwrap()
        .browse([s.user])
        .unwrap()
        .write(
            &mut tx,
            RawVals::new().set_commands("group_ids", [Command::unlink(rid(s.secret_readers))]),
        )
        .await
        .unwrap();
    blocked.store(true, Ordering::SeqCst);
    let cancelled = tokio::time::timeout(Duration::from_millis(300), visible(&mut tx)).await;
    assert!(cancelled.is_err(), "the refresh waits in the resolver");
    assert!(tx.is_poisoned());
    let err = tx
        .env()
        .records("res.groups.privilege")
        .unwrap()
        .search(&mut tx, &Domain::constant(true))
        .await
        .unwrap_err();
    assert_eq!(
        (err.kind(), err.message()),
        (ErrorKind::Programming, CANCELLED)
    );
    drop(tx);
    drop(client);
    h.cleanup().await.unwrap();
}

/// Review F-2: an unlink of a policy row dropped while its DELETE waits (another transaction
/// holds the row) poisons the transaction, so the policy change it may have made is never
/// answered from the old policy.
#[tokio::test]
async fn a_cancelled_unlink_of_a_policy_row_poisons_the_transaction() {
    let h = harness().await;
    let mut client = connect(&h).await;
    let s = seed(&client).await;
    let rule = secret_rule(&client).await;
    let orm = policy_orm(&h).await;
    let mut tx = orm.begin(UserId::new(s.user).unwrap()).await.unwrap();
    assert_eq!(visible(&mut tx).await, [s.public, s.secret]);
    // another transaction holds the row, so the DELETE waits
    let holder = client.transaction().await.unwrap();
    holder
        .query(
            "SELECT id FROM ir_access WHERE id = $1 FOR UPDATE",
            &[&rule],
        )
        .await
        .unwrap();
    let unlink = tx
        .env()
        .sudo(true)
        .records("ir.access")
        .unwrap()
        .browse([rule])
        .unwrap();
    let cancelled = tokio::time::timeout(Duration::from_millis(500), unlink.unlink(&mut tx)).await;
    assert!(cancelled.is_err(), "the DELETE waits for the row lock");
    assert!(tx.is_poisoned());
    let err = tx
        .env()
        .records("res.groups.privilege")
        .unwrap()
        .search(&mut tx, &Domain::constant(true))
        .await
        .unwrap_err();
    assert_eq!(
        (err.kind(), err.message()),
        (ErrorKind::Programming, CANCELLED)
    );
    holder.rollback().await.unwrap();
    drop(tx);
    drop(client);
    h.cleanup().await.unwrap();
}

/// An `Orm` cannot be built with a policy that decides on resolved principals and no resolver:
/// a caller's resolution would be trusted.
#[tokio::test]
async fn a_compiled_policy_without_a_resolver_does_not_build() {
    let h = harness().await;
    let source = PolicySource::build(Arc::clone(h.registry()), pool(&h, 2))
        .await
        .unwrap();
    let provider: Arc<dyn PolicyProvider> = Arc::new(source);
    let err = Orm::builder(Arc::clone(h.registry()), pool(&h, 2))
        .policy(provider)
        .build()
        .err()
        .unwrap();
    assert_eq!(
        (err.kind(), err.message()),
        (
            ErrorKind::Programming,
            "the installed access policy decides on the groups and companies of the subject, but \
             no principal resolver is installed (OrmBuilder::resolver): a resolution built by \
             the caller would be trusted and a membership change never re-read"
        )
    );
    h.cleanup().await.unwrap();
}

/// A decision on a subject the resolver never read in this transaction is refused with one
/// readable sentence (here the synchronous optimizer, which cannot await the resolver).
#[tokio::test]
async fn an_unresolved_subject_is_refused_in_plain_words() {
    let h = harness().await;
    let client = connect(&h).await;
    let s = seed(&client).await;
    let internal = group_user(&client).await;
    let orm = policy_orm(&h).await;
    let ann = built(s.user, &[internal], &[s.company_a]);
    let tx = orm
        .begin_opts(BeginOptions::for_principal(ann))
        .await
        .unwrap();
    let access = Domain::condition("id", erp_domain::Operator::Access, "read").unwrap();
    let err = tx
        .env()
        .records("res.groups.privilege")
        .unwrap()
        .optimize_domain_in(&tx, &access, erp_domain::OptimizationLevel::Full, None)
        .unwrap_err();
    assert_eq!(
        (err.kind(), err.message()),
        (
            ErrorKind::Programming,
            format!(
                "the user principal of user {} was not resolved by the principal resolver in \
                 this transaction: a resolution built by the caller is not trusted, and the \
                 operation decided without Transaction::prepare_access",
                s.user
            )
            .as_str()
        )
    );
    drop(tx);
    drop(client);
    h.cleanup().await.unwrap();
}

/// Review R-4: a `sudo()` search whose domain has an `access` condition decides against the
/// non-superuser environment after a pending policy write, and on a principal the transaction
/// never resolved.
#[tokio::test]
async fn a_sudo_search_with_an_access_condition_sees_pending_policy_writes() {
    let h = harness().await;
    let client = connect(&h).await;
    let s = seed(&client).await;
    let rule = secret_rule(&client).await;
    let internal = group_user(&client).await;
    let orm = policy_orm(&h).await;
    let access = Domain::condition("id", erp_domain::Operator::Access, "read").unwrap();
    let sudo_search = async |tx: &mut Transaction| {
        let mut ids = tx
            .env()
            .sudo(true)
            .records("res.groups.privilege")
            .unwrap()
            .search(tx, &access)
            .await
            .unwrap()
            .ids();
        ids.sort_unstable();
        ids
    };
    // the principal comes from the caller: resolved on first use
    let ann = built(
        s.user,
        &[internal, s.secret_readers],
        &[s.company_a, s.company_b],
    );
    let mut tx = orm
        .begin_opts(BeginOptions::for_principal(ann))
        .await
        .unwrap();
    assert_eq!(sudo_search(&mut tx).await, [s.public, s.secret]);
    let mut narrowed = BTreeMap::new();
    narrowed.insert("domain".to_owned(), Value::str("[('name', '=', 'public')]"));
    tx.env()
        .sudo(true)
        .records("ir.access")
        .unwrap()
        .browse([rule])
        .unwrap()
        .write_values(&mut tx, &narrowed)
        .await
        .unwrap();
    assert_eq!(sudo_search(&mut tx).await, [s.public]);
    drop(tx);
    drop(client);
    h.cleanup().await.unwrap();
}
