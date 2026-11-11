//! the statement-level triggers that bump the `policy` generation (
//! and §8.3): one row
//! and one wake-up per transaction for any write to a policy table, whatever wrote it (ORM,
//! raw SQL, a foreign-key action, `TRUNCATE`); none for membership; undone with a savepoint;
//! schema sync creates them once and leaves an unchanged schema alone.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod support;

use std::time::Duration;

use erp_sdk::{Command, ErrorKind, RawVals, RecId, Records, Transaction, Value};
use erp_testkit::Harness;
use erp_testkit::tokio_postgres::{AsyncMessage, Client, NoTls};
use support::map;

/// `base` (with its policy tables) and `ir.model.data` (the group rules of `res.groups` look
/// groups up by external id) on a fresh database, without a `Signal`: schema sync creates the
/// generation table the triggers write to.
async fn harness() -> Harness {
    Harness::builder([&erp_addon_base::ADDON, &erp_data::model_data::ADDON])
        .with_decls([erp_data::ir_model_data_decl()])
        .build()
        .await
        .expect("harness")
}

/// The `policy` rows the transaction sees (its own uncommitted ones included).
async fn policy_rows(tx: &mut Transaction) -> i64 {
    let rows = tx
        .query(
            "SELECT count(*)::int8 FROM erp_signal_generation WHERE key = 'policy'",
            &[],
        )
        .await
        .unwrap();
    rows[0].get(0)
}

/// A connection of its own to the harness database, outside the pool (session settings and
/// temporary tables made on it never reach the ORM's connections).
async fn connect(h: &Harness) -> Client {
    let (client, connection) = h.db().config().connect(NoTls).await.unwrap();
    tokio::spawn(async move {
        if let Err(error) = connection.await {
            eprintln!("access_triggers: connection closed with an error: {error}");
        }
    });
    client
}

/// These databases have none of `base`'s data (no `base.group_user_regular`, no administrator
/// group): the rules of `res.groups` and `res.users` run in the state of the base installation.
fn installing(tx: &mut Transaction) {
    erp_testkit::base_install::set_base_installing(tx, true).unwrap();
}

/// The committed `policy` rows, read on a connection of its own.
async fn committed_policy_rows(h: &Harness) -> i64 {
    let client = connect(h).await;
    let row = client
        .query_one(
            "SELECT count(*)::int8 FROM public.erp_signal_generation WHERE key = 'policy'",
            &[],
        )
        .await
        .unwrap();
    row.get(0)
}

async fn create(tx: &mut Transaction, model: &str, rows: &[&[(&str, Value)]]) -> Records {
    let records = tx.env().records(model).unwrap();
    let vals: Vec<_> = rows.iter().map(|r| map(r)).collect();
    records.create_values(tx, &vals).await.unwrap()
}

fn rid(id: i64) -> RecId {
    RecId::from_real(id).unwrap()
}

/// Trigger name → comment of the user triggers on `table`.
async fn triggers(tx: &mut Transaction, table: &str) -> Vec<(String, Option<String>)> {
    let rows = tx
        .query(
            "SELECT t.tgname::text, obj_description(t.oid, 'pg_trigger') FROM pg_trigger t \
             WHERE t.tgrelid = to_regclass($1) AND NOT t.tgisinternal ORDER BY 1",
            &[&table],
        )
        .await
        .unwrap();
    rows.iter().map(|r| (r.get(0), r.get(1))).collect()
}

/// Statement tests: ORM writes on every policy table bump once per transaction, however many
/// statements run; the next transaction bumps again; the rows commit with the transaction.
#[tokio::test]
async fn orm_writes_bump_policy_once_per_transaction() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    installing(&mut tx);
    assert_eq!(policy_rows(&mut tx).await, 0, "schema sync bumps nothing");
    let privilege = create(
        &mut tx,
        "res.groups.privilege",
        &[&[("name", Value::str("P"))]],
    )
    .await;
    tx.flush().await.unwrap();
    assert_eq!(policy_rows(&mut tx).await, 1);
    let groups = create(
        &mut tx,
        "res.groups",
        &[
            &[
                ("name", Value::str("user")),
                ("privilege_id", Value::Int(privilege.ids()[0])),
            ],
            &[("name", Value::str("manager"))],
        ],
    )
    .await;
    let manager = groups.browse([groups.ids()[1]]).unwrap();
    manager
        .write(
            &mut tx,
            RawVals::new().set_commands("implied_ids", [Command::link(rid(groups.ids()[0]))]),
        )
        .await
        .unwrap();
    create(
        &mut tx,
        "ir.capability",
        &[&[
            ("name", Value::str("test.cap")),
            ("groups", Value::str("base.group_system")),
        ]],
    )
    .await;
    tx.flush().await.unwrap();
    assert_eq!(policy_rows(&mut tx).await, 1, "one row per transaction");
    assert_eq!(
        committed_policy_rows(&h).await,
        0,
        "nothing before the commit"
    );
    tx.commit().await.unwrap();
    drop(tx);
    assert_eq!(committed_policy_rows(&h).await, 1);

    let mut tx = h.begin().await.unwrap();
    installing(&mut tx);
    let manager = tx
        .env()
        .records("res.groups")
        .unwrap()
        .browse([groups.ids()[1]])
        .unwrap();
    manager
        .write_values(&mut tx, &map(&[("name", Value::str("Manager"))]))
        .await
        .unwrap();
    tx.flush().await.unwrap();
    assert_eq!(
        policy_rows(&mut tx).await,
        2,
        "a new transaction bumps again"
    );
    tx.rollback().await.unwrap();
    drop(tx);
    assert_eq!(
        committed_policy_rows(&h).await,
        1,
        "a rollback takes its bump back"
    );
    h.cleanup().await.unwrap();
}

/// Statement test: raw SQL bumps (the triggers are the only bumpers, so writers outside the ORM
/// are covered), `TRUNCATE` included.
#[tokio::test]
async fn raw_sql_and_truncate_bump() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    installing(&mut tx);
    tx.execute(
        "INSERT INTO ir_capability (name, groups) VALUES ('raw.cap', 'base.group_system')",
        &[],
    )
    .await
    .unwrap();
    assert_eq!(policy_rows(&mut tx).await, 1);
    tx.execute("UPDATE ir_capability SET description = 'x'", &[])
        .await
        .unwrap();
    assert_eq!(policy_rows(&mut tx).await, 1);
    tx.commit().await.unwrap();
    drop(tx);
    let mut tx = h.begin().await.unwrap();
    installing(&mut tx);
    tx.batch("TRUNCATE ir_capability").await.unwrap();
    assert_eq!(policy_rows(&mut tx).await, 2);
    tx.commit().await.unwrap();
    drop(tx);
    assert_eq!(committed_policy_rows(&h).await, 2);
    h.cleanup().await.unwrap();
}

async fn membership_rows(tx: &mut Transaction) -> i64 {
    let rows = tx
        .query(
            "SELECT count(*)::int8 FROM erp_signal_generation WHERE key = 'membership'",
            &[],
        )
        .await
        .unwrap();
    rows[0].get(0)
}

/// Membership is read by the resolver at every transaction start, not compiled: adding a user
/// to a group through `res.users.group_ids` writes `res_groups_users_rel` and `res_users` only,
/// and bumps the `membership` generation (once per transaction), not the policy.
#[tokio::test]
async fn membership_bumps_its_own_generation_not_the_policy() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    installing(&mut tx);
    let group = create(&mut tx, "res.groups", &[&[("name", Value::str("g"))]]).await;
    tx.commit().await.unwrap();
    drop(tx);
    let mut tx = h.begin().await.unwrap();
    installing(&mut tx);
    let before = membership_rows(&mut tx).await;
    // (a raw group insert would bump the policy and spoil the count below)
    let users = create(&mut tx, "res.users", &[&[("name", Value::str("u"))]]).await;
    users
        .write(
            &mut tx,
            RawVals::new().set_commands("group_ids", [Command::link(rid(group.ids()[0]))]),
        )
        .await
        .unwrap();
    tx.flush().await.unwrap();
    let members = tx
        .query("SELECT count(*)::int8 FROM res_groups_users_rel", &[])
        .await
        .unwrap();
    assert_eq!(members[0].get::<_, i64>(0), 1);
    assert_eq!(policy_rows(&mut tx).await, 1, "only the group's creation");
    assert_eq!(
        membership_rows(&mut tx).await - before,
        1,
        "the user's creation and the link bump membership once in this transaction"
    );
    h.cleanup().await.unwrap();
}

/// Statement test (DN §8.3): a savepoint rollback removes the bump row and its flag, so a later
/// bump in the same transaction inserts again; a released savepoint keeps it.
#[tokio::test]
async fn savepoint_rollback_removes_the_bump_and_a_later_bump_inserts_again() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    installing(&mut tx);
    let err = tx
        .savepoint(async |tx: &mut Transaction| {
            tx.execute(
                "INSERT INTO ir_capability (name, groups) VALUES ('sp.cap', 'base.group_system')",
                &[],
            )
            .await?;
            assert_eq!(policy_rows(tx).await, 1);
            Err::<(), _>(erp_sdk::Error::new(ErrorKind::Validation, "roll back"))
        })
        .await
        .unwrap_err();
    assert_eq!(err.message(), "roll back");
    assert_eq!(
        policy_rows(&mut tx).await,
        0,
        "the row went with the savepoint"
    );
    tx.execute(
        "INSERT INTO ir_capability (name, groups) VALUES ('later.cap', 'base.group_system')",
        &[],
    )
    .await
    .unwrap();
    assert_eq!(
        policy_rows(&mut tx).await,
        1,
        "the flag went too: a later write bumps"
    );
    tx.savepoint(async |tx: &mut Transaction| {
        tx.execute("UPDATE ir_capability SET description = 'y'", &[])
            .await?;
        Ok(())
    })
    .await
    .unwrap();
    assert_eq!(policy_rows(&mut tx).await, 1, "still one row");
    tx.commit().await.unwrap();
    drop(tx);
    assert_eq!(committed_policy_rows(&h).await, 1);
    h.cleanup().await.unwrap();
}

/// Statement test (DN §8.3): foreign-key actions fire the triggers of the tables they change, and
/// the transaction still bumps once. Deleting a privilege sets its groups' `privilege_id` to
/// NULL; deleting a group deletes its implied-group rows.
#[tokio::test]
async fn foreign_key_actions_bump_once() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    installing(&mut tx);
    let privilege = create(
        &mut tx,
        "res.groups.privilege",
        &[&[("name", Value::str("P"))]],
    )
    .await;
    let groups = create(
        &mut tx,
        "res.groups",
        &[
            &[
                ("name", Value::str("a")),
                ("privilege_id", Value::Int(privilege.ids()[0])),
            ],
            &[("name", Value::str("b"))],
        ],
    )
    .await;
    groups
        .browse([groups.ids()[1]])
        .unwrap()
        .write(
            &mut tx,
            RawVals::new().set_commands("implied_ids", [Command::link(rid(groups.ids()[0]))]),
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();
    drop(tx);

    // raw deletes, so only the database's foreign-key actions touch the other tables
    let mut tx = h.begin().await.unwrap();
    installing(&mut tx);
    tx.execute(
        "DELETE FROM res_groups_privilege WHERE id = $1",
        &[&privilege.ids()[0]],
    )
    .await
    .unwrap();
    tx.execute("DELETE FROM res_groups WHERE id = $1", &[&groups.ids()[0]])
        .await
        .unwrap();
    let implied = tx
        .query("SELECT count(*)::int8 FROM res_groups_implied_rel", &[])
        .await
        .unwrap();
    assert_eq!(
        implied[0].get::<_, i64>(0),
        0,
        "the cascade removed the edge"
    );
    assert_eq!(
        policy_rows(&mut tx).await,
        2,
        "one more row for the whole transaction"
    );
    tx.commit().await.unwrap();
    drop(tx);

    // the relation table's own trigger: removing one implied-group edge
    let mut tx = h.begin().await.unwrap();
    installing(&mut tx);
    let more = create(
        &mut tx,
        "res.groups",
        &[&[("name", Value::str("c"))], &[("name", Value::str("d"))]],
    )
    .await;
    more.browse([more.ids()[1]])
        .unwrap()
        .write(
            &mut tx,
            RawVals::new().set_commands("implied_ids", [Command::link(rid(more.ids()[0]))]),
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let mut tx = h.begin().await.unwrap();
    installing(&mut tx);
    tx.execute(
        "DELETE FROM res_groups_implied_rel WHERE gid = $1",
        &[&more.ids()[1]],
    )
    .await
    .unwrap();
    assert_eq!(policy_rows(&mut tx).await, 4);
    h.cleanup().await.unwrap();
}

/// Hardening (design note): the guard compares with the transaction id, so a flag left at
/// session level on a pooled connection cannot suppress the bumps of later transactions.
#[tokio::test]
async fn a_session_level_flag_cannot_suppress_bumps() {
    let h = harness().await;
    let client = connect(&h).await;
    client
        .batch_execute("SET erp.policy_bumped = '1'")
        .await
        .unwrap();
    client
        .batch_execute(
            "BEGIN; \
             INSERT INTO ir_capability (name, groups) VALUES ('s.cap', 'base.group_system'); \
             COMMIT",
        )
        .await
        .unwrap();
    // the transaction-local value reverted to the session's at commit
    let flag: String = client
        .query_one("SELECT current_setting('erp.policy_bumped')", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(flag, "1");
    client
        .batch_execute("BEGIN; UPDATE ir_capability SET description = 'z'; COMMIT")
        .await
        .unwrap();
    assert_eq!(committed_policy_rows(&h).await, 2);
    drop(client);
    h.cleanup().await.unwrap();
}

/// Hardening (design note): the function pins its `search_path` with `pg_temp` last, so a
/// temporary table of the same name cannot take the bump.
#[tokio::test]
async fn a_temporary_table_cannot_take_the_bump() {
    let h = harness().await;
    let client = connect(&h).await;
    client
        .batch_execute(
            "CREATE TEMP TABLE erp_signal_generation (id uuid, key text, weight int8, \
                                                      created_at timestamptz); \
             BEGIN; \
             INSERT INTO ir_capability (name, groups) VALUES ('t.cap', 'base.group_system'); \
             COMMIT",
        )
        .await
        .unwrap();
    let temp: i64 = client
        .query_one(
            "SELECT count(*)::int8 FROM pg_temp.erp_signal_generation",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(temp, 0);
    drop(client);
    assert_eq!(committed_policy_rows(&h).await, 1);
    h.cleanup().await.unwrap();
}

/// The wake-up: one empty notification on `erp_signal` per committing transaction, none before
/// the commit and none after a rollback.
#[tokio::test]
async fn one_notification_per_committing_transaction() {
    let h = harness().await;
    let (client, mut connection) = h.db().config().connect(NoTls).await.unwrap();
    let (sender, mut received) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(message) = std::future::poll_fn(|cx| connection.poll_message(cx)).await {
            if let Ok(AsyncMessage::Notification(n)) = message
                && sender
                    .send((n.channel().to_owned(), n.payload().to_owned()))
                    .is_err()
            {
                break;
            }
        }
    });
    client.batch_execute("LISTEN erp_signal").await.unwrap();

    let mut tx = h.begin().await.unwrap();
    installing(&mut tx);
    for name in ["n1", "n2", "n3"] {
        tx.execute(
            "INSERT INTO ir_capability (name, groups) VALUES ($1, 'base.group_system')",
            &[&name],
        )
        .await
        .unwrap();
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(received.try_recv().is_err(), "nothing before the commit");
    tx.rollback().await.unwrap();
    drop(tx);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(received.try_recv().is_err(), "nothing after a rollback");

    let mut tx = h.begin().await.unwrap();
    installing(&mut tx);
    for name in ["m1", "m2"] {
        tx.execute(
            "INSERT INTO ir_capability (name, groups) VALUES ($1, 'base.group_system')",
            &[&name],
        )
        .await
        .unwrap();
    }
    tx.batch("TRUNCATE ir_capability").await.unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let got = tokio::time::timeout(Duration::from_secs(5), received.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(got, ("erp_signal".to_owned(), String::new()));
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        received.try_recv().is_err(),
        "one notification per transaction"
    );
    drop(client);
    h.cleanup().await.unwrap();
}

/// Schema sync: the triggers exist with their canonical comments; a second sync of an unchanged
/// schema sends no trigger or function DDL; a dropped or altered trigger is put back.
#[tokio::test]
async fn schema_sync_creates_the_triggers_once_and_repairs_them() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    installing(&mut tx);
    let all = "erpforall signal trigger v1: bump policy on INSERT OR UPDATE OR DELETE OR TRUNCATE";
    for (table, name) in [
        ("ir_access", "ir_access_policy_bump"),
        ("res_groups", "res_groups_policy_bump"),
        ("res_groups_implied_rel", "res_groups_policy_bump_implied"),
        ("res_groups_privilege", "res_groups_privilege_policy_bump"),
        ("ir_capability", "ir_capability_policy_bump"),
        (
            "ir_model_fields_group_rel",
            "ir_model_fields_policy_bump_groups",
        ),
    ] {
        assert_eq!(
            triggers(&mut tx, table).await,
            vec![(name.to_owned(), Some(all.to_owned()))],
            "{table}"
        );
    }
    assert_eq!(
        triggers(&mut tx, "ir_model_fields").await,
        vec![(
            "ir_model_fields_policy_bump".to_owned(),
            Some(
                "erpforall signal trigger v1: bump policy on INSERT OR DELETE OR TRUNCATE"
                    .to_owned()
            )
        )]
    );
    // membership is read by the resolver: it bumps its own generation, so a read
    // replica that has not replayed a change is detected
    let membership = |events: &str| {
        Some(format!(
            "erpforall signal trigger v1: bump membership on {events}"
        ))
    };
    for (table, name, events) in [
        (
            "res_users",
            "res_users_membership_bump",
            "INSERT OR DELETE OR TRUNCATE OR UPDATE OF \"active\", \"share\", \"company_id\"",
        ),
        (
            "res_groups_users_rel",
            "res_users_membership_bump_groups",
            "INSERT OR UPDATE OR DELETE OR TRUNCATE",
        ),
        (
            "res_company_users_rel",
            "res_users_membership_bump_companies",
            "INSERT OR UPDATE OR DELETE OR TRUNCATE",
        ),
        (
            "res_company",
            "res_company_membership_bump",
            "INSERT OR DELETE OR TRUNCATE OR UPDATE OF \"active\"",
        ),
    ] {
        assert_eq!(
            triggers(&mut tx, table).await,
            vec![(name.to_owned(), membership(events))],
            "{table}"
        );
    }
    // ir_model's removal bumps through its cascades
    assert!(
        triggers(&mut tx, "ir_model").await.is_empty(),
        "ir_model has no trigger"
    );

    tx.capture_statements(true);
    tx.init_models(&[]).await.unwrap();
    let ddl: Vec<&String> = tx
        .captured()
        .iter()
        .filter(|s| {
            s.contains("CREATE OR REPLACE")
                || s.contains("CREATE TABLE IF NOT EXISTS erp_signal_generation")
        })
        .collect();
    assert!(
        ddl.is_empty(),
        "an unchanged sync sends no trigger DDL: {ddl:?}"
    );

    tx.batch(
        "DROP TRIGGER res_groups_policy_bump ON res_groups; \
         COMMENT ON TRIGGER ir_capability_policy_bump ON ir_capability IS 'edited'",
    )
    .await
    .unwrap();
    tx.init_models(&[]).await.unwrap();
    assert_eq!(
        triggers(&mut tx, "res_groups").await,
        vec![("res_groups_policy_bump".to_owned(), Some(all.to_owned()))]
    );
    assert_eq!(
        triggers(&mut tx, "ir_capability").await,
        vec![("ir_capability_policy_bump".to_owned(), Some(all.to_owned()))]
    );
    h.cleanup().await.unwrap();
}

/// An `ir.model` row and one `ir.model.fields` row of it, committed; returns their ids.
async fn reflected_model(h: &Harness) -> (i64, i64) {
    let mut tx = h.begin().await.unwrap();
    installing(&mut tx);
    let model = create(
        &mut tx,
        "ir.model",
        &[&[
            ("name", Value::str("Thing")),
            ("model", Value::str("x_thing")),
        ]],
    )
    .await;
    let field = create(
        &mut tx,
        "ir.model.fields",
        &[&[
            ("name", Value::str("x_secret")),
            ("model", Value::str("x_thing")),
            ("model_id", Value::Int(model.ids()[0])),
            ("ttype", Value::str("char")),
            ("state", Value::str("manual")),
        ]],
    )
    .await;
    tx.commit().await.unwrap();
    (model.ids()[0], field.ids()[0])
}

/// Statement tests (DN §8.3): `ir_model_fields` bumps on insert and delete but not when its other
/// columns change (a field's label or help text); its groups relation bumps on every change.
#[tokio::test]
async fn field_groups_bump_but_field_texts_do_not() {
    let h = harness().await;
    let (_, field) = reflected_model(&h).await;
    assert_eq!(
        committed_policy_rows(&h).await,
        1,
        "inserting the field bumped"
    );

    let mut tx = h.begin().await.unwrap();
    installing(&mut tx);
    let fields = tx
        .env()
        .records("ir.model.fields")
        .unwrap()
        .browse([field])
        .unwrap();
    fields
        .write_values(
            &mut tx,
            &map(&[
                ("field_description", Value::str("Secret")),
                ("help", Value::str("Only for managers")),
            ]),
        )
        .await
        .unwrap();
    tx.execute(
        "UPDATE ir_model_fields SET help = 'raw help' WHERE id = $1",
        &[&field],
    )
    .await
    .unwrap();
    assert_eq!(
        policy_rows(&mut tx).await,
        1,
        "label and help updates do not bump"
    );

    let group = create(
        &mut tx,
        "res.groups",
        &[&[("name", Value::str("managers"))]],
    )
    .await;
    tx.commit().await.unwrap();
    drop(tx);
    assert_eq!(committed_policy_rows(&h).await, 2, "the new group bumped");

    let mut tx = h.begin().await.unwrap();
    installing(&mut tx);
    let fields = tx
        .env()
        .records("ir.model.fields")
        .unwrap()
        .browse([field])
        .unwrap();
    fields
        .write(
            &mut tx,
            RawVals::new().set_commands("groups", [Command::link(rid(group.ids()[0]))]),
        )
        .await
        .unwrap();
    tx.flush().await.unwrap();
    assert_eq!(
        policy_rows(&mut tx).await,
        3,
        "adding a group to the field bumps"
    );
    tx.commit().await.unwrap();
    drop(tx);

    let mut tx = h.begin().await.unwrap();
    installing(&mut tx);
    tx.execute("DELETE FROM ir_model_fields WHERE id = $1", &[&field])
        .await
        .unwrap();
    assert_eq!(
        policy_rows(&mut tx).await,
        4,
        "deleting the field bumps once"
    );
    h.cleanup().await.unwrap();
}

/// Statement test (DN §8.3, Revision 3): `ir_model` carries no trigger; removing a model bumps
/// once through its cascades into `ir_access` and `ir_model_fields`.
#[tokio::test]
async fn removing_a_model_bumps_once_through_its_cascades() {
    let h = harness().await;
    let (model, _) = reflected_model(&h).await;
    let mut tx = h.begin().await.unwrap();
    installing(&mut tx);
    create(
        &mut tx,
        "ir.access",
        &[&[
            ("name", Value::str("everyone reads things")),
            ("model_id", Value::Int(model)),
            ("operation", Value::str("r")),
        ]],
    )
    .await;
    assert!(
        triggers(&mut tx, "ir_model").await.is_empty(),
        "no trigger on ir_model"
    );
    tx.commit().await.unwrap();
    drop(tx);
    assert_eq!(committed_policy_rows(&h).await, 2);

    let mut tx = h.begin().await.unwrap();
    installing(&mut tx);
    tx.execute("DELETE FROM ir_model WHERE id = $1", &[&model])
        .await
        .unwrap();
    let left = tx
        .query(
            "SELECT (SELECT count(*) FROM ir_access)::int8, \
                    (SELECT count(*) FROM ir_model_fields WHERE model = 'x_thing')::int8",
            &[],
        )
        .await
        .unwrap();
    assert_eq!((left[0].get::<_, i64>(0), left[0].get::<_, i64>(1)), (0, 0));
    assert_eq!(policy_rows(&mut tx).await, 3, "one bump for the cascades");
    tx.commit().await.unwrap();
    drop(tx);
    assert_eq!(committed_policy_rows(&h).await, 3);
    h.cleanup().await.unwrap();
}

/// `ir.access` itself: ORM create, write and unlink bump once per transaction; deleting the
/// group of a permission removes the permission (cascade) and bumps once.
#[tokio::test]
async fn access_rows_bump_and_follow_their_group() {
    let h = harness().await;
    let (model, _) = reflected_model(&h).await;
    let mut tx = h.begin().await.unwrap();
    installing(&mut tx);
    let group = create(&mut tx, "res.groups", &[&[("name", Value::str("readers"))]]).await;
    let access = create(
        &mut tx,
        "ir.access",
        &[&[
            ("name", Value::str("readers read things")),
            ("model_id", Value::Int(model)),
            ("group_id", Value::Int(group.ids()[0])),
            ("operation", Value::str("r")),
            ("domain", Value::str("[('x_secret', '!=', False)]")),
        ]],
    )
    .await;
    access
        .write_values(&mut tx, &map(&[("operation", Value::str("ru"))]))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    assert_eq!(committed_policy_rows(&h).await, 2);

    let mut tx = h.begin().await.unwrap();
    installing(&mut tx);
    let group = tx
        .env()
        .records("res.groups")
        .unwrap()
        .browse(group.ids())
        .unwrap();
    group.unlink(&mut tx).await.unwrap();
    let rules = tx
        .query("SELECT count(*)::int8 FROM ir_access", &[])
        .await
        .unwrap();
    assert_eq!(
        rules[0].get::<_, i64>(0),
        0,
        "the permission went with its group"
    );
    assert_eq!(policy_rows(&mut tx).await, 3);
    tx.commit().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}
