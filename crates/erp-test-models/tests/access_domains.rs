//! Access predicates in searches, the `access` operator, relational values and query budgets
//! (design note §3.3, §7): DOM, REL, GRP and SEC cases that depend on
//! record rules, with the testkit's `DomainPolicy` and `RulesPolicy` standing in for compiled
//! `ir.access` rules.
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
    BeginOptions, CompanyId, GroupSet, IntoPolicyProvider, Operation, PrincipalBinding,
    PrincipalBuilder, Resolution, UserId,
};
use erp_registry::{FieldDecl, ModelDecl, ModelSpec};
use erp_sdk::domain::{NowKind, PrincipalParam};
use erp_sdk::{Domain, ErrorKind, Operator, OptimizationLevel, Records, Transaction, Value};
use erp_test_models::{base_users, test_access};
use erp_testkit::{DomainPolicy, Harness, Otherwise, RulesPolicy, TestRules};

// Membership-only language model for these minimal registries (the full base addon is absent).
const LANGUAGE_FIELDS: &[FieldDecl] = &[
    FieldDecl::char("code").required(true),
    FieldDecl::boolean("active"),
];
use support::{assert_sql, dom};

const OBJ: &str = "test_access.some_obj";
const CATEG: &str = "test_access.categ";
const PARTNER: &str = "test_access.partner";
const DEMO: i64 = 2;
const OTHER: i64 = 3;
const PORTAL: i64 = 4;

async fn harness(policy: impl IntoPolicyProvider) -> Harness {
    let h = Harness::builder([&base_users::ADDON, &test_access::ADDON])
        .with_decls([ModelDecl::new("base_users", "res.lang").fields(LANGUAGE_FIELDS)])
        .policy(policy)
        .build()
        .await
        .expect("harness");
    let mut tx = h.begin().await.unwrap();
    tx.batch(
        "INSERT INTO res_users (id, name, login, active) VALUES (2, 'demo', 'demo', true), \
         (3, 'other', 'other', true), (4, 'portal', 'portal', true); \
         SELECT setval(pg_get_serial_sequence('res_users', 'id'), 4)",
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    h
}

async fn begin_as(h: &Harness, uid: i64) -> Transaction {
    let companies = [1, 2].map(|c| CompanyId::new(c).unwrap());
    let principal = PrincipalBuilder::user(UserId::new(uid).unwrap())
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

fn cond(field: &str, op: Operator, value: impl Into<erp_sdk::domain::Value>) -> Domain {
    Domain::condition(field, op, value).unwrap()
}

fn set(ids: &[i64]) -> BTreeSet<i64> {
    ids.iter().copied().collect()
}

async fn search(tx: &mut Transaction, records: &Records, domain: &Domain) -> BTreeSet<i64> {
    set(&records.search(tx, domain).await.unwrap().ids())
}

/// The SQL of a search (its last statement).
async fn sql_of(tx: &mut Transaction, records: &Records, domain: &Domain) -> String {
    tx.capture_statements(true);
    records.search(tx, domain).await.unwrap();
    let sql = tx.captured().last().cloned().unwrap_or_default();
    tx.capture_statements(false);
    sql
}

struct Rights;

impl TestRules for Rights {
    fn check_model(
        &self,
        b: &PrincipalBinding<'_>,
        model: &ModelSpec,
        op: Operation,
    ) -> erp_sdk::Result<()> {
        if self.access_domain(b, model, op)?.is_false() {
            return Err(erp_sdk::Error::access(format!(
                "You are not allowed to {} '{}' ({}) records.",
                op.as_str(),
                model.description,
                model.name
            )));
        }
        Ok(())
    }

    fn access_domain(
        &self,
        b: &PrincipalBinding<'_>,
        model: &ModelSpec,
        op: Operation,
    ) -> erp_sdk::Result<Domain> {
        let uid = b.principal().subject().get();
        Ok(match (model.name, uid, op) {
            (_, PORTAL, _) if model.name == OBJ || model.name == CATEG => Domain::FALSE,
            (OBJ, DEMO, _) => cond("val", Operator::Gt, 20),
            (CATEG, DEMO, Operation::Write) => cond("name", Operator::Eq, "foo"),
            (CATEG, OTHER, Operation::Write) => cond(
                "name",
                Operator::In,
                erp_sdk::domain::Value::list(["foo", "bar"]),
            ),
            _ => Domain::TRUE,
        })
    }
}

/// Operator half with the testkit's `RulesPolicy`: optimization against the
/// transaction's policy, searches, in-memory evaluation and errors. The whole case, with the
/// rules as `ir_access` rows, is `spec_dom_035_access_operator` in `erp-security`'s
/// `tests/spec_whole.rs`.
#[tokio::test]
async fn spec_dom_035_access_operator() {
    let h = harness(RulesPolicy::new(Rights).unwrap()).await;
    let categ = create(&h, CATEG, &[row(&[("name", Value::str("bar"))])]).await[0];
    let objs = create(
        &h,
        OBJ,
        &[
            row(&[("val", Value::Int(11))]),
            row(&[("val", Value::Int(22))]),
            row(&[("val", Value::Int(23)), ("categ_id", Value::Int(categ))]),
        ],
    )
    .await;
    let id_read = cond("id", Operator::Access, "read");
    let categ_write = cond("categ_id", Operator::Access, "write");
    for uid in [DEMO, OTHER, PORTAL] {
        for sudo in [false, true] {
            let tx = begin_as(&h, uid).await;
            let model = tx.env().records(OBJ).unwrap().sudo(sudo);
            let basic = model
                .optimize_domain(&id_read, OptimizationLevel::Basic, None)
                .unwrap();
            assert_eq!(basic, id_read, "BASIC leaves it unchanged");
            let full = model
                .optimize_domain_in(&tx, &id_read, OptimizationLevel::Full, None)
                .unwrap();
            let expected = match uid {
                DEMO => model
                    .optimize_domain(
                        &cond("val", Operator::Gt, 20),
                        OptimizationLevel::Full,
                        None,
                    )
                    .unwrap(),
                OTHER => Domain::TRUE,
                _ => Domain::FALSE,
            };
            assert_eq!(full, expected, "uid {uid} sudo {sudo}");
            let m2o = model
                .optimize_domain_in(&tx, &categ_write, OptimizationLevel::Full, None)
                .unwrap();
            let write_domain = match uid {
                DEMO => Some(cond("name", Operator::Eq, "foo")),
                OTHER => Some(cond(
                    "name",
                    Operator::In,
                    erp_sdk::domain::Value::list(["foo", "bar"]),
                )),
                _ => None,
            };
            let expected = match write_domain {
                Some(d) => model
                    .optimize_domain(
                        &cond("categ_id", Operator::AnyBypass, d),
                        OptimizationLevel::Full,
                        None,
                    )
                    .unwrap(),
                None => Domain::FALSE,
            };
            assert_eq!(m2o, expected, "uid {uid} sudo {sudo}");
            drop(tx);
        }
    }
    // with full access to the comodel: the many2one is set
    let tx = begin_as(&h, OTHER).await;
    let model = tx.env().records(OBJ).unwrap();
    assert_eq!(
        model
            .optimize_domain_in(
                &tx,
                &cond("categ_id", Operator::Access, "read"),
                OptimizationLevel::Full,
                None
            )
            .unwrap(),
        model
            .optimize_domain(
                &cond("categ_id", Operator::Ne, false),
                OptimizationLevel::Full,
                None
            )
            .unwrap()
    );
    drop(tx);
    // in-memory evaluation of the dynamic form equals the filtered access, and the many2one
    // form never keeps a record without a target
    let mut tx = begin_as(&h, DEMO).await;
    let records = tx.env().records(OBJ).unwrap().browse(objs.clone()).unwrap();
    let in_memory = records.filtered_domain(&mut tx, &id_read).await.unwrap();
    let filtered = records
        .filtered_access(&mut tx, Operation::Read)
        .await
        .unwrap();
    assert_eq!(in_memory.ids(), filtered.ids());
    assert_eq!(in_memory.ids(), objs[1..]);
    let with_target = records
        .sudo(true)
        .filtered_domain(&mut tx, &categ_write)
        .await
        .unwrap();
    assert!(with_target.is_empty(), "the category is not named foo");
    assert_eq!(search_ids(&mut tx, &records, &id_read).await, objs[1..]);
    // errors
    let container = tx.env().records("test_access.container").unwrap();
    for (records, domain, message) in [
        (
            &container,
            cond("some_ids", Operator::Access, "read"),
            "The 'access' operator works only for many2one and 'id' fields in condition \
             ('some_ids', 'access', 'read')",
        ),
        (
            &records,
            cond("val", Operator::Access, "read"),
            "The 'access' operator works only for many2one and 'id' fields in condition \
             ('val', 'access', 'read')",
        ),
        (
            &records,
            cond("categ_id", Operator::Access, "blabla"),
            "Invalid value for 'access' operator in condition ('categ_id', 'access', 'blabla')",
        ),
    ] {
        let err = records.search(&mut tx, &domain).await.unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Programming, "{err}");
        assert_eq!(err.message(), message);
    }
    drop(tx);
    h.cleanup().await.unwrap();
}

async fn search_ids(tx: &mut Transaction, records: &Records, domain: &Domain) -> Vec<i64> {
    records.search(tx, domain).await.unwrap().ids()
}

/// An `access` condition in a rule compiles into the comodel's rule inline (one statement), and
/// a chain that comes back to a (model, operation) being resolved is refused (R-S4).
#[tokio::test]
async fn access_conditions_in_rules_inline_and_cycles_are_refused() {
    let policy = DomainPolicy::builder(Otherwise::Allow)
        .rule(
            OBJ,
            Operation::Read,
            cond("categ_id", Operator::Access, "read"),
        )
        .rule(CATEG, Operation::Read, cond("name", Operator::Ne, "hidden"))
        .build()
        .unwrap();
    let h = harness(policy).await;
    let categs = create(
        &h,
        CATEG,
        &[
            row(&[("name", Value::str("seen"))]),
            row(&[("name", Value::str("hidden"))]),
        ],
    )
    .await;
    let objs = create(
        &h,
        OBJ,
        &[
            row(&[("categ_id", Value::Int(categs[0]))]),
            row(&[("categ_id", Value::Int(categs[1]))]),
            row(&[]),
        ],
    )
    .await;
    let mut tx = begin_as(&h, DEMO).await;
    let records = tx.env().records(OBJ).unwrap();
    let found = erp_testkit::assert_query_count!(tx, >= 1, <= 1, {
        search_ids(&mut tx, &records, &Domain::TRUE).await
    });
    assert_eq!(found, [objs[0]]);
    drop(tx);
    h.cleanup().await.unwrap();

    // a self cycle and a cycle two hops deep
    for (policy, message) in [
        (
            DomainPolicy::builder(Otherwise::Allow)
                .rule(OBJ, Operation::Read, cond("id", Operator::Access, "read"))
                .build()
                .unwrap(),
            "The 'access' operator forms a cycle: test_access.some_obj read → \
             test_access.some_obj read",
        ),
        (
            DomainPolicy::builder(Otherwise::Allow)
                .rule(
                    OBJ,
                    Operation::Read,
                    cond("partner_id", Operator::Access, "read"),
                )
                .rule(
                    PARTNER,
                    Operation::Read,
                    cond("commercial_partner_id", Operator::Access, "read"),
                )
                .build()
                .unwrap(),
            "The 'access' operator forms a cycle: test_access.partner read → \
             test_access.partner read",
        ),
    ] {
        let h = harness(policy).await;
        let mut tx = begin_as(&h, DEMO).await;
        let err = tx
            .env()
            .records(OBJ)
            .unwrap()
            .search(&mut tx, &Domain::TRUE)
            .await
            .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Programming);
        assert_eq!(err.message(), message);
        drop(tx);
        h.cleanup().await.unwrap();
    }
}

#[tokio::test]
async fn spec_dom_037_many2one_any_under_rules_is_an_in_subselect() {
    let rule = cond("id", Operator::Lt, 1000);
    let policy = DomainPolicy::builder(Otherwise::Allow)
        .rule("test_access.related", Operation::Read, rule.clone())
        .rule("test_access.related_foo", Operation::Read, rule.clone())
        .rule("test_access.related_bar", Operation::Read, rule)
        .build()
        .unwrap();
    let h = harness(policy).await;
    let mut tx = begin_as(&h, DEMO).await;
    let related = tx.env().records("test_access.related").unwrap();
    let sql = sql_of(
        &mut tx,
        &related,
        &dom(serde_json::json!([["foo_id", "any", [["name", "=", "a"]]]])),
    )
    .await;
    assert_sql(
        &sql,
        "SELECT \"test_access_related\".\"id\" FROM \"test_access_related\" WHERE \
         \"test_access_related\".\"foo_id\" IN (SELECT \"test_access_related_foo\".\"id\" FROM \
         \"test_access_related_foo\" WHERE (\"test_access_related_foo\".\"id\" < $1::int8 AND \
         \"test_access_related_foo\".\"name\" = ANY($2::text[]))) AND \
         \"test_access_related\".\"id\" < $3::int8 ORDER BY \"test_access_related\".\"id\"",
    );
    let sql = sql_of(
        &mut tx,
        &related,
        &dom(serde_json::json!([[
            "foo_id",
            "any",
            [["bar_id", "any", [["name", "=", "a"]]]]
        ]])),
    )
    .await;
    assert_sql(
        &sql,
        "SELECT \"test_access_related\".\"id\" FROM \"test_access_related\" WHERE \
         \"test_access_related\".\"foo_id\" IN (SELECT \"test_access_related_foo\".\"id\" FROM \
         \"test_access_related_foo\" WHERE (\"test_access_related_foo\".\"bar_id\" IN (SELECT \
         \"test_access_related_bar\".\"id\" FROM \"test_access_related_bar\" WHERE \
         (\"test_access_related_bar\".\"id\" < $1::int8 AND \
         \"test_access_related_bar\".\"name\" = ANY($2::text[]))) AND \
         \"test_access_related_foo\".\"id\" < $3::int8)) AND \"test_access_related\".\"id\" < \
         $4::int8 ORDER BY \"test_access_related\".\"id\"",
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_dom_049_access_domain_merged_into_search() {
    let policy = DomainPolicy::builder(Otherwise::Allow)
        .rule(OBJ, Operation::Read, cond("name", Operator::Like, "a"))
        .deny(CATEG, Operation::Read)
        .build()
        .unwrap();
    let h = harness(policy).await;
    let ids = create(
        &h,
        OBJ,
        &[
            row(&[("name", Value::str("alpha")), ("val", Value::Int(1))]),
            row(&[("name", Value::str("beta")), ("val", Value::Int(2))]),
            row(&[("name", Value::str("gamma")), ("val", Value::Int(3))]),
        ],
    )
    .await;
    let mut tx = begin_as(&h, DEMO).await;
    let objs = tx.env().records(OBJ).unwrap();
    // the search domain first, the rule as a separate AND clause
    let sql = sql_of(&mut tx, &objs, &cond("val", Operator::Gt, 0)).await;
    assert_sql(
        &sql,
        "SELECT \"test_access_some_obj\".\"id\" FROM \"test_access_some_obj\" WHERE \
         \"test_access_some_obj\".\"val\" > $1::int8 AND \"test_access_some_obj\".\"name\" LIKE \
         $2::text ORDER BY \"test_access_some_obj\".\"id\"",
    );
    // pending writes of the field the rule reads are flushed before the query
    objs.browse([ids[1]])
        .unwrap()
        .sudo(true)
        .write_values(&mut tx, &row(&[("name", Value::str("bar"))]))
        .await
        .unwrap();
    assert_eq!(
        search_ids(&mut tx, &objs, &Domain::TRUE).await,
        [ids[0], ids[1], ids[2]]
    );
    objs.browse([ids[1]])
        .unwrap()
        .sudo(true)
        .write_values(&mut tx, &row(&[("name", Value::str("xyz"))]))
        .await
        .unwrap();
    assert_eq!(
        search_ids(&mut tx, &objs, &Domain::TRUE).await,
        [ids[0], ids[2]]
    );
    // a hidden record: absent from the search, an error when read
    let err = objs
        .browse([ids[1]])
        .unwrap()
        .cells(&mut tx, "val")
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access);
    assert_eq!(
        err.message(),
        format!(
            "You are not allowed to access 'Object For Test Access Right' \
             (test_access.some_obj) records.\n\nRecords: test_access.some_obj({},), User: {DEMO}",
            ids[1]
        )
    );
    // no model access: the model check refuses before any SQL
    let categs = tx.env().records(CATEG).unwrap();
    let err = erp_testkit::assert_query_count!(tx, <= 0, {
        categs.search(&mut tx, &Domain::TRUE).await.unwrap_err()
    });
    assert_eq!(err.kind(), ErrorKind::Access);
    assert_eq!(
        err.message(),
        "You are not allowed to read 'Access Category' (test_access.categ) records."
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_dom_079_hierarchy_operators_under_rules() {
    let policy = DomainPolicy::builder(Otherwise::Allow)
        .rule(
            PARTNER,
            Operation::Read,
            cond(
                "name",
                Operator::In,
                erp_sdk::domain::Value::list(["Top", "Bottom"]),
            ),
        )
        .build()
        .unwrap();
    let h = harness(policy).await;
    let top = create(&h, PARTNER, &[row(&[("name", Value::str("Top"))])]).await[0];
    let medium = create(
        &h,
        PARTNER,
        &[row(&[
            ("name", Value::str("Medium")),
            ("parent_id", Value::Int(top)),
        ])],
    )
    .await[0];
    let bottom = create(
        &h,
        PARTNER,
        &[row(&[
            ("name", Value::str("Bottom")),
            ("parent_id", Value::Int(medium)),
        ])],
    )
    .await[0];
    let banks = create(
        &h,
        OBJ,
        &[
            row(&[("partner_id", Value::Int(top))]),
            row(&[("partner_id", Value::Int(medium))]),
            row(&[("partner_id", Value::Int(bottom))]),
        ],
    )
    .await;
    let mut tx = begin_as(&h, DEMO).await;
    let partners = tx.env().records(PARTNER).unwrap();
    let top_bottom = set(&[top, bottom]);
    assert_eq!(search(&mut tx, &partners, &Domain::TRUE).await, top_bottom);
    assert_eq!(
        search(&mut tx, &partners, &cond("id", Operator::ChildOf, top)).await,
        top_bottom
    );
    assert_eq!(
        search(&mut tx, &partners, &cond("id", Operator::ParentOf, bottom)).await,
        top_bottom
    );
    let objs = tx.env().records(OBJ).unwrap();
    assert_eq!(
        search(
            &mut tx,
            &objs,
            &cond(
                "partner_id",
                Operator::In,
                erp_sdk::domain::Value::list([top, bottom])
            )
        )
        .await,
        set(&[banks[0], banks[2]])
    );
    assert_eq!(
        search(&mut tx, &objs, &cond("partner_id", Operator::ChildOf, top)).await,
        set(&banks)
    );
    assert_eq!(
        search(
            &mut tx,
            &objs,
            &cond("partner_id", Operator::ParentOf, bottom)
        )
        .await,
        set(&banks)
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_dom_080_many2one_not_like_under_rules() {
    let policy = DomainPolicy::builder(Otherwise::Allow)
        .rule(
            PARTNER,
            Operation::Read,
            cond("name", Operator::Ne, "Parent 1"),
        )
        .build()
        .unwrap();
    let h = harness(policy).await;
    let parents = create(
        &h,
        PARTNER,
        &[
            row(&[("name", Value::str("Parent 1"))]),
            row(&[("name", Value::str("Parent 2"))]),
        ],
    )
    .await;
    let children = create(
        &h,
        PARTNER,
        &[
            row(&[
                ("name", Value::str("Child 1")),
                ("parent_id", Value::Int(parents[0])),
            ]),
            row(&[
                ("name", Value::str("Child 2")),
                ("parent_id", Value::Int(parents[1])),
            ]),
            row(&[("name", Value::str("Other"))]),
        ],
    )
    .await;
    let all: Vec<i64> = parents.iter().chain(&children).copied().collect();
    let mut su = h.begin().await.unwrap();
    let partners = su.env().records(PARTNER).unwrap();
    let in_all = cond(
        "id",
        Operator::In,
        erp_sdk::domain::Value::list(all.clone()),
    );
    let like = cond("parent_id", Operator::Like, "Parent") & in_all.clone();
    assert_eq!(search(&mut su, &partners, &like).await, set(&children[..2]));
    drop(su);
    let mut tx = begin_as(&h, DEMO).await;
    let partners = tx.env().records(PARTNER).unwrap();
    let not_like = cond("parent_id", Operator::NotLike, "Parent") & in_all;
    let expected = set(&[parents[1], children[0], children[2]]);
    assert_eq!(search(&mut tx, &partners, &not_like).await, expected);
    // filtered_domain agrees on the records the user can see
    let visible = partners.search(&mut tx, &Domain::TRUE).await.unwrap();
    let kept = visible
        .filtered_domain(&mut tx, &cond("parent_id", Operator::NotLike, "Parent"))
        .await
        .unwrap();
    assert_eq!(set(&kept.ids()), expected);
    drop(tx);
    h.cleanup().await.unwrap();
}

/// A container linking a readable and an unreadable object through a plain many2many and through
/// one with `bypass_search_access`.
async fn linked(h: &Harness) -> (i64, i64, i64) {
    let objs = create(
        h,
        OBJ,
        &[
            row(&[("name", Value::str("allowed")), ("val", Value::Int(1))]),
            row(&[("name", Value::str("blocked")), ("val", Value::Int(-1))]),
        ],
    )
    .await;
    let both = Value::list([Value::Int(objs[0]), Value::Int(objs[1])]);
    let container = create(
        h,
        "test_access.container",
        &[row(&[("some_ids", both.clone()), ("bypass_ids", both)])],
    )
    .await[0];
    (container, objs[0], objs[1])
}

fn read_positive() -> DomainPolicy {
    DomainPolicy::builder(Otherwise::Allow)
        .rule(OBJ, Operation::Read, cond("val", Operator::Gt, 0))
        .build()
        .unwrap()
}

#[tokio::test]
async fn spec_rel_026_x2many_values_filtered_unless_bypassed() {
    let h = harness(read_positive()).await;
    let (container, allowed, blocked) = linked(&h).await;
    let mut tx = begin_as(&h, DEMO).await;
    let boxed = tx
        .env()
        .records("test_access.container")
        .unwrap()
        .browse([container])
        .unwrap();
    // with bypass_search_access the unreadable record is returned, without it it is silently
    // filtered out
    assert_eq!(
        boxed.related(&mut tx, "bypass_ids").await.unwrap().ids(),
        [allowed, blocked]
    );
    assert_eq!(
        boxed.related(&mut tx, "some_ids").await.unwrap().ids(),
        [allowed]
    );
    // a superuser always sees it
    let su = boxed.sudo(true);
    assert_eq!(
        su.related(&mut tx, "some_ids").await.unwrap().ids(),
        [allowed, blocked]
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_grp_027_web_read_filters_inaccessible_x2many_records() {
    let h = harness(read_positive()).await;
    let (container, allowed, _blocked) = linked(&h).await;
    let mut tx = begin_as(&h, DEMO).await;
    let boxed = tx
        .env()
        .records("test_access.container")
        .unwrap()
        .browse([container])
        .unwrap();
    let spec = erp_orm::Spec::from_value(&Value::Map(std::sync::Arc::new(
        [(
            "some_ids".to_owned(),
            Value::Map(std::sync::Arc::new(
                [(
                    "fields".to_owned(),
                    Value::Map(std::sync::Arc::new(
                        [("display_name".to_owned(), Value::Map(Default::default()))]
                            .into_iter()
                            .collect(),
                    )),
                )]
                .into_iter()
                .collect(),
            )),
        )]
        .into_iter()
        .collect(),
    )))
    .unwrap();
    let ids_of = |rows: &[Value]| -> Vec<i64> {
        let first = support::deep(&rows[0]);
        let Value::Map(record) = &first else {
            panic!("a record dictionary")
        };
        let Some(Value::List(lines)) = record.get("some_ids") else {
            panic!("some_ids as a list: {record:?}")
        };
        lines
            .iter()
            .map(|line| match line {
                Value::Map(m) => match m.get("id") {
                    Some(Value::Int(i)) => *i,
                    other => panic!("an id: {other:?}"),
                },
                other => panic!("a line dictionary: {other:?}"),
            })
            .collect()
    };
    let before = boxed.web_read(&mut tx, spec.clone()).await.unwrap();
    assert_eq!(ids_of(&before), [allowed]);
    // a superuser read of the same relation fills the cache with every target
    boxed.sudo(true).related(&mut tx, "some_ids").await.unwrap();
    let after = boxed.web_read(&mut tx, spec).await.unwrap();
    assert_eq!(ids_of(&after), [allowed]);
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Engine half with the testkit's `DomainPolicy`: access predicates are bound and
/// evaluated without the caller's context (timezone, language, active test). The whole case,
/// with `user.env[...].search()` rule strings from `ir_access` rows, is
/// `spec_sec_015_context_does_not_affect_access_domains` in `erp-security`'s
/// `tests/spec_whole.rs`.
#[tokio::test]
async fn spec_sec_015_context_does_not_affect_access_predicates() {
    let rule = Domain::or([
        cond(
            "deadline",
            Operator::Ge,
            erp_sdk::domain::Value::Param(PrincipalParam::Now(NowKind::Date)),
        ),
        cond(
            "categ_id",
            Operator::AnyBypass,
            cond("name", Operator::Eq, "media"),
        ),
    ]);
    let policy = DomainPolicy::builder(Otherwise::Allow)
        .rule(OBJ, Operation::Read, rule)
        .build()
        .unwrap();
    let h = harness(policy).await;
    let mut setup = h.begin().await.unwrap();
    setup
        .batch("INSERT INTO res_lang (code, active) VALUES ('fr_FR', true)")
        .await
        .unwrap();
    setup.commit().await.unwrap();
    let media = create(&h, CATEG, &[row(&[("name", Value::str("media"))])]).await[0];
    let ids = create(
        &h,
        OBJ,
        &[
            row(&[("deadline", Value::str("2999-12-31"))]),
            row(&[("deadline", Value::str("2000-01-01"))]),
            row(&[("categ_id", Value::Int(media))]),
            row(&[]),
        ],
    )
    .await;
    let mut results = Vec::new();
    for context in [
        vec![],
        vec![("tz", erp_sdk::Value::str("Pacific/Kiritimati"))],
        vec![
            ("active_test", erp_sdk::Value::Bool(false)),
            ("lang", erp_sdk::Value::str("fr_FR")),
            ("only_media", erp_sdk::Value::Bool(true)),
        ],
    ] {
        let mut tx = begin_as(&h, DEMO).await;
        let objs = tx.env().with_context(context).records(OBJ).unwrap();
        let sql = sql_of(&mut tx, &objs, &Domain::TRUE).await;
        let found = search_ids(&mut tx, &objs, &Domain::TRUE).await;
        results.push((sql, found));
        drop(tx);
    }
    assert_eq!(results[0].1, [ids[0], ids[2]]);
    assert!(results.iter().all(|r| *r == results[0]), "{results:#?}");
    h.cleanup().await.unwrap();
}

/// Statement budgets of the design note §7 (S1 rows): a restricting predicate adds no statement
/// to a search, an explicit read or a create whose predicate the INSERT decides (fused,
/// decision 11), a record check one per batch to a write and an unlink, an onchange's checks are
/// free the second time.
#[tokio::test]
async fn query_budgets_under_a_restricting_policy() {
    let rule = Domain::or([
        cond("company_id", Operator::Eq, false),
        cond(
            "company_id",
            Operator::In,
            erp_sdk::domain::Value::Param(PrincipalParam::CompanyIds),
        ),
    ]);
    let policy = DomainPolicy::builder(Otherwise::Allow)
        .rule_all(OBJ, &rule)
        .build()
        .unwrap();
    let h = harness(policy).await;
    let rows: Vec<_> = (0..50).map(|i| row(&[("val", Value::Int(i))])).collect();
    let ids = create(&h, OBJ, &rows).await;
    // counts of one operation as superuser and as the user, in fresh transactions
    async fn counts<F>(h: &Harness, op: F) -> (u64, u64)
    where
        F: AsyncFn(&mut Transaction, Records) -> (),
    {
        let mut su = h.begin().await.unwrap();
        let objs = su.env().records(OBJ).unwrap();
        let before = su.query_count();
        op(&mut su, objs).await;
        let as_su = su.query_count() - before;
        su.rollback().await.unwrap();
        let mut tx = begin_as(h, DEMO).await;
        let objs = tx.env().records(OBJ).unwrap();
        let before = tx.query_count();
        op(&mut tx, objs).await;
        let as_user = tx.query_count() - before;
        tx.rollback().await.unwrap();
        (as_su, as_user)
    }
    let ids_ref = &ids;
    // search: +0
    let (su, user) = counts(&h, async |tx: &mut Transaction, objs: Records| {
        objs.search(tx, &cond("val", Operator::Ge, 10))
            .await
            .unwrap();
    })
    .await;
    assert_eq!(user, su, "search");
    // explicit read of n ids: +0
    let (su, user) = counts(&h, async |tx: &mut Transaction, objs: Records| {
        objs.browse(ids_ref.iter().copied())
            .unwrap()
            .fetch(tx, Some(&["val"]))
            .await
            .unwrap();
    })
    .await;
    assert_eq!(user, su, "explicit read");
    // write n records: one check statement for the batch
    let (su, user) = counts(&h, async |tx: &mut Transaction, objs: Records| {
        objs.browse(ids_ref.iter().copied())
            .unwrap()
            .write_values(tx, &row(&[("val", Value::Int(7))]))
            .await
            .unwrap();
        tx.flush().await.unwrap();
    })
    .await;
    assert_eq!(user, su + 1, "write");
    // create a batch: the predicate reads the row's own columns, so the INSERT decides it
    // : +0
    let (su, user) = counts(&h, async |tx: &mut Transaction, objs: Records| {
        let rows: Vec<_> = (0..10).map(|i| row(&[("val", Value::Int(i))])).collect();
        objs.create_values(tx, &rows).await.unwrap();
    })
    .await;
    assert_eq!(user, su, "create");
    // unlink a batch: one check statement before the DELETE
    let (su, user) = counts(&h, async |tx: &mut Transaction, objs: Records| {
        objs.browse(ids_ref.iter().copied())
            .unwrap()
            .unlink(tx)
            .await
            .unwrap();
    })
    .await;
    assert_eq!(user, su + 1, "unlink");
    // onchange of an existing record: its checks cost nothing the second time
    let mut tx = begin_as(&h, DEMO).await;
    let one = tx.env().records(OBJ).unwrap().browse([ids[0]]).unwrap();
    let spec = erp_orm::Spec::new().field("val");
    let values: erp_sdk::ValueMap = [("val".to_owned(), erp_sdk::Value::Int(3))]
        .into_iter()
        .collect();
    one.onchange(
        &mut tx,
        values.clone(),
        vec!["val".to_owned()],
        spec.clone(),
    )
    .await
    .unwrap();
    let before = tx.query_count();
    one.onchange(
        &mut tx,
        values.clone(),
        vec!["val".to_owned()],
        spec.clone(),
    )
    .await
    .unwrap();
    let warm = tx.query_count() - before;
    drop(tx);
    let mut su = h.begin().await.unwrap();
    let one = su.env().records(OBJ).unwrap().browse([ids[0]]).unwrap();
    one.onchange(
        &mut su,
        values.clone(),
        vec!["val".to_owned()],
        spec.clone(),
    )
    .await
    .unwrap();
    let before = su.query_count();
    one.onchange(&mut su, values, vec!["val".to_owned()], spec)
        .await
        .unwrap();
    assert_eq!(
        warm,
        su.query_count() - before + 1,
        "onchange, memoized checks"
    );
    drop(su);
    h.cleanup().await.unwrap();
}

fn related_rules() -> DomainPolicy {
    let rule = cond("id", Operator::Lt, 1000);
    DomainPolicy::builder(Otherwise::Allow)
        .rule_all("test_access.related", &rule)
        .rule_all("test_access.related_foo", &rule)
        .rule_all("test_access.related_bar", &rule)
        .build()
        .unwrap()
}

/// `_as_query().table` column access to related fields has no ERPForAll counterpart)
#[tokio::test]
async fn spec_comp_040_related_sudo() {
    let policy = DomainPolicy::builder(Otherwise::Allow)
        .rule_all("test_access.related_foo", &cond("name", Operator::Eq, "F1"))
        .build()
        .unwrap();
    let h = harness(policy).await;
    let bars = create(
        &h,
        "test_access.related_bar",
        &[
            row(&[("name", Value::str("B1"))]),
            row(&[("name", Value::str("B2"))]),
        ],
    )
    .await;
    let foos = create(
        &h,
        "test_access.related_foo",
        &[
            row(&[("name", Value::str("F1")), ("bar_id", Value::Int(bars[0]))]),
            row(&[("name", Value::str("F2")), ("bar_id", Value::Int(bars[1]))]),
        ],
    )
    .await;
    let records = create(
        &h,
        "test_access.related",
        &[
            row(&[("name", Value::str("A1")), ("foo_id", Value::Int(foos[0]))]),
            row(&[("name", Value::str("A2")), ("foo_id", Value::Int(foos[1]))]),
        ],
    )
    .await;
    let mut tx = begin_as(&h, DEMO).await;
    let related = tx.env().records("test_access.related").unwrap();
    let first = related.browse([records[0]]).unwrap();
    let second = related.browse([records[1]]).unwrap();
    let text = |cells: Vec<erp_sdk::Cell>| -> Vec<String> {
        cells
            .into_iter()
            .map(|c| match c {
                erp_sdk::Cell::Str(s) => s.to_string(),
                other => panic!("not a string: {other:?}"),
            })
            .collect()
    };
    assert_eq!(
        text(first.cells(&mut tx, "foo_bar_name").await.unwrap()),
        ["B1"]
    );
    let err = second.cells(&mut tx, "foo_bar_name").await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access, "{err}");
    assert_eq!(
        err.message(),
        format!(
            "You are not allowed to access 'Access Related Foo' (test_access.related_foo) \
             records.\n\nRecords: test_access.related_foo({},), User: {DEMO}",
            foos[1]
        )
    );
    assert_eq!(
        text(second.cells(&mut tx, "foo_bar_name_sudo").await.unwrap()),
        ["B2"]
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// `test_related_many2one`, sudo and non-sudo, without the `.name` paths through a related
/// many2one; the binary statements need the attachment store, and the many2many,
/// one2many and `_inherits` ones need related fixtures (`related_foo` x2many fields, a
/// `related_inherits` child) that `erp-test-models` does not have and no board task carries yet.
#[tokio::test]
async fn spec_comp_070_related_search_sql() {
    let h = harness(related_rules()).await;
    let mut tx = begin_as(&h, DEMO).await;
    let related = tx.env().records("test_access.related").unwrap();
    let json = |j: serde_json::Value| dom(j);
    let sudo_join = "SELECT \"test_access_related\".\"id\" FROM \"test_access_related\" LEFT JOIN \
                     \"test_access_related_foo\" AS \"test_access_related__foo_id\" ON \
                     (\"test_access_related\".\"foo_id\" = \"test_access_related__foo_id\".\"id\") \
                     WHERE (\"test_access_related\".\"foo_id\" IS NOT NULL AND \
                     \"test_access_related__foo_id\".\"id\" IS NOT NULL AND";
    let order = "ORDER BY \"test_access_related\".\"id\"";
    let sql = sql_of(
        &mut tx,
        &related,
        &json(serde_json::json!([["foo_name_sudo", "=", "a"]])),
    )
    .await;
    assert_sql(
        &sql,
        &format!(
            "{sudo_join} \"test_access_related__foo_id\".\"name\" = ANY($1::text[])) AND \
             \"test_access_related\".\"id\" < $2::int8 {order}"
        ),
    );
    let sql = sql_of(
        &mut tx,
        &related,
        &json(serde_json::json!([["foo_name", "=", "a"]])),
    )
    .await;
    assert_sql(
        &sql,
        &format!(
            "SELECT \"test_access_related\".\"id\" FROM \"test_access_related\" WHERE \
             \"test_access_related\".\"foo_id\" IN (SELECT \"test_access_related_foo\".\"id\" FROM \
             \"test_access_related_foo\" WHERE (\"test_access_related_foo\".\"id\" < $1::int8 AND \
             \"test_access_related_foo\".\"name\" = ANY($2::text[]))) AND \
             \"test_access_related\".\"id\" < $3::int8 {order}"
        ),
    );
    let sql = sql_of(
        &mut tx,
        &related,
        &json(serde_json::json!([["foo_bar_id", "=", 42]])),
    )
    .await;
    assert_sql(
        &sql,
        &format!(
            "SELECT \"test_access_related\".\"id\" FROM \"test_access_related\" WHERE \
             \"test_access_related\".\"foo_id\" IN (SELECT \"test_access_related_foo\".\"id\" FROM \
             \"test_access_related_foo\" WHERE (\"test_access_related_foo\".\"bar_id\" = \
             ANY($1::int8[]) AND \"test_access_related_foo\".\"id\" < $2::int8)) AND \
             \"test_access_related\".\"id\" < $3::int8 {order}"
        ),
    );
    let sql = sql_of(
        &mut tx,
        &related,
        &json(serde_json::json!([["foo_bar_sudo_id", "=", 42]])),
    )
    .await;
    assert_sql(
        &sql,
        &format!(
            "{sudo_join} \"test_access_related__foo_id\".\"bar_id\" = ANY($1::int8[])) AND \
             \"test_access_related\".\"id\" < $2::int8 {order}"
        ),
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

// ----- SM-2 of the merge review: predicates are compiled under a fixed context ---------

const SHELF: &str = "test_access.shelf";
const BOOK: &str = "test_access.book";

/// Whether `records` may be read by the transaction's user (an `Access` refusal is `false`,
/// anything else is a failure).
async fn may_read(tx: &mut Transaction, records: &Records) -> bool {
    match records.fetch(tx, None).await {
        Ok(()) => true,
        Err(error) if error.kind() == ErrorKind::Access => false,
        Err(error) => panic!("unexpected error: {error}"),
    }
}

/// A rule over an x2many traversal sees archived targets whatever the reader's `active_test`
/// (the predicate is compiled with `active_test` off, design note §3.3): the shelf whose only
/// matching book is archived is readable in every context, in either order, and what one context
/// remembers never changes the answer of another.
#[tokio::test]
async fn an_archived_x2many_target_in_a_rule_is_seen_whatever_the_readers_active_test() {
    let policy = DomainPolicy::builder(Otherwise::Allow)
        .rule(
            SHELF,
            Operation::Read,
            cond("book_ids", Operator::Any, cond("val", Operator::Eq, 1)),
        )
        .build()
        .unwrap();
    let h = harness(policy).await;
    let shelf = create(&h, SHELF, &[row(&[("name", Value::str("s"))])]).await[0];
    create(
        &h,
        BOOK,
        &[row(&[
            ("val", Value::Int(1)),
            ("active", Value::Bool(false)),
            ("shelf_id", Value::Int(shelf)),
        ])],
    )
    .await;
    for order in [[true, false], [false, true]] {
        let mut tx = begin_as(&h, DEMO).await;
        let shelves = tx.env().records(SHELF).unwrap().browse([shelf]).unwrap();
        for active_test in order {
            let records = shelves.with_context([("active_test", Value::Bool(active_test))]);
            assert!(
                may_read(&mut tx, &records).await,
                "fetch, active_test {active_test}, order {order:?}"
            );
            assert_eq!(
                search_ids(&mut tx, &records, &Domain::TRUE).await,
                [shelf],
                "search, active_test {active_test}, order {order:?}"
            );
        }
        drop(tx);
    }
    h.cleanup().await.unwrap();
}

/// A date literal compared with a datetime field in a rule means midnight UTC, whatever the
/// reader's `tz`: the same record gets the same decision under two time zones, in either order.
#[tokio::test]
async fn a_date_literal_in_a_rule_means_the_same_instant_in_every_time_zone() {
    let policy = DomainPolicy::builder(Otherwise::Allow)
        .rule(
            OBJ,
            Operation::Read,
            cond("moment", Operator::Lt, "2024-01-01"),
        )
        .build()
        .unwrap();
    let h = harness(policy).await;
    let obj = create(
        &h,
        OBJ,
        &[row(&[("moment", Value::str("2023-12-31 23:00:00"))])],
    )
    .await[0];
    for order in [["UTC", "Asia/Jakarta"], ["Asia/Jakarta", "UTC"]] {
        let mut tx = begin_as(&h, DEMO).await;
        let objs = tx.env().records(OBJ).unwrap().browse([obj]).unwrap();
        for tz in order {
            let records = objs.with_context([("tz", Value::str(tz))]);
            assert!(
                may_read(&mut tx, &records).await,
                "fetch, tz {tz}, order {order:?}"
            );
        }
        drop(tx);
    }
    h.cleanup().await.unwrap();
}

// ----- SM-9 of the merge review: inlined comodel rules and `has_access` agree -----------

/// The RPC `has_access(operation)` of `records`.
async fn rpc_has_access(h: &Harness, tx: &mut Transaction, records: &Records, op: &str) -> bool {
    let Some(erp_orm::rpc::Exposure::Exposed(exposed)) =
        h.orm().exposure().get(records.model_id(), "has_access")
    else {
        panic!("has_access is exposed on every model");
    };
    let args = erp_orm::rpc::RpcArgs::new(
        [("operation".to_owned(), Value::str(op))]
            .into_iter()
            .collect(),
    );
    match exposed.call(tx, records.clone(), args).await.unwrap() {
        Value::Bool(answer) => answer,
        other => panic!("has_access answered {other:?}"),
    }
}

/// The RPC `has_access` and the enforced decision agree under the default context and a non-UTC
/// `tz`, for a rule with an x2many traversal over an archived target and for one with a date
/// literal on a datetime field (both are compiled under the fixed predicate context).
#[tokio::test]
async fn rpc_has_access_agrees_with_enforcement_whatever_the_context() {
    let policy = DomainPolicy::builder(Otherwise::Allow)
        .rule(
            SHELF,
            Operation::Read,
            cond("book_ids", Operator::Any, cond("val", Operator::Eq, 1)),
        )
        .rule(
            OBJ,
            Operation::Read,
            cond("moment", Operator::Lt, "2024-01-01"),
        )
        .build()
        .unwrap();
    let h = harness(policy).await;
    let shelf = create(&h, SHELF, &[row(&[("name", Value::str("s"))])]).await[0];
    create(
        &h,
        BOOK,
        &[row(&[
            ("val", Value::Int(1)),
            ("active", Value::Bool(false)),
            ("shelf_id", Value::Int(shelf)),
        ])],
    )
    .await;
    let obj = create(
        &h,
        OBJ,
        &[row(&[("moment", Value::str("2023-12-31 23:00:00"))])],
    )
    .await[0];
    let mut tx = begin_as(&h, DEMO).await;
    for (model, id) in [(SHELF, shelf), (OBJ, obj)] {
        let records = tx.env().records(model).unwrap().browse([id]).unwrap();
        for context in [
            vec![],
            vec![("tz", Value::str("Asia/Jakarta"))],
            vec![("active_test", Value::Bool(false))],
        ] {
            let records = records.with_context(context.clone());
            let enforced = records.has_access(&mut tx, Operation::Read).await.unwrap();
            let rpc = rpc_has_access(&h, &mut tx, &records, "read").await;
            assert!(enforced, "{model}, {context:?}: enforced");
            assert_eq!(rpc, enforced, "{model}, {context:?}");
        }
    }
    drop(tx);
    h.cleanup().await.unwrap();
}

/// A comodel's read rule inlined into the user's search (a many2one `any` that does not bypass
/// the rules) is compiled under the same fixed context: books of a shelf that is readable only
/// through an archived book are found under the default context and under a non-UTC `tz`.
#[tokio::test]
async fn an_inlined_comodel_rule_is_compiled_under_the_fixed_context() {
    let policy = DomainPolicy::builder(Otherwise::Allow)
        .rule(
            SHELF,
            Operation::Read,
            cond("book_ids", Operator::Any, cond("val", Operator::Eq, 1)),
        )
        .build()
        .unwrap();
    let h = harness(policy).await;
    let shelf = create(&h, SHELF, &[row(&[("name", Value::str("s"))])]).await[0];
    let books = create(
        &h,
        BOOK,
        &[row(&[
            ("val", Value::Int(1)),
            ("active", Value::Bool(false)),
            ("shelf_id", Value::Int(shelf)),
        ])],
    )
    .await;
    let mut tx = begin_as(&h, DEMO).await;
    let domain = cond("shelf_id", Operator::Any, Domain::TRUE);
    for context in [
        vec![],
        vec![("tz", Value::str("Asia/Jakarta"))],
        vec![("active_test", Value::Bool(true))],
    ] {
        let records = tx
            .env()
            .records(BOOK)
            .unwrap()
            .with_context(context.clone());
        let found = records
            .search_with(
                &mut tx,
                &domain,
                &erp_orm::SearchOptions {
                    active_test: false,
                    ..erp_orm::SearchOptions::default()
                },
            )
            .await
            .unwrap()
            .ids();
        assert_eq!(found, books, "{context:?}");
    }
    drop(tx);
    h.cleanup().await.unwrap();
}
