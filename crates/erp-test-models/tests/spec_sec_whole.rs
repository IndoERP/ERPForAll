//! `ir_access` rows read by the real `PolicySource`, compiled into the snapshot, and enforced by
//! the ORM entry points (`has_access`, `filtered_access`, `check_access`, `search`, reads) for a
//! principal the real `Resolver` builds from `res_groups_users_rel`.
//!
//! Every checked value is what our ORM returns; the
//! expectations are the spec's named records (resolved to the ids of the seeded rows) and the
//! spec's message texts.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use std::sync::Arc;

use erp_orm::{Operation, Orm, PolicyProvider, Transaction, UserId};
use erp_sdk::{Domain, ErrorKind};
use erp_security::resolver::Resolver;
use erp_security::source::PolicySource;
use erp_test_models::{base_users, test_access};
use erp_testkit::Harness;
use erp_testkit::tokio_postgres::types::ToSql;
use erp_testkit::tokio_postgres::{Client, NoTls};

const OBJ: &str = "test_access.some_obj";
const NAMES: [&str; 6] = ["Mario", "Luigi", "Peach", "Toad", "Yoshi", "Bowser"];

async fn harness() -> Harness {
    Harness::builder([
        &erp_addon_base::ADDON,
        &erp_data::model_data::ADDON,
        &base_users::ADDON,
        &test_access::ADDON,
    ])
    .with_decls([erp_data::ir_model_data_decl()])
    .build()
    .await
    .expect("harness")
}

async fn connect(h: &Harness) -> Client {
    let (client, connection) = h.db().config().connect(NoTls).await.unwrap();
    tokio::spawn(async move {
        if let Err(error) = connection.await {
            eprintln!("spec_sec_whole: connection closed with an error: {error}");
        }
    });
    client
}

async fn one(client: &Client, sql: &str, params: &[&(dyn ToSql + Sync)]) -> i64 {
    client.query_one(sql, params).await.unwrap().get(0)
}

/// Bob, his groups (Group 3 exists but he is not in it), the model's `ir.model` row and the six
/// records, all committed.
struct World {
    bob: i64,
    group1: i64,
    group2: i64,
    group3: i64,
    model: i64,
    /// Mario, Luigi, Peach, Toad, Yoshi, Bowser.
    records: Vec<i64>,
}

impl World {
    /// The ids of the named records, ascending.
    fn named(&self, names: &[&str]) -> Vec<i64> {
        let mut ids: Vec<i64> = names
            .iter()
            .map(|n| self.records[NAMES.iter().position(|x| x == n).unwrap()])
            .collect();
        ids.sort_unstable();
        ids
    }

    /// All records except the named ones, ascending.
    fn except(&self, names: &[&str]) -> Vec<i64> {
        let rest: Vec<&str> = NAMES
            .iter()
            .copied()
            .filter(|n| !names.contains(n))
            .collect();
        self.named(&rest)
    }

    fn all(&self) -> Vec<i64> {
        self.named(&NAMES)
    }
}

async fn seed(client: &Client) -> World {
    let group = async |name: &str| -> i64 {
        one(
            client,
            "INSERT INTO res_groups (name) VALUES ($1) RETURNING id::int8",
            &[&name],
        )
        .await
    };
    let internal = group("Internal User").await;
    let group1 = group("Group 1").await;
    let group2 = group("Group 2").await;
    let group3 = group("Group 3").await;
    client
        .execute(
            "INSERT INTO ir_model_data (module, name, model, res_id) \
             VALUES ('base', 'group_user', 'res.groups', $1)",
            &[&internal],
        )
        .await
        .unwrap();
    let bob = one(
        client,
        "INSERT INTO res_users (name, login, active, share) \
         VALUES ('Bob Bobman', 'bob', true, false) RETURNING id::int8",
        &[],
    )
    .await;
    for gid in [internal, group1, group2] {
        client
            .execute(
                "INSERT INTO res_groups_users_rel (uid, gid) VALUES ($1, $2)",
                &[&bob, &gid],
            )
            .await
            .unwrap();
    }
    let model = one(
        client,
        "INSERT INTO ir_model (name, model, state, transient) \
         VALUES ('Object For Test Access Right', $1, 'base', false) RETURNING id::int8",
        &[&OBJ],
    )
    .await;
    let mut records = Vec::new();
    for (name, val) in NAMES.iter().zip(1_i32..) {
        records.push(
            one(
                client,
                "INSERT INTO test_access_some_obj (name, val) VALUES ($1, $2) RETURNING id::int8",
                &[name, &val],
            )
            .await,
        );
    }
    World {
        bob,
        group1,
        group2,
        group3,
        model,
        records,
    }
}

struct Access {
    name: &'static str,
    records: Option<Vec<i64>>,
    group: Option<i64>,
    operation: &'static str,
}

fn access(
    name: &'static str,
    records: Option<Vec<i64>>,
    group: Option<i64>,
    operation: &'static str,
) -> Access {
    Access {
        name,
        records,
        group,
        operation,
    }
}

/// Replace the model's accesses with `rules`, and return the ORM deciding with the policy now in
/// the database.
async fn set_accesses(h: &Harness, client: &Client, w: &World, rules: Vec<Access>) -> Orm {
    client
        .execute("DELETE FROM ir_access WHERE model_id = $1", &[&w.model])
        .await
        .unwrap();
    for rule in rules {
        let domain = rule.records.map(|ids| {
            let list: Vec<String> = ids.iter().map(i64::to_string).collect();
            format!("[('id', 'in', [{}])]", list.join(", "))
        });
        client
            .execute(
                "INSERT INTO ir_access (name, model_id, group_id, operation, domain, active) \
                 VALUES ($1, $2, $3, $4, $5, true)",
                &[&rule.name, &w.model, &rule.group, &rule.operation, &domain],
            )
            .await
            .unwrap();
    }
    let pool = || erp_orm::pool::create_pool(h.db().config(), 2).unwrap();
    let source = PolicySource::build(Arc::clone(h.registry()), pool())
        .await
        .unwrap();
    let provider: Arc<dyn PolicyProvider> = Arc::new(source);
    Orm::builder(Arc::clone(h.registry()), pool())
        .policy(provider)
        .resolver(Resolver::id_order())
        .build()
        .unwrap()
}

async fn begin_bob(orm: &Orm, w: &World) -> Transaction {
    orm.begin(UserId::new(w.bob).unwrap()).await.unwrap()
}

fn records(tx: &Transaction, ids: &[i64]) -> erp_sdk::Records {
    tx.env()
        .records(OBJ)
        .unwrap()
        .browse(ids.iter().copied())
        .unwrap()
}

const OPS: [Operation; 4] = [
    Operation::Read,
    Operation::Write,
    Operation::Create,
    Operation::Unlink,
];

/// The model-level refusal ('s text) of `op`, with no group allowing it.
fn model_refused(op: Operation) -> String {
    let verb = match op {
        Operation::Read => "access",
        Operation::Write => "modify",
        Operation::Create => "create",
        Operation::Unlink => "delete",
    };
    format!(
        "You are not allowed to {verb} 'Object For Test Access Right' (test_access.some_obj) \
         records.\n\nNo group currently allows this operation.\n\nContact your administrator to \
         request access if necessary."
    )
}

fn op_name(op: Operation) -> &'static str {
    match op {
        Operation::Read => "read",
        Operation::Write => "write",
        Operation::Create => "create",
        Operation::Unlink => "delete",
    }
}

/// The record-level refusal ('s normal-mode text) of `operation`.
fn record_refused(w: &World, operation: &str) -> String {
    format!(
        "Uh-oh! Looks like you have stumbled upon some top-secret records.\n\nSorry, Bob Bobman \
         (id={}) doesn't have '{operation}' access to:\n- Object For Test Access Right \
         (test_access.some_obj)\n\nIf you really, really need access, perhaps you can win over \
         your friendly administrator with a batch of freshly baked cookies.",
        w.bob
    )
}

#[track_caller]
fn assert_refused(error: &erp_sdk::Error, message: &str) {
    assert_eq!(error.kind(), ErrorKind::Access, "{error}");
    assert_eq!(error.message(), message);
}

fn sorted(mut ids: Vec<i64>) -> Vec<i64> {
    ids.sort_unstable();
    ids
}

async fn assert_access(orm: &Orm, w: &World, allowed: &[i64], op: Operation, label: &str) {
    let mut tx = begin_bob(orm, w).await;
    let model = records(&tx, &[]);
    assert!(
        model.has_access(&mut tx, op).await.unwrap(),
        "{label}: model"
    );
    model.check_access(&mut tx, op).await.unwrap();

    let record_message = record_refused(w, op_name(op));
    for id in &w.records {
        let single = records(&tx, &[*id]);
        if allowed.contains(id) {
            assert!(
                single.has_access(&mut tx, op).await.unwrap(),
                "{label}: {id}"
            );
            single.check_access(&mut tx, op).await.unwrap();
        } else {
            assert!(
                !single.has_access(&mut tx, op).await.unwrap(),
                "{label}: {id}"
            );
            let err = single.check_access(&mut tx, op).await.unwrap_err();
            assert_refused(&err, &record_message);
        }
    }
    records(&tx, allowed)
        .check_access(&mut tx, op)
        .await
        .unwrap();

    let all = records(&tx, &w.records);
    if allowed.len() == w.records.len() {
        assert!(all.has_access(&mut tx, op).await.unwrap(), "{label}: all");
        all.check_access(&mut tx, op).await.unwrap();
    } else {
        assert!(!all.has_access(&mut tx, op).await.unwrap(), "{label}: all");
        let err = all.check_access(&mut tx, op).await.unwrap_err();
        assert_refused(&err, &record_message);
    }
    let filtered = all.filtered_access(&mut tx, op).await.unwrap();
    assert_eq!(sorted(filtered.ids()), allowed, "{label}: filtered");

    if op == Operation::Read {
        let found = tx
            .env()
            .records(OBJ)
            .unwrap()
            .search(&mut tx, &Domain::TRUE)
            .await
            .unwrap();
        assert_eq!(sorted(found.ids()), allowed, "{label}: search");
        let cells = records(&tx, allowed).cells(&mut tx, "name").await.unwrap();
        assert_eq!(cells.len(), allowed.len(), "{label}: read");
        if allowed.len() != w.records.len() {
            let err = all.cells(&mut tx, "name").await.unwrap_err();
            assert_refused(&err, &record_message);
        }
    }
}

/// Nothing is allowed for `ops`: not on the empty model, not on the records, the filtered set is
/// empty, every check raises the model-level error, and a read or search is refused likewise.
async fn assert_nothing(orm: &Orm, w: &World, ops: &[Operation], label: &str) {
    let mut tx = begin_bob(orm, w).await;
    for op in ops {
        for set in [records(&tx, &[]), records(&tx, &w.records)] {
            assert!(
                !set.has_access(&mut tx, *op).await.unwrap(),
                "{label} {op:?}"
            );
            assert!(
                set.filtered_access(&mut tx, *op).await.unwrap().is_empty(),
                "{label} {op:?}"
            );
            let err = set.check_access(&mut tx, *op).await.unwrap_err();
            assert_refused(&err, &model_refused(*op));
        }
    }
    if ops.contains(&Operation::Read) {
        let err = tx
            .env()
            .records(OBJ)
            .unwrap()
            .search(&mut tx, &Domain::TRUE)
            .await
            .unwrap_err();
        assert_refused(&err, &model_refused(Operation::Read));
        let err = records(&tx, &w.records)
            .cells(&mut tx, "name")
            .await
            .unwrap_err();
        assert_refused(&err, &model_refused(Operation::Read));
    }
}

#[tokio::test]
async fn spec_sec_001_no_access_record_means_no_access() {
    let h = harness().await;
    let client = connect(&h).await;
    let w = seed(&client).await;
    let orm = set_accesses(&h, &client, &w, vec![]).await;
    assert_nothing(&orm, &w, &OPS, "none").await;
    drop(client);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_sec_002_restrictions_alone_grant_nothing() {
    let h = harness().await;
    let client = connect(&h).await;
    let w = seed(&client).await;
    let orm = set_accesses(
        &h,
        &client,
        &w,
        vec![access("all", Some(w.all()), None, "r")],
    )
    .await;
    assert_nothing(
        &orm,
        &w,
        &[Operation::Read, Operation::Write],
        "restriction",
    )
    .await;
    drop(client);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_sec_003_permissions_are_ored() {
    let h = harness().await;
    let client = connect(&h).await;
    let w = seed(&client).await;
    let expected = w.named(&["Mario", "Luigi", "Peach"]);
    let orm = set_accesses(
        &h,
        &client,
        &w,
        vec![
            access("a", Some(w.named(&["Mario", "Luigi"])), Some(w.group1), "r"),
            access("b", Some(w.named(&["Mario", "Peach"])), Some(w.group1), "r"),
        ],
    )
    .await;
    assert_access(&orm, &w, &expected, Operation::Read, "in group").await;
    let orm = set_accesses(
        &h,
        &client,
        &w,
        vec![
            access("a", Some(w.named(&["Mario", "Luigi"])), Some(w.group1), "r"),
            access("b", Some(w.named(&["Mario", "Peach"])), Some(w.group2), "r"),
            access("c", Some(w.named(&["Mario", "Yoshi"])), Some(w.group3), "r"),
        ],
    )
    .await;
    assert_access(&orm, &w, &expected, Operation::Read, "distinct groups").await;
    drop(client);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_sec_004_restrictions_are_anded() {
    let h = harness().await;
    let client = connect(&h).await;
    let w = seed(&client).await;
    let not_bowser = w.except(&["Bowser"]);
    let not_peach = w.except(&["Peach"]);
    let g1 = Some(w.group1);
    let g2 = Some(w.group2);
    let read = Operation::Read;

    let orm = set_accesses(
        &h,
        &client,
        &w,
        vec![
            access("p", Some(w.named(&["Mario", "Peach", "Bowser"])), g1, "r"),
            access("r", Some(not_bowser.clone()), None, "r"),
        ],
    )
    .await;
    assert_access(&orm, &w, &w.named(&["Mario", "Peach"]), read, "one/one").await;

    let two_permissions = || {
        vec![
            access("p1", Some(w.named(&["Mario", "Luigi", "Bowser"])), g1, "r"),
            access("p2", Some(w.named(&["Mario", "Peach", "Bowser"])), g2, "r"),
            access("r1", Some(not_bowser.clone()), None, "r"),
        ]
    };
    let orm = set_accesses(&h, &client, &w, two_permissions()).await;
    let three = w.named(&["Mario", "Luigi", "Peach"]);
    assert_access(&orm, &w, &three, read, "two/one").await;

    let mut rules = two_permissions();
    rules.push(access("r2", Some(not_peach), None, "r"));
    let orm = set_accesses(&h, &client, &w, rules).await;
    assert_access(&orm, &w, &w.named(&["Mario", "Luigi"]), read, "two/two").await;

    let orm = set_accesses(
        &h,
        &client,
        &w,
        vec![
            access("p1", Some(w.named(&["Mario", "Luigi", "Bowser"])), g1, "r"),
            access("p2", None, g2, "r"),
            access("r1", Some(not_bowser.clone()), None, "r"),
        ],
    )
    .await;
    assert_access(&orm, &w, &not_bowser, read, "special manager").await;
    drop(client);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_sec_005_operation_subsets() {
    let h = harness().await;
    let client = connect(&h).await;
    let w = seed(&client).await;
    let not_bowser = w.except(&["Bowser"]);
    let orm = set_accesses(
        &h,
        &client,
        &w,
        vec![
            access("read all", None, Some(w.group1), "r"),
            access("everything", None, Some(w.group2), "crud"),
            access("keep bowser", Some(not_bowser.clone()), None, "cud"),
        ],
    )
    .await;
    assert_access(&orm, &w, &w.all(), Operation::Read, "restrict").await;
    for op in [Operation::Write, Operation::Create, Operation::Unlink] {
        assert_access(&orm, &w, &not_bowser, op, "restrict").await;
    }

    let humans = w.named(&["Mario", "Luigi", "Peach"]);
    let orm = set_accesses(
        &h,
        &client,
        &w,
        vec![
            access("read", None, Some(w.group1), "r"),
            access("humans", Some(humans.clone()), Some(w.group1), "u"),
        ],
    )
    .await;
    assert_access(&orm, &w, &w.all(), Operation::Read, "one permission").await;
    assert_access(&orm, &w, &humans, Operation::Write, "one permission").await;
    drop(client);
    h.cleanup().await.unwrap();
}
