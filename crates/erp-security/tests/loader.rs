//! The policy loader (
//! and §7 "Snapshot build ≤ 5 statements"): every policy row and the `policy` generation in four
//! statements of one `REPEATABLE READ` snapshot, from the base tables.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use erp_fields::{ErrorKind, Value};
use erp_orm::{GroupId, QualifiedXmlId, Records, Transaction};
use erp_registry::RegistryBuilder;
use erp_security::loader::{STATEMENTS, check_registry, load_rows, load_rows_from};
use erp_security::rows::{
    CapabilityRow, FieldGroupsRow, GroupRow, Implication, PolicyRows, PrivilegeId, PrivilegeRow,
    RuleId, RuleRow,
};
use erp_testkit::Harness;
use erp_testkit::tokio_postgres::NoTls;

async fn harness() -> Harness {
    Harness::builder([&erp_addon_base::ADDON, &erp_data::model_data::ADDON])
        .with_decls([erp_data::ir_model_data_decl()])
        .build()
        .await
        .expect("harness")
}

fn vals(pairs: &[(&str, Value)]) -> BTreeMap<String, Value> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), v.clone()))
        .collect()
}

async fn create(tx: &mut Transaction, model: &str, rows: &[&[(&str, Value)]]) -> Records {
    let records = tx.env().records(model).unwrap();
    let rows: Vec<_> = rows.iter().map(|r| vals(r)).collect();
    // these databases have none of base's data: the base installation state lets groups be
    // created without the regular-user marker group
    let was = erp_addon_base::group_graph::base_installing(tx);
    erp_testkit::base_install::set_base_installing(tx, true).unwrap();
    let created = records.create_values(tx, &rows).await.unwrap();
    erp_testkit::base_install::set_base_installing(tx, was).unwrap();
    created
}

async fn xmlid(tx: &mut Transaction, module: &str, name: &str, model: &str, id: i64) {
    tx.execute(
        "INSERT INTO ir_model_data (module, name, model, res_id) VALUES ($1, $2, $3, $4)",
        &[&module, &name, &model, &id],
    )
    .await
    .unwrap();
}

fn qx(text: &str) -> QualifiedXmlId {
    QualifiedXmlId::parse(text).unwrap()
}

fn gid(id: i64) -> GroupId {
    GroupId::new(id).unwrap()
}

/// What `seed` wrote, for building the expected rows.
struct Seeded {
    privilege: i64,
    user: i64,
    manager: i64,
    rule_user: i64,
    restriction: i64,
}

/// A small policy: a privilege, two groups (manager implies user) with XML ids, a model with a
/// permission, a restriction and an inactive rule, two capability rows and a custom field with
/// groups. Not committed.
async fn seed(tx: &mut Transaction) -> Seeded {
    let privilege = create(
        tx,
        "res.groups.privilege",
        &[&[("name", Value::str("Sales")), ("sequence", Value::Int(5))]],
    )
    .await
    .ids()[0];
    let groups = create(
        tx,
        "res.groups",
        &[
            &[
                ("name", Value::str("Internal User")),
                ("privilege_id", Value::Int(privilege)),
                ("sequence", Value::Int(1)),
            ],
            &[
                ("name", Value::str("Gérant ✓")),
                ("share", Value::Bool(false)),
                ("api_key_duration", Value::Float(30.0)),
            ],
        ],
    )
    .await
    .ids();
    let (user, manager) = (groups[0], groups[1]);
    tx.execute(
        "INSERT INTO res_groups_implied_rel (gid, hid) VALUES ($1, $2)",
        &[&manager, &user],
    )
    .await
    .unwrap();
    xmlid(tx, "base", "group_user", "res.groups", user).await;
    xmlid(tx, "sale", "group_user_alias", "res.groups", user).await;
    xmlid(tx, "", "no_module", "res.groups", user).await;
    xmlid(tx, "base", "group_manager", "res.groups", manager).await;
    let model = create(
        tx,
        "ir.model",
        &[&[
            ("name", Value::str("Thing")),
            ("model", Value::str("x_thing")),
        ]],
    )
    .await
    .ids()[0];
    let rules = create(
        tx,
        "ir.access",
        &[
            &[
                ("name", Value::str("users read things")),
                ("model_id", Value::Int(model)),
                ("group_id", Value::Int(user)),
                ("operation", Value::str("r")),
            ],
            &[
                ("name", Value::str("own company")),
                ("model_id", Value::Int(model)),
                ("operation", Value::str("crud")),
                ("domain", Value::str("[('company_id', 'in', company_ids)]")),
            ],
            &[
                ("name", Value::str("archived rule")),
                ("model_id", Value::Int(model)),
                ("operation", Value::str("d")),
                ("active", Value::Bool(false)),
            ],
        ],
    )
    .await
    .ids();
    xmlid(tx, "x", "access_thing_user", "ir.access", rules[0]).await;
    xmlid(tx, "x", "access_thing_user_later", "ir.access", rules[0]).await;
    create(
        tx,
        "ir.capability",
        &[
            &[
                ("name", Value::str("principal.impersonate")),
                ("groups", Value::str("base.group_system")),
            ],
            &[
                ("name", Value::str("audit.read")),
                (
                    "groups",
                    Value::str("base.group_system,base.group_erp_manager"),
                ),
            ],
        ],
    )
    .await;
    let fields = create(
        tx,
        "ir.model.fields",
        &[
            &[
                ("name", Value::str("x_secret")),
                ("model", Value::str("x_thing")),
                ("model_id", Value::Int(model)),
                ("ttype", Value::str("char")),
                ("state", Value::str("manual")),
            ],
            &[
                ("name", Value::str("x_open")),
                ("model", Value::str("x_thing")),
                ("model_id", Value::Int(model)),
                ("ttype", Value::str("char")),
                ("state", Value::str("manual")),
            ],
        ],
    )
    .await
    .ids();
    tx.execute(
        "INSERT INTO ir_model_fields_group_rel (field_id, group_id) VALUES ($1, $2), ($1, $3)",
        &[&fields[0], &manager, &user],
    )
    .await
    .unwrap();
    tx.flush().await.unwrap();
    Seeded {
        privilege,
        user,
        manager,
        rule_user: rules[0],
        restriction: rules[1],
    }
}

/// The rows `seed` wrote, built with the rows API.
fn expected(s: &Seeded, generation: u64) -> PolicyRows {
    PolicyRows::builder(generation)
        .privilege(
            PrivilegeRow::new(PrivilegeId::new(s.privilege).unwrap(), "Sales")
                .unwrap()
                .with_sequence(5),
        )
        .group(
            GroupRow::new(gid(s.user), "Internal User")
                .unwrap()
                .with_privilege(PrivilegeId::new(s.privilege).unwrap())
                .with_sequence(1)
                .with_xmlid(qx("base.group_user"))
                .with_xmlid(qx("sale.group_user_alias")),
        )
        .group(
            GroupRow::new(gid(s.manager), "Gérant ✓")
                .unwrap()
                .with_xmlid(qx("base.group_manager"))
                .with_api_key_duration(30.0)
                .unwrap(),
        )
        .implication(Implication::new(gid(s.manager), gid(s.user)))
        .rule(
            RuleRow::new(
                RuleId::new(s.rule_user).unwrap(),
                "users read things",
                "x_thing",
                "r",
            )
            .unwrap()
            .with_group(gid(s.user))
            .with_xmlid(qx("x.access_thing_user")),
        )
        .rule(
            RuleRow::new(
                RuleId::new(s.restriction).unwrap(),
                "own company",
                "x_thing",
                "crud",
            )
            .unwrap()
            .with_domain("[('company_id', 'in', company_ids)]"),
        )
        .capability(CapabilityRow::new("principal.impersonate", "base.group_system").unwrap())
        .capability(
            CapabilityRow::new("audit.read", "base.group_system,base.group_erp_manager").unwrap(),
        )
        .field_group(
            FieldGroupsRow::new("x_thing", "x_secret", [gid(s.user), gid(s.manager)]).unwrap(),
        )
        .build()
        .unwrap()
}

/// DN §7 "Snapshot build" / "In-transaction private rebuild": at most 5 statements (the loader
/// sends exactly its five, in order); the rows come back sorted and complete: inactive rules,
/// XML ids without a module and fields without groups are not read, a rule keeps its oldest XML
/// id, a group all of its own. Inside the writing transaction the loader sees the uncommitted
/// rows and their uncommitted bump.
#[tokio::test]
async fn loads_every_policy_row_in_five_statements() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let seeded = seed(&mut tx).await;
    tx.capture_statements(true);
    let rows = erp_testkit::assert_query_count!(tx, >= 5, <= 5, {
        load_rows(&mut tx).await.unwrap()
    });
    assert_eq!(tx.captured(), STATEMENTS.map(str::to_owned).as_slice());
    assert_eq!(
        rows.generation(),
        1,
        "the transaction's own bump is in its snapshot"
    );
    assert_eq!(rows, expected(&seeded, 1));
    h.cleanup().await.unwrap();
}

/// The generation rides on a one-row driver: an empty policy still reads it, 0 without any
/// `policy` row and the committed sum after bumps.
#[tokio::test]
async fn the_generation_is_read_without_rules() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let rows = load_rows(&mut tx).await.unwrap();
    assert_eq!(rows.generation(), 0);
    assert!(rows.rules().is_empty() && rows.groups().is_empty());
    tx.rollback().await.unwrap();
    drop(tx);
    let mut tx = h.begin().await.unwrap();
    tx.batch(
        "INSERT INTO erp_signal_generation (key, weight) VALUES ('policy', 5), ('registry', 7)",
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let mut client = connect(&h).await;
    let rows = load_rows_from(&mut client).await.unwrap();
    assert_eq!(rows.generation(), 5, "only the policy key counts");
    h.cleanup().await.unwrap();
}

async fn connect(h: &Harness) -> erp_testkit::tokio_postgres::Client {
    let (client, connection) = h.db().config().connect(NoTls).await.unwrap();
    tokio::spawn(async move {
        if let Err(error) = connection.await {
            eprintln!("loader test: connection closed with an error: {error}");
        }
    });
    client
}

/// One snapshot (DN §2.3, R-M2): while writers commit transactions that each add one group, one
/// permission of that group and (by trigger) one generation, every load on its own connection
/// sees exactly as many groups as permissions as generation steps. The groups are read first and
/// the rules (with the generation) second, so a load that read the two in different snapshots
/// would see a rule of a group it did not read (a row error) or a generation ahead of its groups.
#[tokio::test]
async fn a_shared_load_reads_one_snapshot() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let model = create(
        &mut tx,
        "ir.model",
        &[&[
            ("name", Value::str("Thing")),
            ("model", Value::str("x_thing")),
        ]],
    )
    .await
    .ids()[0];
    tx.commit().await.unwrap();
    drop(tx);
    let base = {
        let mut client = connect(&h).await;
        load_rows_from(&mut client).await.unwrap().generation()
    };

    let stop = Arc::new(AtomicBool::new(false));
    let mut writers = Vec::new();
    for w in 0..3 {
        let stop = Arc::clone(&stop);
        let mut client = connect(&h).await;
        writers.push(tokio::spawn(async move {
            let mut n = 0u32;
            while !stop.load(Ordering::Relaxed) {
                let tx = client.transaction().await.unwrap();
                let group: i64 = tx
                    .query_one(
                        "INSERT INTO res_groups (name) VALUES ($1) RETURNING id::int8",
                        &[&format!("w{w}-{n}")],
                    )
                    .await
                    .unwrap()
                    .get(0);
                tx.execute(
                    "INSERT INTO ir_access (name, model_id, group_id, operation, active) \
                     VALUES ('r', $1, $2, 'r', true)",
                    &[&model, &group],
                )
                .await
                .unwrap();
                tx.commit().await.unwrap();
                n += 1;
            }
            n
        }));
    }
    let mut client = connect(&h).await;
    // load until commits have landed between loads several times (bounded), so the loads
    // really interleave with the writers
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    let mut loads = 0;
    let mut seen: std::collections::BTreeSet<usize> = std::collections::BTreeSet::new();
    while loads < 40 || seen.len() < 5 {
        assert!(
            std::time::Instant::now() < deadline,
            "the writers did not commit between loads ({loads} loads, {} generations seen)",
            seen.len()
        );
        let rows = load_rows_from(&mut client).await.unwrap();
        let steps = usize::try_from(rows.generation() - base).unwrap();
        assert_eq!(
            rows.groups().len(),
            steps,
            "groups and generation of one snapshot"
        );
        assert_eq!(
            rows.rules().len(),
            steps,
            "rules and generation of one snapshot"
        );
        seen.insert(steps);
        loads += 1;
    }
    stop.store(true, Ordering::Relaxed);
    let mut committed = 0;
    for writer in writers {
        committed += writer.await.unwrap();
    }
    assert!(committed > 0, "the writers committed while the loads ran");
    h.cleanup().await.unwrap();
}

/// The loader's models are checked once, with the fix in the message.
#[test]
fn missing_policy_models_are_named() {
    let without_data = RegistryBuilder::new([&erp_addon_base::ADDON])
        .build()
        .unwrap();
    let err = check_registry(&without_data).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "the access policy reads model 'ir.model.data', which is not in the registry: add \
         erp_data::model_data::ADDON with the declaration erp_data::IR_MODEL_DATA"
    );
    let complete = RegistryBuilder::new([&erp_addon_base::ADDON, &erp_data::model_data::ADDON])
        .with_decls([&erp_data::IR_MODEL_DATA])
        .build()
        .unwrap();
    check_registry(&complete).unwrap();
}

/// A stored external id that is not `module.name` fails the load loudly; so does a group row the
/// rows refuse (a negative API key duration written past the ORM).
#[tokio::test]
async fn invalid_stored_rows_fail_the_load() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let group = create(&mut tx, "res.groups", &[&[("name", Value::str("g"))]])
        .await
        .ids()[0];
    xmlid(&mut tx, "base", "two.dots", "res.groups", group).await;
    let err = load_rows(&mut tx).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "the access policy cannot use the external id 'base.two.dots': invalid XML id \
         'base.two.dots': it has more than one dot"
    );
    tx.rollback().await.unwrap();
    drop(tx);

    let mut tx = h.begin().await.unwrap();
    tx.batch("ALTER TABLE res_groups DROP CONSTRAINT res_groups_check_api_key_duration")
        .await
        .unwrap();
    let bad: i64 = tx
        .query(
            "INSERT INTO res_groups (name, api_key_duration) VALUES ('bad', -1) RETURNING id::int8",
            &[],
        )
        .await
        .unwrap()[0]
        .get(0);
    let err = load_rows(&mut tx).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        format!(
            "policy rows: res.groups {bad} has the API key duration -1: it must be a finite \
             number of days, at least 0"
        )
    );
    h.cleanup().await.unwrap();
}

/// Rows are untrusted input: a policy larger than the limits of the rows is
/// refused by the size check, before its rows are read into memory.
#[tokio::test]
async fn an_oversized_policy_is_refused_before_it_is_loaded() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let max = erp_security::rows::MAX_PRIVILEGES;
    tx.execute(
        "INSERT INTO res_groups_privilege (name) \
         SELECT 'p' || n FROM generate_series(1, $1::int8) n",
        &[&i64::try_from(max + 1).unwrap()],
    )
    .await
    .unwrap();
    tx.capture_statements(true);
    let err = load_rows(&mut tx).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::ResourceExhausted);
    assert_eq!(
        err.message(),
        format!(
            "policy rows: {} res.groups.privilege rows are more than the {max} a policy may hold",
            max + 1
        )
    );
    assert_eq!(
        tx.captured(),
        [STATEMENTS[0].to_owned()],
        "nothing else was read"
    );
    tx.rollback().await.unwrap();
    drop(tx);

    let mut tx = h.begin().await.unwrap();
    let model = create(
        &mut tx,
        "ir.model",
        &[&[
            ("name", Value::str("Thing")),
            ("model", Value::str("x_thing")),
        ]],
    )
    .await
    .ids()[0];
    let limit = i64::try_from(erp_security::rows::MAX_TOTAL_TEXT_BYTES).unwrap();
    tx.execute(
        "INSERT INTO ir_access (name, model_id, operation, domain, active) \
         VALUES ('huge', $1, 'r', repeat('x', $2::int4), true)",
        &[&model, &i32::try_from(limit).unwrap()],
    )
    .await
    .unwrap();
    let err = load_rows(&mut tx).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::ResourceExhausted);
    // the byte count in the message is the size statement's sum (domain, name, model): only the
    // prefix and the bound are fixed
    let prefix = format!(
        "policy rows: more than {limit} bytes of names, domains and group specs (at least "
    );
    let counted: i64 = err
        .message()
        .strip_prefix(prefix.as_str())
        .and_then(|rest| rest.strip_suffix(')'))
        .unwrap_or_else(|| panic!("unexpected message: {}", err.message()))
        .parse()
        .unwrap();
    assert_eq!(
        counted,
        limit + 4 + 7,
        "the domain, the name 'huge' and the model 'x_thing'"
    );
    h.cleanup().await.unwrap();
}

/// The loader reads active generated company restrictions as ordinary rows flagged `generated`
/// and archived generated ones into their own list, still in five statements (design
/// ); an archived ordinary row stays invisible.
#[tokio::test]
async fn the_loader_reads_generated_and_archived_generated_rows() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let model = create(
        &mut tx,
        "ir.model",
        &[&[
            ("name", Value::str("Thing")),
            ("model", Value::str("x_thing")),
        ]],
    )
    .await
    .ids()[0];
    // `generated` is readonly through the ORM: the module installer writes it, here SQL does
    let ids: Vec<i64> = tx
        .query(
            "INSERT INTO ir_access (name, active, model_id, operation, domain, generated) VALUES \
             ('ordinary', true, $1, 'crud', NULL, false), \
             ('ordinary archived', false, $1, 'd', NULL, false), \
             ('generated', true, $1, 'crud', '[(''company_id'', ''in'', company_ids)]', true) \
             RETURNING id",
            &[&model],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect();
    assert_eq!(STATEMENTS.len(), 5);
    let rows = load_rows(&mut tx).await.unwrap();
    let flags: Vec<(i64, bool)> = rows
        .rules()
        .iter()
        .map(|r| (r.id().get(), r.generated()))
        .collect();
    assert_eq!(flags, [(ids[0], false), (ids[2], true)]);
    assert!(rows.archived_generated().is_empty());

    // archive the generated row: it leaves the rules and appears, archived, in its own list
    tx.execute(
        "UPDATE ir_access SET active = false WHERE id = $1",
        &[&ids[2]],
    )
    .await
    .unwrap();
    let rows = load_rows(&mut tx).await.unwrap();
    assert_eq!(
        rows.rules()
            .iter()
            .map(|r| r.id().get())
            .collect::<Vec<_>>(),
        [ids[0]]
    );
    let archived = rows.archived_generated();
    assert_eq!(archived.len(), 1);
    assert_eq!(archived[0].id().get(), ids[2]);
    assert!(!archived[0].active());
    assert_eq!(archived[0].model(), "x_thing");
    assert_eq!(
        archived[0].domain(),
        Some("[('company_id', 'in', company_ids)]")
    );
    assert_eq!(rows.generated_rows().len(), 1);

    // one generated restriction per model (the partial unique index)
    let err = tx
        .execute(
            "INSERT INTO ir_access (name, model_id, operation, generated) VALUES ('again', $1, \
             'crud', true)",
            &[&model],
        )
        .await
        .unwrap_err();
    assert_eq!(err.sqlstate(), Some("23505"), "{err:?}");
}
