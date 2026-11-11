//! Relational fields of new records and commands in new-record mode (slice S4b): ENV §4.9,
//! REL §4.8.6–4.8.8.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

mod support;

use erp_sdk::{Cell, Command, RawVals, RecId, Records, Transaction, Value};
use erp_testkit::assert_query_count;
use support::map;

async fn create(tx: &mut Transaction, model: &str, rows: &[&[(&str, Value)]]) -> Records {
    let records = tx.env().records(model).unwrap();
    let vals: Vec<_> = rows.iter().map(|r| map(r)).collect();
    records.create_values(tx, &vals).await.unwrap()
}

fn s(v: &str) -> Value {
    Value::str(v)
}

fn int(records: &Records, i: usize) -> Value {
    Value::Int(records.ids()[i])
}

fn text(v: &str) -> Cell {
    Cell::Str(v.into())
}

fn rid(records: &Records, i: usize) -> RecId {
    records.rec_ids()[i]
}

async fn new(tx: &mut Transaction, model: &str, vals: RawVals, origin: Option<RecId>) -> Records {
    let records = tx.env().records(model).unwrap();
    tx.new_record(&records, vals, origin, None).await.unwrap()
}

async fn rel(tx: &mut Transaction, records: &Records, field: &str) -> Records {
    records.related(tx, field).await.unwrap()
}

#[tokio::test]
async fn spec_env_044_relational_fields_of_new_records() {
    let h = support::dep_orm().await;
    let mut tx = h.begin().await.unwrap();
    let msgs = create(
        &mut tx,
        "test_dep.message",
        &[&[("body", s("XXX"))], &[("body", s("WWW"))]],
    )
    .await;
    let users = create(
        &mut tx,
        "test_dep.user",
        &[&[("name", s("AAA"))], &[("name", s("DDD"))]],
    )
    .await;
    let disc = new(
        &mut tx,
        "test_dep.discussion",
        RawVals::new()
            .set_cell("name", text("Stuff"))
            .set_commands(
                "message_ids",
                [
                    Command::link(rid(&msgs, 0)),
                    Command::link(rid(&msgs, 1)),
                    Command::update(rid(&msgs, 1), RawVals::new().set_cell("body", text("YYY"))),
                    Command::create(RawVals::new().set_cell("body", text("ZZZ"))),
                ],
            )
            .set_commands(
                "participants",
                [
                    Command::link(rid(&users, 0)),
                    Command::link(rid(&users, 1)),
                    Command::update(rid(&users, 1), RawVals::new().set_cell("name", text("BBB"))),
                    Command::create(RawVals::new().set_cell("name", text("CCC"))),
                ],
            ),
        None,
    )
    .await;
    assert!(disc.rec_ids()[0].is_new());
    // x2many values are new records, connected to their origins
    let messages = rel(&mut tx, &disc, "message_ids").await;
    assert_eq!(messages.len(), 3);
    assert!(messages.rec_ids().iter().all(|i| i.is_new()));
    assert_eq!(messages.origin().ids(), msgs.ids());
    // the one2many lines point back at the new record; the database lines do not
    for line in messages.iter() {
        assert_eq!(rel(&mut tx, &line, "discussion_id").await, disc);
    }
    assert!(rel(&mut tx, &msgs, "discussion_id").await.is_empty());
    // values are specific, or the origin's
    assert_eq!(
        messages.values(&mut tx, "body").await.unwrap(),
        [s("XXX"), s("YYY"), s("ZZZ")]
    );
    assert_eq!(
        msgs.values(&mut tx, "body").await.unwrap(),
        [s("XXX"), s("WWW")]
    );
    let people = rel(&mut tx, &disc, "participants").await;
    assert!(people.rec_ids().iter().all(|i| i.is_new()));
    assert_eq!(
        people.values(&mut tx, "name").await.unwrap(),
        [s("AAA"), s("BBB"), s("CCC")]
    );
    assert_eq!(
        users.values(&mut tx, "name").await.unwrap(),
        [s("AAA"), s("DDD")]
    );
    // a new record with an origin: the origin's lines, as new records
    let real = create(
        &mut tx,
        "test_dep.discussion",
        &[&[
            ("name", s("Real")),
            ("message_ids", Value::list([int(&msgs, 0), int(&msgs, 1)])),
        ]],
    )
    .await;
    let copy = new(
        &mut tx,
        "test_dep.discussion",
        RawVals::new(),
        Some(rid(&real, 0)),
    )
    .await;
    assert_eq!(copy.value(&mut tx, "name").await.unwrap(), s("Real"));
    let lines = rel(&mut tx, &copy, "message_ids").await;
    assert_eq!(lines.len(), 2);
    assert_ne!(lines, rel(&mut tx, &real, "message_ids").await);
    assert_eq!(lines.origin(), rel(&mut tx, &real, "message_ids").await);
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_comp_067_recompute_on_new_records_follows_real_dependencies() {
    let h = support::dep_orm().await;
    let mut tx = h.begin().await.unwrap();
    let mv = create(&mut tx, "test_dep.move", &[&[]]).await;
    create(
        &mut tx,
        "test_dep.move_line",
        &[
            &[("move_id", int(&mv, 0)), ("quantity", Value::Int(1))],
            &[("move_id", int(&mv, 0)), ("quantity", Value::Int(1))],
        ],
    )
    .await;
    tx.flush().await.unwrap();
    let line = rel(&mut tx, &mv, "line_ids")
        .await
        .browse_ids([rel(&mut tx, &mv, "line_ids").await.rec_ids()[0]]);
    let new_move = new(&mut tx, "test_dep.move", RawVals::new(), Some(rid(&mv, 0))).await;
    let new_line = new(
        &mut tx,
        "test_dep.move_line",
        RawVals::new(),
        Some(rid(&line, 0)),
    )
    .await;
    // move_id is read from the origin
    assert_eq!(rel(&mut tx, &new_line, "move_id").await, mv);
    assert_eq!(
        new_move.value(&mut tx, "quantity").await.unwrap(),
        Value::Int(2)
    );
    assert_eq!(mv.value(&mut tx, "quantity").await.unwrap(), Value::Int(2));
    // the new line changes: the new move is recomputed, even though new_line.move_id is the
    // database move
    new_line
        .write_values(&mut tx, &map(&[("quantity", Value::Int(2))]))
        .await
        .unwrap();
    assert_eq!(rel(&mut tx, &new_line, "move_id").await, mv);
    assert_eq!(
        new_move.value(&mut tx, "quantity").await.unwrap(),
        Value::Int(3)
    );
    assert_eq!(mv.value(&mut tx, "quantity").await.unwrap(), Value::Int(2));
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_rel_056_commands_on_new_records() {
    let h = support::dep_orm().await;
    let mut tx = h.begin().await.unwrap();
    let mv = create(&mut tx, "test_dep.move", &[&[]]).await;
    let line = create(
        &mut tx,
        "test_dep.move_line",
        &[&[("move_id", int(&mv, 0)), ("quantity", Value::Int(1))]],
    )
    .await;
    tx.flush().await.unwrap();
    let new_move = new(&mut tx, "test_dep.move", RawVals::new(), Some(rid(&mv, 0))).await;
    let new_line = new(
        &mut tx,
        "test_dep.move_line",
        RawVals::new(),
        Some(rid(&line, 0)),
    )
    .await;
    assert_eq!(rel(&mut tx, &new_move, "line_ids").await, new_line);
    // drop the line, create one: nothing is written
    new_move
        .write(
            &mut tx,
            RawVals::new().set_commands(
                "line_ids",
                [
                    Command::delete(rid(&new_line, 0)),
                    Command::create(RawVals::new().set_cell("quantity", Cell::Int(2))),
                ],
            ),
        )
        .await
        .unwrap();
    let lines = rel(&mut tx, &new_move, "line_ids").await;
    assert_eq!(lines.len(), 1);
    assert!(lines.rec_ids()[0].is_new() && lines.rec_ids()[0].origin().is_none());
    assert_eq!(
        lines.value(&mut tx, "quantity").await.unwrap(),
        Value::Int(2)
    );
    tx.flush().await.unwrap();
    tx.invalidate_all().await.unwrap();
    assert_eq!(
        rel(&mut tx, &mv, "line_ids").await,
        line,
        "the database line still exists"
    );
    // a database line assigned to a new move without origin
    let other = new(&mut tx, "test_dep.move", RawVals::new(), None).await;
    other.assign(&mut tx, "line_ids", &line).await.unwrap();
    let lines = rel(&mut tx, &other, "line_ids").await;
    assert!(lines.rec_ids()[0].is_new());
    assert_eq!(lines.origin(), line);
    assert_eq!(rel(&mut tx, &lines, "move_id").await, other);
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_rel_057_new_and_real_records_across_a_relation() {
    let h = support::dep_orm().await;
    let mut tx = h.begin().await.unwrap();
    // REAL - NEW
    let parent = create(&mut tx, "test_dep.parent_m2o", &[&[("name", s("parentB"))]]).await;
    let new_child = new(
        &mut tx,
        "test_dep.child_m2o",
        RawVals::new()
            .set_cell("name", text("B"))
            .set_cell("parent_id", Cell::Id(parent.ids()[0])),
        None,
    )
    .await;
    assert!(rel(&mut tx, &parent, "child_ids").await.is_empty());
    assert_eq!(rel(&mut tx, &new_child, "parent_id").await, parent);
    // a new record added to a database record's one2many is created
    let value = rel(&mut tx, &parent, "child_ids")
        .await
        .concat(&new_child)
        .unwrap();
    parent.assign(&mut tx, "child_ids", &value).await.unwrap();
    let children = rel(&mut tx, &parent, "child_ids").await;
    assert_eq!(children.len(), 1);
    assert_ne!(children, new_child);
    assert!(!children.rec_ids()[0].is_new());
    assert_eq!(children.value(&mut tx, "name").await.unwrap(), s("B"));
    // NEW - NEW
    let new_parent = new(
        &mut tx,
        "test_dep.parent_m2o",
        RawVals::new()
            .set_cell("name", text("parentC3PO"))
            .set_commands(
                "child_ids",
                [Command::create(RawVals::new().set_cell("name", text("C3")))],
            ),
        None,
    )
    .await;
    let kids = rel(&mut tx, &new_parent, "child_ids").await;
    assert_eq!(rel(&mut tx, &kids, "parent_id").await, new_parent);
    let po = new(
        &mut tx,
        "test_dep.child_m2o",
        RawVals::new().set_cell("name", text("PO")),
        None,
    )
    .await;
    let value = kids.concat(&po).unwrap();
    new_parent
        .assign(&mut tx, "child_ids", &value)
        .await
        .unwrap();
    let kids = rel(&mut tx, &new_parent, "child_ids").await;
    assert_eq!(
        kids.values(&mut tx, "name").await.unwrap(),
        [s("C3"), s("PO")]
    );
    new(
        &mut tx,
        "test_dep.child_m2o",
        RawVals::new()
            .set_cell("name", text("R2D2"))
            .set_cell("parent_id", Cell::Id(new_parent.rec_ids()[0].get())),
        None,
    )
    .await;
    let kids = rel(&mut tx, &new_parent, "child_ids").await;
    assert_eq!(
        kids.values(&mut tx, "name").await.unwrap(),
        [s("C3"), s("PO"), s("R2D2")]
    );
    // NEW TO REAL
    let commands = new_parent
        .x2many_commands(&mut tx, "child_ids")
        .await
        .unwrap();
    let parents = tx.env().records("test_dep.parent_m2o").unwrap();
    let created = parents
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", text("parentC3PO"))
                    .set_commands("child_ids", commands),
            ],
        )
        .await
        .unwrap();
    let kids = rel(&mut tx, &created, "child_ids").await;
    assert_eq!(
        kids.values(&mut tx, "name").await.unwrap(),
        [s("C3"), s("PO"), s("R2D2")]
    );
    assert_eq!(rel(&mut tx, &kids, "parent_id").await, created);
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_rel_058_many2many_on_new_records_does_not_fetch_inverses() {
    let h = support::dep_orm().await;
    let mut tx = h.begin().await.unwrap();
    let group = create(&mut tx, "test_dep.group", &[&[("name", s("G"))]]).await;
    let user0 = create(
        &mut tx,
        "test_dep.user",
        &[&[
            ("name", s("u0")),
            ("group_ids", Value::list([int(&group, 0)])),
        ]],
    )
    .await;
    let new_user0 = new(
        &mut tx,
        "test_dep.user",
        RawVals::new(),
        Some(rid(&user0, 0)),
    )
    .await;
    let new_group = new(
        &mut tx,
        "test_dep.group",
        RawVals::new(),
        Some(rid(&group, 0)),
    )
    .await;
    tx.flush().await.unwrap();
    tx.invalidate_all().await.unwrap();
    let link = || RawVals::new().set_commands("group_ids", [Command::link(group.rec_ids()[0])]);
    // linking does not read the group's users
    let new_user1 = assert_query_count!(tx, <= 0, {
        let u = new(&mut tx, "test_dep.user", link(), None).await;
        assert_eq!(rel(&mut tx, &u, "group_ids").await, new_group);
        u
    });
    // reading them: one query, patched with the new link
    assert_query_count!(tx, >= 1, <= 1, {
        assert_eq!(
            rel(&mut tx, &new_group, "user_ids").await,
            new_user0.concat(&new_user1).unwrap()
        );
    });
    // in the cache now: patched immediately
    assert_query_count!(tx, <= 0, {
        let u2 = new(&mut tx, "test_dep.user", link(), None).await;
        assert_eq!(rel(&mut tx, &u2, "group_ids").await, new_group);
        assert_eq!(
            rel(&mut tx, &new_group, "user_ids").await,
            new_user0.concat(&new_user1).unwrap().concat(&u2).unwrap()
        );
    });
    // the database group is untouched
    assert_eq!(rel(&mut tx, &group, "user_ids").await, user0);
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_rel_061_x2many_recordset_to_commands() {
    let h = support::dep_orm().await;
    let mut tx = h.begin().await.unwrap();
    let o = create(&mut tx, "test_dep.order", &[&[("name", s("o"))]]).await;
    let lines = create(
        &mut tx,
        "test_dep.line",
        &[
            &[
                ("order_id", int(&o, 0)),
                ("product", s("l1")),
                ("price_unit", Value::Float(1.0)),
            ],
            &[
                ("order_id", int(&o, 0)),
                ("product", s("l2")),
                ("price_unit", Value::Float(2.0)),
            ],
        ],
    )
    .await;
    tx.flush().await.unwrap();
    let order = new(&mut tx, "test_dep.order", RawVals::new(), Some(rid(&o, 0))).await;
    let current = rel(&mut tx, &order, "line_ids").await;
    // l1 changes in memory, a line is added
    current
        .browse_ids([current.rec_ids()[0]])
        .write_values(&mut tx, &map(&[("product", s("l1 bis"))]))
        .await
        .unwrap();
    order
        .write(
            &mut tx,
            RawVals::new().set_commands(
                "line_ids",
                [Command::create(
                    RawVals::new().set_cell("product", text("l3")),
                )],
            ),
        )
        .await
        .unwrap();
    let commands = order.x2many_commands(&mut tx, "line_ids").await.unwrap();
    assert_eq!(commands.len(), 3, "{commands:?}");
    assert_eq!(commands[0], Command::set(lines.rec_ids().to_vec()));
    let Command::Update(id, vals) = &commands[1] else {
        unreachable!("{commands:?}")
    };
    assert_eq!(*id, rid(&lines, 0));
    assert_eq!(vals.iter().map(|(n, _)| n).collect::<Vec<_>>(), ["product"]);
    assert_eq!(vals.get("product"), Some(&text("l1 bis")));
    let Command::Create(vals) = &commands[2] else {
        unreachable!("{commands:?}")
    };
    assert_eq!(vals.get("product"), Some(&text("l3")));
    assert!(
        vals.get("order_id").is_none(),
        "the inverse field is left out"
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_rel_083_new_many2many_converts_back_to_a_plain_set() {
    let h = support::dep_orm().await;
    let mut tx = h.begin().await.unwrap();
    let groups = create(
        &mut tx,
        "test_dep.group",
        &[&[("name", s("g1"))], &[("name", s("g2"))]],
    )
    .await;
    let users = create(
        &mut tx,
        "test_dep.user",
        &[
            &[
                ("name", s("u1")),
                ("group_ids", Value::list([int(&groups, 0)])),
            ],
            &[
                ("name", s("u2")),
                ("group_ids", Value::list([int(&groups, 0), int(&groups, 1)])),
            ],
        ],
    )
    .await;
    let d = new(
        &mut tx,
        "test_dep.discussion",
        RawVals::new()
            .set_cell("name", text("Stuff"))
            .set_commands("participants", [Command::set(users.rec_ids().to_vec())]),
        None,
    )
    .await;
    // the new participants' cache holds new groups whose origins are the database groups
    let people = rel(&mut tx, &d, "participants").await;
    let their_groups = rel(&mut tx, &people, "group_ids").await;
    assert!(their_groups.rec_ids().iter().all(|i| i.is_new()));
    let commands = d.x2many_commands(&mut tx, "participants").await.unwrap();
    assert_eq!(commands, [Command::set(users.rec_ids().to_vec())]);
    drop(tx);
    h.cleanup().await.unwrap();
}

async fn lines_named(tx: &mut Transaction, multi: &Records) -> Vec<Value> {
    rel(tx, multi, "lines")
        .await
        .values(tx, "name")
        .await
        .unwrap()
}

async fn operations(tx: &mut Transaction, multi: &Records) {
    let mut names = lines_named(tx, multi).await;
    names.sort_by_key(|v| v.to_string());
    assert_eq!(
        names,
        (0..10).map(|i| s(&i.to_string())).collect::<Vec<_>>()
    );
    let lines = rel(tx, multi, "lines").await;
    lines
        .browse_ids([lines.rec_ids()[0]])
        .write_values(tx, &map(&[("name", s("hello"))]))
        .await
        .unwrap();
    let kept = lines.browse_ids(lines.rec_ids()[..9].to_vec());
    multi.assign(tx, "lines", &kept).await.unwrap();
    let names = lines_named(tx, multi).await;
    assert_eq!(names.len(), 9);
    assert!(names.contains(&s("hello")));
    if multi.rec_ids()[0].is_new() {
        return;
    }
    tx.invalidate_all().await.unwrap();
    let names = lines_named(tx, multi).await;
    assert_eq!(names.len(), 9, "no new record is left in the cache");
    assert!(names.contains(&s("hello")));
}

#[tokio::test]
async fn spec_rel_081_building_a_one2many_step_by_step() {
    let h = support::dep_orm().await;
    let line = |n: usize| RawVals::new().set_cell("name", text(&n.to_string()));
    // new lines appended one by one to a database record
    let mut tx = h.begin().await.unwrap();
    let multi = create(&mut tx, "test_dep.multi", &[&[("name", s("What is up?"))]]).await;
    for n in 0..10 {
        let l = new(&mut tx, "test_dep.multi_line", line(n), None).await;
        let value = rel(&mut tx, &multi, "lines").await.union(&l).unwrap();
        multi.assign(&mut tx, "lines", &value).await.unwrap();
    }
    operations(&mut tx, &multi).await;
    tx.rollback().await.unwrap();
    drop(tx);
    // new lines assigned in one step
    let mut tx = h.begin().await.unwrap();
    let multi = create(&mut tx, "test_dep.multi", &[&[("name", s("What is up?"))]]).await;
    let mut ids = Vec::new();
    for n in 0..10 {
        ids.push(
            new(&mut tx, "test_dep.multi_line", line(n), None)
                .await
                .rec_ids()[0],
        );
    }
    let value = tx
        .env()
        .records("test_dep.multi_line")
        .unwrap()
        .browse_ids(ids);
    multi.assign(&mut tx, "lines", &value).await.unwrap();
    operations(&mut tx, &multi).await;
    tx.rollback().await.unwrap();
    drop(tx);
    // created lines appended one by one, then in one step
    let mut tx = h.begin().await.unwrap();
    let multi = create(&mut tx, "test_dep.multi", &[&[("name", s("What is up?"))]]).await;
    let model = tx.env().records("test_dep.multi_line").unwrap();
    for n in 0..10 {
        let l = model.create(&mut tx, vec![line(n)]).await.unwrap();
        let value = rel(&mut tx, &multi, "lines").await.union(&l).unwrap();
        multi.assign(&mut tx, "lines", &value).await.unwrap();
    }
    operations(&mut tx, &multi).await;
    tx.rollback().await.unwrap();
    drop(tx);
    let mut tx = h.begin().await.unwrap();
    let multi = create(&mut tx, "test_dep.multi", &[&[("name", s("What is up?"))]]).await;
    let model = tx.env().records("test_dep.multi_line").unwrap();
    let created = model
        .create(&mut tx, (0..10).map(line).collect())
        .await
        .unwrap();
    multi.assign(&mut tx, "lines", &created).await.unwrap();
    operations(&mut tx, &multi).await;
    tx.rollback().await.unwrap();
    drop(tx);
    // RPC style, one CREATE at a time: each adds a line
    for on_new in [false, true] {
        let mut tx = h.begin().await.unwrap();
        let multi = if on_new {
            new(
                &mut tx,
                "test_dep.multi",
                RawVals::new().set_cell("name", text("What is up?")),
                None,
            )
            .await
        } else {
            create(&mut tx, "test_dep.multi", &[&[("name", s("What is up?"))]]).await
        };
        for n in 0..10 {
            multi
                .write(
                    &mut tx,
                    RawVals::new().set_commands("lines", [Command::create(line(n))]),
                )
                .await
                .unwrap();
        }
        operations(&mut tx, &multi).await;
        tx.rollback().await.unwrap();
        drop(tx);
    }
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_comp_073_create_on_a_computed_one2many_with_inverse() {
    let h = support::dep_orm().await;
    let mut tx = h.begin().await.unwrap();
    let model = tx.env().records("test_dep.inverse_parent").unwrap();
    let record = model
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", text("SuperRecord"))
                    .set_commands(
                        "low_priority_line_ids",
                        [Command::create(
                            RawVals::new()
                                .set_cell("name", text("SuperChild 01"))
                                .set_cell("priority", Cell::Int(1)),
                        )],
                    ),
            ],
        )
        .await
        .unwrap();
    let low = rel(&mut tx, &record, "low_priority_line_ids").await;
    assert_eq!(low.len(), 1);
    assert!(!low.rec_ids()[0].is_new(), "a database line");
    record
        .write(
            &mut tx,
            RawVals::new().set_commands(
                "low_priority_line_ids",
                [Command::create(
                    RawVals::new()
                        .set_cell("name", text("SuperChild 02"))
                        .set_cell("priority", Cell::Int(2)),
                )],
            ),
        )
        .await
        .unwrap();
    let low = rel(&mut tx, &record, "low_priority_line_ids").await;
    assert_eq!(low.len(), 2);
    assert!(low.rec_ids().iter().all(|i| !i.is_new()));
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_env_045_new_records_prefetch_from_their_origins() {
    let h = support::dep_orm().await;
    let mut tx = h.begin().await.unwrap();
    let orders = tx.env().records("test_dep.order").unwrap();
    let vals: Vec<RawVals> = [1.0, 2.0, 3.0]
        .into_iter()
        .map(|p| {
            RawVals::new().set_commands(
                "line_ids",
                [Command::create(
                    RawVals::new().set_cell("price_unit", Cell::Float(p)),
                )],
            )
        })
        .collect();
    let records = orders.create(&mut tx, vals).await.unwrap();
    tx.flush().await.unwrap();
    tx.invalidate_all().await.unwrap();
    let fresh = new(
        &mut tx,
        "test_dep.order",
        RawVals::new().set_commands(
            "line_ids",
            [Command::create(
                RawVals::new().set_cell("price_unit", Cell::Float(4.0)),
            )],
        ),
        None,
    )
    .await;
    let mut ids = Vec::new();
    for r in records.rec_ids() {
        ids.push(
            new(&mut tx, "test_dep.order", RawVals::new(), Some(*r))
                .await
                .rec_ids()[0],
        );
    }
    ids.push(fresh.rec_ids()[0]);
    let all = orders.browse_ids(ids);
    let prices = assert_query_count!(tx, >= 1, <= 2, {
        let mut prices = Vec::new();
        for record in all.iter() {
            for line in rel(&mut tx, &record, "line_ids").await.iter() {
                prices.push(line.value(&mut tx, "price_unit").await.unwrap());
            }
        }
        prices
    });
    assert_eq!(prices, [1.0, 2.0, 3.0, 4.0].map(Value::Float).to_vec());
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_comp_043_related_fields_on_new_records() {
    let h = support::dep_orm().await;
    let mut tx = h.begin().await.unwrap();
    let disc = create(&mut tx, "test_dep.discussion", &[&[("name", s("Real"))]]).await;
    let message = new(
        &mut tx,
        "test_dep.message",
        RawVals::new().set_cell("discussion_id", Cell::Id(disc.ids()[0])),
        None,
    )
    .await;
    assert_eq!(rel(&mut tx, &message, "discussion_id").await, disc);
    assert_eq!(
        message.value(&mut tx, "discussion_name").await.unwrap(),
        s("Real")
    );
    // through a new discussion
    let new_disc = new(
        &mut tx,
        "test_dep.discussion",
        RawVals::new().set_cell("name", text("Stuff")),
        None,
    )
    .await;
    let message2 = new(
        &mut tx,
        "test_dep.message",
        RawVals::new().set_cell("discussion_id", Cell::Id(new_disc.rec_ids()[0].get())),
        None,
    )
    .await;
    assert_eq!(
        message2.value(&mut tx, "discussion_name").await.unwrap(),
        s("Stuff")
    );
    // writing the related field on a new record changes only a new target
    message2
        .write_values(&mut tx, &map(&[("discussion_name", s("Changed"))]))
        .await
        .unwrap();
    assert_eq!(new_disc.value(&mut tx, "name").await.unwrap(), s("Changed"));
    message
        .write_values(&mut tx, &map(&[("discussion_name", s("Nope"))]))
        .await
        .unwrap();
    assert_eq!(disc.value(&mut tx, "name").await.unwrap(), s("Real"));
    drop(tx);
    h.cleanup().await.unwrap();
}

/// an empty new record)
#[tokio::test]
async fn spec_comp_074_computes_without_dependencies_on_empty_new_records() {
    let h = support::dep_orm().await;
    let mut tx = h.begin().await.unwrap();
    let registry = std::sync::Arc::clone(tx.registry());
    let mut checked = 0;
    for spec in registry.models() {
        if !spec.has_table() {
            continue;
        }
        for field in &spec.fields {
            if !field.is_computed() || field.related.is_some() {
                continue;
            }
            if !tx.field_depends(spec.name, field.name).unwrap().is_empty() {
                continue;
            }
            let model = tx.env().records(spec.name).unwrap();
            let record = tx
                .new_record(&model, RawVals::new(), None, None)
                .await
                .unwrap();
            let result = tx
                .savepoint(async |tx| record.value(tx, field.name).await.map(|_| ()))
                .await;
            assert!(result.is_ok(), "{}.{}: {result:?}", spec.name, field.name);
            checked += 1;
        }
    }
    assert!(checked > 0, "display_name at least");
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Delegated (`_inherits`) values on new records are refused loudly instead of being lost: `new`
/// with delegated values (a new parent record would be needed; not supported yet), and a write on
/// a new record without an in-memory parent.
#[tokio::test]
async fn delegated_values_on_new_records_are_refused() {
    let h = support::dep_orm().await;
    let mut tx = h.begin().await.unwrap();
    let model = tx.env().records("test_dep.boxed").unwrap();
    let err = tx
        .new_record(
            &model,
            RawVals::new().set_cell("name", text("U")),
            None,
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(err.kind(), erp_sdk::ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "test_dep.boxed.name is a field of an _inherits parent: values of delegated fields on new \
         records are not supported yet (a new parent record would be needed; S4b follow-up, not \
         scheduled), and would otherwise be lost"
    );
    let boxed = tx
        .new_record(&model, RawVals::new(), None, None)
        .await
        .unwrap();
    let err = boxed
        .write_values(&mut tx, &map(&[("name", s("U"))]))
        .await
        .unwrap_err();
    assert_eq!(err.kind(), erp_sdk::ErrorKind::Programming);
    assert_eq!(
        err.message(),
        format!(
            "test_dep.boxed.name: new record {} has no unit_id; delegated values of new records \
             go to an in-memory parent, so the value would be lost",
            boxed.rec_ids()[0]
        )
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_env_057_new_rejects_unknown_fields() {
    let h = support::orm().await;
    let mut tx = h.begin().await.unwrap();
    let records = tx.env().records("test_orm.category").unwrap();
    let mut vals = RawVals::new();
    vals.insert("name".into(), Cell::Str(std::sync::Arc::from("Foo")));
    vals.insert("zzz".into(), Cell::Int(42));
    let err = tx.new_record(&records, vals, None, None).await.unwrap_err();
    assert_eq!(err.kind(), erp_sdk::ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "Invalid field 'zzz' on model 'test_orm.category'"
    );
    drop(tx);
    h.cleanup().await.unwrap();
}
