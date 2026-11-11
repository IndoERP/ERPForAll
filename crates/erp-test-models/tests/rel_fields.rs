//! Relational values (REL §4.4–4.11, ENV-034, RSET-005): many2one conversion, record and read
//! formats, reading x2many values, relational prefetch, references.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

mod support;

use erp_registry::{FieldDecl, ModelDecl};
use erp_sdk::{Cell, ErrorKind, Load, OnDelete, Records, Transaction, Value};
use erp_test_models::base_users;
use erp_testkit::{Harness, assert_query_count};
use support::{map, one, rows};

async fn create(tx: &mut Transaction, model: &str, rows: &[&[(&str, Value)]]) -> Records {
    let records = tx.env().records(model).unwrap();
    let vals: Vec<_> = rows.iter().map(|r| map(r)).collect();
    records.create_values(tx, &vals).await.unwrap()
}

#[tokio::test]
async fn spec_rel_001_many2one_value_conversion() {
    let h = support::rel_orm().await;
    let mut tx = h.begin().await.unwrap();
    let r = create(&mut tx, "test_rel.mixed", &[&[("name", Value::str("r"))]]).await;
    let t = create(
        &mut tx,
        "test_rel.mixed_rel",
        &[&[("name", Value::str("t"))]],
    )
    .await;
    let tid = t.ids()[0];
    let column = async |tx: &mut Transaction| {
        tx.flush().await.unwrap();
        one(
            tx,
            &format!(
                "SELECT many2one_id::text FROM test_rel_mixed WHERE id = {}",
                r.ids()[0]
            ),
        )
        .await
    };
    let cases: Vec<(Value, Option<i64>)> = vec![
        (Value::Int(tid), Some(tid)),
        (Value::list([Value::Int(tid), Value::str("x")]), Some(tid)),
        (Value::False, None),
    ];
    for (value, expected) in cases {
        r.set_value(&mut tx, "many2one_id", value.clone())
            .await
            .unwrap();
        let rel = r.related(&mut tx, "many2one_id").await.unwrap();
        assert_eq!(
            rel.ids(),
            expected.into_iter().collect::<Vec<_>>(),
            "{value}"
        );
        assert_eq!(rel.model_name(), "test_rel.mixed_rel");
        assert_eq!(
            column(&mut tx).await,
            expected.map(|i| i.to_string()),
            "{value}"
        );
    }
    // a recordset, then an empty recordset
    r.assign(&mut tx, "many2one_id", &t).await.unwrap();
    assert_eq!(r.related(&mut tx, "many2one_id").await.unwrap(), t);
    assert_eq!(column(&mut tx).await, Some(tid.to_string()));
    r.assign(&mut tx, "many2one_id", &t.browse_ids([]))
        .await
        .unwrap();
    assert!(r.related(&mut tx, "many2one_id").await.unwrap().is_empty());
    assert_eq!(column(&mut tx).await, None);
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_rel_002_many2one_rejects_wrong_recordsets() {
    let h = support::rel_orm().await;
    let mut tx = h.begin().await.unwrap();
    let r = create(&mut tx, "test_rel.mixed", &[&[]]).await;
    let partner = create(&mut tx, "test_rel.partner", &[&[("name", Value::str("p"))]]).await;
    let two = create(&mut tx, "test_rel.mixed_rel", &[&[], &[]]).await;
    for value in [&partner, &two] {
        let err = r.assign(&mut tx, "many2one_id", value).await.unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Programming);
        assert!(
            err.message()
                .starts_with("Wrong value for test_rel.mixed.many2one_id:"),
            "{err}"
        );
    }
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_rel_003_many2one_lossy_scalars_are_refused() {
    let h = support::rel_orm().await;
    let mut tx = h.begin().await.unwrap();
    let r = create(&mut tx, "test_rel.mixed", &[&[]]).await;
    for value in [Value::str("5"), Value::Bool(true), Value::Float(5.0)] {
        let err = r
            .set_value(&mut tx, "many2one_id", value.clone())
            .await
            .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Validation, "{value}");
        assert!(
            err.message().contains("test_rel.mixed.many2one_id"),
            "{err}"
        );
    }
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_rel_004_many2one_read_format() {
    let h = support::rel_orm().await;
    let mut tx = h.begin().await.unwrap();
    let users = tx.env().records("res.users").unwrap();
    let demo = users
        .create_values(
            &mut tx,
            &[map(&[
                ("name", Value::str("Demo")),
                ("login", Value::str("demo")),
            ])],
        )
        .await
        .unwrap();
    let uid = demo.ids()[0];
    let messages = create(
        &mut tx,
        "test_rel.message",
        &[&[("author", Value::Int(uid))], &[]],
    )
    .await;
    tx.invalidate_all().await.unwrap();
    let read = messages.read(&mut tx, &["author"]).await.unwrap();
    assert_eq!(
        read[0]["author"],
        Value::list([Value::Int(uid), Value::str("Demo")])
    );
    assert_eq!(read[1]["author"], Value::False);
    let raw = messages
        .read_with(&mut tx, &["author"], Load::Raw)
        .await
        .unwrap();
    assert_eq!(raw[0]["author"], Value::Int(uid));
    assert_eq!(raw[1]["author"], Value::False);
    // as a user who may only read messages, the name is still computed as superuser
    let read = messages
        .with_user(uid)
        .read(&mut tx, &["author"])
        .await
        .unwrap();
    assert_eq!(
        read[0]["author"],
        Value::list([Value::Int(uid), Value::str("Demo")])
    );
    // a dangling id (target deleted behind the cache's back) reads False
    let msg = messages.at(0).unwrap();
    msg.value(&mut tx, "author").await.unwrap();
    tx.execute("DELETE FROM res_users WHERE id = $1", &[&uid])
        .await
        .unwrap();
    // the message still caches the old id; the user's own values are gone from the cache
    tx.invalidate_model("res.users", None).await.unwrap();
    let read = msg.read(&mut tx, &["author"]).await.unwrap();
    assert_eq!(read[0]["author"], Value::False);
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_rel_005_007_rset_005_many2one_on_several_records() {
    let h = support::rel_orm().await;
    let mut tx = h.begin().await.unwrap();
    let p = create(
        &mut tx,
        "test_rel.partner",
        &[&[("name", Value::str("p1"))], &[("name", Value::str("p2"))]],
    )
    .await;
    let (p1, p2) = (p.ids()[0], p.ids()[1]);
    let rs = create(
        &mut tx,
        "test_rel.base",
        &[
            &[("partner_id", Value::Int(p1))],
            &[("partner_id", Value::Int(p1))],
            &[("partner_id", Value::Int(p2))],
            &[],
        ],
    )
    .await;
    let partners = rs.related(&mut tx, "partner_id").await.unwrap();
    assert_eq!(
        partners.ids(),
        [p1, p2],
        "union without duplicates, first-seen order"
    );
    assert_eq!(partners.env(), rs.env());
    // derived records keep the environment, also through relations
    let demo = rs.with_user(7);
    let rel = demo
        .at(0)
        .unwrap()
        .related(&mut tx, "partner_id")
        .await
        .unwrap();
    assert_eq!(rel.env(), &rs.env().with_user(7));
    assert_eq!(
        rs.at(0)
            .unwrap()
            .related(&mut tx, "partner_id")
            .await
            .unwrap()
            .env(),
        rs.env()
    );
    // assigning a record obtained in another environment: the value lives in the record's env
    let other = p.with_user(7).at(1).unwrap();
    let first = rs.at(0).unwrap();
    first.assign(&mut tx, "partner_id", &other).await.unwrap();
    let rel = first.related(&mut tx, "partner_id").await.unwrap();
    assert_eq!(rel.ids(), [p2]);
    assert_eq!(rel.env(), first.env());
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_rel_006_empty_values_chain() {
    let h = support::rel_orm().await;
    let mut tx = h.begin().await.unwrap();
    let partner = create(
        &mut tx,
        "test_rel.partner",
        &[&[("name", Value::str("lonely"))]],
    )
    .await;
    let parent = partner.related(&mut tx, "parent_id").await.unwrap();
    assert!(parent.is_empty());
    assert_eq!(parent.model_name(), "test_rel.partner");
    assert_query_count!(tx, <= 0, {
        assert_eq!(parent.id().unwrap(), None);
        let country = parent.related(&mut tx, "country_id").await.unwrap();
        assert!(country.is_empty() && country.model_name() == "test_rel.country");
        assert_eq!(country.value(&mut tx, "name").await.unwrap(), Value::False);
        let categories = parent.related(&mut tx, "category_id").await.unwrap();
        assert!(categories.is_empty() && categories.model_name() == "test_rel.category");
    });
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_rel_021_many2one_assignment_keeps_one2many_caches() {
    let h = support::rel_orm().await;
    let mut tx = h.begin().await.unwrap();
    let ds = create(
        &mut tx,
        "test_rel.discussion",
        &[&[("name", Value::str("d1"))], &[("name", Value::str("d2"))]],
    )
    .await;
    let (d1, d2) = (ds.at(0).unwrap(), ds.at(1).unwrap());
    let msgs = create(
        &mut tx,
        "test_rel.message",
        &[
            &[
                ("discussion", Value::Int(d1.ids()[0])),
                ("important", Value::Bool(true)),
            ],
            &[
                ("discussion", Value::Int(d1.ids()[0])),
                ("important", Value::Bool(false)),
            ],
        ],
    )
    .await;
    let (m, plain) = (msgs.at(0).unwrap(), msgs.at(1).unwrap());
    tx.invalidate_all().await.unwrap();
    // load the one2many values (and the messages' fields)
    for d in [&d1, &d2] {
        d.cells(&mut tx, "messages").await.unwrap();
        d.cells(&mut tx, "important_messages").await.unwrap();
    }
    msgs.cells(&mut tx, "important").await.unwrap();
    assert_query_count!(tx, <= 0, {
        m.set_value(&mut tx, "discussion", Value::Int(d2.ids()[0]))
            .await
            .unwrap();
        let in_d1 = d1.related(&mut tx, "messages").await.unwrap();
        let in_d2 = d2.related(&mut tx, "messages").await.unwrap();
        let important_d2 = d2.related(&mut tx, "important_messages").await.unwrap();
        assert!(!in_d1.contains(&m).unwrap());
        assert!(in_d2.contains(&m).unwrap());
        assert!(important_d2.contains(&m).unwrap());
        plain
            .set_value(&mut tx, "discussion", Value::Int(d2.ids()[0]))
            .await
            .unwrap();
        let in_d2 = d2.related(&mut tx, "messages").await.unwrap();
        let important_d2 = d2.related(&mut tx, "important_messages").await.unwrap();
        assert!(in_d2.contains(&plain).unwrap());
        assert!(!important_d2.contains(&plain).unwrap(), "the domain is followed");
    });
    // a one2many not in cache is not created by an assignment
    let d3 = create(&mut tx, "test_rel.discussion", &[&[]]).await;
    let field_cached = tx.is_cached(&d3, "messages").unwrap();
    assert!(field_cached, "a new record's one2many is known to be empty");
    tx.invalidate_all().await.unwrap();
    m.set_value(&mut tx, "discussion", Value::Int(d3.ids()[0]))
        .await
        .unwrap();
    assert!(!tx.is_cached(&d3, "messages").unwrap());
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_rel_024_one2many_read_is_one_query() {
    let h = support::rel_orm().await;
    let mut tx = h.begin().await.unwrap();
    let orders = create(&mut tx, "test_rel.base", &[&[], &[], &[], &[], &[]]).await;
    let lines: Vec<Vec<(&str, Value)>> = orders
        .ids()
        .iter()
        .map(|o| vec![("base_id", Value::Int(*o)), ("value", Value::Int(1))])
        .collect();
    let refs: Vec<&[(&str, Value)]> = lines.iter().map(Vec::as_slice).collect();
    create(&mut tx, "test_rel.line", &refs).await;
    for env_orders in [orders.clone(), orders.with_user(7)] {
        tx.invalidate_all().await.unwrap();
        let values = assert_query_count!(tx, >= 1, <= 1, {
            env_orders.cells(&mut tx, "line_ids").await.unwrap()
        });
        assert!(
            values
                .iter()
                .all(|c| matches!(c, Cell::Ids(ids) if ids.len() == 1))
        );
        // grouped per order: each line belongs to its order
        for (order, cell) in env_orders.iter().zip(&values) {
            let Cell::Ids(ids) = cell else { unreachable!() };
            let line = tx
                .env()
                .records("test_rel.line")
                .unwrap()
                .browse_ids(ids.iter().copied());
            assert_eq!(
                line.value(&mut tx, "base_id").await.unwrap(),
                Value::Int(order.ids()[0])
            );
        }
    }
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_rel_025_archived_lines_and_field_context() {
    let h = support::rel_orm().await;
    let mut tx = h.begin().await.unwrap();
    let parent = create(&mut tx, "test_rel.active", &[&[("name", Value::str("P"))]]).await;
    let pid = parent.ids()[0];
    let children = create(
        &mut tx,
        "test_rel.active",
        &[
            &[("name", Value::str("c1")), ("parent_id", Value::Int(pid))],
            &[
                ("name", Value::str("c2")),
                ("parent_id", Value::Int(pid)),
                ("active", Value::Bool(false)),
            ],
        ],
    )
    .await;
    let (c1, c2) = (children.ids()[0], children.ids()[1]);
    let ctx = |active: Option<bool>| match active {
        Some(a) => parent.with_context([("active_test", Value::Bool(a))]),
        None => parent.clone(),
    };
    for invalidate in [false, true] {
        for active in [None, Some(true), Some(false)] {
            if invalidate {
                tx.invalidate_all().await.unwrap();
            }
            let p = ctx(active);
            let children = p.related(&mut tx, "children_ids").await.unwrap().ids();
            let all = p.related(&mut tx, "all_children_ids").await.unwrap().ids();
            let act = p
                .related(&mut tx, "active_children_ids")
                .await
                .unwrap()
                .ids();
            if active == Some(false) {
                assert_eq!(children, [c1, c2]);
            } else {
                assert_eq!(children, [c1]);
            }
            assert_eq!(all, [c1, c2], "field context wins");
            assert_eq!(act, [c1], "field context wins");
        }
    }
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_rel_037_many2many_read_query() {
    let h = support::rel_orm().await;
    let mut tx = h.begin().await.unwrap();
    let tags = create(
        &mut tx,
        "test_rel.tag",
        &[&[("name", Value::str("b"))], &[("name", Value::str("a"))]],
    )
    .await;
    let set = Value::list(tags.ids().into_iter().map(Value::Int));
    let recs = create(
        &mut tx,
        "test_rel.base",
        &[&[("tag_ids", set.clone())], &[("tag_ids", set)], &[]],
    )
    .await;
    tx.invalidate_all().await.unwrap();
    tx.capture_statements(true);
    let values = assert_query_count!(tx, >= 1, <= 1, {
        recs.cells(&mut tx, "tag_ids").await.unwrap()
    });
    let sql = tx.captured()[0].clone();
    // the id list behind a sub-select: every plan is the generic one (5)
    support::assert_sql(
        &sql,
        "SELECT \"test_rel_base_test_rel_tag_rel\".\"test_rel_base_id\", \
         \"test_rel_base_test_rel_tag_rel\".\"test_rel_tag_id\" FROM \"test_rel_tag\" JOIN \
         \"test_rel_base_test_rel_tag_rel\" ON (\"test_rel_base_test_rel_tag_rel\".\"test_rel_tag_id\" \
         = \"test_rel_tag\".\"id\") WHERE \"test_rel_base_test_rel_tag_rel\".\"test_rel_base_id\" = \
         ANY((SELECT $1::int8[])::int8[]) ORDER BY \"test_rel_tag\".\"id\"",
    );
    assert_eq!(values[0], Cell::Ids(tags.rec_ids().into()));
    assert_eq!(values[2], Cell::Ids([].into()));
    drop(tx);
    h.cleanup().await.unwrap();
}

/// the loop again costs nothing, and `value_pc` came with the first query.
#[tokio::test]
async fn spec_env_034_relational_prefetch() {
    let h = support::rel_orm().await;
    let mut tx = h.begin().await.unwrap();
    let partners = create(
        &mut tx,
        "test_rel.partner",
        &[&[("name", Value::str("a"))], &[("name", Value::str("b"))]],
    )
    .await;
    let (a, b) = (partners.ids()[0], partners.ids()[1]);
    let records = create(
        &mut tx,
        "test_rel.base",
        &[
            &[("partner_id", Value::Int(a))],
            &[("partner_id", Value::Int(a))],
            &[("partner_id", Value::Int(b))],
            &[("partner_id", Value::Int(b))],
            &[("partner_id", Value::Int(a))],
        ],
    )
    .await;
    tx.invalidate_all().await.unwrap();
    let names = assert_query_count!(tx, >= 2, <= 2, {
        let mut names = Vec::new();
        for r in records.iter() {
            let partner = r.related(&mut tx, "partner_id").await.unwrap();
            let country = partner.related(&mut tx, "country_id").await.unwrap();
            names.push(country.value(&mut tx, "name").await.unwrap());
        }
        names
    });
    assert!(names.iter().all(|n| *n == Value::False));
    assert_query_count!(tx, <= 0, {
        for r in records.iter() {
            let partner = r.related(&mut tx, "partner_id").await.unwrap();
            let country = partner.related(&mut tx, "country_id").await.unwrap();
            country.value(&mut tx, "name").await.unwrap();
            r.value(&mut tx, "value_pc").await.unwrap();
        }
    });
    // with countries, one more query for the third hop, still for the whole set
    let countries = create(
        &mut tx,
        "test_rel.country",
        &[&[("name", Value::str("BE"))], &[("name", Value::str("US"))]],
    )
    .await;
    partners
        .at(0)
        .unwrap()
        .set_value(&mut tx, "country_id", Value::Int(countries.ids()[0]))
        .await
        .unwrap();
    partners
        .at(1)
        .unwrap()
        .set_value(&mut tx, "country_id", Value::Int(countries.ids()[1]))
        .await
        .unwrap();
    tx.invalidate_all().await.unwrap();
    let names = assert_query_count!(tx, >= 3, <= 3, {
        let mut names = Vec::new();
        for r in records.iter() {
            let partner = r.related(&mut tx, "partner_id").await.unwrap();
            let country = partner.related(&mut tx, "country_id").await.unwrap();
            assert_eq!(country.prefetch_ids().len(), 2, "every partner's country");
            names.push(country.value(&mut tx, "name").await.unwrap());
        }
        names
    });
    assert_eq!(
        names,
        [
            Value::str("BE"),
            Value::str("BE"),
            Value::str("US"),
            Value::str("US"),
            Value::str("BE")
        ]
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// `Missing` error instead of becoming empty)
#[tokio::test]
async fn spec_rel_065_066_reference_values_and_formats() {
    let h = support::rel_orm().await;
    let mut tx = h.begin().await.unwrap();
    let r = create(&mut tx, "test_rel.mixed", &[&[]]).await;
    let p = create(
        &mut tx,
        "test_rel.partner",
        &[&[("name", Value::str("Agrolait"))]],
    )
    .await;
    let pid = p.ids()[0];
    assert!(r.reference(&mut tx, "reference").await.unwrap().is_none());
    assert_eq!(
        r.read(&mut tx, &["reference"]).await.unwrap()[0]["reference"],
        Value::False
    );
    // the superuser, then a partner
    r.set_value(&mut tx, "reference", Value::str("res.users,1"))
        .await
        .unwrap();
    let user = r.reference(&mut tx, "reference").await.unwrap().unwrap();
    assert_eq!((user.model_name(), user.ids()), ("res.users", vec![1]));
    let text = format!("test_rel.partner,{pid}");
    r.set_value(&mut tx, "reference", Value::str(&text))
        .await
        .unwrap();
    let partner = r.reference(&mut tx, "reference").await.unwrap().unwrap();
    assert_eq!(partner, p);
    assert_eq!(
        r.read(&mut tx, &["reference"]).await.unwrap()[0]["reference"],
        Value::str(&text)
    );
    tx.flush().await.unwrap();
    assert_eq!(
        one(
            &mut tx,
            &format!(
                "SELECT reference FROM test_rel_mixed WHERE id = {}",
                r.ids()[0]
            )
        )
        .await,
        Some(text.clone())
    );
    // a model outside the selection is refused
    let err = r
        .set_value(&mut tx, "reference", Value::str("test_rel.tag,1"))
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert!(
        err.message()
            .starts_with("Wrong value for test_rel.mixed.reference:"),
        "{err}"
    );
    // a record that does not exist is refused (F4)
    let err = tx
        .savepoint(async |tx| {
            r.set_value(tx, "reference", Value::str("test_rel.partner,999999"))
                .await
        })
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Missing, "{err}");
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_rel_067_many2one_reference() {
    let h = support::rel_orm().await;
    let mut tx = h.begin().await.unwrap();
    let col = rows(
        &mut tx,
        "SELECT data_type::text FROM information_schema.columns \
         WHERE table_name = 'test_rel_mixed' AND column_name = 'res_id'",
    )
    .await;
    assert_eq!(col, vec![vec![Some("bigint".into())]]);
    let p = create(&mut tx, "test_rel.partner", &[&[("name", Value::str("p"))]]).await;
    let recs = create(
        &mut tx,
        "test_rel.mixed",
        &[
            &[],
            &[
                ("res_model", Value::str("test_rel.partner")),
                ("res_id", Value::Int(p.ids()[0])),
            ],
        ],
    )
    .await;
    tx.invalidate_all().await.unwrap();
    let values = recs.values(&mut tx, "res_id").await.unwrap();
    assert_eq!(values, [Value::Int(0), Value::Int(p.ids()[0])]);
    // no foreign key: deleting the target leaves a dangling value, without error
    p.unlink(&mut tx).await.unwrap();
    assert_eq!(
        recs.at(1).unwrap().value(&mut tx, "res_id").await.unwrap(),
        Value::Int(p.ids()[0])
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_rset_051_display_name_through_delegation() {
    let h = support::rel_orm().await;
    let mut tx = h.begin().await.unwrap();
    let boxes = create(&mut tx, "test_rel.box", &[&[("name", Value::str("U1"))]]).await;
    tx.invalidate_all().await.unwrap();
    assert_eq!(
        boxes.value(&mut tx, "display_name").await.unwrap(),
        Value::str("U1")
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_rel_016_no_existence_check_on_many2one_assignment() {
    let h = support::rel_orm().await;
    let mut tx = h.begin().await.unwrap();
    let r = create(&mut tx, "test_rel.mixed", &[&[("name", Value::str("r"))]]).await;
    // assigning a non-existent id succeeds in cache without existence check
    r.set_value(&mut tx, "many2one_id", Value::Int(999_999))
        .await
        .unwrap();
    let rel = r.related(&mut tx, "many2one_id").await.unwrap();
    assert_eq!(rel.ids(), [999_999]);
    // flushing fails with a foreign key violation (SQLSTATE 23503)
    let err = tx.flush().await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(err.sqlstate(), Some("23503"));
    // Error message format from CRUD schema (/ crud-schema.md:318-320)
    assert!(
        err.message()
            .contains("The troublemaker is: 'Rel Mixed' (test_rel.mixed)"),
        "{err}"
    );
    assert!(err.message().contains("many2one_id"), "{err}");
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_rel_028_one2many_on_many2one_reference_inverse() {
    let custom_owner = ModelDecl::new("test_rel", "test_rel.custom_owner")
        .table("test_rel_custom_owner")
        .fields(
            const {
                &[
                    FieldDecl::char("name"),
                    FieldDecl::one2many("filtered_attachment_ids", "test_rel.attachment", "res_id")
                        .domain(r#"[["name", "=", "mine"]]"#),
                ]
            },
        );
    let h = Harness::builder([&base_users::ADDON, &erp_test_models::test_rel::ADDON])
        .with_decls([custom_owner])
        .build()
        .await
        .expect("harness");

    let mut tx = h.begin().await.unwrap();
    let owners = create(
        &mut tx,
        "test_rel.custom_owner",
        &[&[("name", Value::str("o1"))]],
    )
    .await;
    let o1 = owners.ids()[0];
    let atts = create(
        &mut tx,
        "test_rel.attachment",
        &[
            &[
                ("name", Value::str("mine")),
                ("res_model", Value::str("test_rel.custom_owner")),
                ("res_id", Value::Int(o1)),
            ],
            &[
                ("name", Value::str("other")),
                ("res_model", Value::str("test_rel.custom_owner")),
                ("res_id", Value::Int(o1)),
            ],
            &[
                ("name", Value::str("mine")),
                ("res_model", Value::str("test_rel.partner")),
                ("res_id", Value::Int(o1)),
            ],
        ],
    )
    .await;
    tx.flush().await.unwrap();
    tx.invalidate_all().await.unwrap();

    let related = owners
        .related(&mut tx, "filtered_attachment_ids")
        .await
        .unwrap();
    assert_eq!(related.ids(), [atts.ids()[0]]);
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_rel_073_relational_attributes_in_fields_get() {
    let h = support::rel_orm().await;
    let mut tx = h.begin().await.unwrap();
    let mixed = tx.env().records("test_rel.mixed").unwrap();
    let desc = mixed
        .fields_get(
            &mut tx,
            Some(&[
                "many2one_id",
                "one2many_ids",
                "many2many_ids",
                "reference",
                "res_id",
            ]),
            None,
        )
        .await
        .unwrap();

    assert_eq!(
        desc["many2one_id"]["relation"],
        Value::str("test_rel.mixed_rel")
    );
    assert_eq!(
        desc["one2many_ids"]["relation"],
        Value::str("test_rel.mixed_rel")
    );
    assert_eq!(
        desc["one2many_ids"]["relation_field"],
        Value::str("mixed_id")
    );
    assert_eq!(
        desc["many2many_ids"]["relation"],
        Value::str("test_rel.mixed_rel")
    );
    assert_eq!(desc["res_id"]["model_field"], Value::str("res_model"));
    assert_eq!(
        desc["reference"]["selection"],
        Value::list([
            Value::list([Value::str("test_rel.partner"), Value::str("Partner")]),
            Value::list([Value::str("res.users"), Value::str("User")]),
        ])
    );

    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_rel_015_company_dependent_many2one_unlink() {
    let comp_holder = ModelDecl::new("test_rel", "test_rel.comp_holder")
        .table("test_rel_comp_holder")
        .fields(
            const {
                &[
                    FieldDecl::char("name"),
                    FieldDecl::many2one("tag_id", "test_rel.tag")
                        .company_dependent(true)
                        .ondelete(OnDelete::Restrict),
                    FieldDecl::many2one("null_tag_id", "test_rel.tag").company_dependent(true),
                ]
            },
        );
    let h = Harness::builder([&base_users::ADDON, &erp_test_models::test_rel::ADDON])
        .with_decls([comp_holder])
        .build()
        .await
        .expect("harness");

    let mut tx = h.begin().await.unwrap();
    let tag_t = create(&mut tx, "test_rel.tag", &[&[("name", Value::str("t"))]]).await;
    let tag_s = create(&mut tx, "test_rel.tag", &[&[("name", Value::str("s"))]]).await;
    let t_id = tag_t.ids()[0];
    let s_id = tag_s.ids()[0];

    let rec_a = create(
        &mut tx,
        "test_rel.comp_holder",
        &[&[("name", Value::str("a"))]],
    )
    .await;
    let a_id = rec_a.ids()[0];

    let company = 1_i64;
    let company_key = company.to_string();

    // Seed company-dependent jsonb in the database
    tx.execute(
        "UPDATE test_rel_comp_holder SET tag_id = jsonb_build_object($1::text, $2::bigint), null_tag_id = jsonb_build_object($1::text, $3::bigint) WHERE id = $4",
        &[&company_key, &t_id, &s_id, &a_id],
    )
    .await
    .unwrap();

    // 1. Deleting target t of ondelete=restrict raises UserError
    let err = tag_t.unlink(&mut tx).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::UserError);
    assert_eq!(
        err.message(),
        format!(
            "You cannot delete test_rel.tag,{t_id}, as it is used by test_rel.comp_holder,{a_id}"
        )
    );

    // After restrict refusal, target t still exists and tag_id remains intact in jsonb
    assert_eq!(tag_t.exists(&mut tx).await.unwrap().ids(), [t_id]);
    let rows_after_refusal = tx
        .query(
            "SELECT tag_id FROM test_rel_comp_holder WHERE id = $1",
            &[&a_id],
        )
        .await
        .unwrap();
    let tag_val: serde_json::Value = rows_after_refusal[0].try_get(0).unwrap();
    assert_eq!(tag_val, serde_json::json!({ company_key.as_str(): t_id }));

    // 2. Deleting target s of default set null sets jsonb entry to null
    tag_s.unlink(&mut tx).await.unwrap();
    let rows = tx
        .query(
            "SELECT null_tag_id FROM test_rel_comp_holder WHERE id = $1",
            &[&a_id],
        )
        .await
        .unwrap();
    let val: serde_json::Value = rows[0].try_get(0).unwrap();
    assert_eq!(val, serde_json::json!({ company_key.as_str(): null }));

    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_rel_076_export_and_display_formats() {
    let spec_decl = ModelDecl::new("test_rel", "test_rel.export_spec")
        .table("test_rel_export_spec")
        .fields(
            const {
                &[
                    FieldDecl::char("name"),
                    FieldDecl::many2one("p", "test_rel.partner"),
                    FieldDecl::many2many("tags", "test_rel.tag"),
                    FieldDecl::reference("ref_field", &[("test_rel.partner", "Partner")]),
                    FieldDecl::many2one("empty_p", "test_rel.partner"),
                ]
            },
        );
    let h = Harness::builder([
        &base_users::ADDON,
        &erp_test_models::test_rel::ADDON,
        &erp_data::model_data::ADDON,
    ])
    .with_decls([erp_data::ir_model_data_decl(), spec_decl])
    .build()
    .await
    .expect("harness");

    let mut tx = h.begin().await.unwrap();
    let partner = create(
        &mut tx,
        "test_rel.partner",
        &[&[("name", Value::str("Agrolait"))]],
    )
    .await;
    let tag_a = create(&mut tx, "test_rel.tag", &[&[("name", Value::str("a"))]]).await;
    let tag_b = create(&mut tx, "test_rel.tag", &[&[("name", Value::str("b"))]]).await;

    let p_id = partner.ids()[0];
    let tag_a_id = tag_a.ids()[0];
    let tag_b_id = tag_b.ids()[0];

    let r = create(
        &mut tx,
        "test_rel.export_spec",
        &[&[
            ("name", Value::str("rec1")),
            ("p", Value::Int(p_id)),
            (
                "tags",
                Value::list([Value::Int(tag_a_id), Value::Int(tag_b_id)]),
            ),
        ]],
    )
    .await;

    use erp_data::ImpexExt;
    let exported = r
        .with_context([("import_file", Value::Bool(true))])
        .export_data(
            &mut tx,
            &["p", "tags", "ref_field", "empty_p"],
            &erp_data::SuperuserOnly,
        )
        .await
        .unwrap();

    assert_eq!(exported.datas.len(), 1);
    assert_eq!(
        exported.datas[0],
        vec![
            Value::str("Agrolait"),
            Value::str("a,b"),
            Value::str(""),
            Value::str(""),
        ]
    );

    drop(tx);
    h.cleanup().await.unwrap();
}
