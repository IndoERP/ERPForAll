// SPDX-License-Identifier: MIT OR Apache-2.0
//! `project.task.type`: a stage tasks move through. A stage belongs to one project for its
//! lifetime; a stage without a project is a template that new projects copy.

use erp_sdk::prelude::*;

/// A task stage of one project, or a template stage when `project_id` is empty.
#[model(
    name = "project.task.type",
    description = "Task Stage",
    order = "sequence, id"
)]
pub struct ProjectTaskType {
    /// Stage name.
    #[field(char, required = true, index = true, string = "Name")]
    pub name: Option<Arc<str>>,
    /// Board order.
    #[field(integer, default = 10, string = "Sequence")]
    pub sequence: i32,
    /// Finished or abandoned work: meant to be drawn folded on the task board, and never the
    /// stage a new task starts in while an unfolded stage exists.
    #[field(boolean, default = false, string = "Folded in Kanban")]
    pub fold: bool,
    /// What the stage means.
    #[field(text, string = "Description")]
    pub description: Option<Arc<str>>,
    /// The project the stage belongs to; empty for a template stage. Fixed after creation.
    #[field(
        many2one,
        comodel = "project.project",
        index = true,
        ondelete = "cascade",
        string = "Project"
    )]
    pub project_id: Option<RecId>,
}

#[methods(ProjectTaskType, addon = "project")]
impl ProjectTaskType {
    #[method(override)]
    async fn write(
        this: &RecordSet<ProjectTaskType>,
        tx: &mut Transaction,
        vals: RawVals,
        sup: Super,
    ) -> Result<()> {
        if let Some(new) = vals.get("project_id") {
            let new = as_project(new)?;
            for stage in this.as_records().iter() {
                if as_project(&stage.cell(tx, "project_id").await?)? != new {
                    return Err(Error::validation(
                        "A stage belongs to one project for its lifetime: its project_id cannot \
                         change. Create the stage in the other project instead.",
                    )
                    .with_fields(["project_id"]));
                }
            }
        }
        sup.call(tx, this.as_records(), (vals,)).await
    }
}

/// A project cell as an optional id: `false` and NULL are both "no project".
fn as_project(cell: &Cell) -> Result<Option<i64>> {
    match cell {
        Cell::Id(id) => Ok(Some(*id)),
        Cell::Null | Cell::Bool(false) => Ok(None),
        other => Err(Error::validation(format!(
            "A stage's project_id must be a project id or empty, not {other:?}."
        ))
        .with_fields(["project_id"])),
    }
}

/// The stage a new task of `project` starts in: unfolded stages first, then by board order.
pub(crate) async fn first_stage(
    env: &Env,
    tx: &mut Transaction,
    project: i64,
) -> Result<Option<i64>> {
    Ok(env
        .records("project.task.type")?
        .search_with(
            tx,
            &Domain::field("project_id")?.eq(project),
            &erp_sdk::SearchOptions::new()
                .order("fold, sequence, id")
                .limit(1),
        )
        .await?
        .ids()
        .first()
        .copied())
}
