// SPDX-License-Identifier: MIT OR Apache-2.0
//! The company of a task: a task of a project has the project's company, kept in step when the
//! task moves and when the project changes company; a task without a project has a company of its
//! own, by default the user's current one, or none (shared).
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

/// A company (the harness loads no data file: the first call creates a currency too).
async fn company(tx: &mut Transaction, name: &str) -> i64 {
    tx.flush().await.unwrap();
    let existing: Option<i64> = tx
        .query("SELECT id::int8 FROM res_currency ORDER BY id LIMIT 1", &[])
        .await
        .unwrap()
        .first()
        .map(|row| row.get(0));
    let currency = match existing {
        Some(id) => id,
        None => create(
            tx,
            "res.currency",
            RawVals::new()
                .set_cell("name", text("USD"))
                .set_cell("symbol", text("$"))
                .set_cell("rounding", Cell::Decimal(Decimal::new(1, 2))),
        )
        .await
        .unwrap()
        .ids()[0],
    };
    create(
        tx,
        "res.company",
        RawVals::new()
            .set_cell("name", text(name))
            .set_cell("currency_id", Cell::Id(currency)),
    )
    .await
    .unwrap()
    .ids()[0]
}

async fn project_of(tx: &mut Transaction, name: &str, company: Option<i64>) -> i64 {
    create(
        tx,
        "project.project",
        RawVals::new()
            .set_cell("name", text(name))
            .set_cell("company_id", company.map_or(Cell::Null, Cell::Id)),
    )
    .await
    .unwrap()
    .ids()[0]
}

async fn task_company(tx: &mut Transaction, id: i64) -> Cell {
    cell(tx, "project.task", id, "company_id").await
}

fn some(id: Option<i64>) -> Cell {
    id.map_or(Cell::Null, Cell::Id)
}

#[tokio::test]
async fn a_task_of_a_project_has_the_projects_company() {
    let h = harness().await;
    let mut tx = begin(&h).await;
    templates(&mut tx).await;
    let north = company(&mut tx, "North").await;
    let south = company(&mut tx, "South").await;
    let p_north = project_of(&mut tx, "North work", Some(north)).await;
    let p_south = project_of(&mut tx, "South work", Some(south)).await;
    let shared = project_of(&mut tx, "Shared work", None).await;

    // on create: the project's company, also when the values hold an empty company
    let plain = task(
        &mut tx,
        task_vals("Plain").set_cell("project_id", Cell::Id(p_north)),
    )
    .await;
    assert_eq!(task_company(&mut tx, plain).await, Cell::Id(north));
    let emptied = task(
        &mut tx,
        task_vals("Emptied")
            .set_cell("project_id", Cell::Id(p_north))
            .set_cell("company_id", Cell::Null),
    )
    .await;
    assert_eq!(task_company(&mut tx, emptied).await, Cell::Id(north));
    let in_shared = task(
        &mut tx,
        task_vals("In shared").set_cell("project_id", Cell::Id(shared)),
    )
    .await;
    assert_eq!(task_company(&mut tx, in_shared).await, Cell::Null);
    // a subtask takes the company with its parent's project
    let sub = task(
        &mut tx,
        task_vals("Sub").set_cell("parent_id", Cell::Id(plain)),
    )
    .await;
    assert_eq!(task_company(&mut tx, sub).await, Cell::Id(north));

    // another company than the project's is refused, on create and on write
    for (project, other) in [(p_north, south), (shared, north)] {
        assert_refused(
            try_task(
                &mut tx,
                task_vals("Mismatch")
                    .set_cell("project_id", Cell::Id(project))
                    .set_cell("company_id", Cell::Id(other)),
            )
            .await,
            "company_id",
        );
    }
    for value in [Cell::Id(south), Cell::Null] {
        assert_refused(
            try_write(
                &mut tx,
                "project.task",
                &[plain, emptied],
                RawVals::new().set_cell("company_id", value),
            )
            .await,
            "company_id",
        );
    }
    assert_eq!(task_company(&mut tx, plain).await, Cell::Id(north));

    // a move to another project takes that project's company; an explicit matching one is fine
    try_write(
        &mut tx,
        "project.task",
        &[plain],
        RawVals::new().set_cell("project_id", Cell::Id(p_south)),
    )
    .await
    .unwrap();
    assert_eq!(task_company(&mut tx, plain).await, Cell::Id(south));
    try_write(
        &mut tx,
        "project.task",
        &[plain],
        RawVals::new()
            .set_cell("project_id", Cell::Id(p_north))
            .set_cell("company_id", Cell::Id(north)),
    )
    .await
    .unwrap();
    assert_eq!(task_company(&mut tx, plain).await, Cell::Id(north));
    assert_refused(
        try_write(
            &mut tx,
            "project.task",
            &[plain],
            RawVals::new()
                .set_cell("project_id", Cell::Id(p_south))
                .set_cell("company_id", Cell::Id(north)),
        )
        .await,
        "company_id",
    );
    try_write(
        &mut tx,
        "project.task",
        &[plain],
        RawVals::new().set_cell("project_id", Cell::Id(shared)),
    )
    .await
    .unwrap();
    assert_eq!(task_company(&mut tx, plain).await, Cell::Null);

    // a task leaving every project keeps its company, and may then change it
    try_write(
        &mut tx,
        "project.task",
        &[emptied],
        RawVals::new().set_cell("project_id", Cell::Null),
    )
    .await
    .unwrap();
    assert_eq!(task_company(&mut tx, emptied).await, Cell::Id(north));
    try_write(
        &mut tx,
        "project.task",
        &[emptied],
        RawVals::new().set_cell("company_id", Cell::Id(south)),
    )
    .await
    .unwrap();
    assert_eq!(task_company(&mut tx, emptied).await, Cell::Id(south));
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn a_projects_new_company_passes_to_all_its_tasks() {
    let h = harness().await;
    let mut tx = begin(&h).await;
    templates(&mut tx).await;
    let north = company(&mut tx, "North").await;
    let south = company(&mut tx, "South").await;
    let project = project_of(&mut tx, "Moving", Some(north)).await;
    let other = project_of(&mut tx, "Staying", Some(north)).await;
    let active = task(
        &mut tx,
        task_vals("Active").set_cell("project_id", Cell::Id(project)),
    )
    .await;
    let archived = task(
        &mut tx,
        task_vals("Archived").set_cell("project_id", Cell::Id(project)),
    )
    .await;
    try_write(
        &mut tx,
        "project.task",
        &[archived],
        RawVals::new().set_cell("active", Cell::Bool(false)),
    )
    .await
    .unwrap();
    let untouched = task(
        &mut tx,
        task_vals("Elsewhere").set_cell("project_id", Cell::Id(other)),
    )
    .await;

    try_write(
        &mut tx,
        "project.project",
        &[project],
        RawVals::new().set_cell("company_id", Cell::Id(south)),
    )
    .await
    .unwrap();
    for id in [active, archived] {
        assert_eq!(task_company(&mut tx, id).await, Cell::Id(south), "{id}");
    }
    assert_eq!(task_company(&mut tx, untouched).await, Cell::Id(north));

    try_write(
        &mut tx,
        "project.project",
        &[project],
        RawVals::new().set_cell("company_id", Cell::Null),
    )
    .await
    .unwrap();
    for id in [active, archived] {
        assert_eq!(task_company(&mut tx, id).await, Cell::Null, "{id}");
    }
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn a_task_without_a_project_has_a_company_of_its_own() {
    let h = harness().await;
    let mut tx = begin(&h).await;
    let north = company(&mut tx, "North").await;
    let south = company(&mut tx, "South").await;

    // the user's current company by default
    let current = tasks(&tx)
        .with_context([(
            "allowed_company_ids",
            Value::list([Value::Int(south), Value::Int(north)]),
        )])
        .create(&mut tx, vec![task_vals("Loose")])
        .await
        .unwrap()
        .ids()[0];
    assert_eq!(task_company(&mut tx, current).await, Cell::Id(south));
    // without a current company, none
    let expected = tx.env().company_id().unwrap();
    let plain = task(&mut tx, task_vals("Plain")).await;
    assert_eq!(task_company(&mut tx, plain).await, some(expected));
    // an explicit company, an explicit empty one (shared) and a context default stay
    let explicit = task(
        &mut tx,
        task_vals("Explicit").set_cell("company_id", Cell::Id(north)),
    )
    .await;
    assert_eq!(task_company(&mut tx, explicit).await, Cell::Id(north));
    let shared = tasks(&tx)
        .with_context([("allowed_company_ids", Value::list([Value::Int(south)]))])
        .create(
            &mut tx,
            vec![task_vals("Shared").set_cell("company_id", Cell::Null)],
        )
        .await
        .unwrap()
        .ids()[0];
    assert_eq!(task_company(&mut tx, shared).await, Cell::Null);
    let defaulted = tasks(&tx)
        .with_context([
            ("allowed_company_ids", Value::list([Value::Int(south)])),
            ("default_company_id", Value::Int(north)),
        ])
        .create(&mut tx, vec![task_vals("Defaulted")])
        .await
        .unwrap()
        .ids()[0];
    assert_eq!(task_company(&mut tx, defaulted).await, Cell::Id(north));
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

#[tokio::test]
async fn the_form_proposes_the_company_of_the_project() {
    let h = harness().await;
    let mut tx = begin(&h).await;
    templates(&mut tx).await;
    let north = company(&mut tx, "North").await;
    let south = company(&mut tx, "South").await;
    let project = project_of(&mut tx, "North work", Some(north)).await;
    let shared = project_of(&mut tx, "Shared work", None).await;

    // a new task: the user's current company, or the company of the context project
    let current = Value::list([Value::Int(south)]);
    for (project, expected) in [
        (None, Value::Int(south)),
        (Some(project), Value::Int(north)),
        (Some(shared), Value::False),
    ] {
        let mut context = vec![("allowed_company_ids", current.clone())];
        if let Some(project) = project {
            context.push(("default_project_id", Value::Int(project)));
        }
        let defaults = tasks(&tx)
            .with_context(context)
            .default_get(&mut tx, &["company_id"])
            .await
            .unwrap();
        assert_eq!(defaults.get("company_id"), Some(&expected), "{project:?}");
    }

    // choosing a project proposes its company
    let spec =
        obj(["name", "project_id", "stage_id", "partner_id", "company_id"].map(|f| (f, obj([]))));
    for (chosen, expected) in [(project, Value::Int(north)), (shared, Value::False)] {
        let result = tasks(&tx)
            .web_call(
                &mut tx,
                "onchange",
                &obj([
                    (
                        "values",
                        obj([
                            ("name", Value::str("New")),
                            ("project_id", Value::Int(chosen)),
                            ("company_id", Value::Int(south)),
                        ]),
                    ),
                    ("field_names", Value::list([Value::str("project_id")])),
                    ("fields_spec", spec.clone()),
                ]),
            )
            .await
            .unwrap();
        let Ok(Value::Map(value)) = result.dict().unwrap().get("value") else {
            panic!("onchange value is not a map: {result:?}");
        };
        let company = match value.get("company_id") {
            Some(Value::Map(map)) => map.get("id").cloned().unwrap_or(Value::False),
            Some(Value::List(pair)) => pair.first().cloned().unwrap_or(Value::False),
            Some(other) => other.clone(),
            None => panic!("no company proposed: {value:?}"),
        };
        assert_eq!(company, expected, "project {chosen}");
    }
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}
