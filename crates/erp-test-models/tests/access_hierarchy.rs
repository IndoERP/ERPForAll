//! Hierarchy operators over principal parameters in access predicates (
//! §3.1): a `child_of`/`parent_of` whose start records are `user.<path>`, `company_ids` or a mix
//! with literal ids selects them in the statement and walks from there, by `parent_path` ranges on
//! a `_parent_store` tree and recursively otherwise. One compiled text serves every user. The
//! `DomainPolicy` of the testkit stands in for compiled `ir.access` rules.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod support;

use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroU64;

use erp_orm::{
    BeginOptions, CompanyId, GroupSet, IntoPolicyProvider, Operation, PrincipalBuilder, Resolution,
    UserId,
};
use erp_sdk::domain::{NowKind, PrincipalParam, UserPath};
use erp_sdk::{Domain, ErrorKind, Operator, Records, Transaction, Value};
use erp_test_models::{base_users, test_access};
use erp_testkit::{DomainPolicy, Harness, Otherwise, assert_query_count};
use support::assert_sql;

const ORG: &str = "test_access.org";
const DOC: &str = "test_access.org_doc";
const OBJ: &str = "test_access.some_obj";
const PARTNER: &str = "test_access.partner";
const DEMO: i64 = 2;
const OTHER: i64 = 3;

async fn harness(policy: impl IntoPolicyProvider) -> Harness {
    let h = Harness::builder([&base_users::ADDON, &test_access::ADDON])
        .policy(policy)
        .build()
        .await
        .expect("harness");
    let mut tx = h.begin().await.unwrap();
    tx.batch(
        "INSERT INTO res_users (id, name, login, active) VALUES (2, 'demo', 'demo', true), \
         (3, 'other', 'other', true); \
         SELECT setval(pg_get_serial_sequence('res_users', 'id'), 3)",
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    h
}

/// A user principal resolved with companies 1 to 4, `allowed` active (current first).
async fn begin_as(h: &Harness, uid: i64, allowed: &[i64]) -> Transaction {
    let company = |c: i64| CompanyId::new(c).unwrap();
    let principal = PrincipalBuilder::user(UserId::new(uid).unwrap())
        .resolution(
            Resolution::in_id_order(GroupSet::default(), (1..=4).map(company), NonZeroU64::MIN)
                .unwrap(),
        )
        .companies(allowed.iter().map(|c| company(*c)).collect())
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

/// Records created as superuser, one transaction.
async fn create(h: &Harness, model: &str, rows: &[BTreeMap<String, Value>]) -> Vec<i64> {
    let mut tx = h.begin().await.unwrap();
    let ids = tx
        .env()
        .records(model)
        .unwrap()
        .create_values(&mut tx, rows)
        .await
        .unwrap()
        .ids();
    tx.commit().await.unwrap();
    ids
}

/// Write `values` on records as superuser, one transaction.
async fn write(h: &Harness, model: &str, ids: &[i64], values: &[(&str, Value)]) {
    let mut tx = h.begin().await.unwrap();
    tx.env()
        .records(model)
        .unwrap()
        .browse(ids.iter().copied())
        .unwrap()
        .write_values(&mut tx, &row(values))
        .await
        .unwrap();
    tx.commit().await.unwrap();
}

fn cond(field: &str, op: Operator, value: impl Into<erp_sdk::domain::Value>) -> Domain {
    Domain::condition(field, op, value).unwrap()
}

fn user_path(path: &str) -> erp_sdk::domain::Value {
    erp_sdk::domain::Value::Param(PrincipalParam::UserPath(UserPath::parse(path).unwrap()))
}

fn rule(model: &str, domain: Domain) -> DomainPolicy {
    DomainPolicy::builder(Otherwise::Allow)
        .rule(model, Operation::Read, domain)
        .build()
        .unwrap()
}

async fn search(tx: &mut Transaction, model: &str) -> BTreeSet<i64> {
    let records = tx.env().records(model).unwrap();
    records
        .search(tx, &Domain::TRUE)
        .await
        .unwrap()
        .ids()
        .into_iter()
        .collect()
}

/// The SQL of a search on `model` (its last statement).
async fn sql_of(tx: &mut Transaction, model: &str) -> String {
    let records = tx.env().records(model).unwrap();
    tx.capture_statements(true);
    records.search(tx, &Domain::TRUE).await.unwrap();
    let sql = tx.captured().last().cloned().unwrap_or_default();
    tx.capture_statements(false);
    sql
}

fn set(ids: &[i64]) -> BTreeSet<i64> {
    ids.iter().copied().collect()
}

fn records(tx: &Transaction, model: &str, ids: &[i64]) -> Records {
    tx.env()
        .records(model)
        .unwrap()
        .browse(ids.iter().copied())
        .unwrap()
}

/// The organisation tree A > B > C and D, and one document per organisation plus one without.
struct Orgs {
    a: i64,
    b: i64,
    c: i64,
    d: i64,
    /// Documents of A, B, C, D and of no organisation.
    docs: [i64; 5],
}

async fn orgs(h: &Harness) -> Orgs {
    let a = create(h, ORG, &[row(&[("name", Value::str("A"))])]).await[0];
    let b = create(
        h,
        ORG,
        &[row(&[
            ("name", Value::str("B")),
            ("parent_id", Value::Int(a)),
        ])],
    )
    .await[0];
    let c = create(
        h,
        ORG,
        &[row(&[
            ("name", Value::str("C")),
            ("parent_id", Value::Int(b)),
        ])],
    )
    .await[0];
    let d = create(h, ORG, &[row(&[("name", Value::str("D"))])]).await[0];
    let docs = create(
        h,
        DOC,
        &[
            row(&[("name", Value::str("of A")), ("org_id", Value::Int(a))]),
            row(&[("name", Value::str("of B")), ("org_id", Value::Int(b))]),
            row(&[("name", Value::str("of C")), ("org_id", Value::Int(c))]),
            row(&[("name", Value::str("of D")), ("org_id", Value::Int(d))]),
            row(&[("name", Value::str("of nobody"))]),
        ],
    )
    .await;
    Orgs {
        a,
        b,
        c,
        d,
        docs: docs.try_into().unwrap(),
    }
}

const DOC_SELECT: &str = "SELECT \"test_access_org_doc\".\"id\" FROM \"test_access_org_doc\" WHERE";
const DOC_ORDER: &str = "ORDER BY \"test_access_org_doc\".\"id\"";

/// The subtree under the start records `start` (a condition on `"__s"."id"`), by path ranges.
fn subtree(start: &str) -> String {
    format!(
        "(SELECT \"c\".\"id\" FROM \"test_access_org\" AS \"__s\" JOIN \"test_access_org\" AS \
         \"c\" ON \"c\".\"parent_path\" COLLATE \"C\" >= \"__s\".\"parent_path\" COLLATE \"C\" AND \
         \"c\".\"parent_path\" COLLATE \"C\" < (left(\"__s\".\"parent_path\", -1) || '0') COLLATE \
         \"C\" WHERE {start})"
    )
}

/// `"__s"."id"` against the user's organisation (`user.access_org_id.id`: the id of the record
/// reached), the sub-select binding `$n`.
fn user_org(n: usize) -> String {
    format!(
        "\"__s\".\"id\" = (SELECT \"__up1\".\"id\" FROM \"res_users\" AS \"__up0\" JOIN \
         \"test_access_org\" AS \"__up1\" ON (\"__up0\".\"access_org_id\" = \"__up1\".\"id\") WHERE \
         \"__up0\".\"id\" = ${n}::int8)"
    )
}

/// `child_of` over a scalar user path on a `_parent_store` tree: the user's organisation and
/// everything below it, selected by path ranges in the search statement itself (one statement),
/// the same text for every user; a user without organisation gets nothing.
#[tokio::test]
async fn child_of_a_user_path_selects_the_subtree_in_sql() {
    let h = harness(rule(
        DOC,
        cond("org_id", Operator::ChildOf, user_path("access_org_id.id")),
    ))
    .await;
    let o = orgs(&h).await;
    write(
        &h,
        "res.users",
        &[DEMO],
        &[("access_org_id", Value::Int(o.b))],
    )
    .await;

    let mut tx = begin_as(&h, DEMO, &[]).await;
    assert_query_count!(tx, >= 1, <= 1, {
        assert_eq!(search(&mut tx, DOC).await, set(&[o.docs[1], o.docs[2]]));
    });
    let sql = sql_of(&mut tx, DOC).await;
    let expected = format!(
        "{DOC_SELECT} \"test_access_org_doc\".\"org_id\" IN {} {DOC_ORDER}",
        subtree(&user_org(1))
    );
    assert_sql(&sql, &expected);
    // record checks select with the same predicate
    records(&tx, DOC, &o.docs[1..3])
        .check_access(&mut tx, Operation::Read)
        .await
        .unwrap();
    let err = records(&tx, DOC, &o.docs[..1])
        .check_access(&mut tx, Operation::Read)
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access, "{err}");
    drop(tx);

    let mut tx = begin_as(&h, OTHER, &[]).await;
    assert!(search(&mut tx, DOC).await.is_empty(), "no organisation");
    assert_sql(&sql_of(&mut tx, DOC).await, &expected);
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Literal ids and parameters start the same hierarchy (`child_of [user.access_org_id.id, D]`):
/// the literal ids come first, as one array.
#[tokio::test]
async fn child_of_mixes_literal_ids_and_parameters() {
    // D is the fourth organisation of a fresh database (asserted below)
    let d = 4;
    let mixed = cond(
        "org_id",
        Operator::ChildOf,
        erp_sdk::domain::Value::List(vec![
            user_path("access_org_id.id"),
            erp_sdk::domain::Value::Int(d),
        ]),
    );
    let h = harness(rule(DOC, mixed)).await;
    let o = orgs(&h).await;
    assert_eq!(o.d, d);
    write(
        &h,
        "res.users",
        &[DEMO],
        &[("access_org_id", Value::Int(o.c))],
    )
    .await;
    let mut tx = begin_as(&h, DEMO, &[]).await;
    assert_eq!(search(&mut tx, DOC).await, set(&[o.docs[2], o.docs[3]]));
    let expected = format!(
        "{DOC_SELECT} \"test_access_org_doc\".\"org_id\" IN {} {DOC_ORDER}",
        subtree(&format!(
            "(\"__s\".\"id\" = ANY($1::int8[]) OR {})",
            user_org(2)
        ))
    );
    assert_sql(&sql_of(&mut tx, DOC).await, &expected);
    drop(tx);
    h.cleanup().await.unwrap();
}

/// `parent_of company_ids`, the shape of `base`'s bank-account rule
/// (`['|', ('company_id', 'parent_of', company_ids), ('company_id', '=', False)]`): the allowed
/// companies and every record above them, walked recursively from the bound array (the
/// organisations stand in for a company tree).
#[tokio::test]
async fn parent_of_company_ids_walks_up_from_the_allowed_companies() {
    let bank_rule = Domain::or([
        cond(
            "org_id",
            Operator::ParentOf,
            erp_sdk::domain::Value::Param(PrincipalParam::CompanyIds),
        ),
        cond("org_id", Operator::Eq, false),
    ]);
    let h = harness(rule(DOC, bank_rule)).await;
    let o = orgs(&h).await;
    assert_eq!(
        (o.a, o.b, o.c, o.d),
        (1, 2, 3, 4),
        "ids double as company ids"
    );

    let mut tx = begin_as(&h, DEMO, &[o.c]).await;
    assert_eq!(
        search(&mut tx, DOC).await,
        set(&[o.docs[0], o.docs[1], o.docs[2], o.docs[4]])
    );
    let sql = sql_of(&mut tx, DOC).await;
    assert_sql(
        &sql,
        &format!(
            "{DOC_SELECT} (\"test_access_org_doc\".\"org_id\" IS NULL OR \
             \"test_access_org_doc\".\"org_id\" IN (WITH RECURSIVE \"__h\"(\"id\") AS (SELECT \
             \"__s\".\"id\" FROM \"test_access_org\" AS \"__s\" WHERE \"__s\".\"id\" = \
             ANY($1::int8[]) UNION SELECT \"test_access_org\".\"parent_id\" FROM \
             \"test_access_org\" JOIN \"__h\" ON \"test_access_org\".\"id\" = \"__h\".\"id\" WHERE \
             \"test_access_org\".\"parent_id\" IS NOT NULL) SELECT \"__h\".\"id\" FROM \"__h\")) \
             {DOC_ORDER}"
        ),
    );
    drop(tx);
    let mut tx = begin_as(&h, DEMO, &[o.d, o.a]).await;
    assert_eq!(
        search(&mut tx, DOC).await,
        set(&[o.docs[0], o.docs[3], o.docs[4]])
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn child_of_through_a_many2many_like_the_portal_ticket_rule() {
    let ticket_rule = Domain::or([
        cond(
            "follower_org_ids",
            Operator::ChildOf,
            erp_sdk::domain::Value::List(vec![user_path("access_org_id.id")]),
        ),
        cond(
            "follower_org_ids",
            Operator::In,
            erp_sdk::domain::Value::List(vec![user_path("access_org_id.id")]),
        ),
    ]);
    let h = harness(rule(DOC, ticket_rule)).await;
    let o = orgs(&h).await;
    let followed = create(
        &h,
        DOC,
        &[
            row(&[
                ("name", Value::str("follows A and C")),
                (
                    "follower_org_ids",
                    Value::List(vec![Value::Int(o.a), Value::Int(o.c)].into()),
                ),
            ]),
            row(&[
                ("name", Value::str("follows A")),
                (
                    "follower_org_ids",
                    Value::List(vec![Value::Int(o.a)].into()),
                ),
            ]),
            row(&[
                ("name", Value::str("follows D")),
                (
                    "follower_org_ids",
                    Value::List(vec![Value::Int(o.d)].into()),
                ),
            ]),
        ],
    )
    .await;
    write(
        &h,
        "res.users",
        &[DEMO],
        &[("access_org_id", Value::Int(o.b))],
    )
    .await;
    let mut tx = begin_as(&h, DEMO, &[]).await;
    assert_query_count!(tx, >= 1, <= 1, {
        assert_eq!(search(&mut tx, DOC).await, set(&followed[..1]));
    });
    drop(tx);
    write(
        &h,
        "res.users",
        &[DEMO],
        &[("access_org_id", Value::Int(o.d))],
    )
    .await;
    let mut tx = begin_as(&h, DEMO, &[]).await;
    assert_eq!(search(&mut tx, DOC).await, set(&followed[2..]));
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Without `_parent_store` the hierarchy is walked by a recursive query from the selected start
/// records, down (`child_of`) and up (`parent_of`).
#[tokio::test]
async fn hierarchies_without_parent_store_are_walked_recursively() {
    let down = cond(
        "partner_id",
        Operator::ChildOf,
        user_path("access_partner_id.id"),
    );
    let h = harness(rule(OBJ, down)).await;
    let p1 = create(&h, PARTNER, &[row(&[("name", Value::str("P1"))])]).await[0];
    let p2 = create(
        &h,
        PARTNER,
        &[row(&[
            ("name", Value::str("P2")),
            ("parent_id", Value::Int(p1)),
        ])],
    )
    .await[0];
    let p3 = create(
        &h,
        PARTNER,
        &[row(&[
            ("name", Value::str("P3")),
            ("parent_id", Value::Int(p2)),
        ])],
    )
    .await[0];
    let objs = create(
        &h,
        OBJ,
        &[
            row(&[("partner_id", Value::Int(p1))]),
            row(&[("partner_id", Value::Int(p2))]),
            row(&[("partner_id", Value::Int(p3))]),
        ],
    )
    .await;
    write(
        &h,
        "res.users",
        &[DEMO],
        &[("access_partner_id", Value::Int(p2))],
    )
    .await;
    let mut tx = begin_as(&h, DEMO, &[]).await;
    assert_eq!(search(&mut tx, OBJ).await, set(&objs[1..]));
    let sql = sql_of(&mut tx, OBJ).await;
    assert_sql(
        &sql,
        "SELECT \"test_access_some_obj\".\"id\" FROM \"test_access_some_obj\" WHERE \
         \"test_access_some_obj\".\"partner_id\" IN (WITH RECURSIVE \"__h\"(\"id\") AS (SELECT \
         \"__s\".\"id\" FROM \"test_access_partner\" AS \"__s\" WHERE \"__s\".\"id\" = (SELECT \
         \"__up1\".\"id\" FROM \"res_users\" AS \"__up0\" JOIN \"test_access_partner\" AS \
         \"__up1\" ON (\"__up0\".\"access_partner_id\" = \"__up1\".\"id\") WHERE \
         \"__up0\".\"id\" = $1::int8) UNION SELECT \"test_access_partner\".\"id\" FROM \"test_access_partner\" JOIN \
         \"__h\" ON \"test_access_partner\".\"parent_id\" = \"__h\".\"id\") SELECT \"__h\".\"id\" \
         FROM \"__h\") ORDER BY \"test_access_some_obj\".\"id\"",
    );
    drop(tx);
    h.cleanup().await.unwrap();

    let up = cond(
        "partner_id",
        Operator::ParentOf,
        user_path("access_partner_id.id"),
    );
    let h = harness(rule(OBJ, up)).await;
    let p1 = create(&h, PARTNER, &[row(&[("name", Value::str("P1"))])]).await[0];
    let p2 = create(
        &h,
        PARTNER,
        &[row(&[
            ("name", Value::str("P2")),
            ("parent_id", Value::Int(p1)),
        ])],
    )
    .await[0];
    let p3 = create(
        &h,
        PARTNER,
        &[row(&[
            ("name", Value::str("P3")),
            ("parent_id", Value::Int(p2)),
        ])],
    )
    .await[0];
    let objs = create(
        &h,
        OBJ,
        &[
            row(&[("partner_id", Value::Int(p1))]),
            row(&[("partner_id", Value::Int(p2))]),
            row(&[("partner_id", Value::Int(p3))]),
        ],
    )
    .await;
    write(
        &h,
        "res.users",
        &[DEMO],
        &[("access_partner_id", Value::Int(p2))],
    )
    .await;
    let mut tx = begin_as(&h, DEMO, &[]).await;
    assert_eq!(search(&mut tx, OBJ).await, set(&objs[..2]));
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn negated_hierarchy_over_a_parameter() {
    let outside = !cond("org_id", Operator::ChildOf, user_path("access_org_id.id"));
    let h = harness(rule(DOC, outside)).await;
    let o = orgs(&h).await;
    write(
        &h,
        "res.users",
        &[DEMO],
        &[("access_org_id", Value::Int(o.b))],
    )
    .await;
    let mut tx = begin_as(&h, DEMO, &[]).await;
    assert_eq!(
        search(&mut tx, DOC).await,
        set(&[o.docs[0], o.docs[3], o.docs[4]])
    );
    assert_sql(
        &sql_of(&mut tx, DOC).await,
        &format!(
            "{DOC_SELECT} (\"test_access_org_doc\".\"org_id\" IS NULL OR \
             \"test_access_org_doc\".\"org_id\" NOT IN {}) {DOC_ORDER}",
            subtree(&user_org(1))
        ),
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// The hierarchy's links are read by the predicate: moving an organisation in the transaction
/// is flushed before the next check and forgets the memoized checks that depended on it.
#[tokio::test]
async fn moving_an_org_forgets_memoized_checks() {
    let h = harness(rule(
        DOC,
        cond("org_id", Operator::ChildOf, user_path("access_org_id.id")),
    ))
    .await;
    let o = orgs(&h).await;
    write(
        &h,
        "res.users",
        &[DEMO],
        &[("access_org_id", Value::Int(o.b))],
    )
    .await;
    let mut tx = begin_as(&h, DEMO, &[]).await;
    let doc_c = records(&tx, DOC, &o.docs[2..3]);
    doc_c.check_access(&mut tx, Operation::Read).await.unwrap();
    assert_query_count!(tx, <= 0, {
        doc_c.check_access(&mut tx, Operation::Read).await.unwrap();
    });
    // C moves under D (as superuser, buffered)
    records(&tx, ORG, &[o.c])
        .sudo(true)
        .write_values(&mut tx, &row(&[("parent_id", Value::Int(o.d))]))
        .await
        .unwrap();
    let err = doc_c
        .check_access(&mut tx, Operation::Read)
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access, "{err}");
    drop(tx);
    h.cleanup().await.unwrap();
}

/// A hierarchy starts from record ids: "now" names none.
#[tokio::test]
async fn now_cannot_start_a_hierarchy() {
    let h = harness(rule(
        DOC,
        cond(
            "org_id",
            Operator::ChildOf,
            erp_sdk::domain::Value::Param(PrincipalParam::Now(NowKind::Date)),
        ),
    ))
    .await;
    let mut tx = begin_as(&h, DEMO, &[]).await;
    let records = tx.env().records(DOC).unwrap();
    let err = records.search(&mut tx, &Domain::TRUE).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming, "{err}");
    assert_eq!(
        err.message(),
        "The principal parameter context_today() in ('org_id', 'child_of', context_today()) \
         names no record: child_of starts from record ids"
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Policy: the user reads only the organisation named `D` (so it cannot see the start record `A`
/// of the document rule), and the document rule is `doc_rule`.
fn blind_to_a(doc_rule: Domain) -> DomainPolicy {
    DomainPolicy::builder(Otherwise::Allow)
        .rule(ORG, Operation::Read, cond("name", Operator::Eq, "D"))
        .rule(DOC, Operation::Read, doc_rule)
        .build()
        .unwrap()
}

#[tokio::test]
async fn a_negated_child_of_restriction_holds_when_the_start_record_is_hidden() {
    let h = harness(blind_to_a(!cond("org_id", Operator::ChildOf, "A"))).await;
    let o = orgs(&h).await;
    let mut tx = begin_as(&h, DEMO, &[]).await;
    assert_eq!(search(&mut tx, ORG).await, set(&[o.d]), "A is hidden");
    let seen = search(&mut tx, DOC).await;
    for hidden in &o.docs[..3] {
        assert!(!seen.contains(hidden), "doc {hidden} is under A: {seen:?}");
    }
    assert!(
        seen.contains(&o.docs[3]),
        "the document of D stays: {seen:?}"
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Review S-2, positive rule:
/// A positive `child_of` rule grants the subtree of a start record the user cannot read.
#[tokio::test]
async fn a_child_of_rule_grants_the_subtree_of_a_hidden_start_record() {
    let h = harness(blind_to_a(cond("org_id", Operator::ChildOf, "A"))).await;
    let o = orgs(&h).await;
    let mut tx = begin_as(&h, DEMO, &[]).await;
    assert_eq!(
        search(&mut tx, DOC).await,
        set(&[o.docs[0], o.docs[1], o.docs[2]])
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

// ----- a hierarchy rule of the comodel inside a relational condition -----------

const FOLDER: &str = "test_access.org_folder";

/// The document rule in the shape of the product rules
/// (`['|', ('company_id', '=', False), ('company_id', 'parent_of', company_ids)]`), the
/// organisations standing in for companies.
fn doc_company_rule() -> Domain {
    Domain::or([
        cond("org_id", Operator::Eq, false),
        cond(
            "org_id",
            Operator::ParentOf,
            erp_sdk::domain::Value::Param(PrincipalParam::CompanyIds),
        ),
    ])
}

/// One folder per document (its main document, the document in its one2many and in its
/// many2many) and one folder below each of those (`upper_id`).
struct Folders {
    by_doc: [i64; 5],
    below: [i64; 5],
}

impl Folders {
    fn all(&self) -> BTreeSet<i64> {
        self.by_doc.iter().chain(&self.below).copied().collect()
    }
}

async fn folders(h: &Harness, o: &Orgs) -> Folders {
    let rows: Vec<_> = o
        .docs
        .iter()
        .map(|d| {
            row(&[
                ("main_doc_id", Value::Int(*d)),
                ("shared_doc_ids", Value::List(vec![Value::Int(*d)].into())),
            ])
        })
        .collect();
    let by_doc: [i64; 5] = create(h, FOLDER, &rows).await.try_into().unwrap();
    for (folder, doc) in by_doc.iter().zip(o.docs) {
        write(h, DOC, &[doc], &[("folder_id", Value::Int(*folder))]).await;
    }
    let rows: Vec<_> = by_doc
        .iter()
        .map(|f| row(&[("upper_id", Value::Int(*f))]))
        .collect();
    let below = create(h, FOLDER, &rows).await.try_into().unwrap();
    Folders { by_doc, below }
}

/// `name ilike 'of'`: every document.
fn any_doc() -> Domain {
    cond("name", Operator::ILike, "of")
}

/// The branch user (company C: the documents of A, B, C and of nobody) and the parent user
/// (company A: the documents of A and of nobody), each with the indexes of the documents the
/// rule lets them read.
const USERS: [(i64, &[usize], &str); 2] = [
    (DEMO, &[0, 1, 2, 4], "branch user in C"),
    (OTHER, &[0, 4], "parent user in A"),
];

/// Search folders with `domain` as both users; `expected(visible document indexes)` is the
/// folder set each must get. Prints and bounds the statements of each search (one, as the
/// top-level form of the rule).
async fn folders_by(
    domain: impl Fn(&Orgs) -> Domain,
    expected: impl Fn(&Folders, &[usize]) -> BTreeSet<i64>,
    label: &str,
) {
    let h = harness(rule(DOC, doc_company_rule())).await;
    let o = orgs(&h).await;
    assert_eq!((o.a, o.c), (1, 3), "ids double as company ids");
    let f = folders(&h, &o).await;
    let domain = domain(&o);
    for (uid, visible, who) in USERS {
        let company = if uid == DEMO { o.c } else { o.a };
        let mut tx = begin_as(&h, uid, &[company]).await;
        // the rule itself, at the top level, as the reference point
        let docs: BTreeSet<i64> = visible.iter().map(|i| o.docs[*i]).collect();
        assert_eq!(search(&mut tx, DOC).await, docs, "{who}: documents");
        tx.capture_statements(true);
        let found = tx
            .env()
            .records(FOLDER)
            .unwrap()
            .search(&mut tx, &domain)
            .await;
        let statements = tx.captured().len();
        tx.capture_statements(false);
        let found: BTreeSet<i64> = found
            .unwrap_or_else(|e| panic!("{label}, {who}: {e}"))
            .ids()
            .into_iter()
            .collect();
        eprintln!("{label}, {who}: {statements} statement(s)");
        assert_eq!(found, expected(&f, visible), "{label}, {who}");
        assert!(statements <= 1, "{label}, {who}: {statements} statements");
        drop(tx);
    }
    h.cleanup().await.unwrap();
}

fn by_doc(f: &Folders, visible: &[usize]) -> BTreeSet<i64> {
    visible.iter().map(|i| f.by_doc[*i]).collect()
}

/// (a) `any` through a many2one into a model whose rule walks `parent_of company_ids`: the
/// folders whose main document the user may read, as the branch and as the parent user.
#[tokio::test]
async fn any_through_a_many2one_applies_the_comodel_hierarchy_rule() {
    folders_by(
        |_| cond("main_doc_id", Operator::Any, any_doc()),
        by_doc,
        "many2one any",
    )
    .await;
}

/// (b) `any` through a one2many.
#[tokio::test]
async fn any_through_a_one2many_applies_the_comodel_hierarchy_rule() {
    folders_by(
        |_| cond("doc_ids", Operator::Any, any_doc()),
        by_doc,
        "one2many any",
    )
    .await;
}

/// (c) `any` through a many2many.
#[tokio::test]
async fn any_through_a_many2many_applies_the_comodel_hierarchy_rule() {
    folders_by(
        |_| cond("shared_doc_ids", Operator::Any, any_doc()),
        by_doc,
        "many2many any",
    )
    .await;
}

/// (d) A dotted path (`main_doc_id.name`), the same condition as (a).
#[tokio::test]
async fn a_dotted_path_applies_the_comodel_hierarchy_rule() {
    folders_by(
        |_| cond("main_doc_id.name", Operator::ILike, "of"),
        by_doc,
        "dotted path",
    )
    .await;
}

/// (e) `not any`: the folders without a readable main document (a document the rule hides
/// counts as no document), the folders below included.
#[tokio::test]
async fn not_any_applies_the_comodel_hierarchy_rule() {
    folders_by(
        |_| cond("main_doc_id", Operator::NotAny, any_doc()),
        |f, visible| &f.all() - &by_doc(f, visible),
        "many2one not any",
    )
    .await;
}

/// (f) Two levels: through a folder without a rule into the documents
/// (`upper_id.main_doc_id.name`), and through the documents, their folder and the documents
/// again (`main_doc_id.folder_id.main_doc_id.name`, the rule at both nested levels).
#[tokio::test]
async fn two_levels_apply_the_comodel_hierarchy_rule() {
    folders_by(
        |_| cond("upper_id.main_doc_id.name", Operator::ILike, "of"),
        |f, visible| visible.iter().map(|i| f.below[*i]).collect(),
        "two levels, rule at the inner one",
    )
    .await;
    folders_by(
        |_| {
            cond(
                "main_doc_id",
                Operator::Any,
                cond(
                    "folder_id",
                    Operator::Any,
                    cond("main_doc_id", Operator::Any, any_doc()),
                ),
            )
        },
        by_doc,
        "two levels, rule at both",
    )
    .await;
}

/// (h) A rule's own relational condition traverses as the superuser (the comodel's rule does
/// not apply inside it) and its own hierarchy condition through the relation is
/// resolved like one at its top level: a folder rule `main_doc_id.org_id parent_of
/// company_ids` lets each user read the folders of the documents of its company and above
/// (the document of nobody does not satisfy it).
#[tokio::test]
async fn a_rule_with_a_hierarchy_condition_through_a_relation() {
    let folder_rule = cond(
        "main_doc_id.org_id",
        Operator::ParentOf,
        erp_sdk::domain::Value::Param(PrincipalParam::CompanyIds),
    );
    let policy = DomainPolicy::builder(Otherwise::Allow)
        .rule(DOC, Operation::Read, doc_company_rule())
        .rule(FOLDER, Operation::Read, folder_rule)
        .build()
        .unwrap();
    let h = harness(policy).await;
    let o = orgs(&h).await;
    let f = folders(&h, &o).await;
    for (uid, visible, who) in USERS {
        let company = if uid == DEMO { o.c } else { o.a };
        let mut tx = begin_as(&h, uid, &[company]).await;
        let with_org: Vec<usize> = visible.iter().copied().filter(|i| *i != 4).collect();
        assert_eq!(
            search(&mut tx, FOLDER).await,
            by_doc(&f, &with_org),
            "{who}"
        );
        // and a search through the folders' relation on top of that rule
        let found: BTreeSet<i64> = tx
            .env()
            .records(FOLDER)
            .unwrap()
            .search(&mut tx, &cond("doc_ids", Operator::Any, any_doc()))
            .await
            .unwrap_or_else(|e| panic!("{who}: {e}"))
            .ids()
            .into_iter()
            .collect();
        assert_eq!(found, by_doc(&f, &with_org), "{who}: through doc_ids");
        drop(tx);
    }
    h.cleanup().await.unwrap();
}

/// The `access` operator on a many2one inlines the comodel's rule, its
/// hierarchy condition included: the folders whose main document the user may read.
#[tokio::test]
async fn the_access_operator_inlines_the_comodel_hierarchy_rule() {
    folders_by(
        |_| cond("main_doc_id", Operator::Access, "read"),
        by_doc,
        "access operator",
    )
    .await;
}

/// A folder rule `main_doc_id.name ilike 'of'` admits every folder with a main document, also
/// those whose document the user cannot read.
#[tokio::test]
async fn a_dotted_path_in_a_rule_does_not_apply_the_comodel_rule() {
    let policy = DomainPolicy::builder(Otherwise::Allow)
        .rule(DOC, Operation::Read, doc_company_rule())
        .rule(
            FOLDER,
            Operation::Read,
            cond("main_doc_id.name", Operator::ILike, "of"),
        )
        .build()
        .unwrap();
    let h = harness(policy).await;
    let o = orgs(&h).await;
    let f = folders(&h, &o).await;
    for (uid, visible, who) in USERS {
        let company = if uid == DEMO { o.c } else { o.a };
        let mut tx = begin_as(&h, uid, &[company]).await;
        assert!(visible.len() < 5, "{who}: some documents are hidden");
        assert_eq!(
            search(&mut tx, FOLDER).await,
            by_doc(&f, &[0, 1, 2, 3, 4]),
            "{who}"
        );
        drop(tx);
    }
    h.cleanup().await.unwrap();
}
