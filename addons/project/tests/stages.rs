// SPDX-License-Identifier: MIT OR Apache-2.0
//! Per-project stages: new projects copy the template stages, a stage stays in its project, and
//! the project counts, the task action and the tag names.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod support;

use std::sync::Arc;

use erp_sdk::prelude::*;
use erp_sdk::{Command, ValueMap};
use support::*;

fn template_rows() -> Vec<(String, i32, bool)> {
    vec![
        ("New".to_owned(), 10, false),
        ("In Progress".to_owned(), 20, false),
        ("Done".to_owned(), 30, true),
        ("Cancelled".to_owned(), 40, true),
    ]
}

#[tokio::test]
async fn a_new_project_gets_its_own_copies_of_the_template_stages() {
    let h = harness().await;
    let mut tx = begin(&h).await;
    let templates = templates(&mut tx).await;
    // a template with a description: the copy keeps it
    stages(&tx)
        .browse([templates[0]])
        .unwrap()
        .write(
            &mut tx,
            RawVals::new().set_cell("description", text("Work not started yet")),
        )
        .await
        .unwrap();
    let website = project(&mut tx, "Website").await;
    assert_eq!(stages_of(&mut tx, website).await, template_rows());
    let copies = stage_ids_of(&mut tx, website).await;
    assert!(
        copies.iter().all(|id| !templates.contains(id)),
        "the project must get copies, not the templates"
    );
    assert_eq!(
        cell(&mut tx, "project.task.type", copies[0], "description").await,
        text("Work not started yet")
    );
    // the templates stay templates
    tx.flush().await.unwrap();
    let left: i64 = tx
        .query(
            "SELECT count(*)::int8 FROM project_task_type WHERE project_id IS NULL",
            &[],
        )
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(left, 4);
    // the project's one2many shows its own stages
    assert_eq!(
        cell(&mut tx, "project.project", website, "type_ids").await,
        Cell::Ids(
            copies
                .iter()
                .map(|id| RecId::from_real(*id).unwrap())
                .collect()
        )
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn a_batch_of_projects_gives_each_project_its_own_stages() {
    let h = harness().await;
    let mut tx = begin(&h).await;
    templates(&mut tx).await;
    let created = projects(&tx)
        .create(
            &mut tx,
            ["Alpha", "Beta", "Gamma"]
                .into_iter()
                .map(|name| RawVals::new().set_cell("name", text(name)))
                .collect(),
        )
        .await
        .unwrap();
    let mut seen = Vec::new();
    for id in created.ids() {
        assert_eq!(
            stages_of(&mut tx, id).await,
            template_rows(),
            "project {id}"
        );
        let own = stage_ids_of(&mut tx, id).await;
        assert!(own.iter().all(|stage| !seen.contains(stage)));
        seen.extend(own);
    }
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn explicit_stages_replace_the_templates_and_no_templates_means_no_stages() {
    let h = harness().await;
    let mut tx = begin(&h).await;
    // no templates yet: the project starts without stages, without an error
    let bare = project(&mut tx, "Bare").await;
    assert!(stages_of(&mut tx, bare).await.is_empty());

    templates(&mut tx).await;
    let custom = create(
        &mut tx,
        "project.project",
        RawVals::new()
            .set_cell("name", text("Custom"))
            .set_commands(
                "type_ids",
                [
                    Command::create(
                        RawVals::new()
                            .set_cell("name", text("Backlog"))
                            .set_cell("sequence", Cell::Int(1)),
                    ),
                    Command::create(
                        RawVals::new()
                            .set_cell("name", text("Shipped"))
                            .set_cell("sequence", Cell::Int(2))
                            .set_cell("fold", Cell::Bool(true)),
                    ),
                ],
            ),
    )
    .await
    .unwrap()
    .ids()[0];
    assert_eq!(
        stages_of(&mut tx, custom).await,
        [
            ("Backlog".to_owned(), 1, false),
            ("Shipped".to_owned(), 2, true)
        ]
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn a_stage_never_moves_to_another_project() {
    let h = harness().await;
    let mut tx = begin(&h).await;
    templates(&mut tx).await;
    let website = project(&mut tx, "Website").await;
    let mobile = project(&mut tx, "Mobile").await;
    let stage = stage_ids_of(&mut tx, website).await[0];
    let second = stage_ids_of(&mut tx, website).await[1];
    for target in [Cell::Id(mobile), Cell::Null] {
        let error = assert_refused(
            try_write(
                &mut tx,
                "project.task.type",
                &[stage],
                RawVals::new().set_cell("project_id", target.clone()),
            )
            .await,
            "project_id",
        );
        assert_eq!(error.kind(), erp_sdk::ErrorKind::Validation);
    }
    assert_eq!(
        cell(&mut tx, "project.task.type", stage, "project_id").await,
        Cell::Id(website)
    );
    // a template cannot be pulled into a project either
    let first_template = tx
        .query(
            "SELECT id::int8 FROM project_task_type WHERE project_id IS NULL ORDER BY id LIMIT 1",
            &[],
        )
        .await
        .unwrap()[0]
        .get::<_, i64>(0);
    assert_refused(
        try_write(
            &mut tx,
            "project.task.type",
            &[first_template],
            RawVals::new().set_cell("project_id", Cell::Id(mobile)),
        )
        .await,
        "project_id",
    );
    // writing the same project, and other fields, is fine
    try_write(
        &mut tx,
        "project.task.type",
        &[stage, second],
        RawVals::new()
            .set_cell("project_id", Cell::Id(website))
            .set_cell("fold", Cell::Bool(true)),
    )
    .await
    .unwrap();
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn deleting_a_project_without_tasks_deletes_its_stages() {
    let h = harness().await;
    let mut tx = begin(&h).await;
    templates(&mut tx).await;
    let website = project(&mut tx, "Website").await;
    projects(&tx)
        .browse([website])
        .unwrap()
        .unlink(&mut tx)
        .await
        .unwrap();
    assert!(stages_of(&mut tx, website).await.is_empty());
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn project_counts_report_all_and_open_tasks() {
    let h = harness().await;
    let mut tx = begin(&h).await;
    templates(&mut tx).await;
    let website = project(&mut tx, "Website").await;
    let mobile = project(&mut tx, "Mobile").await;
    let empty = project(&mut tx, "Empty").await;
    for (name, state) in [
        ("Draft copy", "in_progress"),
        ("Review copy", "changes_requested"),
        ("Approve logo", "approved"),
        ("Publish", "done"),
        ("Drop banner", "cancelled"),
    ] {
        task(
            &mut tx,
            task_vals(name)
                .set_cell("project_id", Cell::Id(website))
                .set_cell("state", text(state)),
        )
        .await;
    }
    task(
        &mut tx,
        task_vals("Store listing").set_cell("project_id", Cell::Id(mobile)),
    )
    .await;
    // an archived task is not counted
    task(
        &mut tx,
        task_vals("Old idea")
            .set_cell("project_id", Cell::Id(mobile))
            .set_cell("active", Cell::Bool(false)),
    )
    .await;
    tx.flush().await.unwrap();
    let set = projects(&tx).browse([website, mobile, empty]).unwrap();
    assert_eq!(
        set.cells(&mut tx, "task_count").await.unwrap(),
        [Cell::Int(5), Cell::Int(1), Cell::Int(0)]
    );
    assert_eq!(
        set.cells(&mut tx, "open_task_count").await.unwrap(),
        [Cell::Int(3), Cell::Int(1), Cell::Int(0)]
    );
    // a new task is counted on the next read in the same transaction
    task(
        &mut tx,
        task_vals("Analytics").set_cell("project_id", Cell::Id(empty)),
    )
    .await;
    assert_eq!(
        cell(&mut tx, "project.project", empty, "task_count").await,
        Cell::Int(1)
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

fn map(entries: impl IntoIterator<Item = (&'static str, Value)>) -> Value {
    let mut out = ValueMap::new();
    for (key, value) in entries {
        out.insert(key.to_owned(), value);
    }
    Value::Map(Arc::new(out))
}

#[tokio::test]
async fn the_task_button_opens_the_project_tasks_as_a_list() {
    let h = harness().await;
    let mut tx = begin(&h).await;
    let website = project(&mut tx, "Website").await;
    let action: Value = projects(&tx)
        .browse([website])
        .unwrap()
        .call(&mut tx, "action_view_tasks", ())
        .await
        .unwrap();
    assert_eq!(
        action,
        map([
            ("type", Value::str("ir.actions.act_window")),
            ("target", Value::str("current")),
            ("name", Value::str("Website — Tasks")),
            ("res_model", Value::str("project.task")),
            (
                "views",
                Value::list([
                    Value::list([Value::Bool(false), Value::str("list")]),
                    Value::list([Value::Bool(false), Value::str("form")]),
                ]),
            ),
            (
                "domain",
                Value::list([Value::list([
                    Value::str("project_id"),
                    Value::str("="),
                    Value::Int(website),
                ])]),
            ),
            (
                "context",
                map([("default_project_id", Value::Int(website))])
            ),
        ])
    );
    // the action is for one project
    let other = project(&mut tx, "Other").await;
    let several = projects(&tx).browse([website, other]).unwrap();
    let refused: Result<Value> = several.call(&mut tx, "action_view_tasks", ()).await;
    assert!(refused.is_err(), "{refused:?}");
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn tag_names_are_unique() {
    let h = harness().await;
    let mut tx = begin(&h).await;
    create(
        &mut tx,
        "project.tags",
        RawVals::new().set_cell("name", text("Urgent")),
    )
    .await
    .unwrap();
    let duplicate = tx
        .savepoint(async |tx| {
            create(
                tx,
                "project.tags",
                RawVals::new().set_cell("name", text("Urgent")),
            )
            .await?;
            tx.flush().await
        })
        .await;
    assert_refused(duplicate, "A tag with this name already exists.");
    // the transaction stays usable and another name is fine
    create(
        &mut tx,
        "project.tags",
        RawVals::new()
            .set_cell("name", text("Blocked"))
            .set_cell("color", Cell::Int(3)),
    )
    .await
    .unwrap();
    tx.flush().await.unwrap();
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// Projects created with `type_ids` values that give no stage of their own: no commands at all, an
/// empty list (what a form sends for a stage page left alone), or only clearing commands.
#[tokio::test]
async fn stage_values_without_stages_still_give_the_templates() {
    let h = harness().await;
    let mut tx = begin(&h).await;
    templates(&mut tx).await;
    let named = |name: &str| RawVals::new().set_cell("name", text(name));
    let created = projects(&tx)
        .create(
            &mut tx,
            vec![
                named("Empty list").set_commands("type_ids", []),
                named("Cleared").set_commands("type_ids", [Command::Clear]),
                named("Set empty").set_commands("type_ids", [Command::Set(vec![])]),
                named("Own stage").set_commands(
                    "type_ids",
                    [Command::create(
                        RawVals::new()
                            .set_cell("name", text("Only"))
                            .set_cell("sequence", Cell::Int(5)),
                    )],
                ),
                named("Nothing"),
            ],
        )
        .await
        .unwrap()
        .ids();
    for (index, id) in created.iter().enumerate() {
        let expected = if index == 3 {
            vec![("Only".to_owned(), 5, false)]
        } else {
            template_rows()
        };
        assert_eq!(stages_of(&mut tx, *id).await, expected, "project {index}");
    }
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// The web form's save sends every field it holds, an untouched one2many as `[]`.
#[tokio::test]
async fn a_project_saved_from_the_form_gets_the_templates() {
    let h = harness().await;
    let mut tx = begin(&h).await;
    templates(&mut tx).await;
    let saved = projects(&tx)
        .web_call(
            &mut tx,
            "web_save",
            &map([
                (
                    "vals",
                    map([
                        ("name", Value::str("From the form")),
                        ("type_ids", Value::list([])),
                        ("tag_ids", Value::list([])),
                        ("partner_id", Value::False),
                        ("company_id", Value::False),
                    ]),
                ),
                ("specification", map([])),
            ]),
        )
        .await
        .unwrap();
    let Value::List(rows) = &saved else {
        panic!("web_save returns a list: {saved:?}");
    };
    let Ok(Value::Int(id)) = rows[0].dict().unwrap().get("id") else {
        panic!("web_save row without an id: {saved:?}");
    };
    assert_eq!(stages_of(&mut tx, id).await, template_rows());
    // the dynamic values path of JSON-2 calls and imports
    let set = projects(&tx);
    let values = std::collections::BTreeMap::from([
        ("name".to_owned(), Value::str("From JSON")),
        ("type_ids".to_owned(), Value::list([])),
    ]);
    let id = set.create_values(&mut tx, &[values]).await.unwrap().ids()[0];
    assert_eq!(stages_of(&mut tx, id).await, template_rows());
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn a_copied_project_copies_its_own_stages() {
    let h = harness().await;
    let mut tx = begin(&h).await;
    templates(&mut tx).await;
    let website = project(&mut tx, "Website").await;
    let own = stage_ids_of(&mut tx, website).await;
    // the project's stages now differ from the templates
    try_write(
        &mut tx,
        "project.task.type",
        &[own[1]],
        RawVals::new()
            .set_cell("name", text("Building"))
            .set_cell("description", text("Work in progress")),
    )
    .await
    .unwrap();
    stage(&mut tx, "Review", 25, false, Some(website)).await;
    let before = stages_of(&mut tx, website).await;
    let copy: Records = projects(&tx)
        .browse([website])
        .unwrap()
        .call(&mut tx, "copy", (None::<ValueMap>,))
        .await
        .unwrap();
    let copy = copy.ids()[0];
    assert_ne!(copy, website);
    assert_eq!(stages_of(&mut tx, copy).await, before);
    assert_eq!(
        stages_of(&mut tx, website).await,
        before,
        "the original keeps its stages"
    );
    let copies = stage_ids_of(&mut tx, copy).await;
    assert!(
        copies.iter().all(|id| !own.contains(id)),
        "new stage records"
    );
    assert_eq!(
        cell(&mut tx, "project.task.type", copies[1], "description").await,
        text("Work in progress")
    );
    // the templates are untouched
    tx.flush().await.unwrap();
    let templates: i64 = tx
        .query(
            "SELECT count(*) FROM project_task_type WHERE project_id IS NULL",
            &[],
        )
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(templates, 4);
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn a_project_with_tasks_and_a_stage_in_use_are_not_deleted() {
    let h = harness().await;
    let mut tx = begin(&h).await;
    templates(&mut tx).await;
    let website = project(&mut tx, "Website").await;
    let own = stage_ids_of(&mut tx, website).await;
    let id = task(
        &mut tx,
        task_vals("Keep me").set_cell("project_id", Cell::Id(website)),
    )
    .await;
    for (model, record) in [("project.project", website), ("project.task.type", own[0])] {
        let set = tx.env().records(model).unwrap().browse([record]).unwrap();
        let outcome = tx
            .savepoint(async move |tx| {
                set.unlink(tx).await?;
                tx.flush().await
            })
            .await;
        assert!(
            outcome.is_err(),
            "deleting {model} {record} must be refused"
        );
    }
    assert_eq!(
        cell(&mut tx, "project.task", id, "stage_id").await,
        Cell::Id(own[0])
    );
    // an unused stage goes
    stages(&tx)
        .browse([own[3]])
        .unwrap()
        .unlink(&mut tx)
        .await
        .unwrap();
    assert_eq!(stage_ids_of(&mut tx, website).await.len(), 3);
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn counts_follow_a_task_to_another_project() {
    let h = harness().await;
    let mut tx = begin(&h).await;
    templates(&mut tx).await;
    let website = project(&mut tx, "Website").await;
    let mobile = project(&mut tx, "Mobile").await;
    let id = task(
        &mut tx,
        task_vals("Mover").set_cell("project_id", Cell::Id(website)),
    )
    .await;
    let set = projects(&tx).browse([website, mobile]).unwrap();
    assert_eq!(
        set.cells(&mut tx, "task_count").await.unwrap(),
        [Cell::Int(1), Cell::Int(0)]
    );
    try_write(
        &mut tx,
        "project.task",
        &[id],
        RawVals::new().set_cell("project_id", Cell::Id(mobile)),
    )
    .await
    .unwrap();
    assert_eq!(
        set.cells(&mut tx, "task_count").await.unwrap(),
        [Cell::Int(0), Cell::Int(1)]
    );
    assert_eq!(
        set.cells(&mut tx, "open_task_count").await.unwrap(),
        [Cell::Int(0), Cell::Int(1)]
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}
