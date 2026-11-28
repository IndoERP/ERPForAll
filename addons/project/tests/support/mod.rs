// SPDX-License-Identifier: MIT OR Apache-2.0
//! Shared setup of the database-backed project tests: a harness with the real addons, the mail
//! subtype the history needs (the harness loads no data file), and record helpers.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    dead_code,
    reason = "test support shared by several test files"
)]

use erp_sdk::prelude::*;
use erp_testkit::Harness;

pub async fn harness() -> Harness {
    builder().build().await.unwrap()
}

/// A harness that writes the chatter history at commit, as the server does.
pub async fn harness_with_history() -> Harness {
    builder()
        .precommit_hook(erp_addon_mail::TRACKING_HOOK)
        .build()
        .await
        .unwrap()
}

fn builder() -> erp_testkit::HarnessBuilder {
    Harness::builder([
        &erp_addon_base::ADDON,
        &erp_addon_web::ADDON,
        &erp_addon_mail::ADDON,
        &erp_addon_project::ADDON,
    ])
    // the external ids table the data loader owns; the history looks `mail.mt_note` up in it
    .before_schema(
        "CREATE TABLE IF NOT EXISTS ir_model_data (\
         id bigserial PRIMARY KEY, \
         module text NOT NULL, \
         name text NOT NULL, \
         model text NOT NULL, \
         res_id int8 NOT NULL, \
         noupdate boolean NOT NULL DEFAULT false\
         )",
    )
}

pub fn text(value: &str) -> Cell {
    Cell::Str(value.into())
}

pub async fn create(tx: &mut Transaction, model: &str, vals: RawVals) -> Result<Records> {
    let set = tx.env().records(model)?;
    set.create(tx, vec![vals]).await
}

/// A transaction with the mail note subtype registered under `mail.mt_note`, so creation lines and
/// tracked changes can be written as on an installed database.
pub async fn begin(h: &Harness) -> Transaction {
    let mut tx = h.begin().await.unwrap();
    let note = create(
        &mut tx,
        "mail.message.subtype",
        RawVals::new()
            .set_cell("name", text("Notes"))
            .set_cell("default", Cell::Bool(false))
            .set_cell("internal", Cell::Bool(true))
            .set_cell("sequence", Cell::Int(90)),
    )
    .await
    .unwrap();
    tx.flush().await.unwrap();
    tx.execute(
        "INSERT INTO ir_model_data (module, name, model, res_id) \
         VALUES ('mail', 'mt_note', 'mail.message.subtype', $1)",
        &[&note.ids()[0]],
    )
    .await
    .unwrap();
    tx
}

/// A stage; `project` empty makes it a template stage.
pub async fn stage(
    tx: &mut Transaction,
    name: &str,
    sequence: i32,
    fold: bool,
    project: Option<i64>,
) -> i64 {
    let mut vals = RawVals::new()
        .set_cell("name", text(name))
        .set_cell("sequence", Cell::Int(sequence))
        .set_cell("fold", Cell::Bool(fold));
    if let Some(project) = project {
        vals.insert("project_id".into(), Cell::Id(project));
    }
    create(tx, "project.task.type", vals).await.unwrap().ids()[0]
}

/// The four template stages of the shipped data: New, In Progress, Done (folded), Cancelled
/// (folded), created out of sequence order to prove the copies follow `sequence`.
pub async fn templates(tx: &mut Transaction) -> Vec<i64> {
    let done = stage(tx, "Done", 30, true, None).await;
    let new = stage(tx, "New", 10, false, None).await;
    let progress = stage(tx, "In Progress", 20, false, None).await;
    let cancelled = stage(tx, "Cancelled", 40, true, None).await;
    vec![new, progress, done, cancelled]
}

pub async fn partner(tx: &mut Transaction, name: &str) -> i64 {
    create(
        tx,
        "res.partner",
        RawVals::new().set_cell("name", text(name)),
    )
    .await
    .unwrap()
    .ids()[0]
}

pub async fn project(tx: &mut Transaction, name: &str) -> i64 {
    create(
        tx,
        "project.project",
        RawVals::new().set_cell("name", text(name)),
    )
    .await
    .unwrap()
    .ids()[0]
}

pub async fn project_for(tx: &mut Transaction, name: &str, customer: i64) -> i64 {
    create(
        tx,
        "project.project",
        RawVals::new()
            .set_cell("name", text(name))
            .set_cell("partner_id", Cell::Id(customer)),
    )
    .await
    .unwrap()
    .ids()[0]
}

/// Try a task creation inside a savepoint (a refusal leaves the transaction usable).
pub async fn try_task(tx: &mut Transaction, vals: RawVals) -> Result<i64> {
    tx.savepoint(async move |tx| {
        let created = create(tx, "project.task", vals).await?;
        tx.flush().await?;
        Ok(created.ids()[0])
    })
    .await
}

pub async fn task(tx: &mut Transaction, vals: RawVals) -> i64 {
    try_task(tx, vals).await.unwrap()
}

pub fn task_vals(name: &str) -> RawVals {
    RawVals::new().set_cell("name", text(name))
}

pub fn tasks(tx: &Transaction) -> Records {
    tx.env().records("project.task").unwrap()
}

pub fn projects(tx: &Transaction) -> Records {
    tx.env().records("project.project").unwrap()
}

pub fn stages(tx: &Transaction) -> Records {
    tx.env().records("project.task.type").unwrap()
}

/// A field of one record.
pub async fn cell(tx: &mut Transaction, model: &str, id: i64, field: &str) -> Cell {
    tx.env()
        .records(model)
        .unwrap()
        .browse([id])
        .unwrap()
        .cell(tx, field)
        .await
        .unwrap()
}

/// Write `vals` on the given records inside a savepoint.
pub async fn try_write(
    tx: &mut Transaction,
    model: &str,
    ids: &[i64],
    vals: RawVals,
) -> Result<()> {
    let set = tx.env().records(model)?.browse(ids.iter().copied())?;
    tx.savepoint(async move |tx| {
        set.write(tx, vals).await?;
        tx.flush().await
    })
    .await
}

/// The stages of `project` in board order: (name, sequence, fold).
pub async fn stages_of(tx: &mut Transaction, project: i64) -> Vec<(String, i32, bool)> {
    tx.flush().await.unwrap();
    tx.query(
        "SELECT name, sequence, fold FROM project_task_type WHERE project_id = $1 \
         ORDER BY sequence, id",
        &[&project],
    )
    .await
    .unwrap()
    .iter()
    .map(|row| (row.get(0), row.get(1), row.get(2)))
    .collect()
}

/// The stage ids of `project` in board order.
pub async fn stage_ids_of(tx: &mut Transaction, project: i64) -> Vec<i64> {
    tx.flush().await.unwrap();
    tx.query(
        "SELECT id::int8 FROM project_task_type WHERE project_id = $1 ORDER BY sequence, id",
        &[&project],
    )
    .await
    .unwrap()
    .iter()
    .map(|row| row.get(0))
    .collect()
}

pub fn assert_refused(result: Result<impl std::fmt::Debug>, needle: &str) -> Error {
    let error = result.expect_err("the operation must be refused");
    assert_eq!(
        error.kind(),
        erp_sdk::ErrorKind::Validation,
        "unexpected error kind: {error:?}"
    );
    assert!(
        error.message().contains(needle),
        "error {error:?} does not mention {needle:?}"
    );
    error
}
