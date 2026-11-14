//! The record check of `create` fused into the INSERT (
//! design note §7): a create predicate that reads only the inserted row's own columns is
//! evaluated by `INSERT … RETURNING id, (<predicate>) IS TRUE`, 0 statements more than the
//! INSERT; a false or NULL row raises the same error as the separate check. Predicates that join,
//! sub-select the model's own table or read a column the create writes later keep the check
//! after the create (≤ 1 statement per batch). The per-call totals on the M1 create fixtures'
//! shapes are the table of the design note §7.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod support;

use std::collections::BTreeMap;
use std::num::NonZeroU64;

use erp_orm::{
    BeginOptions, CompanyId, GroupSet, IntoPolicyProvider, Operation, PrincipalBuilder, Resolution,
    UserId,
};
use erp_sdk::domain::{PrincipalParam, UserPath};
use erp_sdk::{Cell, Command, Domain, ErrorKind, Operator, OrmConfig, RawVals, Transaction, Value};
use erp_test_models::{base_users, test_access};
use erp_testkit::{DomainPolicy, Harness, Otherwise, assert_query_count};

const OBJ: &str = "test_access.some_obj";
const CATEG: &str = "test_access.categ";
const PARTNER: &str = "test_access.partner";
const ORG: &str = "test_access.org";
const CONTAINER: &str = "test_access.container";
const CHILD: &str = "test_access.child";
const DEMO: i64 = 2;

async fn harness(policy: impl IntoPolicyProvider) -> Harness {
    let h = Harness::builder([&base_users::ADDON, &test_access::ADDON])
        .policy(policy)
        .config(OrmConfig {
            insert_batch: 100,
            ..OrmConfig::default()
        })
        .build()
        .await
        .expect("harness");
    let mut tx = h.begin().await.unwrap();
    tx.batch(
        "INSERT INTO res_users (id, name, login, active) VALUES (2, 'demo', 'demo', true); \
         SELECT setval(pg_get_serial_sequence('res_users', 'id'), 2)",
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    h
}

/// Demo, resolved with companies 1 and 2, company 1 active.
async fn begin_demo(h: &Harness) -> Transaction {
    let companies = [1, 2].map(|c| CompanyId::new(c).unwrap());
    let principal = PrincipalBuilder::user(UserId::new(DEMO).unwrap())
        .resolution(
            Resolution::in_id_order(GroupSet::default(), companies, NonZeroU64::MIN).unwrap(),
        )
        .companies(vec![companies[0]])
        .build()
        .unwrap();
    h.orm()
        .begin_opts(BeginOptions::for_principal(principal))
        .await
        .unwrap()
}

fn row(pairs: &[(&str, Value)]) -> BTreeMap<String, Value> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), v.clone()))
        .collect()
}

fn cond(field: &str, op: Operator, value: impl Into<erp_sdk::domain::Value>) -> Domain {
    Domain::condition(field, op, value).unwrap()
}

fn create_rule(model: &str, domain: Domain) -> DomainPolicy {
    DomainPolicy::builder(Otherwise::Allow)
        .rule(model, Operation::Create, domain)
        .build()
        .unwrap()
}

/// `n` objects with a positive `val` (the M1 fixtures' single column set).
fn objs(n: i64) -> Vec<BTreeMap<String, Value>> {
    (1..=n)
        .map(|i| {
            row(&[
                ("name", Value::str(format!("o{i}"))),
                ("val", Value::Int(i)),
            ])
        })
        .collect()
}

/// The statements of creating `rows` of `model` as demo.
async fn create_counted(
    tx: &mut Transaction,
    model: &str,
    rows: &[BTreeMap<String, Value>],
) -> (u64, Vec<String>) {
    let records = tx.env().records(model).unwrap();
    tx.capture_statements(true);
    let before = tx.query_count();
    records.create_values(tx, rows).await.unwrap();
    let used = tx.query_count() - before;
    let statements = tx.captured().to_vec();
    tx.capture_statements(false);
    (used, statements)
}

/// A predicate on the inserted row's own column: decided by the INSERT itself, one statement for
/// the whole create, the predicate in its `RETURNING` with its parameter after the columns'.
#[tokio::test]
async fn an_own_column_predicate_is_decided_by_the_insert() {
    let h = harness(create_rule(OBJ, cond("val", Operator::Gt, 0))).await;
    let mut tx = begin_demo(&h).await;
    let (used, statements) = create_counted(&mut tx, OBJ, &objs(2)).await;
    assert_eq!(used, 1, "{statements:#?}");
    let insert = &statements[0];
    assert!(
        insert.contains(
            "RETURNING \"test_access_some_obj\".\"id\", (\"test_access_some_obj\".\"val\" > \
             $5::int8) IS TRUE AS \"__ok\")"
        ),
        "{insert}"
    );
    assert!(
        insert.ends_with(
            "SELECT \"__v\".\"id\", \"__v\".\"__ord\", (now() AT TIME ZONE 'UTC'), \
             \"__ins\".\"__ok\" FROM \"__v\" JOIN \"__ins\" ON \"__ins\".\"id\" = \"__v\".\"id\" \
             ORDER BY \"__v\".\"__ord\""
        ),
        "{insert}"
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// A row the fused predicate makes false raises the record-level error of the separate check,
/// naming the first denied record; a NULL predicate (`company_id in company_ids` without a
/// company) denies too. The error path adds its one statement.
#[tokio::test]
async fn false_and_null_rows_raise_the_record_level_error() {
    let h = harness(create_rule(OBJ, cond("val", Operator::Gt, 0))).await;
    let mut tx = begin_demo(&h).await;
    let records = tx.env().records(OBJ).unwrap();
    let rows = [
        row(&[("name", Value::str("ok")), ("val", Value::Int(1))]),
        row(&[("name", Value::str("zero")), ("val", Value::Int(0))]),
        row(&[("name", Value::str("negative")), ("val", Value::Int(-1))]),
    ];
    let err = assert_query_count!(tx, >= 2, <= 2, {
        records.create_values(&mut tx, &rows).await.unwrap_err()
    });
    assert_eq!(err.kind(), ErrorKind::Access, "{err}");
    // the first denied row is the second one inserted (ids follow the input order)
    let first = tx
        .env()
        .records(OBJ)
        .unwrap()
        .sudo(true)
        .search(&mut tx, &cond("name", Operator::Eq, "zero"))
        .await
        .unwrap()
        .ids();
    assert_eq!(
        err.message(),
        format!(
            "Your access rights do not let you create 'Object For Test Access Right' (test_access.some_obj) \
             records.\n\nRecords: test_access.some_obj({},), User: {DEMO}",
            first[0]
        )
    );
    drop(tx);
    h.cleanup().await.unwrap();

    let mine = cond(
        "company_id",
        Operator::In,
        erp_sdk::domain::Value::Param(PrincipalParam::CompanyIds),
    );
    let h = harness(create_rule(OBJ, mine)).await;
    let company = {
        let mut su = h.begin().await.unwrap();
        let c = su
            .env()
            .records("test_access.company")
            .unwrap()
            .create_values(&mut su, &[row(&[("name", Value::str("one"))])])
            .await
            .unwrap()
            .ids();
        su.commit().await.unwrap();
        c[0]
    };
    assert_eq!(company, 1, "the active company");
    let mut tx = begin_demo(&h).await;
    let (used, _) = create_counted(
        &mut tx,
        OBJ,
        &[row(&[
            ("name", Value::str("mine")),
            ("company_id", Value::Int(1)),
        ])],
    )
    .await;
    assert_eq!(used, 1, "fused");
    let records = tx.env().records(OBJ).unwrap();
    tx.capture_statements(true);
    let err = records
        .create_values(&mut tx, &[row(&[("name", Value::str("nobody's"))])])
        .await
        .unwrap_err();
    let statements = tx.captured().to_vec();
    tx.capture_statements(false);
    // the NULL is denied by the fused `IS TRUE`, not by a later check
    assert!(
        statements[0].contains("IS TRUE AS \"__ok\""),
        "{statements:#?}"
    );
    assert_eq!(err.kind(), ErrorKind::Access, "{err}");
    assert!(
        err.message().starts_with(
            "Your access rights do not let you create 'Object For Test Access Right' \
             (test_access.some_obj) records."
        ),
        "{err}"
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// A predicate through another table keeps the check after the create: one statement more,
/// which sees the related row.
#[tokio::test]
async fn a_predicate_through_a_relation_is_checked_after_the_create() {
    let rule = cond(
        "categ_id",
        Operator::Any,
        erp_sdk::domain::Value::Domain(cond("name", Operator::Ne, "secret")),
    );
    let h = harness(create_rule(OBJ, rule)).await;
    let categs = {
        let mut su = h.begin().await.unwrap();
        let ids = su
            .env()
            .records(CATEG)
            .unwrap()
            .create_values(
                &mut su,
                &[
                    row(&[("name", Value::str("ok"))]),
                    row(&[("name", Value::str("secret"))]),
                ],
            )
            .await
            .unwrap()
            .ids();
        su.commit().await.unwrap();
        ids
    };
    let mut tx = begin_demo(&h).await;
    let (used, statements) = create_counted(
        &mut tx,
        OBJ,
        &[row(&[
            ("name", Value::str("a")),
            ("categ_id", Value::Int(categs[0])),
        ])],
    )
    .await;
    assert_eq!(used, 2, "{statements:#?}");
    assert!(!statements[0].contains("RETURNING"), "{}", statements[0]);
    let records = tx.env().records(OBJ).unwrap();
    let err = records
        .create_values(
            &mut tx,
            &[row(&[
                ("name", Value::str("b")),
                ("categ_id", Value::Int(categs[1])),
            ])],
        )
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access, "{err}");
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn a_predicate_over_the_own_table_is_checked_after_the_create() {
    let rule = cond(
        "id",
        Operator::ChildOf,
        erp_sdk::domain::Value::Param(PrincipalParam::UserPath(
            UserPath::parse("access_partner_id.id").unwrap(),
        )),
    );
    let h = harness(create_rule(PARTNER, rule)).await;
    let company = {
        let mut su = h.begin().await.unwrap();
        let ids = su
            .env()
            .records(PARTNER)
            .unwrap()
            .create_values(&mut su, &[row(&[("name", Value::str("Company"))])])
            .await
            .unwrap()
            .ids();
        su.env()
            .records("res.users")
            .unwrap()
            .browse([DEMO])
            .unwrap()
            .write_values(&mut su, &row(&[("access_partner_id", Value::Int(ids[0]))]))
            .await
            .unwrap();
        su.commit().await.unwrap();
        ids[0]
    };
    let mut tx = begin_demo(&h).await;
    let (used, statements) = create_counted(
        &mut tx,
        PARTNER,
        &[row(&[
            ("name", Value::str("Contact")),
            ("parent_id", Value::Int(company)),
        ])],
    )
    .await;
    assert_eq!(used, 2, "{statements:#?}");
    assert!(!statements[0].contains("RETURNING"), "{}", statements[0]);
    let records = tx.env().records(PARTNER).unwrap();
    let err = records
        .create_values(&mut tx, &[row(&[("name", Value::str("Elsewhere"))])])
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access, "{err}");
    drop(tx);
    h.cleanup().await.unwrap();
}

/// A column the create writes after its INSERT (here the parent path of a `_parent_store`
/// tree) keeps the check after the create, which sees the value.
#[tokio::test]
async fn a_column_written_after_the_insert_is_checked_after_the_create() {
    let h = harness(create_rule(ORG, cond("parent_path", Operator::Ne, false))).await;
    let mut tx = begin_demo(&h).await;
    let (_, statements) =
        create_counted(&mut tx, ORG, &[row(&[("name", Value::str("Root"))])]).await;
    assert!(!statements[0].contains("RETURNING"), "{}", statements[0]);
    let last = statements.last().unwrap();
    assert!(
        last.starts_with("SELECT \"test_access_org\".\"id\" FROM \"test_access_org\""),
        "the check runs last: {statements:#?}"
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Statements per create call on the shapes of the M1 create fixtures (`crud.rs` 250 rows in 3
/// batches of 100, `perf_s7.rs` 64 rows of mixed column sets, `rel_commands.rs` one record with
/// ten one2many lines): as superuser, under a create rule on own columns (fused) and under one
/// through a relation (checked after the create). The design note §7 records these totals.
#[tokio::test]
async fn create_budgets_on_the_m1_fixture_shapes() {
    let own = cond("val", Operator::Gt, 0);
    let through = cond(
        "categ_id",
        Operator::NotAny,
        erp_sdk::domain::Value::Domain(cond("name", Operator::Eq, "secret")),
    );
    let lines_own = cond("val", Operator::Ge, 0);
    let lines_through = cond(
        "container_id",
        Operator::Any,
        erp_sdk::domain::Value::Domain(cond("name", Operator::Ne, "secret")),
    );
    // rows giving different column sets (one column set for the call, NULL where not given)
    let mixed: Vec<BTreeMap<String, Value>> = (0..64usize)
        .map(|mask| {
            let mut pairs = vec![("val", Value::Int(1))];
            if mask & 1 != 0 {
                pairs.push(("name", Value::str("n")));
            }
            if mask & 2 != 0 {
                pairs.push(("deadline", Value::str("2020-01-01")));
            }
            row(&pairs)
        })
        .collect();
    let order = || {
        RawVals::new()
            .set_cell("name", Cell::Str("order".into()))
            .set_commands(
                "child_ids",
                (0..10).map(|i| {
                    Command::create(
                        RawVals::new()
                            .set_cell("name", Cell::Str(format!("line {i}").into()))
                            .set_cell("val", Cell::Int(1)),
                    )
                }),
            )
    };
    // (label, policy (none: superuser), statements of the three shapes)
    let cases: [(&str, Option<DomainPolicy>, [u64; 3]); 3] = [
        ("superuser", None, [3, 1, 2]),
        (
            "fused",
            Some(
                DomainPolicy::builder(Otherwise::Allow)
                    .rule(OBJ, Operation::Create, own)
                    .rule(CHILD, Operation::Create, lines_own)
                    .build()
                    .unwrap(),
            ),
            [3, 1, 2],
        ),
        (
            "after the create",
            Some(
                DomainPolicy::builder(Otherwise::Allow)
                    .rule(OBJ, Operation::Create, through)
                    .rule(CHILD, Operation::Create, lines_through)
                    .build()
                    .unwrap(),
            ),
            [4, 2, 3],
        ),
    ];
    for (label, policy, expected) in cases {
        let su = policy.is_none();
        let h = harness(
            policy.unwrap_or_else(|| DomainPolicy::builder(Otherwise::Allow).build().unwrap()),
        )
        .await;
        let mut tx = if su {
            h.begin().await.unwrap()
        } else {
            begin_demo(&h).await
        };
        // warm the connection with something unrelated
        tx.env()
            .records(OBJ)
            .unwrap()
            .search(&mut tx, &Domain::TRUE)
            .await
            .unwrap();
        let (bulk, _) = create_counted(&mut tx, OBJ, &objs(250)).await;
        let (columns, _) = create_counted(&mut tx, OBJ, &mixed).await;
        let containers = tx.env().records(CONTAINER).unwrap();
        let before = tx.query_count();
        containers.create(&mut tx, vec![order()]).await.unwrap();
        let lines = tx.query_count() - before;
        assert_eq!([bulk, columns, lines], expected, "{label}");
        drop(tx);
        h.cleanup().await.unwrap();
    }
}

const LEDGER: &str = "test_access.ledger";

fn state_rule(state: &str) -> DomainPolicy {
    create_rule(LEDGER, cond("state", Operator::Eq, state))
}

fn line(val: i32) -> Command {
    Command::create(
        RawVals::new()
            .set_cell("name", Cell::Str("line".into()))
            .set_cell("val", Cell::Int(val)),
    )
}

/// Creates one ledger from `vals`, capturing its statements even when the create fails.
async fn create_ledger(
    tx: &mut Transaction,
    vals: RawVals,
) -> (erp_sdk::Result<erp_sdk::Records>, Vec<String>) {
    let ledgers = tx.env().records(LEDGER).unwrap();
    tx.capture_statements(true);
    let result = ledgers.create(tx, vec![vals]).await;
    let statements = tx.captured().to_vec();
    tx.capture_statements(false);
    (result, statements)
}

fn ledger(state: &str) -> RawVals {
    RawVals::new().set_cell("state", Cell::Str(state.into()))
}

/// The record-level create error naming the one ledger in the database.
async fn assert_create_refused(tx: &mut Transaction, err: &erp_sdk::Error) {
    assert_eq!(err.kind(), ErrorKind::Access, "{err}");
    let ids = tx
        .env()
        .records(LEDGER)
        .unwrap()
        .sudo(true)
        .search(tx, &Domain::TRUE)
        .await
        .unwrap()
        .ids();
    assert_eq!(ids.len(), 1, "one ledger was inserted");
    assert_eq!(
        err.message(),
        format!(
            "Your access rights do not let you create 'Access Ledger' (test_access.ledger) \
             records.\n\nRecords: test_access.ledger({},), User: {DEMO}",
            ids[0]
        )
    );
}

fn is_fused(statements: &[String]) -> bool {
    statements[0].contains("IS TRUE AS \"__ok\"")
}

/// The plain case is fused (the gate tests below prove each excluded shape is not).
#[tokio::test]
async fn a_plain_ledger_create_is_fused() {
    let h = harness(state_rule("draft")).await;
    let mut tx = begin_demo(&h).await;
    let (result, statements) = create_ledger(&mut tx, ledger("draft")).await;
    result.unwrap();
    assert!(is_fused(&statements), "{statements:#?}");
    drop(tx);
    h.cleanup().await.unwrap();
}

/// A non-stored x2many with an inverse given at create rewrites the row's own `state` after the
/// INSERT: the check must see the final row, so it is not fused.
#[tokio::test]
async fn an_x2many_inverse_writing_the_gated_column_is_refused() {
    let h = harness(state_rule("draft")).await;
    let mut tx = begin_demo(&h).await;
    let (result, statements) =
        create_ledger(&mut tx, ledger("draft").set_commands("tag_ids", [line(1)])).await;
    assert!(!is_fused(&statements), "{statements:#?}");
    let err = result.unwrap_err();
    assert_create_refused(&mut tx, &err).await;
    drop(tx);
    h.cleanup().await.unwrap();
}

/// A stored total precomputed over lines is 0 at the INSERT and the sum once the lines exist:
/// the rule reads the final value, so the create is not fused.
#[tokio::test]
async fn a_total_over_lines_is_checked_on_the_final_row() {
    let h = harness(create_rule(LEDGER, cond("total", Operator::Eq, 0))).await;
    let mut tx = begin_demo(&h).await;
    let (result, statements) =
        create_ledger(&mut tx, ledger("draft").set_commands("line_ids", [line(5)])).await;
    assert!(!is_fused(&statements), "{statements:#?}");
    let err = result.unwrap_err();
    assert_create_refused(&mut tx, &err).await;
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Lines that the final row allows are created, unfused (the fused predicate must not decide
/// before the lines: a rule the final total satisfies and the INSERT's 0 would deny).
#[tokio::test]
async fn a_total_over_lines_allowed_by_the_final_row_is_created() {
    let h = harness(create_rule(LEDGER, cond("total", Operator::Gt, 0))).await;
    let mut tx = begin_demo(&h).await;
    let (result, statements) =
        create_ledger(&mut tx, ledger("draft").set_commands("line_ids", [line(5)])).await;
    result.unwrap();
    assert!(!is_fused(&statements), "{statements:#?}");
    drop(tx);
    h.cleanup().await.unwrap();
}

/// A given field with an inverse (its method writes `state`) keeps the check after the create.
#[tokio::test]
async fn a_given_inversed_field_keeps_the_check_after_the_create() {
    let h = harness(state_rule("draft")).await;
    let mut tx = begin_demo(&h).await;
    let (result, statements) = create_ledger(
        &mut tx,
        ledger("draft").set_cell("label", Cell::Str("x".into())),
    )
    .await;
    assert!(!is_fused(&statements), "{statements:#?}");
    let err = result.unwrap_err();
    assert_create_refused(&mut tx, &err).await;
    drop(tx);
    h.cleanup().await.unwrap();
}

/// A stored compute over lines is not in the INSERT (the lines do not exist yet): the column is
/// written after it, so a rule on it keeps the check after the create even with no lines.
#[tokio::test]
async fn a_stored_compute_written_after_the_insert_keeps_the_check_after_the_create() {
    let h = harness(create_rule(LEDGER, cond("total", Operator::Eq, 0))).await;
    let mut tx = begin_demo(&h).await;
    let (result, statements) = create_ledger(&mut tx, ledger("draft")).await;
    assert!(!is_fused(&statements), "{statements:#?}");
    result.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

/// A stored compute of given values is computed into the INSERT, so a rule on it is fused and
/// still decides on the computed value.
#[tokio::test]
async fn a_stored_compute_in_the_insert_is_decided_by_the_insert() {
    let h = harness(create_rule(LEDGER, cond("shout", Operator::Eq, "DRAFT"))).await;
    let mut tx = begin_demo(&h).await;
    let (result, statements) = create_ledger(&mut tx, ledger("draft")).await;
    result.unwrap();
    assert!(is_fused(&statements), "{statements:#?}");
    let (result, _) = create_ledger(&mut tx, ledger("other")).await;
    assert_eq!(result.unwrap_err().kind(), ErrorKind::Access);
    drop(tx);
    h.cleanup().await.unwrap();
}

const LEDGER_LINE: &str = "test_access.ledger_line";

/// /D5: a stored compute precomputed into the INSERT whose input an editable related
/// field of the same create rewrites afterwards is late for the fused check: a rule on it keeps the
/// check after the create and judges the recomputed value. Without the related write the same rule
/// is fused. Fails on the code before PC-1, where the INSERT decided on the stale `DRAFT` and the
/// line was created.
#[tokio::test]
async fn a_related_write_rewriting_a_precomputed_input_keeps_the_check_after_the_create() {
    let h = harness(create_rule(
        LEDGER_LINE,
        cond("ledger_shout", Operator::Eq, "DRAFT"),
    ))
    .await;
    let mut tx = begin_demo(&h).await;
    let ledger_id = tx
        .env()
        .records(LEDGER)
        .unwrap()
        .create(&mut tx, vec![ledger("draft")])
        .await
        .unwrap()
        .ids()[0];
    let lines = tx.env().records(LEDGER_LINE).unwrap();
    let line = || RawVals::new().set_cell("ledger_id", Cell::Id(ledger_id));
    tx.capture_statements(true);
    lines.create(&mut tx, vec![line()]).await.unwrap();
    let plain = tx.captured().to_vec();
    tx.capture_statements(false);
    assert!(is_fused(&plain), "{plain:#?}");

    tx.capture_statements(true);
    let result = lines
        .create(
            &mut tx,
            vec![line().set_cell("ledger_state", Cell::Str("other".into()))],
        )
        .await;
    let statements = tx.captured().to_vec();
    tx.capture_statements(false);
    assert!(!is_fused(&statements), "{statements:#?}");
    let err = result.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access, "{err}");
    let ids = lines
        .sudo(true)
        .search(&mut tx, &Domain::TRUE)
        .await
        .unwrap()
        .ids();
    assert_eq!(ids.len(), 2, "both lines were inserted");
    assert_eq!(
        err.message(),
        format!(
            "Your access rights do not let you create 'Access Ledger Line' (test_access.ledger_line) \
             records.\n\nRecords: test_access.ledger_line({},), User: {DEMO}",
            ids[1]
        )
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// The editable related field given at create writes the ledger as the creating user (COMP §4.8):
/// a user who may create lines but not write ledgers gets the ledger's write refusal from the
/// create (kind and message). The same create without the related field succeeds.
#[tokio::test]
async fn a_related_write_at_create_needs_write_access_to_the_target() {
    let h = harness(
        DomainPolicy::builder(Otherwise::Allow)
            .deny(LEDGER, Operation::Write)
            .build()
            .unwrap(),
    )
    .await;
    let mut tx = begin_demo(&h).await;
    let target = tx
        .env()
        .records(LEDGER)
        .unwrap()
        .create(&mut tx, vec![ledger("draft")])
        .await
        .unwrap();
    let ledger_id = target.ids()[0];
    let lines = tx.env().records(LEDGER_LINE).unwrap();
    let line = || RawVals::new().set_cell("ledger_id", Cell::Id(ledger_id));
    lines.create(&mut tx, vec![line()]).await.unwrap();
    // the refusal a direct write on the ledger by this user gets
    let direct = target.write(&mut tx, ledger("other")).await.unwrap_err();
    assert_eq!(direct.kind(), ErrorKind::Access, "{direct}");
    let err = lines
        .create(
            &mut tx,
            vec![line().set_cell("ledger_state", Cell::Str("other".into()))],
        )
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access, "{err}");
    assert_eq!(err.message(), direct.message());
    drop(tx);
    h.cleanup().await.unwrap();
}
