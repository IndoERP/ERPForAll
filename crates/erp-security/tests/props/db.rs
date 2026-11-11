//! The policy read from the database (properties P6 and P9): the fixture's groups, implications,
//! models and memberships in `base`'s tables, generated rules as `ir_access` rows (their
//! statement triggers bump the `policy` generation), `erp-security`'s `PolicySource` as the
//! provider and SIGNAL's refresh hook, its `Resolver` for the principals.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use erp_orm::{Operation, Orm, PolicyProvider, Signal, Transaction, UserId};
use erp_security::resolver::Resolver;
use erp_security::source::PolicySource;
use erp_testkit::Harness;
use erp_testkit::tokio_postgres::{Client, NoTls};

use super::generate::{Case, RuleSpec};
use super::oracle::{IdSet, visible};
use super::{CATEG, CHILD, EVERYONE, G1, G2, G3, Ids, NO_ONE, OBJ, PORTAL, USER};

/// The models with generated rows, in answer order.
pub const MODELS: [&str; 3] = [OBJ, CHILD, CATEG];

/// A pool of its own on the harness database.
pub fn pool(h: &Harness, size: usize) -> erp_testkit::deadpool_postgres::Pool {
    erp_orm::pool::create_pool(h.db().config(), size).unwrap()
}

/// A connection of its own to the harness database, outside every pool.
pub async fn connect(h: &Harness) -> Client {
    let (client, connection) = h.db().config().connect(NoTls).await.unwrap();
    tokio::spawn(async move {
        if let Err(error) = connection.await {
            eprintln!("props: connection closed with an error: {error}");
        }
    });
    client
}

/// The `ir_model` ids of the fixture models.
pub type ModelIds = BTreeMap<&'static str, i64>;

/// Groups (with their external ids), implications and the fixture models' `ir_model` rows, once
/// per database (committed).
pub async fn seed_policy_base(h: &Harness) -> ModelIds {
    let client = connect(h).await;
    client
        .batch_execute(&format!(
            "INSERT INTO res_groups (id, name, share) VALUES \
             ({USER}, 'Internal User', false), ({G1}, 'Group 1', false), \
             ({G2}, 'Group 2', false), ({G3}, 'Group 3', false), \
             ({EVERYONE}, 'Everyone', false), ({PORTAL}, 'Portal', true), \
             ({NO_ONE}, 'Technical Features', false); \
             SELECT setval(pg_get_serial_sequence('res_groups', 'id'), 10); \
             INSERT INTO ir_model_data (module, name, model, res_id) VALUES \
             ('base', 'group_user', 'res.groups', {USER}), \
             ('base', 'group_everyone', 'res.groups', {EVERYONE}), \
             ('base', 'group_portal', 'res.groups', {PORTAL}), \
             ('base', 'group_no_one', 'res.groups', {NO_ONE}); \
             INSERT INTO res_groups_implied_rel (gid, hid) VALUES \
             ({USER}, {EVERYONE}), ({PORTAL}, {EVERYONE}), ({G3}, {G1});"
        ))
        .await
        .unwrap();
    let mut ids = ModelIds::new();
    for model in MODELS {
        let row = client
            .query_one(
                "INSERT INTO ir_model (name, model, state, transient) \
                 VALUES ($1, $1, 'base', false) RETURNING id::int8",
                &[&model],
            )
            .await
            .unwrap();
        ids.insert(model, row.get(0));
    }
    ids
}

/// Insert `rule` as an `ir_access` row through `client`; its id.
pub async fn insert_rule(client: &Client, models: &ModelIds, rule: &RuleSpec, name: &str) -> i64 {
    let model = models[rule.on.model()];
    let group: Option<i64> = rule.group_id();
    let operation = rule.operation();
    let domain = rule.text();
    client
        .query_one(
            "INSERT INTO ir_access (name, model_id, group_id, operation, domain, active) \
             VALUES ($1, $2, $3, $4, $5, true) RETURNING id::int8",
            &[&name, &model, &group, &operation, &domain],
        )
        .await
        .unwrap()
        .get(0)
}

/// Replace the rules with `rules` and the memberships of users 2..=5 with the direct groups of
/// `case`'s principals (user `u` takes principal `(u - 2) % n`), committed; the rule ids.
pub async fn set_policy(
    h: &Harness,
    models: &ModelIds,
    case: &Case,
    rules: &[RuleSpec],
) -> Vec<i64> {
    let client = connect(h).await;
    client
        .batch_execute("DELETE FROM ir_access; DELETE FROM res_groups_users_rel;")
        .await
        .unwrap();
    for uid in 2..=5i64 {
        let spec = &case.principals[usize::try_from(uid - 2).unwrap() % case.principals.len()];
        for group in spec.direct_groups() {
            client
                .execute(
                    "INSERT INTO res_groups_users_rel (uid, gid) VALUES ($1, $2)",
                    &[&uid, &group],
                )
                .await
                .unwrap();
        }
    }
    let mut ids = Vec::new();
    for (i, rule) in rules.iter().enumerate() {
        ids.push(insert_rule(&client, models, rule, &format!("rule {i}")).await);
    }
    ids
}

/// A policy source on its own pool (a "process": its own snapshots and memos).
pub async fn source(h: &Harness) -> PolicySource {
    PolicySource::build(Arc::clone(h.registry()), pool(h, 2))
        .await
        .unwrap()
}

/// An `Orm` deciding with `source`, refreshing it through SIGNAL (baseline read now) when
/// `signal` is set, resolving principals with `erp-security`'s resolver.
pub async fn orm(h: &Harness, source: &PolicySource, signal: bool) -> Orm {
    let provider: Arc<dyn PolicyProvider> = Arc::new(source.clone());
    let mut builder = Orm::builder(Arc::clone(h.registry()), pool(h, 4))
        .policy(provider)
        .resolver(Resolver::id_order());
    if signal {
        let baseline = Signal::read(&connect(h).await).await.unwrap();
        builder = builder.signal(
            Signal::new(baseline)
                .with_refresh(source.clone())
                .with_apply_timeout(Duration::from_secs(10))
                .unwrap(),
        );
    }
    builder.build().unwrap()
}

/// A transaction of user `uid` (resolved from the database).
pub async fn begin(orm: &Orm, uid: i64) -> Transaction {
    orm.begin(UserId::new(uid).unwrap()).await.unwrap()
}

/// What the transaction's principal sees, per (model, operation) in [`MODELS`] × all operations.
pub async fn answers(tx: &mut Transaction, ids: &Ids) -> Vec<IdSet> {
    let env = tx.env();
    let mut out = Vec::new();
    for model in MODELS {
        for op in Operation::ALL {
            out.push(visible(tx, &env, model, ids.of(model), op).await);
        }
    }
    out
}

/// What a fresh process (its own source, built from the rows committed now) answers user `uid`.
pub async fn fresh_answers(h: &Harness, uid: i64, ids: &Ids) -> Vec<IdSet> {
    let source = source(h).await;
    let orm = orm(h, &source, false).await;
    let mut tx = begin(&orm, uid).await;
    let out = answers(&mut tx, ids).await;
    drop(tx);
    out
}

/// The (model, operation) of answer `i`.
pub fn pair(i: usize) -> (&'static str, Operation) {
    (MODELS[i / 4], Operation::ALL[i % 4])
}
