//! Actions and menus of addon `base` (design, §6, §10):
//! window action `views` and `view_mode`, paths (MN-3), the
//! shared id space and the key table `ir_actions`, and the menu tree.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod support;

use std::sync::Arc;
use std::time::Duration;

use erp_addon_base::ir_actions::{ActionId, ActionPathError, action_by_id};
use erp_sdk::serde_json::json;
use erp_sdk::{Cell, Command, Error, ErrorKind, Orm, RawVals, RecId, Records, StaticPolicy};
use erp_testkit::Harness;
use support::rows;

async fn harness() -> Harness {
    Harness::builder([&erp_addon_base::ADDON, &erp_data::model_data::ADDON])
        .with_decls([erp_data::ir_model_data_decl()])
        .build()
        .await
        .expect("harness")
}

/// Commit and drop, so the connection goes back to the pool (a finished `Transaction` keeps it).
async fn commit(mut tx: erp_sdk::Transaction) {
    tx.commit().await.unwrap();
}

async fn rollback(mut tx: erp_sdk::Transaction) {
    tx.rollback().await.unwrap();
}

fn s(text: &str) -> Cell {
    Cell::Str(Arc::from(text))
}

fn vals(pairs: &[(&'static str, Cell)]) -> RawVals {
    pairs
        .iter()
        .fold(RawVals::new(), |v, (k, c)| v.set_cell(*k, c.clone()))
}

fn rec(id: i64) -> RecId {
    RecId::from_real(id).unwrap()
}

async fn create(
    tx: &mut erp_sdk::Transaction,
    model: &str,
    list: Vec<RawVals>,
) -> erp_sdk::Result<Records> {
    let records = tx.env().records(model)?;
    records.create(tx, list).await
}

async fn create_one(tx: &mut erp_sdk::Transaction, model: &str, v: RawVals) -> i64 {
    create(tx, model, vec![v]).await.unwrap().ids()[0]
}

/// The `ir_model` row of `model` (the reflection is not filled by the harness).
async fn ir_model(tx: &mut erp_sdk::Transaction, model: &str) -> i64 {
    let rows = tx
        .query(
            "INSERT INTO ir_model (name, model, state, transient) VALUES ($1, $1, 'base', false) \
             RETURNING id",
            &[&model],
        )
        .await
        .unwrap();
    rows[0].get(0)
}

fn assert_error(result: erp_sdk::Result<impl std::fmt::Debug>, kind: ErrorKind, message: &str) {
    let error: Error = result.expect_err(message);
    assert_eq!(error.kind(), kind, "{error}");
    assert_eq!(error.message(), message);
}

/// Every key row has its typed row and every typed row its key row, of the same type (review
/// C-S4: the key table is kept in step by `create`, `write` and the follow-delete triggers, so a
/// count of rows written is not checked statement by statement).
async fn assert_key_table_in_step(tx: &mut erp_sdk::Transaction) {
    let typed = "SELECT id, 'ir.actions.act_window' AS t FROM ir_act_window \
                 UNION ALL SELECT id, 'ir.actions.act_url' FROM ir_act_url \
                 UNION ALL SELECT id, 'ir.actions.client' FROM ir_act_client \
                 UNION ALL SELECT id, 'ir.actions.server' FROM ir_act_server";
    assert_eq!(
        rows(
            tx,
            &format!(
                "SELECT (SELECT count(*) FROM ir_actions a WHERE NOT EXISTS \
                   (SELECT FROM ({typed}) x WHERE x.id = a.id AND x.t = a.type))::text, \
                 (SELECT count(*) FROM ({typed}) x WHERE NOT EXISTS \
                   (SELECT FROM ir_actions a WHERE a.id = x.id AND a.type = x.t))::text"
            )
        )
        .await,
        vec![vec![Some("0".to_owned()), Some("0".to_owned())]],
        "key rows without a typed row, typed rows without a key row"
    );
}

async fn view(tx: &mut erp_sdk::Transaction, name: &str, kind: &str) -> i64 {
    create_one(
        tx,
        "ir.ui.view",
        vals(&[
            ("name", s(name)),
            ("model", s("res.groups")),
            ("type", s(kind)),
            ("arch_db", s(&format!("<{kind}/>"))),
            ("mode", s("primary")),
        ]),
    )
    .await
}

#[tokio::test]
async fn window_action_views_and_view_mode() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let form = view(&mut tx, "groups form", "form").await;
    let kanban = view(&mut tx, "groups kanban", "kanban").await;
    let action = create_one(
        &mut tx,
        "ir.actions.act_window",
        vals(&[
            ("name", s("Groups")),
            ("res_model", s("res.groups")),
            ("view_mode", s("list,form,kanban")),
            ("view_id", Cell::Id(form)),
        ])
        .set_commands(
            "view_ids",
            [Command::create(vals(&[
                ("view_mode", s("kanban")),
                ("view_id", Cell::Id(kanban)),
            ]))],
        ),
    )
    .await;
    let records = tx
        .env()
        .records("ir.actions.act_window")
        .unwrap()
        .browse([action])
        .unwrap();
    let views = records.cells(&mut tx, "views").await.unwrap();
    let expected = json!([[kanban, "kanban"], [form, "form"], [false, "list"]]);
    assert_eq!(views, vec![Cell::Json(Arc::new(expected))]);

    // without view_ids rows and with a view type outside view_mode
    let plain = create_one(
        &mut tx,
        "ir.actions.act_window",
        vals(&[
            ("name", s("Plain")),
            ("res_model", s("res.groups")),
            ("view_id", Cell::Id(kanban)),
        ]),
    )
    .await;
    let plain = tx
        .env()
        .records("ir.actions.act_window")
        .unwrap()
        .browse([plain])
        .unwrap();
    assert_eq!(
        plain.cells(&mut tx, "views").await.unwrap(),
        vec![Cell::Json(Arc::new(json!([
            [false, "list"],
            [false, "form"]
        ])))]
    );
    commit(tx).await;

    for (view_mode, message) in [
        (
            "list,form,list",
            "The modes in view_mode must not be duplicated: ['list', 'form', 'list']",
        ),
        (
            "list, form",
            "No spaces allowed in view_mode: “['list', ' form']”",
        ),
        (
            "list,,form",
            "No spaces allowed in view_mode: “['list', '', 'form']”",
        ),
    ] {
        let mut tx = h.begin().await.unwrap();
        let result = create(
            &mut tx,
            "ir.actions.act_window",
            vec![vals(&[
                ("name", s("Bad")),
                ("res_model", s("res.groups")),
                ("view_mode", s(view_mode)),
            ])],
        )
        .await;
        assert_error(result, ErrorKind::Validation, message);
        rollback(tx).await;

        let mut tx = h.begin().await.unwrap();
        let result = async {
            plain
                .with_env(tx.env())
                .write(&mut tx, vals(&[("view_mode", s(view_mode))]))
                .await?;
            tx.flush().await
        }
        .await;
        assert_error(result, ErrorKind::Validation, message);
        rollback(tx).await;
    }

    // the model checks: res_model and the binding model must be registry models
    let mut tx = h.begin().await.unwrap();
    let result = create(
        &mut tx,
        "ir.actions.act_window",
        vec![vals(&[("name", s("Bad")), ("res_model", s("x.nothing"))])],
    )
    .await;
    assert_error(
        result,
        ErrorKind::Validation,
        "Invalid model name “x.nothing” in action definition.",
    );
    rollback(tx).await;
    let mut tx = h.begin().await.unwrap();
    let gone = ir_model(&mut tx, "x.gone").await;
    let result = create(
        &mut tx,
        "ir.actions.act_window",
        vec![vals(&[
            ("name", s("Bad")),
            ("res_model", s("res.groups")),
            ("binding_model_id", Cell::Id(gone)),
        ])],
    )
    .await;
    assert_error(
        result,
        ErrorKind::Validation,
        "Invalid model name “x.gone” in action definition.",
    );
    rollback(tx).await;

    // without a name, the action takes its model's description
    let mut tx = h.begin().await.unwrap();
    let unnamed = create_one(
        &mut tx,
        "ir.actions.act_window",
        vals(&[("res_model", s("res.groups"))]),
    )
    .await;
    assert_eq!(
        rows(
            &mut tx,
            &format!("SELECT name FROM ir_act_window WHERE id = {unnamed}")
        )
        .await,
        vec![vec![Some(
            h.registry()
                .model("res.groups")
                .unwrap()
                .description
                .clone()
        )]]
    );
    rollback(tx).await;
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn action_paths() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let groups = ir_model(&mut tx, "res.groups").await;
    commit(tx).await;
    let window = |path: &str| {
        vals(&[
            ("name", s("Invoices")),
            ("res_model", s("res.groups")),
            ("path", s(path)),
        ])
    };
    let server = |path: &str| {
        vals(&[
            ("name", s("Invoices job")),
            ("model_id", Cell::Id(groups)),
            ("state", s("code")),
            ("path", s(path)),
        ])
    };
    let format = "The path should contain only lowercase alphanumeric characters, underscore, and \
                  dash, and it should start with a letter.";
    for (path, message) in [
        ("Invoices", format),
        ("m-x", "'m-' is a reserved prefix."),
        ("action-1", "'action-' is a reserved prefix."),
        ("new", "'new' is reserved, and can not be used as path."),
    ] {
        let mut tx = h.begin().await.unwrap();
        let result = create(&mut tx, "ir.actions.act_window", vec![window(path)]).await;
        let error = result.expect_err(path);
        assert_eq!(error.kind(), ErrorKind::Validation);
        assert_eq!(error.message(), message);
        let source = std::error::Error::source(&error)
            .and_then(|e| e.downcast_ref::<ActionPathError>())
            .expect("typed source");
        assert_eq!(source.to_string(), message);
        rollback(tx).await;
    }

    let mut tx = h.begin().await.unwrap();
    create_one(&mut tx, "ir.actions.act_window", window("invoices")).await;
    let refused = create(&mut tx, "ir.actions.server", vec![server("invoices")]).await;
    let error = refused.expect_err("duplicate");
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "Path to show in the URL must be unique! Please choose another one."
    );
    assert_eq!(
        std::error::Error::source(&error).and_then(|e| e.downcast_ref::<ActionPathError>()),
        Some(&ActionPathError::Duplicate)
    );
    rollback(tx).await;

    // at write: a format error, then a duplicate moved onto another action
    let mut tx = h.begin().await.unwrap();
    let invoices_two = create_one(&mut tx, "ir.actions.act_window", window("invoices")).await;
    let job = create_one(&mut tx, "ir.actions.server", server("jobs")).await;
    let job_set = tx
        .env()
        .records("ir.actions.server")
        .unwrap()
        .browse([job])
        .unwrap();
    commit(tx).await;
    // a refused write poisons its transaction (commit refuses): each refusal in its own
    let mut tx = h.begin().await.unwrap();
    assert_error(
        job_set
            .with_env(tx.env())
            .write(&mut tx, vals(&[("path", s("Jobs"))]))
            .await,
        ErrorKind::Validation,
        format,
    );
    rollback(tx).await;
    let mut tx = h.begin().await.unwrap();
    assert_error(
        job_set
            .with_env(tx.env())
            .write(&mut tx, vals(&[("path", s("invoices"))]))
            .await,
        ErrorKind::Validation,
        "Path to show in the URL must be unique! Please choose another one.",
    );
    rollback(tx).await;

    // deleting the action frees its path
    let mut tx = h.begin().await.unwrap();
    tx.env()
        .records("ir.actions.act_window")
        .unwrap()
        .browse([invoices_two])
        .unwrap()
        .unlink(&mut tx)
        .await
        .unwrap();
    let moved = job_set.with_env(tx.env());
    moved
        .write(&mut tx, vals(&[("path", s("invoices"))]))
        .await
        .unwrap();
    assert_eq!(
        rows(
            &mut tx,
            &format!("SELECT type, path FROM ir_actions WHERE id = {job}")
        )
        .await,
        vec![vec![
            Some("ir.actions.server".to_owned()),
            Some("invoices".to_owned())
        ]]
    );
    commit(tx).await;
    h.cleanup().await.unwrap();
}

/// Wait until some backend of the test database waits on a lock (a transaction blocked on the
/// unique index), at most 30 s.
async fn until_a_lock_wait(h: &Harness) {
    let client = h.db().client().await.unwrap();
    for _ in 0..600 {
        let waiting: i64 = client
            .query_one(
                "SELECT count(*) FROM pg_stat_activity WHERE datname = current_database() \
                 AND wait_event_type = 'Lock'",
                &[],
            )
            .await
            .unwrap()
            .get(0);
        if waiting > 0 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("the second transaction never waited on the path index");
}

/// MN-3: two processes (two `Orm`s on one database) save the path `invoices` on a window action
/// and on a server action at the same time. The second insert waits on the unique index; when
/// the first rolls back it succeeds, when the first commits it fails with the duplicate error.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn path_unique_across_tables_concurrently() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let groups = ir_model(&mut tx, "res.groups").await;
    commit(tx).await;
    let other = Orm::builder(Arc::clone(h.registry()), h.db().pool().clone())
        .policy(StaticPolicy::AllowAll)
        .build()
        .unwrap();
    let other = Arc::new(other);

    for first_commits in [false, true] {
        let mut first = h.begin().await.unwrap();
        create_one(
            &mut first,
            "ir.actions.act_window",
            vals(&[
                ("name", s("Invoices")),
                ("res_model", s("res.groups")),
                ("path", s("invoices")),
            ]),
        )
        .await;
        let node2 = Arc::clone(&other);
        let second = tokio::spawn(async move {
            let mut tx = node2.begin(erp_sdk::SUPERUSER_ID).await?;
            let created = tx
                .env()
                .records("ir.actions.server")?
                .create(
                    &mut tx,
                    vec![vals(&[
                        ("name", s("Invoices job")),
                        ("model_id", Cell::Id(groups)),
                        ("state", s("code")),
                        ("path", s("invoices")),
                    ])],
                )
                .await;
            match created {
                Ok(records) => {
                    tx.commit().await?;
                    Ok(records.ids()[0])
                }
                Err(error) => {
                    tx.rollback().await?;
                    Err(error)
                }
            }
        });
        until_a_lock_wait(&h).await;
        assert!(!second.is_finished(), "the second insert waits");
        if first_commits {
            first.commit().await.unwrap();
            let error = second
                .await
                .unwrap()
                .expect_err("duplicate after the commit");
            assert_eq!(error.kind(), ErrorKind::Validation);
            assert_eq!(
                error.message(),
                "Path to show in the URL must be unique! Please choose another one."
            );
        } else {
            first.rollback().await.unwrap();
            let id = second.await.unwrap().expect("free after the rollback");
            // clean up for the next round
            let mut tx = h.begin().await.unwrap();
            tx.env()
                .records("ir.actions.server")
                .unwrap()
                .browse([id])
                .unwrap()
                .unlink(&mut tx)
                .await
                .unwrap();
            commit(tx).await;
        }
    }
    let mut tx = h.begin().await.unwrap();
    assert_eq!(
        rows(
            &mut tx,
            "SELECT type FROM ir_actions WHERE path = 'invoices' ORDER BY id"
        )
        .await,
        vec![vec![Some("ir.actions.act_window".to_owned())]]
    );
    rollback(tx).await;
    h.cleanup().await.unwrap();
}

/// design m2-s3 §6.1: every action type draws from one id sequence, and `ir_actions`
/// holds one row per action with its type, name, path and binding columns, kept in step by
/// create, write and unlink; `action_by_id` reads it. The key table is not written directly, an
/// action's `type` is its model, and the ids follow the creation order across tables.
#[tokio::test]
async fn key_table_follows_typed_actions() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let groups = ir_model(&mut tx, "res.groups").await;
    let window = create(
        &mut tx,
        "ir.actions.act_window",
        vec![
            vals(&[("name", s("W1")), ("res_model", s("res.groups"))]),
            vals(&[
                ("name", s("W2")),
                ("res_model", s("res.groups")),
                ("binding_model_id", Cell::Id(groups)),
            ]),
        ],
    )
    .await
    .unwrap()
    .ids();
    let url = create_one(
        &mut tx,
        "ir.actions.act_url",
        vals(&[("name", s("U")), ("url", s("https://example.com"))]),
    )
    .await;
    let client = create_one(
        &mut tx,
        "ir.actions.client",
        vals(&[("name", s("C")), ("tag", s("reload"))]),
    )
    .await;
    let server = create_one(
        &mut tx,
        "ir.actions.server",
        vals(&[
            ("name", s("S")),
            ("model_id", Cell::Id(groups)),
            ("state", s("code")),
            ("path", s("s-path")),
        ]),
    )
    .await;
    let all = [window[0], window[1], url, client, server];
    assert!(all.windows(2).all(|w| w[0] < w[1]), "{all:?}");
    assert_eq!(
        rows(
            &mut tx,
            "SELECT id::text, type, name, path, binding_model_id::text, binding_type, \
             binding_view_types FROM ir_actions ORDER BY id"
        )
        .await,
        vec![
            vec![
                Some(window[0].to_string()),
                Some("ir.actions.act_window".to_owned()),
                Some("W1".to_owned()),
                None,
                None,
                Some("action".to_owned()),
                Some("list,form".to_owned())
            ],
            vec![
                Some(window[1].to_string()),
                Some("ir.actions.act_window".to_owned()),
                Some("W2".to_owned()),
                None,
                Some(groups.to_string()),
                Some("action".to_owned()),
                Some("list,form".to_owned())
            ],
            vec![
                Some(url.to_string()),
                Some("ir.actions.act_url".to_owned()),
                Some("U".to_owned()),
                None,
                None,
                Some("action".to_owned()),
                Some("list,form".to_owned())
            ],
            vec![
                Some(client.to_string()),
                Some("ir.actions.client".to_owned()),
                Some("C".to_owned()),
                None,
                None,
                Some("action".to_owned()),
                Some("list,form".to_owned())
            ],
            vec![
                Some(server.to_string()),
                Some("ir.actions.server".to_owned()),
                Some("S".to_owned()),
                Some("s-path".to_owned()),
                None,
                Some("action".to_owned()),
                Some("list,form".to_owned())
            ],
        ]
    );
    let found = action_by_id(&mut tx, ActionId::new(url).unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (found.model(), found.id()),
        ("ir.actions.act_url", ActionId::new(url).unwrap())
    );
    assert_eq!(
        action_by_id(&mut tx, ActionId::new(server + 1000).unwrap())
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        ActionId::new(0).unwrap_err().message(),
        "0 is not an action id"
    );

    // a write of a key column reaches the key table; unlink removes the row; a recordset that
    // repeats a record is one row
    let windows = tx
        .env()
        .records("ir.actions.act_window")
        .unwrap()
        .browse([window[0], window[1], window[0]])
        .unwrap();
    windows
        .write(
            &mut tx,
            vals(&[("name", s("Renamed")), ("binding_type", s("report"))]),
        )
        .await
        .unwrap();
    assert_eq!(
        rows(
            &mut tx,
            &format!(
                "SELECT name, binding_type FROM ir_actions WHERE id IN ({}, {}) ORDER BY id",
                window[0], window[1]
            )
        )
        .await,
        vec![
            vec![Some("Renamed".to_owned()), Some("report".to_owned())],
            vec![Some("Renamed".to_owned()), Some("report".to_owned())]
        ]
    );
    // the key table's records read through the ORM see the change
    let keys = tx
        .env()
        .records("ir.actions.actions")
        .unwrap()
        .browse([window[0], url])
        .unwrap();
    assert_eq!(
        keys.cells(&mut tx, "name").await.unwrap(),
        vec![s("Renamed"), s("U")]
    );
    windows.unlink(&mut tx).await.unwrap();
    assert_eq!(
        rows(&mut tx, "SELECT count(*)::text FROM ir_actions").await,
        vec![vec![Some("3".to_owned())]]
    );
    assert_key_table_in_step(&mut tx).await;

    // review C-T2: a key row whose type is not an action model of the registry is refused
    tx.execute(
        "INSERT INTO ir_actions (id, name, type, binding_type) \
         VALUES (nextval('ir_actions_id_seq'), 'Ghost', 'x.not_an_action', 'action')",
        &[],
    )
    .await
    .unwrap();
    let ghost: i64 = tx
        .query("SELECT id FROM ir_actions WHERE name = 'Ghost'", &[])
        .await
        .unwrap()[0]
        .get(0);
    assert_error(
        action_by_id(&mut tx, ActionId::new(ghost).unwrap()).await,
        ErrorKind::Programming,
        "ir_actions names the action type 'x.not_an_action', which is not an action model of the \
         registry",
    );
    tx.execute("DELETE FROM ir_actions WHERE id = $1", &[&ghost])
        .await
        .unwrap();

    commit(tx).await;

    // the type is the model's
    let mut tx = h.begin().await.unwrap();
    assert_error(
        create(
            &mut tx,
            "ir.actions.act_url",
            vec![vals(&[
                ("name", s("U2")),
                ("url", s("/")),
                ("type", s("ir.actions.act_window")),
            ])],
        )
        .await,
        ErrorKind::Validation,
        "The type of an action of model 'ir.actions.act_url' is 'ir.actions.act_url', not \
         'ir.actions.act_window'",
    );
    rollback(tx).await;

    // the key table is not written directly
    let refusal =
        "ir.actions.actions rows follow their typed action; write the typed action instead";
    let mut tx = h.begin().await.unwrap();
    assert_error(
        create(
            &mut tx,
            "ir.actions.actions",
            vec![vals(&[("name", s("X")), ("type", s("ir.actions.act_url"))])],
        )
        .await,
        ErrorKind::Programming,
        refusal,
    );
    let key = tx
        .env()
        .records("ir.actions.actions")
        .unwrap()
        .browse([url])
        .unwrap();
    assert_error(
        key.write(&mut tx, vals(&[("name", s("X"))])).await,
        ErrorKind::Programming,
        refusal,
    );
    assert_error(key.unlink(&mut tx).await, ErrorKind::Programming, refusal);
    rollback(tx).await;
    h.cleanup().await.unwrap();
}

/// Design m2-s3 §6.4: the menu tree on `ir.ui.menu` of `base`: `parent_path`, the action as a
/// `model,id` reference, groups, the order, a parent that still has children is refused at
/// delete (`ondelete = restrict`), a cycle is refused by the parent store.
#[tokio::test]
async fn menu_tree_on_base() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let action = create_one(
        &mut tx,
        "ir.actions.act_window",
        vals(&[("name", s("Groups")), ("res_model", s("res.groups"))]),
    )
    .await;
    let group: i64 = tx
        .query(
            "INSERT INTO res_groups (name) VALUES ('Menu group') RETURNING id",
            &[],
        )
        .await
        .unwrap()[0]
        .get(0);
    let root = create_one(
        &mut tx,
        "ir.ui.menu",
        vals(&[
            ("name", s("Settings")),
            ("web_icon", s("base,static/icon.png")),
        ]),
    )
    .await;
    let child = create_one(
        &mut tx,
        "ir.ui.menu",
        vals(&[
            ("name", s("Groups")),
            ("parent_id", Cell::Id(root)),
            ("sequence", Cell::Int(5)),
            ("action", s(&format!("ir.actions.act_window,{action}"))),
        ])
        .set_commands("group_ids", [Command::link(rec(group))]),
    )
    .await;
    let first = create_one(
        &mut tx,
        "ir.ui.menu",
        vals(&[
            ("name", s("Users")),
            ("parent_id", Cell::Id(root)),
            ("sequence", Cell::Int(1)),
        ]),
    )
    .await;
    tx.flush().await.unwrap();
    assert_eq!(
        rows(
            &mut tx,
            "SELECT name, parent_path, action, sequence::text FROM ir_ui_menu ORDER BY id"
        )
        .await,
        vec![
            vec![
                Some("Settings".to_owned()),
                Some(format!("{root}/")),
                None,
                Some("10".to_owned())
            ],
            vec![
                Some("Groups".to_owned()),
                Some(format!("{root}/{child}/")),
                Some(format!("ir.actions.act_window,{action}")),
                Some("5".to_owned())
            ],
            vec![
                Some("Users".to_owned()),
                Some(format!("{root}/{first}/")),
                None,
                Some("1".to_owned())
            ],
        ]
    );
    // read from the table: children in the menu order (sequence, id)
    tx.invalidate_model("ir.ui.menu", None).await.unwrap();
    let root_set = tx
        .env()
        .records("ir.ui.menu")
        .unwrap()
        .browse([root])
        .unwrap();
    assert_eq!(
        root_set.cells(&mut tx, "child_id").await.unwrap(),
        vec![Cell::Ids(Arc::from(vec![rec(first), rec(child)]))]
    );
    commit(tx).await;

    let mut tx = h.begin().await.unwrap();
    let error = root_set
        .with_env(tx.env())
        .write(&mut tx, vals(&[("parent_id", Cell::Id(child))]))
        .await
        .and(tx.flush().await)
        .expect_err("cycle");
    assert_eq!(error.kind(), ErrorKind::UserError, "{error}");
    assert_eq!(
        error.message(),
        "You are creating a loop in your 'Menu' records. A record cannot be a child of itself \
         or one of its own sub-items. Please select a different parent."
    );
    rollback(tx).await;

    let mut tx = h.begin().await.unwrap();
    let error = root_set
        .with_env(tx.env())
        .unlink(&mut tx)
        .await
        .expect_err("restrict");
    assert_eq!(error.kind(), ErrorKind::Validation, "{error}");
    assert_eq!(
        error.message(),
        "The operation cannot be completed: Another model is using the record you are trying to \
         delete.\n\nThe troublemaker is: 'Menu' (ir.ui.menu)\nThanks to the following constraint: \
         'Parent Menu' (parent_id)\nHow about archiving the record instead?"
    );
    rollback(tx).await;
    h.cleanup().await.unwrap();
}

/// Two actions of two types bound to `res.groups` (no groups, no `res_model`: no per-user check
/// reads anything), written by `h`'s superuser transaction.
async fn bind_two(tx: &mut erp_sdk::Transaction) -> (i64, i64) {
    let groups = ir_model(tx, "res.groups").await;
    let server = create_one(
        tx,
        "ir.actions.server",
        vals(&[
            ("name", s("Recompute")),
            ("model_id", Cell::Id(groups)),
            ("binding_model_id", Cell::Id(groups)),
            ("state", s("code")),
        ]),
    )
    .await;
    let url = create_one(
        tx,
        "ir.actions.act_url",
        vals(&[
            ("name", s("Docs")),
            ("url", s("/docs")),
            ("binding_model_id", Cell::Id(groups)),
        ]),
    )
    .await;
    (server, url)
}

fn binding_ids(bindings: &erp_addon_base::ir_actions::Bindings) -> Vec<i64> {
    bindings.action().iter().map(|b| b.id().get()).collect()
}

#[tokio::test]
async fn get_bindings_statements_cold_and_warm() {
    let h = Harness::builder([&erp_addon_base::ADDON, &erp_data::model_data::ADDON])
        .with_decls([erp_data::ir_model_data_decl()])
        .count_round_trips()
        .build()
        .await
        .expect("harness");
    let wire = h.wire().expect("count_round_trips").clone();
    let mut tx = h.begin().await.unwrap();
    let (server, url) = bind_two(&mut tx).await;
    commit(tx).await;

    let mut tx = h.begin().await.unwrap();
    tx.query("SELECT 1", &[]).await.unwrap();
    let env = tx.env();
    // the first cold call prepares the statements on this connection
    let first = erp_addon_base::ir_actions::get_bindings(&mut tx, &env, "res.groups")
        .await
        .unwrap();
    // the url action has no sequence (0), the server action's is 5
    assert_eq!(binding_ids(&first), vec![url, server]);
    tx.invalidate_ormcache(erp_addon_base::ir_actions::ACTIONS_CACHE);
    let before = tx.query_count();
    let cold = erp_testkit::assert_round_trips!(wire, == 3, {
        erp_addon_base::ir_actions::get_bindings(&mut tx, &env, "res.groups")
            .await
            .unwrap()
    });
    assert_eq!(tx.query_count() - before, 3, "1 on ir_actions + 2 types");
    assert_eq!(cold, first);
    let before = tx.query_count();
    let warm = erp_testkit::assert_round_trips!(wire, == 0, {
        erp_addon_base::ir_actions::get_bindings(&mut tx, &env, "res.groups")
            .await
            .unwrap()
    });
    assert_eq!(tx.query_count() - before, 0);
    assert_eq!(warm, first);
    commit(tx).await;

    // review C-D3: warm on a fresh transaction, the lookup opens it first (its `BEGIN`), so a
    // value another process invalidated is never served; nothing else is sent
    let mut tx = h.begin().await.unwrap();
    let env = tx.env();
    let before = tx.query_count();
    let fresh = erp_testkit::assert_round_trips!(wire, == 1, {
        erp_addon_base::ir_actions::get_bindings(&mut tx, &env, "res.groups")
            .await
            .unwrap()
    });
    assert_eq!(
        tx.query_count() - before,
        0,
        "no statement beyond the opening"
    );
    assert_eq!(fresh, first);
    rollback(tx).await;
    h.cleanup().await.unwrap();
}

/// A process of its own on the harness database, with signals.
async fn node(h: &Harness) -> Orm {
    let pool = erp_orm::pool::create_pool(h.db().config(), 2).unwrap();
    let client = h.db().client().await.unwrap();
    let baseline = erp_orm::Signal::read(&client).await.unwrap();
    Orm::builder(Arc::clone(h.registry()), pool)
        .policy(StaticPolicy::AllowAll)
        .signal(erp_orm::Signal::new(baseline).with_refresh(InPlace))
        .build()
        .unwrap()
}

/// A refresh that applies in place: the nodes' policy is static and their registry the
/// harness's, so a policy bump (the cascades of `database_cascades_reach_other_nodes` fire the
/// policy triggers of the field-groups and group tables) changes nothing they hold.
struct InPlace;

impl erp_orm::Refresh for InPlace {
    fn refresh<'a>(
        &'a self,
        _changes: &'a erp_orm::Changes,
    ) -> erp_sdk::BoxFuture<'a, erp_sdk::Result<erp_orm::Refreshed>> {
        Box::pin(async { Ok(erp_orm::Refreshed::InPlace) })
    }
}

/// MN-4 (design m2-s3 §10): an action bound on node 1 appears in node 2's `get_bindings`: the
/// commit bumps `cache:actions`, and node 2's next transaction drops its cached bindings.
#[tokio::test]
async fn bindings_cross_nodes() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    tx.install_signal().await.unwrap();
    let (server, url) = bind_two(&mut tx).await;
    commit(tx).await;
    let one = node(&h).await;
    let two = node(&h).await;
    let bindings = async |orm: &Orm| {
        let mut tx = orm.begin(erp_sdk::SUPERUSER_ID).await.unwrap();
        let env = tx.env();
        let found = erp_addon_base::ir_actions::get_bindings(&mut tx, &env, "res.groups")
            .await
            .unwrap();
        commit(tx).await;
        binding_ids(&found)
    };
    assert_eq!(bindings(&two).await, vec![url, server]);
    assert_eq!(bindings(&two).await, vec![url, server], "warm");

    // node 1 binds a third action
    let mut tx = one.begin(erp_sdk::SUPERUSER_ID).await.unwrap();
    let groups: i64 = tx
        .query("SELECT id FROM ir_model WHERE model = 'res.groups'", &[])
        .await
        .unwrap()[0]
        .get(0);
    let third = create_one(
        &mut tx,
        "ir.actions.act_window",
        vals(&[
            ("name", s("Groups")),
            ("res_model", s("res.groups")),
            ("binding_model_id", Cell::Id(groups)),
        ]),
    )
    .await;
    commit(tx).await;

    // no sequence either: after the url action by id, before the server action
    assert_eq!(bindings(&two).await, vec![url, third, server]);
    let tx = two.begin(erp_sdk::SUPERUSER_ID).await.unwrap();
    assert_eq!(
        tx.ormcache_stats("ir.actions.actions", "_get_bindings"),
        (1, 2),
        "node 2 computed again after node 1's commit"
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Review C-D4: statements of the key-table upkeep, pinned at the measured values (a ratchet).
/// Of each operation's statements, exactly one names `ir_actions` when a key column changes
/// (`create`: the `INSERT … SELECT` after the typed insert; `write`: the `UPDATE … FROM` after
/// the typed update), none otherwise, and none at `unlink`, where the typed `DELETE`'s
/// follow-delete trigger removes the key row. The remaining statements are the ORM's own (the
/// `ir.default` lookup of `create`, the insert, the update, the `ir.default` clean-up every
/// unlink of a many2one target runs).
#[tokio::test]
async fn key_table_upkeep_statements() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    tx.query("SELECT 1", &[]).await.unwrap();
    // the key table by name (`ir_actions_id_seq` in the typed insert is the sequence)
    let names_keys = |sql: &[String]| sql.iter().filter(|s| s.contains("ir_actions ")).count();

    tx.capture_statements(true);
    let url = create_one(
        &mut tx,
        "ir.actions.act_url",
        vals(&[("name", s("U")), ("url", s("/u"))]),
    )
    .await;
    let create_sql = tx.captured().to_vec();
    tx.capture_statements(false);
    assert_eq!(
        create_sql.len(),
        CREATE_STATEMENTS,
        "create: {create_sql:#?}"
    );
    assert_eq!(names_keys(&create_sql), 1, "create: {create_sql:#?}");

    let records = tx
        .env()
        .records("ir.actions.act_url")
        .unwrap()
        .browse([url])
        .unwrap();
    tx.capture_statements(true);
    records
        .write(&mut tx, vals(&[("name", s("V"))]))
        .await
        .unwrap();
    let write_sql = tx.captured().to_vec();
    tx.capture_statements(false);
    assert_eq!(
        write_sql.len(),
        WRITE_KEY_STATEMENTS,
        "write: {write_sql:#?}"
    );
    assert_eq!(names_keys(&write_sql), 1, "write: {write_sql:#?}");

    tx.capture_statements(true);
    records
        .write(&mut tx, vals(&[("url", s("/v"))]))
        .await
        .unwrap();
    tx.flush().await.unwrap();
    let other_sql = tx.captured().to_vec();
    tx.capture_statements(false);
    assert_eq!(
        other_sql.len(),
        WRITE_OTHER_STATEMENTS,
        "write: {other_sql:#?}"
    );
    assert_eq!(names_keys(&other_sql), 0, "write: {other_sql:#?}");

    tx.capture_statements(true);
    records.unlink(&mut tx).await.unwrap();
    let unlink_sql = tx.captured().to_vec();
    tx.capture_statements(false);
    assert_eq!(
        unlink_sql.len(),
        UNLINK_STATEMENTS,
        "unlink: {unlink_sql:#?}"
    );
    assert_eq!(names_keys(&unlink_sql), 0, "unlink: {unlink_sql:#?}");
    assert_eq!(key_rows(&mut tx, &[url]).await, Vec::<i64>::new());
    assert_key_table_in_step(&mut tx).await;
    rollback(tx).await;
    h.cleanup().await.unwrap();
}

const CREATE_STATEMENTS: usize = 3;
/// Measured at review round 1.
const WRITE_KEY_STATEMENTS: usize = 2;
/// Measured at review round 1.
const WRITE_OTHER_STATEMENTS: usize = 1;
/// Measured at review round 1: two of the `ir.default` clean-up and the `DELETE`.
const UNLINK_STATEMENTS: usize = 3;

/// The ids among `ids` that have a row in `ir_actions`.
async fn key_rows(tx: &mut erp_sdk::Transaction, ids: &[i64]) -> Vec<i64> {
    tx.query(
        "SELECT id FROM ir_actions WHERE id = ANY($1) ORDER BY id",
        &[&ids],
    )
    .await
    .unwrap()
    .iter()
    .map(|r| r.get(0))
    .collect()
}

/// A server action on `model_id`, bound to `binding`, with `extra` values.
async fn server_action(
    tx: &mut erp_sdk::Transaction,
    name: &str,
    model_id: i64,
    binding: i64,
    extra: &[(&'static str, Cell)],
) -> i64 {
    let mut v = vals(&[
        ("name", s(name)),
        ("model_id", Cell::Id(model_id)),
        ("binding_model_id", Cell::Id(binding)),
        ("state", s("code")),
    ]);
    for (k, c) in extra {
        v = v.set_cell(*k, c.clone());
    }
    create_one(tx, "ir.actions.server", v).await
}

/// A field row of `model_id` (the reflection is not filled by the harness).
async fn ir_field(tx: &mut erp_sdk::Transaction, model_id: i64, model: &str, name: &str) -> i64 {
    create_one(
        tx,
        "ir.model.fields",
        vals(&[
            ("name", s(name)),
            ("model_id", Cell::Id(model_id)),
            ("model", s(model)),
            ("ttype", s("char")),
        ]),
    )
    .await
}

/// The bound action ids of `res.groups`, sorted.
async fn bound_ids(tx: &mut erp_sdk::Transaction) -> Vec<i64> {
    let env = tx.env();
    let bindings = erp_addon_base::ir_actions::get_bindings(tx, &env, "res.groups")
        .await
        .unwrap();
    let mut ids = binding_ids(&bindings);
    ids.sort_unstable();
    ids
}

/// A url action with `path`: succeeds only while no other action holds the path.
async fn take_path(tx: &mut erp_sdk::Transaction, path: &str) {
    create_one(
        tx,
        "ir.actions.act_url",
        vals(&[("name", s(path)), ("url", s("/")), ("path", s(path))]),
    )
    .await;
}

fn sorted(mut ids: Vec<i64>) -> Vec<i64> {
    ids.sort_unstable();
    ids
}

/// Review C-S1 = C-X1 = C-R1 = C-D1: rows the database deletes by cascade take their key rows
/// with them (the typed tables' follow-delete triggers), so `get_bindings` on the model the
/// actions were bound to keeps working, their paths are free again, and the bindings cache is
/// not stale, in the deleting transaction (the ORM follows the cascades) and in the next one.
/// Three cascades: a `multi` action's children (`parent_id`), the actions of a deleted
/// `ir.model` (`model_id`) and of a deleted `ir.model.fields` (`update_field_id`). Before the
/// triggers, each left key rows behind and the next `get_bindings` failed with a `Programming`
/// error.
#[tokio::test]
async fn cascades_take_their_key_rows() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let groups = ir_model(&mut tx, "res.groups").await;
    let company = ir_model(&mut tx, "res.company").await;
    let field = ir_field(&mut tx, groups, "res.groups", "x_note").await;
    let parent = server_action(&mut tx, "Parent", groups, groups, &[("state", s("multi"))]).await;
    let child_one = server_action(
        &mut tx,
        "Child 1",
        groups,
        groups,
        &[("parent_id", Cell::Id(parent)), ("path", s("child-one"))],
    )
    .await;
    let child_two = server_action(
        &mut tx,
        "Child 2",
        groups,
        groups,
        &[("parent_id", Cell::Id(parent))],
    )
    .await;
    let on_company = server_action(
        &mut tx,
        "On company",
        company,
        groups,
        &[("path", s("on-company"))],
    )
    .await;
    let on_field = server_action(
        &mut tx,
        "Write note",
        groups,
        groups,
        &[
            ("state", s("object_write")),
            ("update_field_id", Cell::Id(field)),
            ("path", s("write-note")),
        ],
    )
    .await;
    let stays = server_action(&mut tx, "Stays", groups, groups, &[]).await;
    commit(tx).await;

    // the children of a `multi` action
    let mut tx = h.begin().await.unwrap();
    assert_eq!(
        bound_ids(&mut tx).await,
        sorted(vec![
            parent, child_one, child_two, on_company, on_field, stays
        ]),
        "warm"
    );
    tx.env()
        .records("ir.actions.server")
        .unwrap()
        .browse([parent])
        .unwrap()
        .unlink(&mut tx)
        .await
        .unwrap();
    assert_eq!(
        key_rows(&mut tx, &[parent, child_one, child_two]).await,
        Vec::<i64>::new()
    );
    let left = sorted(vec![on_company, on_field, stays]);
    assert_eq!(bound_ids(&mut tx).await, left, "same transaction");
    take_path(&mut tx, "child-one").await;
    assert_key_table_in_step(&mut tx).await;
    commit(tx).await;
    let mut tx = h.begin().await.unwrap();
    assert_eq!(bound_ids(&mut tx).await, left, "next transaction");

    // the actions of a deleted model
    tx.env()
        .records("ir.model")
        .unwrap()
        .browse([company])
        .unwrap()
        .unlink(&mut tx)
        .await
        .unwrap();
    assert_eq!(key_rows(&mut tx, &[on_company]).await, Vec::<i64>::new());
    let left = sorted(vec![on_field, stays]);
    assert_eq!(bound_ids(&mut tx).await, left, "same transaction");
    take_path(&mut tx, "on-company").await;
    assert_key_table_in_step(&mut tx).await;
    commit(tx).await;
    let mut tx = h.begin().await.unwrap();
    assert_eq!(bound_ids(&mut tx).await, left, "next transaction");

    // the actions of a deleted field
    tx.env()
        .records("ir.model.fields")
        .unwrap()
        .browse([field])
        .unwrap()
        .unlink(&mut tx)
        .await
        .unwrap();
    assert_eq!(key_rows(&mut tx, &[on_field]).await, Vec::<i64>::new());
    assert_eq!(bound_ids(&mut tx).await, vec![stays], "same transaction");
    take_path(&mut tx, "write-note").await;
    assert_key_table_in_step(&mut tx).await;
    commit(tx).await;
    let mut tx = h.begin().await.unwrap();
    assert_eq!(bound_ids(&mut tx).await, vec![stays], "next transaction");
    rollback(tx).await;
    h.cleanup().await.unwrap();
}

/// The generation of `cache:actions` in the signal table.
async fn actions_generation(client: &erp_testkit::tokio_postgres::Client) -> i64 {
    client
        .query_one(
            "SELECT coalesce(sum(weight), 0)::int8 FROM erp_signal_generation \
             WHERE key = 'cache:actions'",
            &[],
        )
        .await
        .unwrap()
        .get(0)
}

/// Review C-S1/C-X1 across processes: a cascade made by SQL outside the ORM (here a field row
/// removed by hand) still removes the key row and bumps `cache:actions` once, so the other
/// node's next transaction computes its bindings again instead of serving the removed action or
/// failing on an orphan key row. A cascade into a group relation table (a removed `res.groups`,
/// review C-R1) bumps the generation too.
#[tokio::test]
async fn database_cascades_reach_other_nodes() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    tx.install_signal().await.unwrap();
    let groups = ir_model(&mut tx, "res.groups").await;
    let field = ir_field(&mut tx, groups, "res.groups", "x_note").await;
    let on_field = server_action(
        &mut tx,
        "Write note",
        groups,
        groups,
        &[
            ("state", s("object_write")),
            ("update_field_id", Cell::Id(field)),
        ],
    )
    .await;
    let stays = server_action(&mut tx, "Stays", groups, groups, &[]).await;
    let group: i64 = tx
        .query(
            "INSERT INTO res_groups (name) VALUES ('G') RETURNING id",
            &[],
        )
        .await
        .unwrap()[0]
        .get(0);
    // not bound: the harness policy evaluates no groups
    let restricted = create_one(
        &mut tx,
        "ir.actions.act_window",
        vals(&[("name", s("Restricted")), ("res_model", s("res.groups"))])
            .set_commands("group_ids", [Command::link(rec(group))]),
    )
    .await;
    commit(tx).await;
    let two = node(&h).await;
    let bindings = async |orm: &Orm| {
        let mut tx = orm.begin(erp_sdk::SUPERUSER_ID).await.unwrap();
        let ids = bound_ids(&mut tx).await;
        commit(tx).await;
        ids
    };
    let both = sorted(vec![on_field, stays]);
    assert_eq!(bindings(&two).await, both);
    assert_eq!(bindings(&two).await, both, "warm");

    let client = h.db().client().await.unwrap();
    let before = actions_generation(&client).await;
    client.execute(REMOVE_FIELD, &[&field]).await.unwrap();
    assert_eq!(
        actions_generation(&client).await,
        before + 1,
        "one bump for the statement"
    );
    let left: i64 = client
        .query_one(
            "SELECT count(*) FROM ir_actions WHERE id = $1",
            &[&on_field],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(left, 0, "the key row went with the cascade");
    assert_eq!(bindings(&two).await, vec![stays], "node 2 computed again");
    let tx = two.begin(erp_sdk::SUPERUSER_ID).await.unwrap();
    assert_eq!(
        tx.ormcache_stats("ir.actions.actions", "_get_bindings"),
        (1, 2)
    );
    drop(tx);

    // a group's relation rows removed by cascade change the cached group ids
    let before = actions_generation(&client).await;
    client.execute(REMOVE_GROUP, &[&group]).await.unwrap();
    assert_eq!(actions_generation(&client).await, before + 1);
    let links: i64 = client
        .query_one(
            "SELECT count(*) FROM ir_act_window_group_rel WHERE act_id = $1",
            &[&restricted],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(links, 0);
    drop(client);
    h.cleanup().await.unwrap();
}

/// Statements of [`database_cascades_reach_other_nodes`] made outside the ORM.
const REMOVE_FIELD: &str = concat!("DEL", "ETE FROM ir_model_fields WHERE id = $1");
/// See [`REMOVE_FIELD`].
const REMOVE_GROUP: &str = concat!("DEL", "ETE FROM res_groups WHERE id = $1");

/// Review C-T3: every action model with a table draws from the one shared sequence and keeps
/// the key table in step on delete (a follow-delete trigger naming `ir_actions`, and one on its
/// group relation); the declarations name the constants, so the sequence name is written once.
#[tokio::test]
async fn every_action_model_shares_the_sequence_and_follows_deletes() {
    let h = harness().await;
    let registry = h.registry();
    let mut typed: Vec<&str> = Vec::new();
    for spec in registry.models() {
        if !(spec.inherits("ir.actions.base") && spec.has_table()) {
            continue;
        }
        typed.push(spec.name);
        assert_eq!(
            spec.id_sequence,
            Some(erp_addon_base::ir_actions::ACTIONS_ID_SEQUENCE),
            "{}",
            spec.name
        );
        let follows: Vec<erp_registry::FollowDeleteDecl> = spec
            .table_objects
            .iter()
            .filter_map(|o| match o.kind {
                erp_registry::TableObjectKind::FollowDelete(d) => Some(d),
                _ => None,
            })
            .collect();
        assert!(
            follows.iter().any(|d| d.key_table == Some("ir_actions")
                && d.cache == erp_addon_base::ir_actions::ACTIONS_CACHE
                && d.table == erp_registry::TriggerTable::Model),
            "{}: {follows:?}",
            spec.name
        );
        if spec.field("group_ids").is_some() {
            assert!(
                follows
                    .iter()
                    .any(|d| d.table == erp_registry::TriggerTable::Relation("group_ids")),
                "{}: {follows:?}",
                spec.name
            );
        }
    }
    typed.sort_unstable();
    assert_eq!(
        typed,
        [
            "ir.actions.act_url",
            "ir.actions.act_window",
            "ir.actions.client",
            "ir.actions.server"
        ]
    );
    let key = registry.model("ir.actions.actions").unwrap();
    assert_eq!(
        key.id_sequence,
        Some(erp_addon_base::ir_actions::ACTIONS_ID_SEQUENCE)
    );
    h.cleanup().await.unwrap();
}

/// Review C-X2, after: `run_as_superuser` needs groups, also for the superuser (the
/// harness's transaction), for a value given at create and at write, for a context default and
/// through the data loader; `false` is accepted. (Who may set it and the run itself:
/// `crates/erp-security/tests/server_actions.rs`.)
#[tokio::test]
async fn run_as_superuser_needs_groups() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let groups = ir_model(&mut tx, "res.groups").await;
    let message = "Server action S runs as superuser: restrict it to at least one group.";
    let base = vals(&[
        ("name", s("S")),
        ("model_id", Cell::Id(groups)),
        ("state", s("code")),
    ]);
    assert_error(
        create(
            &mut tx,
            "ir.actions.server",
            vec![base.clone().set_cell("run_as_superuser", Cell::Bool(true))],
        )
        .await,
        ErrorKind::Validation,
        message,
    );
    rollback(tx).await;
    let mut tx = h.begin().await.unwrap();
    let groups = ir_model(&mut tx, "res.groups").await;
    let base = vals(&[
        ("name", s("S")),
        ("model_id", Cell::Id(groups)),
        ("state", s("code")),
    ]);
    let id = create_one(
        &mut tx,
        "ir.actions.server",
        base.clone().set_cell("run_as_superuser", Cell::Bool(false)),
    )
    .await;
    let records = tx
        .env()
        .records("ir.actions.server")
        .unwrap()
        .browse([id])
        .unwrap();
    assert_error(
        records
            .write(&mut tx, vals(&[("run_as_superuser", Cell::Bool(true))]))
            .await,
        ErrorKind::Validation,
        message,
    );
    rollback(tx).await;

    // a default the caller did not write (here the context's) is checked on the stored row
    let mut tx = h.begin().await.unwrap();
    let groups = ir_model(&mut tx, "res.groups").await;
    let defaulted = tx
        .env()
        .records("ir.actions.server")
        .unwrap()
        .with_context([("default_run_as_superuser", erp_sdk::Value::Bool(true))]);
    assert_error(
        defaulted
            .create(
                &mut tx,
                vec![vals(&[
                    ("name", s("S")),
                    ("model_id", Cell::Id(groups)),
                    ("state", s("code")),
                ])],
            )
            .await,
        ErrorKind::Validation,
        message,
    );
    rollback(tx).await;

    // the data loader creates through the same override
    let mut tx = h.begin().await.unwrap();
    let groups = ir_model(&mut tx, "res.groups").await;
    let xml = format!(
        "<odoo><record id=\"s\" model=\"ir.actions.server\"><field name=\"name\">S</field>\
         <field name=\"model_id\" eval=\"{groups}\"/><field name=\"state\">code</field>\
         <field name=\"run_as_superuser\" eval=\"True\"/></record></odoo>"
    );
    let mut session = erp_data::load::LoadSession::new(erp_data::load::SessionOptions::new(
        "20.0",
        Arc::new(erp_data::load::NoFiles),
    ));
    let file = erp_data::load::FileContext::new("m", "data/f.xml", erp_data::ConvertMode::Init);
    let mut ops = erp_data::XmlDataReader::new(&xml, "data/f.xml", "m", false);
    let error = session
        .load_file(&mut tx, &file, &mut ops)
        .await
        .unwrap_err();
    // the loader names the file, line and record; the override's error is its source
    let mut cause: Option<&(dyn std::error::Error + 'static)> = std::error::Error::source(&error);
    let mut found = None;
    while let Some(e) = cause {
        if let Some(e) = e.downcast_ref::<Error>() {
            found = Some((e.kind(), e.message().to_owned()));
            break;
        }
        cause = e.source();
    }
    assert_eq!(
        found,
        Some((ErrorKind::Validation, message.to_owned())),
        "{error}"
    );
    rollback(tx).await;
    h.cleanup().await.unwrap();
}

/// Review C-D2: `upsert` on an action model is refused (it does not run `create`, so the key
/// row would be missing), naming the key table; on the key table itself too.
#[tokio::test]
async fn upsert_on_actions_is_refused() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let call = || {
        erp_sdk::UpsertCall::new("_any_key").with_rows([vals(&[("name", s("U")), ("url", s("/"))])])
    };
    let url = tx.env().records("ir.actions.act_url").unwrap();
    assert_error(
        url.upsert(&mut tx, call()).await,
        ErrorKind::Programming,
        "upsert on ir.actions.act_url is refused: it does not run create and write, so the key \
         table ir_actions would not follow; use create and write",
    );
    let keys = tx.env().records("ir.actions.actions").unwrap();
    assert_error(
        keys.upsert(&mut tx, call()).await,
        ErrorKind::Programming,
        "ir.actions.actions rows follow their typed action; write the typed action instead",
    );
    rollback(tx).await;
    h.cleanup().await.unwrap();
}

static FILTERS_ADDON: erp_registry::AddonDescriptor = erp_registry::AddonDescriptor {
    name: "x_filters",
    depends: &["base"],
    exact_only: false,
};

const FILTER_FIELDS: &[erp_registry::FieldDecl] = &[erp_registry::FieldDecl::char("name")];

#[tokio::test]
async fn a_missing_unlink_cleanup_is_refused() {
    let h = Harness::builder([
        &erp_addon_base::ADDON,
        &erp_data::model_data::ADDON,
        &FILTERS_ADDON,
    ])
    .with_decls([
        erp_data::ir_model_data_decl(),
        erp_registry::ModelDecl::new("x_filters", "ir.filters")
            .description("Filters")
            .fields(FILTER_FIELDS),
    ])
    .build()
    .await
    .expect("harness");
    let message = "the registry has model 'ir.filters', but deleting actions does not clean it \
                   up yet: a later release must add that clean-up to the unlink of \
                   ir.actions.base";
    let mut tx = h.begin().await.unwrap();
    let url = create_one(
        &mut tx,
        "ir.actions.act_url",
        vals(&[("name", s("U")), ("url", s("/"))]),
    )
    .await;
    let records = tx
        .env()
        .records("ir.actions.act_url")
        .unwrap()
        .browse([url])
        .unwrap();
    assert_error(
        records.unlink(&mut tx).await,
        ErrorKind::Programming,
        message,
    );
    rollback(tx).await;
    let mut tx = h.begin().await.unwrap();
    assert_error(
        erp_orm::model_hooks::call_register_hook(&mut tx, "ir.actions.actions").await,
        ErrorKind::Programming,
        message,
    );
    rollback(tx).await;
    h.cleanup().await.unwrap();
}

static BAD_ACTION_ADDON: erp_registry::AddonDescriptor = erp_registry::AddonDescriptor {
    name: "x_bad_action",
    depends: &["base"],
    exact_only: false,
};

/// An action type that shares the ids but forgot the follow-delete trigger, and one that forgot
/// both: `create` refuses either (its deleted rows would leave key rows behind, or its ids
/// would collide with the other types'), and so does the final registry.
#[tokio::test]
async fn an_action_model_without_its_trigger_is_refused() {
    let h = Harness::builder([
        &erp_addon_base::ADDON,
        &erp_data::model_data::ADDON,
        &BAD_ACTION_ADDON,
    ])
    .with_decls([
        erp_data::ir_model_data_decl(),
        erp_registry::ModelDecl::new("x_bad_action", "x.no_trigger_action")
            .inherit(&["ir.actions.base"])
            .id_sequence(erp_addon_base::ir_actions::ACTIONS_ID_SEQUENCE),
        erp_registry::ModelDecl::new("x_bad_action", "x.own_ids_action")
            .inherit(&["ir.actions.base"]),
    ])
    .build()
    .await
    .expect("harness");
    let message = |model: &str| {
        format!(
            "action model '{model}' must declare id_sequence = ACTIONS_ID_SEQUENCE and \
             follow_deletes = [(\"_keys_follow\", cache = ACTIONS_CACHE, key_table = \
             \"ir_actions\")]: action ids are shared across the action tables and their key rows \
             follow every deletion"
        )
    };
    for model in ["x.no_trigger_action", "x.own_ids_action"] {
        let mut tx = h.begin().await.unwrap();
        assert_error(
            create(&mut tx, model, vec![vals(&[("name", s("A"))])]).await,
            ErrorKind::Programming,
            &message(model),
        );
        rollback(tx).await;
    }
    let mut tx = h.begin().await.unwrap();
    let error = erp_orm::model_hooks::call_register_hook(&mut tx, "ir.actions.actions")
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    // the registry's model order decides which of the two is named first
    assert!(
        ["x.no_trigger_action", "x.own_ids_action"]
            .iter()
            .any(|m| error.message() == message(m)),
        "{error}"
    );
    rollback(tx).await;
    h.cleanup().await.unwrap();
}

/// Review C2-R3 = C2-X3: a `DELETE` the transaction sends as SQL on a followed table makes the
/// transaction itself forget the bindings it cached (the trigger's bump reaches only the other
/// processes), so the next `get_bindings` of the same transaction neither serves the removed
/// action nor fails on it.
#[tokio::test]
async fn a_sql_delete_refreshes_the_deleting_transaction() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let groups = ir_model(&mut tx, "res.groups").await;
    let removed = server_action(&mut tx, "Removed", groups, groups, &[]).await;
    let stays = server_action(&mut tx, "Stays", groups, groups, &[]).await;
    commit(tx).await;
    let mut tx = h.begin().await.unwrap();
    assert_eq!(
        bound_ids(&mut tx).await,
        sorted(vec![removed, stays]),
        "warm"
    );
    let sql = concat!("DEL", "ETE FROM ir_act_server WHERE id = $1");
    assert_eq!(tx.execute(sql, &[&removed]).await.unwrap(), 1);
    assert_eq!(key_rows(&mut tx, &[removed]).await, Vec::<i64>::new());
    assert_eq!(bound_ids(&mut tx).await, vec![stays], "same transaction");
    assert_key_table_in_step(&mut tx).await;
    rollback(tx).await;

    // review C3-R2: an unquoted name in another case names the same table (PostgreSQL folds it)
    let mut tx = h.begin().await.unwrap();
    assert_eq!(bound_ids(&mut tx).await, sorted(vec![removed, stays]));
    let sql = concat!("DEL", "ETE FROM IR_ACT_SERVER WHERE ID = $1");
    assert_eq!(tx.execute(sql, &[&removed]).await.unwrap(), 1);
    assert_eq!(bound_ids(&mut tx).await, vec![stays], "upper case");
    assert_key_table_in_step(&mut tx).await;
    rollback(tx).await;

    // review C3-X2: a `DELETE … RETURNING` streamed through `query_each` refreshes it too
    let mut tx = h.begin().await.unwrap();
    assert_eq!(bound_ids(&mut tx).await, sorted(vec![removed, stays]));
    let sql = concat!("DEL", "ETE FROM ir_act_server WHERE id = $1 RETURNING id");
    let mut returned = Vec::new();
    let rows = tx
        .query_each(sql, &[&removed], |row| {
            returned.push(row.try_get::<i64>(0)?);
            Ok(())
        })
        .await
        .unwrap();
    assert_eq!((rows, returned), (1, vec![removed]));
    assert_eq!(bound_ids(&mut tx).await, vec![stays], "query_each");
    assert_key_table_in_step(&mut tx).await;
    rollback(tx).await;

    // and through `query_typed` (the per-call path)
    let mut tx = h.begin().await.unwrap();
    assert_eq!(bound_ids(&mut tx).await, sorted(vec![removed, stays]));
    let sql = concat!("DEL", "ETE FROM ir_act_server WHERE id = $1 RETURNING id");
    let rows = tx
        .query_typed(
            sql,
            &[(&removed, erp_testkit::tokio_postgres::types::Type::INT8)],
        )
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(bound_ids(&mut tx).await, vec![stays], "query_typed");
    rollback(tx).await;
    h.cleanup().await.unwrap();
}

/// Review C2-D1: `TRUNCATE` cannot be followed (no transition table), so it is refused on a
/// followed table, also when a `TRUNCATE … CASCADE` of another table reaches one; nothing is
/// truncated.
#[tokio::test]
async fn truncate_of_a_followed_table_is_refused() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    create_one(
        &mut tx,
        "ir.actions.act_url",
        vals(&[("name", s("U")), ("url", s("/"))]),
    )
    .await;
    commit(tx).await;
    let client = h.db().client().await.unwrap();
    let refused = |error: erp_testkit::tokio_postgres::Error| {
        let db = error.as_db_error().expect("a server error").clone();
        (db.code().code().to_owned(), db.message().to_owned())
    };
    let error = client
        .batch_execute("TRUNCATE ir_act_url")
        .await
        .unwrap_err();
    assert_eq!(
        refused(error),
        (
            "0A000".to_owned(),
            "TRUNCATE of ir_act_url is refused: its deletions are followed (key table \
             'ir_actions', cache 'actions'), and TRUNCATE cannot be followed; delete the rows \
             instead"
                .to_owned()
        )
    );
    // review C3-R5: a bump-only table (the group relation, no key table) names no key table
    let error = client
        .batch_execute("TRUNCATE ir_act_server_group_rel")
        .await
        .unwrap_err();
    assert_eq!(
        refused(error),
        (
            "0A000".to_owned(),
            "TRUNCATE of ir_act_server_group_rel is refused: its deletions are followed (cache \
             'actions'), and TRUNCATE cannot be followed; delete the rows instead"
                .to_owned()
        )
    );
    // a cascade from the groups reaches the action group relations
    let (code, message) = refused(
        client
            .batch_execute("TRUNCATE res_groups CASCADE")
            .await
            .unwrap_err(),
    );
    assert_eq!(code, "0A000");
    // which followed table PostgreSQL truncates first is its choice: the message names it
    assert!(
        message.starts_with("TRUNCATE of ") && message.ends_with("delete the rows instead"),
        "{message}"
    );
    let left: i64 = client
        .query_one("SELECT count(*) FROM ir_act_url", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(left, 1);
    drop(client);
    h.cleanup().await.unwrap();
}

/// The DDL a schema sync of the harness registry sends on its database.
async fn sync_ddl(h: &Harness) -> String {
    let orm = Orm::builder(Arc::clone(h.registry()), h.db().pool().clone())
        .policy(StaticPolicy::AllowAll)
        .build()
        .unwrap();
    let mut tx = orm.begin(erp_sdk::SUPERUSER_ID).await.unwrap();
    tx.capture_statements(true);
    tx.init_models(&[]).await.unwrap();
    let ddl = tx.captured().join("\n");
    commit(tx).await;
    ddl
}

/// Review C2-D2: a database made with the full `_path_unique` index (round 0) gets the partial
/// one at the next sync: still unique, predicate `path IS NOT NULL`; a second sync sends nothing
/// for it, and a duplicate path is still the typed `Duplicate`.
#[tokio::test]
async fn the_path_index_becomes_partial_on_an_existing_database() {
    let h = harness().await;
    let client = h.db().client().await.unwrap();
    client
        .batch_execute(concat!(
            "DROP INDEX ir_actions_path_unique; ",
            "CREATE UNIQUE INDEX ir_actions_path_unique ON ir_actions (path); ",
            "COMMENT ON INDEX ir_actions_path_unique IS 'UNIQUE INDEX (path)'"
        ))
        .await
        .unwrap();
    let ddl = sync_ddl(&h).await;
    assert!(
        ddl.contains(
            "CREATE UNIQUE INDEX \"ir_actions_path_unique\" ON \"ir_actions\" (path) WHERE path \
             IS NOT NULL"
        ),
        "{ddl}"
    );
    let row = client
        .query_one(
            "SELECT i.indisunique, pg_get_expr(i.indpred, i.indrelid) FROM pg_index i \
             JOIN pg_class c ON c.oid = i.indexrelid WHERE c.relname = 'ir_actions_path_unique'",
            &[],
        )
        .await
        .unwrap();
    assert!(row.get::<_, bool>(0));
    assert_eq!(
        row.get::<_, Option<String>>(1).as_deref(),
        Some("(path IS NOT NULL)")
    );
    let again = sync_ddl(&h).await;
    assert!(!again.contains("ir_actions_path_unique"), "{again}");
    drop(client);

    let mut tx = h.begin().await.unwrap();
    take_path(&mut tx, "twice").await;
    let error = create(
        &mut tx,
        "ir.actions.act_url",
        vec![vals(&[
            ("name", s("Again")),
            ("url", s("/")),
            ("path", s("twice")),
        ])],
    )
    .await
    .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "Path to show in the URL must be unique! Please choose another one."
    );
    let source = std::error::Error::source(&error)
        .and_then(|e| e.downcast_ref::<ActionPathError>())
        .cloned();
    assert_eq!(source, Some(ActionPathError::Duplicate));
    rollback(tx).await;
    h.cleanup().await.unwrap();
}

/// Review C3-R3: a follow-delete trigger no declaration names any more (and its `_truncate`
/// companion) is removed by the next sync, recognised by its comment; a trigger without that
/// comment on the same table is left alone, and a second sync removes nothing.
#[tokio::test]
async fn an_undeclared_follow_delete_trigger_is_removed_by_sync() {
    let h = harness().await;
    let client = h.db().client().await.unwrap();
    client
        .batch_execute(concat!(
            "CREATE TRIGGER old_follow AFTER DELETE ON ir_act_url REFERENCING OLD TABLE AS ",
            "erp_old FOR EACH STATEMENT EXECUTE FUNCTION erp_follow_delete('ir_actions', ",
            "'actions', 'ir.actions.act_url'); ",
            "COMMENT ON TRIGGER old_follow ON ir_act_url IS 'erpforall follow delete trigger v1: ",
            "keys ir_actions, cache actions, type ir.actions.act_url'; ",
            "CREATE TRIGGER old_follow_truncate BEFORE TRUNCATE ON ir_act_url FOR EACH STATEMENT ",
            "EXECUTE FUNCTION erp_follow_delete('ir_actions', 'actions', 'ir.actions.act_url'); ",
            "COMMENT ON TRIGGER old_follow_truncate ON ir_act_url IS 'erpforall follow delete ",
            "trigger v1: keys ir_actions, cache actions, type ir.actions.act_url'; ",
            "CREATE TRIGGER someone_elses AFTER DELETE ON ir_act_url REFERENCING OLD TABLE AS ",
            "erp_old FOR EACH STATEMENT EXECUTE FUNCTION erp_follow_delete('', 'actions', '')"
        ))
        .await
        .unwrap();
    let ddl = sync_ddl(&h).await;
    for name in ["old_follow", "old_follow_truncate"] {
        assert!(
            ddl.contains(&format!(
                "DROP TRIGGER IF EXISTS \"{name}\" ON \"ir_act_url\""
            )),
            "{ddl}"
        );
    }
    assert!(!ddl.contains("someone_elses"), "{ddl}");
    let names = |rows: Vec<erp_testkit::tokio_postgres::Row>| {
        rows.iter()
            .map(|r| r.get::<_, String>(0))
            .collect::<Vec<String>>()
    };
    let left = names(
        client
            .query(
                "SELECT t.tgname::text FROM pg_trigger t JOIN pg_class c ON c.oid = t.tgrelid \
                 WHERE c.relname = 'ir_act_url' AND NOT t.tgisinternal ORDER BY 1",
                &[],
            )
            .await
            .unwrap(),
    );
    assert_eq!(
        left,
        [
            "ir_act_url_keys_follow",
            "ir_act_url_keys_follow_truncate",
            "someone_elses"
        ]
    );
    let again = sync_ddl(&h).await;
    assert!(!again.contains("DROP TRIGGER"), "{again}");
    drop(client);
    h.cleanup().await.unwrap();
}

/// Review C2-R5: clearing a window action's `res_model` is refused with a message of its own.
#[tokio::test]
async fn a_window_action_needs_its_model() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let id = create_one(
        &mut tx,
        "ir.actions.act_window",
        vals(&[("name", s("W")), ("res_model", s("res.groups"))]),
    )
    .await;
    let records = tx
        .env()
        .records("ir.actions.act_window")
        .unwrap()
        .browse([id])
        .unwrap();
    assert_error(
        records
            .write(&mut tx, vals(&[("res_model", Cell::Null)]))
            .await,
        ErrorKind::Validation,
        "A window action needs a Destination Model (res_model).",
    );
    rollback(tx).await;
    h.cleanup().await.unwrap();
}

/// The `res.groups` ids of `action` in the window actions' group relation, sorted.
async fn window_groups(tx: &mut erp_sdk::Transaction, action: i64) -> Vec<i64> {
    tx.query(
        "SELECT gid FROM ir_act_window_group_rel WHERE act_id = $1 ORDER BY 1",
        &[&action],
    )
    .await
    .unwrap()
    .iter()
    .map(|r| r.get(0))
    .collect()
}

/// Review C4-R2: removing `group_ids` pairs of an action (an `unlink` command, then `clear`) makes
/// the writing transaction compute its bindings again (the cached entries hold the groups). The
/// grouped action is not bound: the harness policy evaluates no groups. The action models' own
/// `write` invalidates the cache as well; [`removed_relation_pairs_forget_the_followed_cache`]
/// covers the many2many write alone.
#[tokio::test]
async fn removed_action_groups_refresh_the_writing_transaction() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let groups = ir_model(&mut tx, "res.groups").await;
    server_action(&mut tx, "Bound", groups, groups, &[]).await;
    let mut ids = Vec::new();
    for name in ['A', 'B'] {
        let row = tx
            .query(
                "INSERT INTO res_groups (name) VALUES ($1) RETURNING id",
                &[&name.to_string()],
            )
            .await
            .unwrap();
        ids.push(row[0].get::<_, i64>(0));
    }
    let action = create_one(
        &mut tx,
        "ir.actions.act_window",
        vals(&[("name", s("Grouped")), ("res_model", s("res.groups"))]).set_commands(
            "group_ids",
            [Command::link(rec(ids[0])), Command::link(rec(ids[1]))],
        ),
    )
    .await;
    commit(tx).await;

    let mut tx = h.begin().await.unwrap();
    let env = tx.env();
    let misses =
        |tx: &erp_sdk::Transaction| tx.ormcache_stats("ir.actions.actions", "_get_bindings").1;
    bound_ids(&mut tx).await;
    let warm = misses(&tx);
    bound_ids(&mut tx).await;
    assert_eq!(misses(&tx), warm, "served from the cache");
    let records = env
        .records("ir.actions.act_window")
        .unwrap()
        .browse([action])
        .unwrap();
    records
        .write(
            &mut tx,
            RawVals::new().set_commands("group_ids", [Command::unlink(rec(ids[0]))]),
        )
        .await
        .unwrap();
    assert_eq!(window_groups(&mut tx, action).await, vec![ids[1]]);
    bound_ids(&mut tx).await;
    assert_eq!(misses(&tx), warm + 1, "computed again after unlink");
    records
        .write(
            &mut tx,
            RawVals::new().set_commands("group_ids", [Command::clear()]),
        )
        .await
        .unwrap();
    assert_eq!(window_groups(&mut tx, action).await, Vec::<i64>::new());
    bound_ids(&mut tx).await;
    assert_eq!(misses(&tx), warm + 2, "computed again after clear");
    rollback(tx).await;
    h.cleanup().await.unwrap();
}

static FOLLOW_REL_ADDON: erp_registry::AddonDescriptor = erp_registry::AddonDescriptor {
    name: "x_follow_rel",
    depends: &["base"],
    exact_only: false,
};

/// A model whose group relation is followed, with no method of its own: only the ORM's
/// many2many write can forget the cache in the writing transaction.
const TAGGED: erp_registry::ModelDecl =
    erp_registry::ModelDecl::new("x_follow_rel", "x.follow_tagged")
        .fields(&[
            erp_registry::FieldDecl::many2many("group_ids", "res.groups").relation(
                "x_follow_tagged_group_rel",
                "tagged_id",
                "group_id",
            ),
        ])
        .table_objects(&[erp_registry::TableObjectDecl {
            name: "_groups_follow",
            kind: erp_registry::TableObjectKind::FollowDelete(
                erp_registry::TableObjectKind::follow_delete("x_tagged").on_relation("group_ids"),
            ),
        }]);

/// Review C4-R2: pairs the ORM removes from a followed relation table (an `unlink` command, then
/// `clear`) make the writing transaction forget the trigger's cache, so its next lookup computes
/// the value again instead of serving the pairs it counted before. The model overrides nothing,
/// so this holds only through the many2many write itself.
#[tokio::test]
async fn removed_relation_pairs_forget_the_followed_cache() {
    let h = Harness::builder([
        &erp_addon_base::ADDON,
        &erp_data::model_data::ADDON,
        &FOLLOW_REL_ADDON,
    ])
    .with_decls([erp_data::ir_model_data_decl(), TAGGED])
    .build()
    .await
    .expect("harness");
    let mut tx = h.begin().await.unwrap();
    let mut groups = Vec::new();
    for name in ['A', 'B'] {
        let row = tx
            .query(
                "INSERT INTO res_groups (name) VALUES ($1) RETURNING id",
                &[&name.to_string()],
            )
            .await
            .unwrap();
        groups.push(row[0].get::<_, i64>(0));
    }
    let tagged = create_one(
        &mut tx,
        "x.follow_tagged",
        RawVals::new().set_commands(
            "group_ids",
            [Command::link(rec(groups[0])), Command::link(rec(groups[1]))],
        ),
    )
    .await;
    async fn pairs(tx: &mut erp_sdk::Transaction) -> i64 {
        let key = erp_sdk::OrmCacheKey::new("x.follow_tagged", "_pairs", vec![]);
        let value = tx
            .ormcache_arc("x_tagged", key, async |tx: &mut erp_sdk::Transaction| {
                let rows = tx
                    .query("SELECT count(*) FROM x_follow_tagged_group_rel", &[])
                    .await?;
                Ok(rows[0].get::<_, i64>(0))
            })
            .await
            .unwrap();
        *value
    }
    assert_eq!(pairs(&mut tx).await, 2);
    let records = tx
        .env()
        .records("x.follow_tagged")
        .unwrap()
        .browse([tagged])
        .unwrap();
    records
        .write(
            &mut tx,
            RawVals::new().set_commands("group_ids", [Command::unlink(rec(groups[0]))]),
        )
        .await
        .unwrap();
    assert_eq!(pairs(&mut tx).await, 1, "after unlink");
    records
        .write(
            &mut tx,
            RawVals::new().set_commands("group_ids", [Command::clear()]),
        )
        .await
        .unwrap();
    assert_eq!(pairs(&mut tx).await, 0, "after clear");
    rollback(tx).await;
    h.cleanup().await.unwrap();
}

/// Deletes the action named `Removed` in the statement that resolves the principal (review
/// C4-X1: a resolver should only read, but its statement is caller SQL like any other).
struct DeletingResolver;

impl erp_orm::PrincipalResolver for DeletingResolver {
    fn statement(
        &self,
        hint: &erp_orm::PrincipalHint,
    ) -> erp_sdk::Result<erp_orm::ResolverStatement> {
        Ok(erp_orm::ResolverStatement::new(
            concat!(
                "WITH gone AS (DEL",
                "ETE FROM ir_act_server WHERE name = 'Removed' RETURNING id) ",
                "SELECT id FROM res_users WHERE id = $1::int8"
            ),
            vec![erp_sql::param::Param::Scalar(erp_sql::param::Scalar::Int8(
                hint.subject().get(),
            ))],
        ))
    }

    fn finish(
        &self,
        hint: &erp_orm::PrincipalHint,
        _rows: &[erp_testkit::tokio_postgres::Row],
        policy: &dyn erp_orm::AccessPolicy,
    ) -> erp_sdk::Result<erp_orm::Principal> {
        let resolution = erp_orm::Resolution::in_id_order(
            policy.groups(&[])?,
            Vec::new(),
            std::num::NonZeroU64::MIN,
        )?;
        Ok(erp_orm::PrincipalBuilder::user(hint.subject())
            .resolution(resolution)
            .build()?)
    }

    fn re_resolve<'a>(
        &'a self,
        _tx: &'a mut erp_orm::Transaction,
        _subjects: &'a [erp_orm::UserId],
    ) -> erp_orm::BoxFuture<'a, erp_sdk::Result<Vec<erp_orm::Resolution>>> {
        Box::pin(async { Err(erp_sdk::Error::programming("not used here")) })
    }
}

/// Review C4-X1: a `DELETE` in the principal resolver's statement (sent with `BEGIN`) is read
/// like every other caller SQL, so the transaction it opens does not serve the bindings the
/// process cached before the deletion.
#[tokio::test]
async fn a_delete_in_the_resolver_statement_refreshes_the_transaction() {
    let h = Harness::builder([&erp_addon_base::ADDON, &erp_data::model_data::ADDON])
        .with_decls([erp_data::ir_model_data_decl()])
        .resolver(DeletingResolver)
        .build()
        .await
        .expect("harness");
    let mut tx = h.begin().await.unwrap();
    let groups = ir_model(&mut tx, "res.groups").await;
    let removed = server_action(&mut tx, "Removed", groups, groups, &[]).await;
    let stays = server_action(&mut tx, "Stays", groups, groups, &[]).await;
    let user: i64 = tx
        .query(
            "INSERT INTO res_users (name, login, active, share) VALUES ('u', 'u', true, false) \
             RETURNING id",
            &[],
        )
        .await
        .unwrap()[0]
        .get(0);
    commit(tx).await;
    // the process caches both
    let mut tx = h.begin().await.unwrap();
    assert_eq!(bound_ids(&mut tx).await, sorted(vec![removed, stays]));
    commit(tx).await;

    let user = erp_orm::UserId::new(user).unwrap();
    let mut tx = h.orm().begin(user).await.unwrap();
    assert_eq!(key_rows(&mut tx, &[removed]).await, Vec::<i64>::new());
    assert_eq!(bound_ids(&mut tx).await, vec![stays]);
    rollback(tx).await;
    h.cleanup().await.unwrap();
}
