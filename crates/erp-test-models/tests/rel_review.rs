//! Regression tests of the S3 review round: dependents of deletions, poisoning after
//! partial writes, transitive cascades, one2many over many2one_reference, strict command and
//! reference inputs, ordered batch ids, hierarchy sub-queries and company checks on cache misses.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

mod support;

use erp_sdk::serde_json::json;
use erp_sdk::{Command, ErrorKind, RawVals, RecId, Records, Transaction, Value};
use erp_testkit::assert_query_count;
use support::{dom, map};

async fn create(tx: &mut Transaction, model: &str, rows: &[&[(&str, Value)]]) -> Records {
    let records = tx.env().records(model).unwrap();
    let vals: Vec<_> = rows.iter().map(|r| map(r)).collect();
    records.create_values(tx, &vals).await.unwrap()
}

fn rid(i: i64) -> RecId {
    RecId::from_real(i).unwrap()
}

fn name(n: &str) -> RawVals {
    let mut v = RawVals::new();
    v.insert("name".into(), erp_sdk::Cell::Str(n.into()));
    v
}

// ----- 1. unlink notifies dependents ---------------------------------------------

/// compute over a many2one set null by the deletion, cached and uncached, and a stored count over
/// a one2many losing a line)
#[tokio::test]
async fn spec_comp_023_unlink_recomputes_dependents() {
    let h = support::rel_orm().await;
    let mut tx = h.begin().await.unwrap();
    let countries = create(
        &mut tx,
        "test_rel.country",
        &[&[("name", Value::str("BE"))], &[("name", Value::str("FR"))]],
    )
    .await;
    let holders = create(
        &mut tx,
        "test_rel.holder",
        &[
            &[
                ("name", Value::str("cached")),
                ("country_id", Value::Int(countries.ids()[0])),
            ],
            &[
                ("name", Value::str("uncached")),
                ("country_id", Value::Int(countries.ids()[1])),
            ],
        ],
    )
    .await;
    assert_eq!(
        holders.values(&mut tx, "has_country").await.unwrap(),
        [Value::Bool(true), Value::Bool(true)]
    );
    tx.flush().await.unwrap();
    // the second holder is not in the cache when its country goes
    tx.invalidate_model("test_rel.holder", None).await.unwrap();
    holders
        .browse_ids([holders.rec_ids()[0]])
        .values(&mut tx, "country_id")
        .await
        .unwrap();
    countries.unlink(&mut tx).await.unwrap();
    assert_eq!(
        holders.values(&mut tx, "has_country").await.unwrap(),
        [Value::Bool(false), Value::Bool(false)]
    );
    tx.flush().await.unwrap();
    tx.invalidate_all().await.unwrap();
    assert_eq!(
        holders.values(&mut tx, "has_country").await.unwrap(),
        [Value::Bool(false), Value::Bool(false)],
        "stored in the database too"
    );
    // a stored count over a one2many losing a line
    let counter = create(&mut tx, "test_rel.counter", &[&[("name", Value::str("c"))]]).await;
    let items = create(
        &mut tx,
        "test_rel.counter_item",
        &[
            &[
                ("name", Value::str("i1")),
                ("counter_id", Value::Int(counter.ids()[0])),
            ],
            &[
                ("name", Value::str("i2")),
                ("counter_id", Value::Int(counter.ids()[0])),
            ],
        ],
    )
    .await;
    assert_eq!(
        counter.value(&mut tx, "item_count").await.unwrap(),
        Value::Int(2)
    );
    tx.flush().await.unwrap();
    tx.invalidate_all().await.unwrap();
    items
        .browse_ids([items.rec_ids()[0]])
        .unlink(&mut tx)
        .await
        .unwrap();
    assert_eq!(
        counter.value(&mut tx, "item_count").await.unwrap(),
        Value::Int(1)
    );
    tx.flush().await.unwrap();
    tx.invalidate_all().await.unwrap();
    assert_eq!(
        counter.value(&mut tx, "item_count").await.unwrap(),
        Value::Int(1)
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

// ----- 2. poisoning after a partial write (F1) ------------------------------------------

async fn company_fixture(tx: &mut Transaction) -> (i64, i64, i64) {
    let companies = create(
        tx,
        "res.company",
        &[&[("name", Value::str("A"))], &[("name", Value::str("B"))]],
    )
    .await;
    let pb = create(
        tx,
        "test_rel.model_parent",
        &[&[
            ("name", Value::str("PB")),
            ("company_id", Value::Int(companies.ids()[1])),
        ]],
    )
    .await;
    (companies.ids()[0], companies.ids()[1], pb.ids()[0])
}

#[tokio::test]
async fn f1_company_check_failure_refuses_commit() {
    let h = support::rel_orm().await;
    let mut tx = h.begin().await.unwrap();
    let (a, _, pb) = company_fixture(&mut tx).await;
    let children = tx.env().records("test_rel.model_child").unwrap();
    let err = children
        .create_values(
            &mut tx,
            &[map(&[
                ("company_id", Value::Int(a)),
                ("parent_id", Value::Int(pb)),
            ])],
        )
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::UserError);
    assert!(tx.is_poisoned());
    assert!(
        tx.commit().await.is_err(),
        "the inserted row must not be committed"
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn f1_inaccessible_link_refuses_commit() {
    let h = support::rel_orm().await;
    let mut tx = h.begin().await.unwrap();
    let tag = create(
        &mut tx,
        "test_rel.multi_tag",
        &[&[("name", Value::str("zzz"))]],
    )
    .await;
    let multis = tx.env().records("test_rel.multi").unwrap();
    let err = multis
        .create_values(
            &mut tx,
            &[map(&[
                ("name", Value::str("m")),
                ("tags", Value::list([Value::Int(tag.ids()[0])])),
            ])],
        )
        .await
        .unwrap_err();
    assert!(
        err.message().contains("Cannot link inaccessible records"),
        "{err}"
    );
    assert!(tx.is_poisoned());
    assert!(tx.commit().await.is_err());
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn f1_inherits_parent_written_then_child_fails_refuses_commit() {
    let h = support::rel_orm().await;
    let mut tx = h.begin().await.unwrap();
    let b = create(&mut tx, "test_rel.box", &[&[("name", Value::str("b"))]]).await;
    tx.flush().await.unwrap();
    let vals = name("renamed").set_commands("note_ids", [Command::link(rid(999_999_999))]);
    let err = b.write(&mut tx, vals).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Missing, "{err}");
    assert!(tx.is_poisoned());
    assert!(
        tx.commit().await.is_err(),
        "the unit's new name must not be committed"
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn f1_errors_before_any_change_do_not_poison() {
    let h = support::rel_orm().await;
    let mut tx = h.begin().await.unwrap();
    let partners = tx.env().records("test_rel.partner").unwrap();
    let err = partners
        .create_values(&mut tx, &[map(&[("nope", Value::Int(1))])])
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert!(!tx.is_poisoned());
    tx.commit().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

// ----- 3. inverse one2many cache after create ---------------------------------------------------

#[tokio::test]
async fn created_lines_join_the_cached_one2many_of_their_final_many2one() {
    let h = support::rel_orm().await;
    let mut tx = h.begin().await.unwrap();
    let p = create(&mut tx, "test_rel.partner", &[&[("name", Value::str("P"))]]).await;
    assert!(p.related(&mut tx, "child_ids").await.unwrap().is_empty());
    let partners = tx
        .env()
        .records("test_rel.partner")
        .unwrap()
        .with_context([("default_parent_id", Value::Int(p.ids()[0]))]);
    let child = partners
        .create_values(&mut tx, &[map(&[("name", Value::str("kid"))])])
        .await
        .unwrap();
    let children = assert_query_count!(tx, <= 0, {
        p.related(&mut tx, "child_ids").await.unwrap()
    });
    assert_eq!(children, child, "the defaulted many2one counts");
    drop(tx);
    h.cleanup().await.unwrap();
}

// ----- 4. transitive cascades over uncached models ----------------------------------------------

#[tokio::test]
async fn cascades_through_an_uncached_model_reach_cached_rows() {
    let h = support::rel_orm().await;
    let mut tx = h.begin().await.unwrap();
    let a = create(&mut tx, "test_rel.chain_a", &[&[("name", Value::str("a"))]]).await;
    let b = create(
        &mut tx,
        "test_rel.chain_b",
        &[&[("name", Value::str("b")), ("a_id", Value::Int(a.ids()[0]))]],
    )
    .await;
    let c = create(
        &mut tx,
        "test_rel.chain_c",
        &[&[("name", Value::str("c")), ("b_id", Value::Int(b.ids()[0]))]],
    )
    .await;
    create(
        &mut tx,
        "test_rel.chain_d",
        &[&[
            ("name", Value::str("d")),
            ("c_id", Value::Int(c.ids()[0])),
            ("b_id", Value::Int(b.ids()[0])),
        ]],
    )
    .await;
    tx.flush().await.unwrap();
    tx.invalidate_all().await.unwrap();
    // C and its lines cached, B and D not
    assert_eq!(
        c.value(&mut tx, "b_id").await.unwrap(),
        Value::Int(b.ids()[0])
    );
    assert_eq!(c.related(&mut tx, "d_ids").await.unwrap().len(), 1);
    tx.invalidate_model("test_rel.chain_b", None).await.unwrap();
    tx.invalidate_model("test_rel.chain_d", None).await.unwrap();
    a.unlink(&mut tx).await.unwrap();
    assert_eq!(
        c.value(&mut tx, "b_id").await.unwrap(),
        Value::False,
        "set null by the database"
    );
    assert!(
        c.related(&mut tx, "d_ids").await.unwrap().is_empty(),
        "deleted by the cascade"
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

// ----- 5. one2many over a many2one_reference, foreign lines, references (F2-F4) ------------------

#[tokio::test]
async fn f2_one2many_over_a_many2one_reference() {
    let h = support::rel_orm().await;
    let mut tx = h.begin().await.unwrap();
    let owners = create(
        &mut tx,
        "test_rel.attach_owner",
        &[&[("name", Value::str("o1"))], &[("name", Value::str("o2"))]],
    )
    .await;
    let o1 = owners.ids()[0];
    // a row of another model with the same id is not an attachment of o1
    let rows = create(
        &mut tx,
        "test_rel.attachment",
        &[
            &[
                ("name", Value::str("mine")),
                ("res_model", Value::str("test_rel.attach_owner")),
                ("res_id", Value::Int(o1)),
            ],
            &[
                ("name", Value::str("foreign")),
                ("res_model", Value::str("test_rel.partner")),
                ("res_id", Value::Int(o1)),
            ],
        ],
    )
    .await;
    tx.flush().await.unwrap();
    tx.invalidate_all().await.unwrap();
    let o1_set = owners.browse_ids([owners.rec_ids()[0]]);
    assert_eq!(
        o1_set
            .related(&mut tx, "attachment_ids")
            .await
            .unwrap()
            .ids(),
        [rows.ids()[0]]
    );
    let all = tx.env().records("test_rel.attach_owner").unwrap();
    for (json, expected) in [
        (
            json!([["attachment_ids.name", "=", "foreign"]]),
            Vec::<i64>::new(),
        ),
        (json!([["attachment_ids.name", "=", "mine"]]), vec![o1]),
        (json!([["attachment_ids", "!=", false]]), vec![o1]),
    ] {
        let found = all.search(&mut tx, &dom(json.clone())).await.unwrap();
        assert_eq!(found.ids(), expected, "search {json}");
        let filtered = owners
            .filtered_domain(&mut tx, &dom(json.clone()))
            .await
            .unwrap();
        assert_eq!(filtered.ids(), expected, "filtered_domain {json}");
    }
    // create through the one2many sets the model field
    let o3 = all
        .create(
            &mut tx,
            vec![name("o3").set_commands("attachment_ids", [Command::create(name("new"))])],
        )
        .await
        .unwrap();
    let new = o3.related(&mut tx, "attachment_ids").await.unwrap();
    assert_eq!(new.len(), 1);
    assert_eq!(
        new.value(&mut tx, "res_model").await.unwrap(),
        Value::str("test_rel.attach_owner")
    );
    assert_eq!(
        new.value(&mut tx, "res_id").await.unwrap(),
        Value::Int(o3.ids()[0])
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn f2_one2many_inverse_must_be_a_many2one() {
    use erp_registry::{AddonDescriptor, FieldDecl, ModelDecl, RegistryBuilder};
    static ADDON: AddonDescriptor = AddonDescriptor {
        name: "t",
        depends: &["base"],
        exact_only: false,
    };
    static OWNER: ModelDecl = ModelDecl::new("t", "t.owner")
        .fields(const { &[FieldDecl::one2many("line_ids", "t.line", "owner")] });
    static LINE: ModelDecl =
        ModelDecl::new("t", "t.line").fields(const { &[FieldDecl::integer("owner")] });
    let err = RegistryBuilder::new([&ADDON])
        .with_decls([&OWNER, &LINE])
        .build()
        .unwrap_err();
    assert!(err.to_string().contains("t.owner.line_ids"), "{err}");
    assert!(err.to_string().contains("many2one"), "{err}");
}

#[tokio::test]
async fn f3_one2many_commands_on_foreign_lines_are_refused() {
    let h = support::rel_orm().await;
    let mut tx = h.begin().await.unwrap();
    let bases = create(
        &mut tx,
        "test_rel.base",
        &[&[("name", Value::str("b1"))], &[("name", Value::str("b2"))]],
    )
    .await;
    let lines = create(
        &mut tx,
        "test_rel.line",
        &[
            &[
                ("base_id", Value::Int(bases.ids()[0])),
                ("value", Value::Int(1)),
            ],
            &[
                ("base_id", Value::Int(bases.ids()[1])),
                ("value", Value::Int(2)),
            ],
        ],
    )
    .await;
    let b1 = bases.browse_ids([bases.rec_ids()[0]]);
    let foreign = lines.rec_ids()[1];
    let mut value = RawVals::new();
    value.insert("value".into(), erp_sdk::Cell::Int(7));
    for command in [
        Command::update(foreign, value.clone()),
        Command::delete(foreign),
        Command::unlink(foreign),
    ] {
        let code = command.code();
        let err = tx
            .savepoint(async |tx| {
                b1.write(tx, RawVals::new().set_commands("line_ids", [command]))
                    .await
            })
            .await
            .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Validation, "command {code}: {err}");
    }
    // the line of b1 itself is fine
    b1.write(
        &mut tx,
        RawVals::new().set_commands("line_ids", [Command::update(lines.rec_ids()[0], value)]),
    )
    .await
    .unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn f4_reference_to_an_unknown_model_or_a_deleted_record() {
    let h = support::rel_orm().await;
    let mut tx = h.begin().await.unwrap();
    let r = create(&mut tx, "test_rel.mixed", &[&[]]).await;
    let err = tx
        .savepoint(async |tx| {
            r.set_value(tx, "reference", Value::str("test_rel.partner,999999999"))
                .await
        })
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Missing, "{err}");
    let err = tx
        .savepoint(async |tx| {
            r.set_value(tx, "reference", Value::str("no.such.model,1"))
                .await
        })
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation, "{err}");
    drop(tx);
    h.cleanup().await.unwrap();
}

// ----- 6. ids follow the input order (F6) -------------------------------------------------------

#[tokio::test]
async fn f6_mixed_column_sets_get_ascending_ids_in_input_order() {
    let h = support::rel_orm().await;
    let mut tx = h.begin().await.unwrap();
    let created = create(
        &mut tx,
        "test_rel.partner",
        &[
            &[("name", Value::str("a")), ("email", Value::str("a@x"))],
            &[("name", Value::str("b"))],
            &[("name", Value::str("c")), ("email", Value::str("c@x"))],
            &[("website", Value::str("d.example"))],
        ],
    )
    .await;
    let ids = created.ids();
    let mut sorted = ids.clone();
    sorted.sort_unstable();
    assert_eq!(ids, sorted, "ids ascend in input order");
    tx.invalidate_all().await.unwrap();
    assert_eq!(
        created.values(&mut tx, "name").await.unwrap(),
        [
            Value::str("a"),
            Value::str("b"),
            Value::str("c"),
            Value::False
        ]
    );
    let rows = tx
        .query(
            "SELECT identity_generation::text FROM information_schema.columns \
             WHERE table_name = 'test_rel_partner' AND column_name = 'id'",
            &[],
        )
        .await
        .unwrap();
    let generation: Option<String> = rows[0].get(0);
    assert_eq!(generation.as_deref(), Some("BY DEFAULT"));
    drop(tx);
    h.cleanup().await.unwrap();
}

// ----- 7. hierarchy conditions as sub-queries ---------------------------------------------------

#[tokio::test]
async fn child_of_is_one_statement_with_a_recursive_subquery() {
    let h = support::rel_orm().await;
    let mut tx = h.begin().await.unwrap();
    let root = create(
        &mut tx,
        "test_rel.category",
        &[&[("name", Value::str("root"))]],
    )
    .await;
    let kid = create(
        &mut tx,
        "test_rel.category",
        &[&[
            ("name", Value::str("kid")),
            ("parent_id", Value::Int(root.ids()[0])),
        ]],
    )
    .await;
    tx.flush().await.unwrap();
    let cats = tx.env().records("test_rel.category").unwrap();
    tx.capture_statements(true);
    let found = assert_query_count!(tx, >= 1, <= 1, {
        cats.search(&mut tx, &dom(json!([["id", "child_of", [root.ids()[0]]]])))
            .await
            .unwrap()
    });
    let sql = tx.captured().last().cloned().unwrap();
    tx.capture_statements(false);
    assert!(sql.contains("WITH RECURSIVE"), "{sql}");
    assert_eq!(found.ids(), [root.ids()[0], kid.ids()[0]]);
    let found = cats
        .search(&mut tx, &dom(json!([["id", "parent_of", [kid.ids()[0]]]])))
        .await
        .unwrap();
    assert_eq!(found.ids(), [root.ids()[0], kid.ids()[0]]);
    drop(tx);
    h.cleanup().await.unwrap();
}

// ----- 8. strict inputs, company check on cache misses (F5) --------------------------------------

#[tokio::test]
async fn f5_many2one_lists_and_command_triples_are_strict() {
    let h = support::rel_orm().await;
    let mut tx = h.begin().await.unwrap();
    let p = create(&mut tx, "test_rel.partner", &[&[("name", Value::str("p"))]]).await;
    let pid = p.ids()[0];
    let tag = create(&mut tx, "test_rel.tag", &[&[("name", Value::str("t"))]]).await;
    let tid = tag.ids()[0];
    let bases = tx.env().records("test_rel.base").unwrap();
    let bad: Vec<(&str, Value)> = vec![
        ("partner_id", Value::list([Value::Int(pid)])),
        (
            "partner_id",
            Value::list([Value::Int(pid), Value::str("p"), Value::Int(3)]),
        ),
        ("partner_id", Value::list([Value::Int(pid), Value::Int(3)])),
        (
            "tag_ids",
            Value::list([Value::list([Value::Int(4), Value::Int(tid)])]),
        ),
        (
            "tag_ids",
            Value::list([Value::list([
                Value::Int(4),
                Value::Int(tid),
                Value::Int(0),
                Value::Int(0),
            ])]),
        ),
        (
            "tag_ids",
            Value::list([Value::list([Value::Int(6), Value::Int(0), Value::Int(tid)])]),
        ),
    ];
    for (field, value) in bad {
        let err = bases
            .create_values(&mut tx, &[map(&[(field, value.clone())])])
            .await
            .unwrap_err();
        assert_eq!(
            err.kind(),
            ErrorKind::Validation,
            "{field} = {value}: {err}"
        );
    }
    // the accepted forms
    bases
        .create_values(
            &mut tx,
            &[map(&[
                (
                    "partner_id",
                    Value::list([Value::Int(pid), Value::str("p")]),
                ),
                (
                    "tag_ids",
                    Value::list([Value::list([
                        Value::Int(6),
                        Value::Int(0),
                        Value::list([Value::Int(tid)]),
                    ])]),
                ),
            ])],
        )
        .await
        .unwrap();
    bases
        .create_values(
            &mut tx,
            &[map(&[(
                "partner_id",
                Value::list([Value::Int(pid), Value::False]),
            )])],
        )
        .await
        .unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn company_check_reads_uncached_and_archived_targets() {
    let h = support::rel_orm().await;
    let mut tx = h.begin().await.unwrap();
    let (a, _, pb) = company_fixture(&mut tx).await;
    let child = create(
        &mut tx,
        "test_rel.model_child",
        &[&[("company_id", Value::Int(a))]],
    )
    .await;
    tx.flush().await.unwrap();
    // nothing cached: the target's company is read, not assumed empty
    tx.invalidate_all().await.unwrap();
    let err = tx
        .savepoint(async |tx| child.set_value(tx, "parent_id", Value::Int(pb)).await)
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::UserError, "{err}");
    // an archived target is checked too
    let parents = tx.env().records("test_rel.model_parent").unwrap();
    parents
        .browse([pb])
        .unwrap()
        .set_value(&mut tx, "active", Value::Bool(false))
        .await
        .unwrap();
    for field in ["parent_ids", "active_parent_ids"] {
        let err = tx
            .savepoint(async |tx| {
                child
                    .set_value(tx, field, Value::list([Value::Int(pb)]))
                    .await
            })
            .await
            .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::UserError, "{field}: {err}");
    }
    drop(tx);
    h.cleanup().await.unwrap();
}

// ----- 14. typed relational fields ---------------------------------------------------------------

#[tokio::test]
async fn typed_relational_fields_carry_their_comodel() {
    use erp_sdk::{Cmd, Many2one, RecordSet, Vals, X2many};
    use erp_test_models::test_rel::{Counter, CounterItem};
    let h = support::rel_orm().await;
    let mut tx = h.begin().await.unwrap();
    let counters = tx.env().model::<Counter>().unwrap();
    let item = |n: &str| Vals::<CounterItem>::new().set(CounterItem::NAME, n);
    let counter = counters
        .create(
            &mut tx,
            vec![Vals::new().set(Counter::NAME, "c").commands(
                Counter::ITEM_IDS,
                [Cmd::create(item("i1")), Cmd::create(item("i2"))],
            )],
        )
        .await
        .unwrap();
    let items: RecordSet<CounterItem> = counter.related(&mut tx, Counter::ITEM_IDS).await.unwrap();
    assert_eq!(items.len(), 2);
    let value: X2many<CounterItem> = counter.get(&mut tx, Counter::ITEM_IDS).await.unwrap();
    assert_eq!(value.ids(), items.rec_ids());
    let first = items.at(0).unwrap();
    let back: RecordSet<Counter> = first
        .related(&mut tx, CounterItem::COUNTER_ID)
        .await
        .unwrap();
    assert_eq!(back, counter);
    let link: Many2one<Counter> = first.get(&mut tx, CounterItem::COUNTER_ID).await.unwrap();
    assert_eq!(link.id(), Some(counter.rec_ids()[0]));
    first
        .set(&mut tx, CounterItem::COUNTER_ID, None::<RecId>)
        .await
        .unwrap();
    assert_eq!(counter.get(&mut tx, Counter::ITEM_COUNT).await.unwrap(), 1);
    drop(tx);
    h.cleanup().await.unwrap();
}

// ----- 13. in-memory x2many conditions with invalid ids -----------------------------------------

#[tokio::test]
async fn filtered_domain_refuses_non_positive_x2many_ids() {
    let h = support::rel_orm().await;
    let mut tx = h.begin().await.unwrap();
    let p = create(&mut tx, "test_rel.partner", &[&[("name", Value::str("p"))]]).await;
    for json in [
        json!([["child_ids", "in", [0]]]),
        json!([["child_ids", "in", [-3]]]),
    ] {
        let err = p
            .filtered_domain(&mut tx, &dom(json.clone()))
            .await
            .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Validation, "{json}: {err}");
    }
    drop(tx);
    h.cleanup().await.unwrap();
}
