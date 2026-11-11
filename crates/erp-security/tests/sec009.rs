#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod support;

use std::num::NonZeroU64;
use std::sync::Arc;

use erp_fields::ErrorKind;
use erp_orm::{
    BeginOptions, GroupId, Operation, Orm, PolicyProvider, PrincipalBuilder, Resolution,
    Transaction, UserId,
};
use erp_registry::{AddonDescriptor, FieldDecl, ModelDecl};
use erp_security::resolver::Resolver;
use erp_security::source::PolicySource;
use erp_testkit::tokio_postgres::Client;
use erp_testkit::tokio_postgres::types::ToSql;
use erp_testkit::{Harness, assert_query_count};
use support::{connect, pool};

static FIXTURE: AddonDescriptor = AddonDescriptor {
    name: "x_access",
    depends: &["base"],
    exact_only: false,
};

const OBJ_FIELDS: &[FieldDecl] = &[FieldDecl::char("name")];

async fn harness() -> Harness {
    Harness::builder([
        &erp_addon_base::ADDON,
        &erp_data::model_data::ADDON,
        &FIXTURE,
    ])
    .with_decls([
        erp_data::ir_model_data_decl(),
        ModelDecl::new("x_access", "x.some_obj")
            .description("Object For Test Access Right")
            .fields(OBJ_FIELDS),
    ])
    .build()
    .await
    .expect("harness")
}

/// SQL compared with whitespace runs collapsed (line wrapping of an expectation never matters).
#[track_caller]
fn assert_sql(actual: &str, expected: &str) {
    let norm = |sql: &str| sql.split_whitespace().collect::<Vec<_>>().join(" ");
    assert_eq!(norm(actual), norm(expected));
}

async fn one(client: &Client, sql: &str, params: &[&(dyn ToSql + Sync)]) -> i64 {
    client.query_one(sql, params).await.unwrap().get(0)
}

struct Seeded {
    bob: i64,
    /// `base.group_user`, Group 1, Group 2 and `base.group_no_one`.
    groups: [i64; 4],
    group1: i64,
    model: i64,
    /// Mario, Luigi, Peach, Toad, Yoshi, Bowser.
    records: Vec<i64>,
}

async fn seed(client: &Client) -> Seeded {
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
    let no_one = group("Technical Features").await;
    client
        .execute(
            "INSERT INTO ir_model_data (module, name, model, res_id) \
             VALUES ('base', 'group_user', 'res.groups', $1), \
                    ('base', 'group_no_one', 'res.groups', $2)",
            &[&internal, &no_one],
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
    for gid in [internal, group1, group2, no_one] {
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
         VALUES ('Object For Test Access Right', 'x.some_obj', 'base', false) \
         RETURNING id::int8",
        &[],
    )
    .await;
    let mut records = Vec::new();
    for name in ["Mario", "Luigi", "Peach", "Toad", "Yoshi", "Bowser"] {
        records.push(
            one(
                client,
                "INSERT INTO x_some_obj (name) VALUES ($1) RETURNING id::int8",
                &[&name],
            )
            .await,
        );
    }
    Seeded {
        bob,
        groups: [internal, group1, group2, no_one],
        group1,
        model,
        records,
    }
}

/// The rows of a case: `(name, ids the domain names (None: no domain), with Group 1)`.
type Rules<'a> = &'a [(&'a str, Option<&'a [i64]>, bool)];

async fn set_rules(client: &Client, s: &Seeded, rules: Rules<'_>) {
    client
        .execute("DELETE FROM ir_access WHERE model_id = $1", &[&s.model])
        .await
        .unwrap();
    for (name, records, with_group) in rules {
        let domain = records.map(|ids| {
            let list: Vec<String> = ids.iter().map(i64::to_string).collect();
            format!("[('id', 'in', [{}])]", list.join(", "))
        });
        let group = with_group.then_some(s.group1);
        client
            .execute(
                "INSERT INTO ir_access (name, model_id, group_id, operation, domain, active) \
                 VALUES ($1, $2, $3, 'r', $4, true)",
                &[name, &s.model, &group, &domain],
            )
            .await
            .unwrap();
    }
}

async fn policy_orm(h: &Harness) -> (Orm, Arc<PolicySource>) {
    let source = Arc::new(
        PolicySource::build(Arc::clone(h.registry()), pool(h, 2))
            .await
            .unwrap(),
    );
    let provider: Arc<dyn PolicyProvider> = Arc::clone(&source) as Arc<dyn PolicyProvider>;
    let orm = Orm::builder(Arc::clone(h.registry()), pool(h, 2))
        .policy(provider)
        .resolver(Resolver::id_order())
        .build()
        .unwrap();
    (orm, source)
}

/// Bob's transaction: resolved by the resolver, or, in debug mode, with his effective groups
/// from the snapshot and the debug flag (the session's, which sets).
async fn begin_bob(orm: &Orm, source: &PolicySource, s: &Seeded, debug: bool) -> Transaction {
    let bob = UserId::new(s.bob).unwrap();
    if !debug {
        return orm.begin(bob).await.unwrap();
    }
    let direct: Vec<GroupId> = s.groups.iter().map(|g| GroupId::new(*g).unwrap()).collect();
    let groups = source.current().groups(&direct).unwrap();
    let principal = PrincipalBuilder::user(bob)
        .resolution(Resolution::in_id_order(groups, [], NonZeroU64::MIN).unwrap())
        .debug(true)
        .build()
        .unwrap();
    orm.begin_opts(BeginOptions::for_principal(principal))
        .await
        .unwrap()
}

/// The error path
/// costs one evidence statement, and one more for the display names in debug mode.
#[tokio::test]
async fn record_level_error_messages() {
    let h = harness().await;
    let client = connect(&h).await;
    let s = seed(&client).await;
    let all = s.records.clone();
    let humans = &all[..3];
    let good_guys = &all[..5];
    let cases: [(Rules<'_>, &[&str]); 3] = [
        (
            &[("Restrict to humans", Some(humans), true)],
            &["Restrict to humans"],
        ),
        (
            &[
                ("See all", None, true),
                ("Restrict to humans", Some(humans), false),
            ],
            &["Restrict to humans"],
        ),
        (
            &[
                ("See good guys", Some(good_guys), true),
                ("Restrict to humans", Some(humans), false),
            ],
            &["See good guys", "Restrict to humans"],
        ),
    ];
    let head = format!(
        "Uh-oh! Looks like you have stumbled upon some top-secret records.\n\nSorry, Bob Bobman \
         (id={}) doesn't have 'read' access to:",
        s.bob
    );
    let tail = "If you really, really need access, perhaps you can win over your friendly \
                administrator with a batch of freshly baked cookies.";
    for (rules, blamed) in cases {
        set_rules(&client, &s, rules).await;
        let (orm, source) = policy_orm(&h).await;
        for debug in [false, true] {
            let mut tx = begin_bob(&orm, &source, &s, debug).await;
            let records = tx
                .env()
                .records("x.some_obj")
                .unwrap()
                .browse(all.iter().copied())
                .unwrap();
            let readable = records
                .filtered_access(&mut tx, Operation::Read)
                .await
                .unwrap();
            assert_eq!(readable.ids(), humans);
            // the humans are memoized: the check selects the other three, then the error path
            let statements = if debug { 3 } else { 2 };
            let err = assert_query_count!(tx, >= statements, <= statements, {
                records
                    .check_access(&mut tx, Operation::Read)
                    .await
                    .unwrap_err()
            });
            assert_eq!(err.kind(), ErrorKind::Access, "{err}");
            let expected = if debug {
                let listing: Vec<String> = ["Toad", "Yoshi", "Bowser"]
                    .iter()
                    .zip(&all[3..])
                    .map(|(name, id)| {
                        format!("- Object For Test Access Right, {name} (x.some_obj: {id})")
                    })
                    .collect();
                let blame: Vec<String> = blamed.iter().map(|n| format!("- {n}")).collect();
                format!(
                    "{head}\n{}\n\nBlame the following accesses:\n{}\n\n{tail}",
                    listing.join("\n"),
                    blame.join("\n")
                )
            } else {
                format!("{head}\n- Object For Test Access Right (x.some_obj)\n\n{tail}")
            };
            assert_eq!(err.message(), expected, "debug: {debug}");
            drop(tx);
        }
    }
    drop(client);
    h.cleanup().await.unwrap();
}

/// The evidence statement: one `(<rule>) IS TRUE` column per applicable rule, the subject's
/// name, the denied records in their order, at most six; a denied record that no longer exists
/// is skipped, and when none exists the operation is left to report them missing.
#[tokio::test]
async fn the_evidence_statement() {
    let h = harness().await;
    let client = connect(&h).await;
    let s = seed(&client).await;
    let all = s.records.clone();
    set_rules(
        &client,
        &s,
        &[
            ("See good guys", Some(&all[..5]), true),
            ("Restrict to humans", Some(&all[..3]), false),
        ],
    )
    .await;
    let (orm, source) = policy_orm(&h).await;
    let mut tx = begin_bob(&orm, &source, &s, false).await;
    // Bowser first, then Yoshi: the message is about the records in their order
    let records = tx
        .env()
        .records("x.some_obj")
        .unwrap()
        .browse([all[5], all[4]])
        .unwrap();
    tx.capture_statements(true);
    let err = records
        .check_access(&mut tx, Operation::Read)
        .await
        .unwrap_err();
    let evidence = tx.captured().last().cloned().unwrap();
    tx.capture_statements(false);
    assert_eq!(err.kind(), ErrorKind::Access, "{err}");
    assert_sql(
        &evidence,
        "SELECT \"x_some_obj\".\"id\", (\"x_some_obj\".\"id\" = ANY($1::int8[])) IS TRUE, \
         (\"x_some_obj\".\"id\" = ANY($2::int8[])) IS TRUE, (SELECT \"__u\".\"name\" FROM \"res_users\" AS \"__u\" WHERE \"__u\".\"id\" = $3::int8) FROM \
         unnest($4::int8[]) WITH ORDINALITY AS \"__d\"(\"id\", \"ord\") JOIN \"x_some_obj\" ON \
         \"x_some_obj\".\"id\" = \"__d\".\"id\" ORDER BY \"__d\".\"ord\" LIMIT 6",
    );
    drop(tx);

    // Toad was deleted meanwhile (by superuser): the message stays about the others
    let mut su = orm.begin(erp_orm::SUPERUSER_ID).await.unwrap();
    su.env()
        .records("x.some_obj")
        .unwrap()
        .browse([all[3]])
        .unwrap()
        .unlink(&mut su)
        .await
        .unwrap();
    su.commit().await.unwrap();
    let mut tx = begin_bob(&orm, &source, &s, true).await;
    let records = tx
        .env()
        .records("x.some_obj")
        .unwrap()
        .browse([all[3], all[4]])
        .unwrap();
    let err = records
        .check_access(&mut tx, Operation::Read)
        .await
        .unwrap_err();
    assert!(
        err.message().contains(&format!(
            "- Object For Test Access Right, Yoshi (x.some_obj: {})",
            all[4]
        )),
        "{err}"
    );
    assert!(!err.message().contains("Toad"), "{err}");
    // only deleted records: no access error, the operation reports them missing
    let gone = tx
        .env()
        .records("x.some_obj")
        .unwrap()
        .browse([all[3]])
        .unwrap();
    gone.check_access(&mut tx, Operation::Read).await.unwrap();
    drop(tx);
    drop(client);
    h.cleanup().await.unwrap();
}

/// Debug mode without `base.group_no_one` gets the plain text and no display-name statement:
/// the policy does not list records for that principal, so the engine reads no names.
#[tokio::test]
async fn debug_without_the_technical_group_reads_no_names() {
    let h = harness().await;
    let client = connect(&h).await;
    let s = seed(&client).await;
    let all = s.records.clone();
    set_rules(
        &client,
        &s,
        &[("Restrict to humans", Some(&all[..3]), true)],
    )
    .await;
    // not in Technical Features: the resolver reads the groups from the database
    client
        .execute(
            "DELETE FROM res_groups_users_rel WHERE uid = $1 AND gid = $2",
            &[&s.bob, &s.groups[3]],
        )
        .await
        .unwrap();
    let (orm, source) = policy_orm(&h).await;
    // internal user, Group 1 and Group 2, debug on, but not Technical Features
    let direct: Vec<GroupId> = s.groups[..3]
        .iter()
        .map(|g| GroupId::new(*g).unwrap())
        .collect();
    let groups = source.current().groups(&direct).unwrap();
    let principal = PrincipalBuilder::user(UserId::new(s.bob).unwrap())
        .resolution(Resolution::in_id_order(groups, [], NonZeroU64::MIN).unwrap())
        .debug(true)
        .build()
        .unwrap();
    let mut tx = orm
        .begin_opts(BeginOptions::for_principal(principal))
        .await
        .unwrap();
    let records = tx
        .env()
        .records("x.some_obj")
        .unwrap()
        .browse(all.iter().copied())
        .unwrap();
    records
        .filtered_access(&mut tx, Operation::Read)
        .await
        .unwrap();
    // the other three are selected by the check, then one evidence statement and no names
    let err = assert_query_count!(tx, >= 2, <= 2, {
        records
            .check_access(&mut tx, Operation::Read)
            .await
            .unwrap_err()
    });
    assert_eq!(err.kind(), ErrorKind::Access, "{err}");
    assert_eq!(
        err.message(),
        format!(
            "Uh-oh! Looks like you have stumbled upon some top-secret records.\n\nSorry, Bob \
             Bobman (id={}) doesn't have 'read' access to:\n- Object For Test Access Right \
             (x.some_obj)\n\nIf you really, really need access, perhaps you can win over your \
             friendly administrator with a batch of freshly baked cookies.",
            s.bob
        )
    );
    drop(tx);
    drop(client);
    h.cleanup().await.unwrap();
}

/// A record set that repeats ids is explained once per record: the evidence statement lists
/// each denied record once, in order of first occurrence, and stays a single statement (the
/// evidence query `unnest`s the ids, which keeps duplicates, so the error path removes them
/// itself instead of relying on its callers).
#[tokio::test]
async fn repeated_ids_are_explained_once() {
    let h = harness().await;
    let client = connect(&h).await;
    let s = seed(&client).await;
    let all = s.records.clone();
    set_rules(
        &client,
        &s,
        &[("Restrict to humans", Some(&all[..3]), true)],
    )
    .await;
    let (orm, source) = policy_orm(&h).await;
    let mut tx = begin_bob(&orm, &source, &s, true).await;
    // Yoshi, Toad, Yoshi, Yoshi, Toad, Bowser, Bowser: three denied records, each repeated
    let records = tx
        .env()
        .records("x.some_obj")
        .unwrap()
        .browse([all[4], all[3], all[4], all[4], all[3], all[5], all[5]])
        .unwrap();
    records
        .filtered_access(&mut tx, Operation::Read)
        .await
        .unwrap();
    tx.capture_statements(true);
    let err = records
        .check_access(&mut tx, Operation::Read)
        .await
        .unwrap_err();
    let statements = tx.captured().to_vec();
    tx.capture_statements(false);
    assert_eq!(err.kind(), ErrorKind::Access, "{err}");
    let listing: Vec<&str> = err
        .message()
        .lines()
        .filter(|line| line.starts_with("- Object For Test Access Right,"))
        .collect();
    let expected: Vec<String> = [("Yoshi", all[4]), ("Toad", all[3]), ("Bowser", all[5])]
        .iter()
        .map(|(name, id)| format!("- Object For Test Access Right, {name} (x.some_obj: {id})"))
        .collect();
    assert_eq!(listing, expected, "{err}");
    let evidence = statements
        .iter()
        .filter(|sql| sql.contains("WITH ORDINALITY"))
        .count();
    assert_eq!(evidence, 1, "{statements:#?}");
    drop(tx);
    drop(client);
    h.cleanup().await.unwrap();
}
