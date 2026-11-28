// SPDX-License-Identifier: MIT OR Apache-2.0
//! Task behaviour: what a new task takes from its parent and project, stage moves inside a
//! project, the state reset, the last stage update, subtask cycles and the form onchanges.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod support;

use std::sync::Arc;

use erp_sdk::ValueMap;
use erp_sdk::prelude::*;
use support::*;

/// A project with the template stages: (project, its stage ids in board order).
async fn board(tx: &mut Transaction, name: &str) -> (i64, Vec<i64>) {
    let id = project(tx, name).await;
    (id, stage_ids_of(tx, id).await)
}

async fn task_cell(tx: &mut Transaction, id: i64, field: &str) -> Cell {
    cell(tx, "project.task", id, field).await
}

#[tokio::test]
async fn a_task_starts_in_the_first_open_stage_of_its_project() {
    let h = harness().await;
    let mut tx = begin(&h).await;
    templates(&mut tx).await;
    let (website, stages) = board(&mut tx, "Website").await;
    // a folded stage with the lowest sequence is not the start: folded stages come last
    let parked = stage(&mut tx, "Parked", 1, true, Some(website)).await;
    let before = DateTime::now();
    let id = task(
        &mut tx,
        task_vals("Draft copy").set_cell("project_id", Cell::Id(website)),
    )
    .await;
    assert_eq!(
        task_cell(&mut tx, id, "stage_id").await,
        Cell::Id(stages[0])
    );
    assert_ne!(stages[0], parked);
    assert_eq!(task_cell(&mut tx, id, "state").await, text("in_progress"));
    let Cell::DateTime(stamped) = task_cell(&mut tx, id, "date_last_stage_update").await else {
        panic!("a new task records when it entered its stage");
    };
    assert!(stamped >= before, "{stamped:?} < {before:?}");

    // a project whose stages are all folded still gives a stage
    let (archive, _) = board(&mut tx, "Archive").await;
    tx.flush().await.unwrap();
    tx.execute(
        "UPDATE project_task_type SET fold = true WHERE project_id = $1",
        &[&archive],
    )
    .await
    .unwrap();
    tx.invalidate_model("project.task.type", None)
        .await
        .unwrap();
    let archived = task(
        &mut tx,
        task_vals("Old").set_cell("project_id", Cell::Id(archive)),
    )
    .await;
    assert_eq!(
        task_cell(&mut tx, archived, "stage_id").await,
        Cell::Id(stage_ids_of(&mut tx, archive).await[0])
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn explicit_values_and_context_defaults_win() {
    let h = harness().await;
    let mut tx = begin(&h).await;
    templates(&mut tx).await;
    let (website, website_stages) = board(&mut tx, "Website").await;
    let project_customer = partner(&mut tx, "Globex").await;
    let mobile = project_for(&mut tx, "Mobile", project_customer).await;
    let mobile_stages = stage_ids_of(&mut tx, mobile).await;
    let customer = partner(&mut tx, "Acme").await;

    // an explicit stage of the project
    let id = task(
        &mut tx,
        task_vals("Review")
            .set_cell("project_id", Cell::Id(website))
            .set_cell("stage_id", Cell::Id(website_stages[2])),
    )
    .await;
    assert_eq!(
        task_cell(&mut tx, id, "stage_id").await,
        Cell::Id(website_stages[2])
    );

    // the board's quick create passes the column and the project as context defaults; the
    // context customer wins over the project's customer
    let quick = tasks(&tx).with_context([
        ("default_project_id", Value::Int(mobile)),
        ("default_stage_id", Value::Int(mobile_stages[1])),
        ("default_partner_id", Value::Int(customer)),
    ]);
    let id = quick
        .create(&mut tx, vec![task_vals("Store listing")])
        .await
        .unwrap()
        .ids()[0];
    assert_eq!(task_cell(&mut tx, id, "project_id").await, Cell::Id(mobile));
    assert_eq!(
        task_cell(&mut tx, id, "stage_id").await,
        Cell::Id(mobile_stages[1])
    );
    assert_eq!(
        task_cell(&mut tx, id, "partner_id").await,
        Cell::Id(customer)
    );

    // the context project wins over the parent's project
    let parent = task(
        &mut tx,
        task_vals("Parent").set_cell("project_id", Cell::Id(website)),
    )
    .await;
    let id = tasks(&tx)
        .with_context([("default_project_id", Value::Int(mobile))])
        .create(
            &mut tx,
            vec![task_vals("Child").set_cell("parent_id", Cell::Id(parent))],
        )
        .await
        .unwrap()
        .ids()[0];
    assert_eq!(task_cell(&mut tx, id, "project_id").await, Cell::Id(mobile));
    assert_eq!(
        task_cell(&mut tx, id, "stage_id").await,
        Cell::Id(mobile_stages[0])
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn a_subtask_takes_its_project_and_customer_from_its_parent() {
    let h = harness().await;
    let mut tx = begin(&h).await;
    templates(&mut tx).await;
    let acme = partner(&mut tx, "Acme").await;
    let globex = partner(&mut tx, "Globex").await;
    let website = project_for(&mut tx, "Website", acme).await;
    let stages = stage_ids_of(&mut tx, website).await;

    // a task without a parent takes the project's customer
    let parent = task(
        &mut tx,
        task_vals("Launch").set_cell("project_id", Cell::Id(website)),
    )
    .await;
    assert_eq!(
        task_cell(&mut tx, parent, "partner_id").await,
        Cell::Id(acme)
    );

    // the parent's customer comes before the project's
    tasks(&tx)
        .browse([parent])
        .unwrap()
        .write(
            &mut tx,
            RawVals::new().set_cell("partner_id", Cell::Id(globex)),
        )
        .await
        .unwrap();
    let child = task(
        &mut tx,
        task_vals("Write copy").set_cell("parent_id", Cell::Id(parent)),
    )
    .await;
    assert_eq!(
        task_cell(&mut tx, child, "project_id").await,
        Cell::Id(website)
    );
    assert_eq!(
        task_cell(&mut tx, child, "partner_id").await,
        Cell::Id(globex)
    );
    assert_eq!(
        task_cell(&mut tx, child, "stage_id").await,
        Cell::Id(stages[0])
    );

    // an explicit customer stays
    let other = task(
        &mut tx,
        task_vals("Photos")
            .set_cell("parent_id", Cell::Id(parent))
            .set_cell("partner_id", Cell::Id(acme)),
    )
    .await;
    assert_eq!(
        task_cell(&mut tx, other, "partner_id").await,
        Cell::Id(acme)
    );

    // a parent without a project gives none
    let loose = task(&mut tx, task_vals("Loose")).await;
    let sub = task(
        &mut tx,
        task_vals("Loose child").set_cell("parent_id", Cell::Id(loose)),
    )
    .await;
    assert_eq!(task_cell(&mut tx, sub, "project_id").await, Cell::Null);
    assert_eq!(task_cell(&mut tx, sub, "stage_id").await, Cell::Null);
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn a_task_only_uses_stages_of_its_own_project() {
    let h = harness().await;
    let mut tx = begin(&h).await;
    let templates = templates(&mut tx).await;
    let (website, website_stages) = board(&mut tx, "Website").await;
    let (_, mobile_stages) = board(&mut tx, "Mobile").await;

    // another project's stage, and a template stage, are refused on create
    for stage in [mobile_stages[0], templates[0]] {
        let error = assert_refused(
            try_task(
                &mut tx,
                task_vals("Wrong")
                    .set_cell("project_id", Cell::Id(website))
                    .set_cell("stage_id", Cell::Id(stage)),
            )
            .await,
            "stage",
        );
        assert!(error.message().contains("project"), "{error:?}");
    }
    // a task without a project has no stage
    assert_refused(
        try_task(
            &mut tx,
            task_vals("Loose").set_cell("stage_id", Cell::Id(website_stages[0])),
        )
        .await,
        "stage",
    );
    let loose = task(&mut tx, task_vals("Loose")).await;
    assert_eq!(task_cell(&mut tx, loose, "stage_id").await, Cell::Null);

    // and on write
    let id = task(
        &mut tx,
        task_vals("Right").set_cell("project_id", Cell::Id(website)),
    )
    .await;
    assert_refused(
        try_write(
            &mut tx,
            "project.task",
            &[id],
            RawVals::new().set_cell("stage_id", Cell::Id(mobile_stages[1])),
        )
        .await,
        "stage",
    );
    assert_eq!(
        task_cell(&mut tx, id, "stage_id").await,
        Cell::Id(website_stages[0])
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn moving_a_task_to_another_project_moves_it_to_that_projects_first_stage() {
    let h = harness().await;
    let mut tx = begin(&h).await;
    templates(&mut tx).await;
    let (website, website_stages) = board(&mut tx, "Website").await;
    let (mobile, mobile_stages) = board(&mut tx, "Mobile").await;
    let first = task(
        &mut tx,
        task_vals("One")
            .set_cell("project_id", Cell::Id(website))
            .set_cell("stage_id", Cell::Id(website_stages[1])),
    )
    .await;
    let second = task(
        &mut tx,
        task_vals("Two")
            .set_cell("project_id", Cell::Id(website))
            .set_cell("stage_id", Cell::Id(website_stages[2])),
    )
    .await;
    // several tasks at once, without a stage
    try_write(
        &mut tx,
        "project.task",
        &[first, second],
        RawVals::new().set_cell("project_id", Cell::Id(mobile)),
    )
    .await
    .unwrap();
    for id in [first, second] {
        assert_eq!(task_cell(&mut tx, id, "project_id").await, Cell::Id(mobile));
        assert_eq!(
            task_cell(&mut tx, id, "stage_id").await,
            Cell::Id(mobile_stages[0])
        );
    }
    // with a stage of the new project, that stage is kept
    try_write(
        &mut tx,
        "project.task",
        &[first],
        RawVals::new()
            .set_cell("project_id", Cell::Id(website))
            .set_cell("stage_id", Cell::Id(website_stages[3])),
    )
    .await
    .unwrap();
    assert_eq!(
        task_cell(&mut tx, first, "stage_id").await,
        Cell::Id(website_stages[3])
    );
    // with a stage of the old project, the move is refused
    assert_refused(
        try_write(
            &mut tx,
            "project.task",
            &[second],
            RawVals::new()
                .set_cell("project_id", Cell::Id(website))
                .set_cell("stage_id", Cell::Id(mobile_stages[1])),
        )
        .await,
        "stage",
    );
    // leaving every project clears the stage
    try_write(
        &mut tx,
        "project.task",
        &[second],
        RawVals::new().set_cell("project_id", Cell::Null),
    )
    .await
    .unwrap();
    assert_eq!(task_cell(&mut tx, second, "stage_id").await, Cell::Null);
    // a several-record write where one task fails changes none of them
    let third = task(
        &mut tx,
        task_vals("Three").set_cell("project_id", Cell::Id(mobile)),
    )
    .await;
    assert_refused(
        try_write(
            &mut tx,
            "project.task",
            &[third, first],
            RawVals::new().set_cell("stage_id", Cell::Id(mobile_stages[2])),
        )
        .await,
        "stage",
    );
    assert_eq!(
        task_cell(&mut tx, third, "stage_id").await,
        Cell::Id(mobile_stages[0])
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn a_stage_change_resets_the_state_unless_the_task_is_closed() {
    let h = harness().await;
    let mut tx = begin(&h).await;
    templates(&mut tx).await;
    let (website, stages) = board(&mut tx, "Website").await;
    let mk = async |tx: &mut Transaction, name: &str, state: &str| {
        task(
            tx,
            task_vals(name)
                .set_cell("project_id", Cell::Id(website))
                .set_cell("state", text(state)),
        )
        .await
    };
    let review = mk(&mut tx, "Review", "changes_requested").await;
    let approved = mk(&mut tx, "Approved", "approved").await;
    let done = mk(&mut tx, "Done", "done").await;
    let cancelled = mk(&mut tx, "Cancelled", "cancelled").await;
    let explicit = mk(&mut tx, "Explicit", "changes_requested").await;
    tx.flush().await.unwrap();
    tx.execute(
        "UPDATE project_task SET date_last_stage_update = '2020-01-01 00:00:00'",
        &[],
    )
    .await
    .unwrap();
    tx.invalidate_model("project.task", None).await.unwrap();
    let old = DateTime::parse_odoo("2020-01-01 00:00:00").unwrap();

    try_write(
        &mut tx,
        "project.task",
        &[review, approved, done, cancelled],
        RawVals::new().set_cell("stage_id", Cell::Id(stages[1])),
    )
    .await
    .unwrap();
    try_write(
        &mut tx,
        "project.task",
        &[explicit],
        RawVals::new()
            .set_cell("stage_id", Cell::Id(stages[1]))
            .set_cell("state", text("approved")),
    )
    .await
    .unwrap();
    for (id, state) in [
        (review, "in_progress"),
        (approved, "in_progress"),
        (done, "done"),
        (cancelled, "cancelled"),
        (explicit, "approved"),
    ] {
        assert_eq!(task_cell(&mut tx, id, "state").await, text(state), "{id}");
        let Cell::DateTime(stamped) = task_cell(&mut tx, id, "date_last_stage_update").await else {
            panic!("stage update missing on {id}");
        };
        assert!(stamped > old, "a stage move stamps the time ({id})");
    }

    // writing the same stage again is not a stage change
    try_write(
        &mut tx,
        "project.task",
        &[review],
        RawVals::new().set_cell("state", text("changes_requested")),
    )
    .await
    .unwrap();
    let before = task_cell(&mut tx, review, "date_last_stage_update").await;
    try_write(
        &mut tx,
        "project.task",
        &[review],
        RawVals::new().set_cell("stage_id", Cell::Id(stages[1])),
    )
    .await
    .unwrap();
    assert_eq!(
        task_cell(&mut tx, review, "state").await,
        text("changes_requested")
    );
    assert_eq!(
        task_cell(&mut tx, review, "date_last_stage_update").await,
        before
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn the_last_stage_update_is_server_managed() {
    let h = harness().await;
    let mut tx = begin(&h).await;
    templates(&mut tx).await;
    let (website, _) = board(&mut tx, "Website").await;
    let stamp = Cell::DateTime(DateTime::parse_odoo("2020-01-01 00:00:00").unwrap());
    assert_refused(
        try_task(
            &mut tx,
            task_vals("Backdated")
                .set_cell("project_id", Cell::Id(website))
                .set_cell("date_last_stage_update", stamp.clone()),
        )
        .await,
        "date_last_stage_update",
    );
    let id = task(
        &mut tx,
        task_vals("Fine").set_cell("project_id", Cell::Id(website)),
    )
    .await;
    assert_refused(
        try_write(
            &mut tx,
            "project.task",
            &[id],
            RawVals::new().set_cell("date_last_stage_update", stamp),
        )
        .await,
        "date_last_stage_update",
    );
    // a copy is stamped anew
    let copy: Records = tasks(&tx)
        .browse([id])
        .unwrap()
        .call(&mut tx, "copy", (None::<ValueMap>,))
        .await
        .unwrap();
    assert!(matches!(
        copy.cell(&mut tx, "date_last_stage_update").await.unwrap(),
        Cell::DateTime(_)
    ));
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn a_task_cannot_be_its_own_ancestor() {
    let h = harness().await;
    let mut tx = begin(&h).await;
    let top = task(&mut tx, task_vals("Top")).await;
    let middle = task(
        &mut tx,
        task_vals("Middle").set_cell("parent_id", Cell::Id(top)),
    )
    .await;
    let bottom = task(
        &mut tx,
        task_vals("Bottom").set_cell("parent_id", Cell::Id(middle)),
    )
    .await;
    for (id, parent) in [(top, bottom), (top, top)] {
        assert_refused(
            try_write(
                &mut tx,
                "project.task",
                &[id],
                RawVals::new().set_cell("parent_id", Cell::Id(parent)),
            )
            .await,
            "subtask",
        );
    }
    assert_eq!(task_cell(&mut tx, top, "parent_id").await, Cell::Null);
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

fn obj(entries: impl IntoIterator<Item = (&'static str, Value)>) -> Value {
    let mut map = ValueMap::new();
    for (key, value) in entries {
        map.insert(key.to_owned(), value);
    }
    Value::Map(Arc::new(map))
}

/// The values an `onchange` call on a new task suggests after `changed` was set.
async fn suggest(
    tx: &mut Transaction,
    values: Vec<(&'static str, Value)>,
    changed: &str,
) -> ValueMap {
    let spec = obj(
        ["name", "project_id", "stage_id", "partner_id", "parent_id"].map(|field| (field, obj([]))),
    );
    let result = tasks(tx)
        .web_call(
            tx,
            "onchange",
            &obj([
                ("values", obj(values)),
                ("field_names", Value::list([Value::str(changed)])),
                ("fields_spec", spec),
            ]),
        )
        .await
        .unwrap();
    let value = result.dict().unwrap().get("value").unwrap();
    match value {
        Value::Map(map) => (*map).clone(),
        other => panic!("onchange value is not a map: {other:?}"),
    }
}

fn suggested_id(values: &ValueMap, field: &str) -> Option<i64> {
    match values.get(field) {
        Some(Value::Int(id)) => Some(*id),
        Some(Value::List(pair)) => match pair.first() {
            Some(Value::Int(id)) => Some(*id),
            _ => None,
        },
        Some(Value::Map(map)) => match map.get("id") {
            Some(Value::Int(id)) => Some(*id),
            _ => None,
        },
        _ => None,
    }
}

#[tokio::test]
async fn the_form_suggests_the_stage_project_and_customer() {
    let h = harness().await;
    let mut tx = begin(&h).await;
    templates(&mut tx).await;
    let acme = partner(&mut tx, "Acme").await;
    let website = project_for(&mut tx, "Website", acme).await;
    let website_stages = stage_ids_of(&mut tx, website).await;
    let (mobile, mobile_stages) = board(&mut tx, "Mobile").await;

    // choosing a project suggests its first stage and its customer
    let values = suggest(
        &mut tx,
        vec![
            ("name", Value::str("New")),
            ("project_id", Value::Int(website)),
        ],
        "project_id",
    )
    .await;
    assert_eq!(suggested_id(&values, "stage_id"), Some(website_stages[0]));
    assert_eq!(suggested_id(&values, "partner_id"), Some(acme));

    // switching project replaces a stage of the other project
    let values = suggest(
        &mut tx,
        vec![
            ("name", Value::str("New")),
            ("project_id", Value::Int(mobile)),
            ("stage_id", Value::Int(website_stages[1])),
        ],
        "project_id",
    )
    .await;
    assert_eq!(suggested_id(&values, "stage_id"), Some(mobile_stages[0]));

    // choosing a parent suggests its project and customer
    let parent = task(
        &mut tx,
        task_vals("Launch").set_cell("project_id", Cell::Id(website)),
    )
    .await;
    let values = suggest(
        &mut tx,
        vec![
            ("name", Value::str("Child")),
            ("parent_id", Value::Int(parent)),
        ],
        "parent_id",
    )
    .await;
    assert_eq!(suggested_id(&values, "project_id"), Some(website));
    assert_eq!(suggested_id(&values, "partner_id"), Some(acme));
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn a_stage_move_writes_a_history_line() {
    let h = harness_with_history().await;
    let mut tx = begin(&h).await;
    templates(&mut tx).await;
    let (website, stages) = board(&mut tx, "Website").await;
    let id = task(
        &mut tx,
        task_vals("Tracked")
            .set_cell("project_id", Cell::Id(website))
            .set_cell("state", text("changes_requested")),
    )
    .await;
    tx.commit().await.unwrap();
    let mut tx = h.begin().await.unwrap();
    try_write(
        &mut tx,
        "project.task",
        &[id],
        RawVals::new().set_cell("stage_id", Cell::Id(stages[1])),
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let mut tx = h.begin().await.unwrap();
    let lines: Vec<String> = tx
        .query(
            "SELECT tracking::text FROM mail_message WHERE model = 'project.task' \
             AND res_id = $1 AND message_type = 'tracking'",
            &[&id],
        )
        .await
        .unwrap()
        .iter()
        .map(|row| row.get(0))
        .collect();
    assert_eq!(lines.len(), 1, "{lines:?}");
    // the stage, the state reset and the stage time are written together, so one line tracks
    // the stage and the state (the stage time is not tracked)
    assert!(lines[0].contains("\"stage_id\""), "{}", lines[0]);
    assert!(lines[0].contains("\"state\""), "{}", lines[0]);
    assert!(!lines[0].contains("date_last_stage_update"), "{}", lines[0]);
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn one_create_call_fills_each_task_on_its_own() {
    let h = harness().await;
    let mut tx = begin(&h).await;
    templates(&mut tx).await;
    let acme = partner(&mut tx, "Acme").await;
    let website = project_for(&mut tx, "Website", acme).await;
    let website_stages = stage_ids_of(&mut tx, website).await;
    let (mobile, mobile_stages) = board(&mut tx, "Mobile").await;
    let parent = task(
        &mut tx,
        task_vals("Parent").set_cell("project_id", Cell::Id(mobile)),
    )
    .await;
    let created = tasks(&tx)
        .create(
            &mut tx,
            vec![
                task_vals("Parent only").set_cell("parent_id", Cell::Id(parent)),
                task_vals("Project only").set_cell("project_id", Cell::Id(website)),
                task_vals("Explicit stage")
                    .set_cell("project_id", Cell::Id(website))
                    .set_cell("stage_id", Cell::Id(website_stages[2])),
                task_vals("Nothing"),
                task_vals("Project again").set_cell("project_id", Cell::Id(website)),
                task_vals("Other project").set_cell("project_id", Cell::Id(mobile)),
            ],
        )
        .await
        .unwrap()
        .ids();
    let expected = [
        (Cell::Id(mobile), Cell::Id(mobile_stages[0]), Cell::Null),
        (
            Cell::Id(website),
            Cell::Id(website_stages[0]),
            Cell::Id(acme),
        ),
        (
            Cell::Id(website),
            Cell::Id(website_stages[2]),
            Cell::Id(acme),
        ),
        (Cell::Null, Cell::Null, Cell::Null),
        (
            Cell::Id(website),
            Cell::Id(website_stages[0]),
            Cell::Id(acme),
        ),
        (Cell::Id(mobile), Cell::Id(mobile_stages[0]), Cell::Null),
    ];
    for (id, (project, stage, customer)) in created.iter().zip(expected) {
        assert_eq!(task_cell(&mut tx, *id, "project_id").await, project, "{id}");
        assert_eq!(task_cell(&mut tx, *id, "stage_id").await, stage, "{id}");
        assert_eq!(
            task_cell(&mut tx, *id, "partner_id").await,
            customer,
            "{id}"
        );
        assert!(matches!(
            task_cell(&mut tx, *id, "date_last_stage_update").await,
            Cell::DateTime(_)
        ));
    }
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn a_context_parent_gives_its_project_and_customer() {
    let h = harness().await;
    let mut tx = begin(&h).await;
    templates(&mut tx).await;
    let acme = partner(&mut tx, "Acme").await;
    let website = project_for(&mut tx, "Website", acme).await;
    let stages = stage_ids_of(&mut tx, website).await;
    let parent = task(
        &mut tx,
        task_vals("Parent").set_cell("project_id", Cell::Id(website)),
    )
    .await;
    let id = tasks(&tx)
        .with_context([("default_parent_id", Value::Int(parent))])
        .create(&mut tx, vec![task_vals("From the subtask list")])
        .await
        .unwrap()
        .ids()[0];
    assert_eq!(task_cell(&mut tx, id, "parent_id").await, Cell::Id(parent));
    assert_eq!(
        task_cell(&mut tx, id, "project_id").await,
        Cell::Id(website)
    );
    assert_eq!(
        task_cell(&mut tx, id, "stage_id").await,
        Cell::Id(stages[0])
    );
    assert_eq!(task_cell(&mut tx, id, "partner_id").await, Cell::Id(acme));
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn a_context_default_for_the_stage_time_is_refused() {
    let h = harness().await;
    let mut tx = begin(&h).await;
    let set = tasks(&tx).with_context([(
        "default_date_last_stage_update",
        Value::str("2020-01-01 00:00:00"),
    )]);
    let outcome = tx
        .savepoint(async move |tx| set.create(tx, vec![task_vals("Backdated")]).await)
        .await;
    assert_refused(outcome, "date_last_stage_update");
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn a_project_move_alone_resets_the_state_and_stamps_the_time() {
    let h = harness().await;
    let mut tx = begin(&h).await;
    templates(&mut tx).await;
    let (website, _) = board(&mut tx, "Website").await;
    let (mobile, mobile_stages) = board(&mut tx, "Mobile").await;
    let bare = project(&mut tx, "Bare").await;
    tx.flush().await.unwrap();
    tx.execute(
        "DELETE FROM project_task_type WHERE project_id = $1",
        &[&bare],
    )
    .await
    .unwrap();
    tx.invalidate_model("project.task.type", None)
        .await
        .unwrap();
    let id = task(
        &mut tx,
        task_vals("Mover")
            .set_cell("project_id", Cell::Id(website))
            .set_cell("state", text("changes_requested")),
    )
    .await;
    tx.flush().await.unwrap();
    tx.execute(
        "UPDATE project_task SET date_last_stage_update = '2020-01-01 00:00:00' WHERE id = $1",
        &[&id],
    )
    .await
    .unwrap();
    tx.invalidate_model("project.task", None).await.unwrap();
    let old = DateTime::parse_odoo("2020-01-01 00:00:00").unwrap();
    try_write(
        &mut tx,
        "project.task",
        &[id],
        RawVals::new().set_cell("project_id", Cell::Id(mobile)),
    )
    .await
    .unwrap();
    assert_eq!(
        task_cell(&mut tx, id, "stage_id").await,
        Cell::Id(mobile_stages[0])
    );
    assert_eq!(task_cell(&mut tx, id, "state").await, text("in_progress"));
    let Cell::DateTime(stamped) = task_cell(&mut tx, id, "date_last_stage_update").await else {
        panic!("a project move stamps the stage time");
    };
    assert!(stamped > old, "{stamped:?}");

    // a project without stages: the task has no stage there
    try_write(
        &mut tx,
        "project.task",
        &[id],
        RawVals::new().set_cell("project_id", Cell::Id(bare)),
    )
    .await
    .unwrap();
    assert_eq!(task_cell(&mut tx, id, "project_id").await, Cell::Id(bare));
    assert_eq!(task_cell(&mut tx, id, "stage_id").await, Cell::Null);
    // and a new task there starts without one
    let new = task(
        &mut tx,
        task_vals("Bare task").set_cell("project_id", Cell::Id(bare)),
    )
    .await;
    assert_eq!(task_cell(&mut tx, new, "stage_id").await, Cell::Null);
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn the_form_clears_the_stage_and_keeps_a_chosen_project() {
    let h = harness().await;
    let mut tx = begin(&h).await;
    templates(&mut tx).await;
    let acme = partner(&mut tx, "Acme").await;
    let globex = partner(&mut tx, "Globex").await;
    let (website, website_stages) = board(&mut tx, "Website").await;
    let mobile = project_for(&mut tx, "Mobile", globex).await;

    // clearing the project clears the stage
    let values = suggest(
        &mut tx,
        vec![
            ("name", Value::str("Loose")),
            ("project_id", Value::False),
            ("stage_id", Value::Int(website_stages[1])),
        ],
        "project_id",
    )
    .await;
    assert_eq!(values.get("stage_id"), Some(&Value::False));

    // a parent of another project does not replace the chosen project; its customer is proposed
    let parent = task(
        &mut tx,
        task_vals("Parent")
            .set_cell("project_id", Cell::Id(mobile))
            .set_cell("partner_id", Cell::Id(acme)),
    )
    .await;
    let values = suggest(
        &mut tx,
        vec![
            ("name", Value::str("Child")),
            ("project_id", Value::Int(website)),
            ("stage_id", Value::Int(website_stages[1])),
            ("parent_id", Value::Int(parent)),
        ],
        "parent_id",
    )
    .await;
    assert!(!values.contains_key("project_id"), "{values:?}");
    assert!(!values.contains_key("stage_id"), "{values:?}");
    assert_eq!(suggested_id(&values, "partner_id"), Some(acme));
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

fn web_save_vals(entries: Vec<(&'static str, Value)>) -> Value {
    obj([("vals", obj(entries)), ("specification", obj([]))])
}

/// Forms and their one2many lines send every field they hold: an empty project, stage or
/// customer counts as not given, so the parent and project still fill them.
#[tokio::test]
async fn empty_values_from_a_form_count_as_not_given() {
    let h = harness().await;
    let mut tx = begin(&h).await;
    templates(&mut tx).await;
    let acme = partner(&mut tx, "Acme").await;
    let website = project_for(&mut tx, "Website", acme).await;
    let stages = stage_ids_of(&mut tx, website).await;
    let parent = task(
        &mut tx,
        task_vals("Parent").set_cell("project_id", Cell::Id(website)),
    )
    .await;
    let empties = || {
        vec![
            ("project_id", Value::False),
            ("stage_id", Value::False),
            ("partner_id", Value::False),
        ]
    };

    // a subtask form with a parent and empty fields
    let mut vals = empties();
    vals.push(("name", Value::str("Form subtask")));
    vals.push(("parent_id", Value::Int(parent)));
    let saved = tasks(&tx)
        .web_call(&mut tx, "web_save", &web_save_vals(vals))
        .await
        .unwrap();
    let Value::List(rows) = &saved else {
        panic!("web_save returns a list: {saved:?}");
    };
    let Ok(Value::Int(sub)) = rows[0].dict().unwrap().get("id") else {
        panic!("no id: {saved:?}");
    };
    for (field, expected) in [
        ("project_id", Cell::Id(website)),
        ("stage_id", Cell::Id(stages[0])),
        ("partner_id", Cell::Id(acme)),
    ] {
        assert_eq!(task_cell(&mut tx, sub, field).await, expected, "{field}");
    }

    // a project form value with an empty stage and customer, through the JSON-2 values path
    let mut values = std::collections::BTreeMap::from([
        ("name".to_owned(), Value::str("JSON task")),
        ("project_id".to_owned(), Value::Int(website)),
    ]);
    values.insert("stage_id".to_owned(), Value::False);
    values.insert("partner_id".to_owned(), Value::False);
    let id = tasks(&tx)
        .create_values(&mut tx, &[values])
        .await
        .unwrap()
        .ids()[0];
    assert_eq!(
        task_cell(&mut tx, id, "stage_id").await,
        Cell::Id(stages[0])
    );
    assert_eq!(task_cell(&mut tx, id, "partner_id").await, Cell::Id(acme));

    // subtasks added on the parent's Subtasks page: one2many create commands with empty fields
    let line = |name: &'static str| {
        let mut entries = empties();
        entries.push(("name", Value::str(name)));
        Value::list([Value::Int(0), Value::Int(0), obj(entries)])
    };
    tasks(&tx)
        .browse([parent])
        .unwrap()
        .web_call(
            &mut tx,
            "web_save",
            &web_save_vals(vec![("child_ids", Value::list([line("Line subtask")]))]),
        )
        .await
        .unwrap();
    // and on a new parent saved together with its subtask
    let saved = tasks(&tx)
        .web_call(
            &mut tx,
            "web_save",
            &web_save_vals(vec![
                ("name", Value::str("New parent")),
                ("project_id", Value::Int(website)),
                ("child_ids", Value::list([line("New line subtask")])),
            ]),
        )
        .await
        .unwrap();
    assert!(matches!(saved, Value::List(_)), "{saved:?}");
    tx.flush().await.unwrap();
    let rows = tx
        .query(
            "SELECT name, project_id::int8, stage_id::int8, partner_id::int8 FROM project_task \
             WHERE name IN ('Line subtask', 'New line subtask') ORDER BY name",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(rows.len(), 2);
    for row in rows {
        let name: String = row.get(0);
        assert_eq!(row.get::<_, Option<i64>>(1), Some(website), "{name}");
        assert_eq!(row.get::<_, Option<i64>>(2), Some(stages[0]), "{name}");
        assert_eq!(row.get::<_, Option<i64>>(3), Some(acme), "{name}");
    }
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}
