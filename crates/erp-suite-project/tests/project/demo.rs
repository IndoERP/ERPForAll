// SPDX-License-Identifier: MIT OR Apache-2.0
//! Installing `project` with demo data: no data notice, each demo project has its own copy of the
//! template stages, the demo tasks sit in stages of their own project and are spread over several
//! of them, and a project user opens every window action of the addon, reads the task counts of
//! the demo projects and gets the task list of a project from its stat button.

use std::collections::BTreeMap;
use std::sync::Arc;

use erp_fields::{Cell, Value};
use erp_modules::InitOptions;
use erp_orm::rpc::{Exposure, RpcArgs};
use erp_orm::{Command, Context, RawVals, SUPERUSER_ID, UserId};
use erp_sdk::ValueMap;
use erp_testkit::TestDb;

use crate::support::{
    STATES, cell, policy_orm, project_module, real, records, run, stages_of, system_for_pool,
    template_stages, user, xid,
};

const DEMO_PROJECTS: [&str; 2] = ["project_demo_website", "project_demo_warehouse"];

async fn count(client: &tokio_postgres::Client, sql: &str, project: i64) -> i64 {
    client
        .query_one(sql, &[&project])
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
        .get(0)
}

fn key<'a>(value: &'a Value, name: &str) -> &'a Value {
    match value {
        Value::Map(map) => map
            .get(name)
            .unwrap_or_else(|| panic!("no {name:?} in {value:?}")),
        other => panic!("not a map: {other:?}"),
    }
}

fn int_cell(cell: &Cell) -> i64 {
    match cell {
        Cell::Int(n) => i64::from(*n),
        other => panic!("not an integer: {other:?}"),
    }
}

/// The counts of the overview's dashboard, read as the transaction's user: name → value.
async fn dashboard(
    tx: &mut erp_orm::Transaction,
    client: &tokio_postgres::Client,
) -> BTreeMap<String, String> {
    let action = xid(client, "project", "project_overview_action").await;
    let view = xid(client, "project", "project_task_overview_list").await;
    let tasks = tx.env().records("project.task").unwrap();
    let Exposure::Exposed(get_views) = tx
        .exposure()
        .get(tasks.model_id(), "get_views")
        .unwrap()
        .clone()
    else {
        panic!("get_views is exposed")
    };
    let views = get_views
        .call(
            tx,
            tasks.clone(),
            RpcArgs::new(ValueMap::from_iter([
                (
                    "views".into(),
                    Value::list([Value::list([Value::Int(view), Value::str("list")])]),
                ),
                ("wire_major".into(), Value::Int(1)),
                (
                    "options".into(),
                    Value::Map(Arc::new(ValueMap::from_iter([(
                        "mobile".into(),
                        Value::Bool(false),
                    )]))),
                ),
            ])),
        )
        .await
        .unwrap();
    let views = erp_fields::convert::value_to_json(&views).unwrap();
    let list = &views["views"][0];
    let board = list["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|node| node["kind"] == "dashboard")
        .unwrap_or_else(|| panic!("the overview list has a dashboard: {list}"));
    let revision = board["revision"].as_str().unwrap().to_owned();
    let view_id = list["identity"]["id"].as_i64().unwrap();
    let Exposure::Exposed(read) = tx
        .exposure()
        .get(tasks.model_id(), "erp_read_dashboard")
        .unwrap()
        .clone()
    else {
        panic!("erp_read_dashboard is exposed")
    };
    let answer = read
        .call(
            tx,
            tasks,
            RpcArgs::new(ValueMap::from_iter([
                ("action_id".into(), Value::Int(action)),
                ("view_id".into(), Value::Int(view_id)),
                ("revision".into(), Value::str(revision)),
                ("period".into(), Value::str("")),
                ("domain_scope".into(), Value::list([])),
            ])),
        )
        .await
        .unwrap_or_else(|e| panic!("reading the overview dashboard: {e}"));
    let answer = erp_fields::convert::value_to_json(&answer).unwrap();
    answer["values"]
        .as_array()
        .unwrap_or_else(|| panic!("no dashboard values: {answer}"))
        .iter()
        .map(|item| {
            (
                item["name"].as_str().unwrap().to_owned(),
                match &item["value"] {
                    serde_json::Value::String(text) => text.clone(),
                    other => other.to_string(),
                },
            )
        })
        .collect()
}

#[test]
fn project_demo_data_loads_cleanly_with_stages_per_project() {
    run(|| async {
        let db = TestDb::create().await.unwrap();
        let system = system_for_pool(db.pool().clone());
        let init = Box::pin(
            system.initialize(
                InitOptions::new()
                    .with_skip_auto_install(true)
                    .with_demo(true),
            ),
        )
        .await
        .unwrap();
        assert!(
            init.data_notices.is_empty(),
            "initialize notices: {:?}",
            init.data_notices
        );
        let env = init.orm.env(SUPERUSER_ID, Context::new());
        let outcome = Box::pin(system.button_immediate_install(&env, &[project_module()]))
            .await
            .unwrap();
        assert!(outcome.post_commit.is_clean(), "{:?}", outcome.post_commit);
        // A failed demo file is rolled back and reported here, not raised.
        assert!(
            outcome.data_notices.is_empty(),
            "demo notices: {:?}",
            outcome.data_notices
        );
        let client = db.client().await.unwrap();

        // The templates are untouched; each demo project has its own four stages.
        let templates: i64 = client
            .query_one(
                "SELECT count(*) FROM project_task_type WHERE project_id IS NULL",
                &[],
            )
            .await
            .unwrap()
            .get(0);
        assert_eq!(templates, 4, "the template stages stay templates");
        let mut projects = Vec::new();
        for name in DEMO_PROJECTS {
            let project = xid(&client, "project", name).await;
            assert_eq!(
                stages_of(&client, project).await,
                template_stages(),
                "{name} has its own copy of the template stages"
            );
            projects.push((name, project));
        }

        // Every demo task has a project (subtasks take their parent's) and a stage of that very
        // project, and a valid state.
        let stray: Vec<String> = client
            .query(
                "SELECT t.name::text FROM project_task t \
                 LEFT JOIN project_task_type s ON s.id = t.stage_id \
                 WHERE t.project_id IS NULL OR t.stage_id IS NULL \
                 OR s.project_id IS DISTINCT FROM t.project_id",
                &[],
            )
            .await
            .unwrap()
            .iter()
            .map(|row| row.get(0))
            .collect();
        assert!(
            stray.is_empty(),
            "tasks without a project or in a stage of another project (or a template): {stray:?}"
        );
        let states: Vec<String> = client
            .query("SELECT DISTINCT state::text FROM project_task", &[])
            .await
            .unwrap()
            .iter()
            .map(|row| row.get(0))
            .collect();
        for state in &states {
            assert!(STATES.contains(&state.as_str()), "unknown state {state}");
        }

        // The tasks of each project are spread over several of its stages.
        for (name, project) in &projects {
            let tasks = count(
                &client,
                "SELECT count(*) FROM project_task WHERE project_id = $1::int8",
                *project,
            )
            .await;
            assert!(tasks > 0, "{name} has demo tasks");
            let used = count(
                &client,
                "SELECT count(DISTINCT stage_id) FROM project_task WHERE project_id = $1::int8",
                *project,
            )
            .await;
            assert!(used >= 2, "{name} tasks use {used} stage(s)");
        }

        // A project user (and the administrator the user table needs first).
        let company = xid(&client, "base", "main_company").await;
        let group_system = xid(&client, "base", "group_system").await;
        let group_user = xid(&client, "base", "group_user").await;
        let group_project_user = xid(&client, "project", "group_project_user").await;
        let mut setup = outcome.orm.begin(SUPERUSER_ID).await.unwrap();
        let created = setup
            .env()
            .records("res.users")
            .unwrap()
            .create(
                &mut setup,
                vec![
                    user("project-demo-admin", company, &[group_system, group_user]),
                    user(
                        "project-demo-user",
                        company,
                        &[group_user, group_project_user],
                    ),
                ],
            )
            .await
            .unwrap()
            .ids();
        let project_user = created[1];
        // one open demo task is the project user's, for the "my open tasks" count
        let wireframes = xid(&client, "project", "task_demo_wireframes").await;
        records(&setup, "project.task", wireframes)
            .write(
                &mut setup,
                RawVals::new().set_commands("user_ids", [Command::Set(vec![real(project_user)])]),
            )
            .await
            .unwrap();
        setup.commit().await.unwrap();
        drop(setup);

        let orm = policy_orm(&db, &outcome.orm).await;
        let mut tx = orm
            .begin_with(
                UserId::new(project_user).unwrap(),
                Context::new().with_company_id(company),
            )
            .await
            .unwrap();

        // Every window action of the addon opens for the project user.
        let actions: Vec<(String, i64)> = client
            .query(
                "SELECT name::text, res_id::int8 FROM ir_model_data \
                 WHERE module = 'project' AND model = 'ir.actions.act_window' ORDER BY name",
                &[],
            )
            .await
            .unwrap()
            .iter()
            .map(|row| (row.get(0), row.get(1)))
            .collect();
        for expected in [
            "project_overview_action",
            "project_project_action",
            "project_task_action",
            "project_task_action_my",
            "project_task_type_action",
            "project_tags_action",
        ] {
            assert!(
                actions.iter().any(|(name, _)| name == expected),
                "missing window action {expected}: {actions:?}"
            );
        }
        let act_window = tx.env().records("ir.actions.act_window").unwrap();
        for (xmlid, id) in &actions {
            let _: Value = act_window
                .browse([*id])
                .unwrap()
                .call(
                    &mut tx,
                    "erp_load_web_action_views",
                    (
                        1u64,
                        Option::<erp_sdk::ValueMap>::None,
                        Option::<bool>::None,
                    ),
                )
                .await
                .unwrap_or_else(|e| panic!("window action project.{xmlid} failed to open: {e}"));
        }

        // The task counts as the project user: every task, and the tasks not done or cancelled.
        for (name, project) in &projects {
            let all = count(
                &client,
                "SELECT count(*) FROM project_task WHERE project_id = $1::int8 AND active",
                *project,
            )
            .await;
            let open = count(
                &client,
                "SELECT count(*) FROM project_task WHERE project_id = $1::int8 AND active \
                 AND state NOT IN ('done', 'cancelled')",
                *project,
            )
            .await;
            assert_eq!(
                int_cell(&cell(&mut tx, "project.project", *project, "task_count").await),
                all,
                "{name} task_count"
            );
            assert_eq!(
                int_cell(&cell(&mut tx, "project.project", *project, "open_task_count").await),
                open,
                "{name} open_task_count"
            );
        }

        // The stat button opens the project's tasks.
        let (_, website) = projects[0];
        let project = records(&tx, "project.project", website);
        let action: Value = project
            .call(&mut tx, "action_view_tasks", ())
            .await
            .unwrap();
        assert_eq!(key(&action, "type"), &Value::str("ir.actions.act_window"));
        assert_eq!(key(&action, "res_model"), &Value::str("project.task"));

        // The overview's counts as the project user, against the same counts in SQL.
        let counts = dashboard(&mut tx, &client).await;
        let open = "t.active AND t.state NOT IN ('done', 'cancelled')";
        let mine = "EXISTS (SELECT 1 FROM project_task_user_rel r \
                    WHERE r.task_id = t.id AND r.user_id = $1::int8)";
        let assigned = "EXISTS (SELECT 1 FROM project_task_user_rel r WHERE r.task_id = t.id)";
        // the `today` of a context without a timezone
        let today = "(now() AT TIME ZONE 'UTC')::date";
        for (name, condition) in [
            ("open", open.to_owned()),
            ("my_open", format!("{open} AND {mine}")),
            ("overdue", format!("{open} AND t.date_deadline < {today}")),
            (
                "changes_requested",
                "t.active AND t.state = 'changes_requested'".to_owned(),
            ),
            ("unassigned", format!("{open} AND NOT {assigned}")),
            ("important", format!("{open} AND t.priority = '1'")),
        ] {
            let sql = format!(
                "SELECT count(*) FROM project_task t WHERE {condition} AND $1::int8 IS NOT NULL"
            );
            let expected = count(&client, &sql, project_user).await;
            assert_eq!(
                counts.get(name).map(String::as_str),
                Some(expected.to_string().as_str()),
                "overview count {name}: {counts:?}"
            );
        }
        assert!(
            counts.get("my_open").is_some_and(|value| value == "1"),
            "the project user's task is counted: {counts:?}"
        );

        tx.rollback().await.unwrap();
        drop(tx);
        drop(orm);

        drop(outcome);
        drop(init);
        drop(system);
        drop(client);
        db.cleanup().await.unwrap();
    });
}
