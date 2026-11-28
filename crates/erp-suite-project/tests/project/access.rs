// SPDX-License-Identifier: MIT OR Apache-2.0
//! Access as real users on an installed `project`: a project user creates and edits tasks and
//! creates tags, but neither creates projects or stages nor deletes tasks; a project manager does
//! all three; a project of one company, its tasks and its stages are hidden from a project user of
//! another company, while the shared project and the template stages stay visible.

use erp_fields::{Cell, ErrorKind};
use erp_orm::{Command, Orm, RawVals, SUPERUSER_ID, Transaction, UserId};
use erp_sdk::{BeginOptions, Domain, OperationIdentity, Operator};

use crate::support::{
    cell, cloned_installed, create, ids, named, other_company, policy_orm, records, run, user, xid,
};

/// The seeded rows and users.
struct Seed {
    /// A project of the main company and one without a company.
    own_project: i64,
    shared_project: i64,
    /// One task in each.
    own_task: i64,
    shared_task: i64,
    project_user: i64,
    manager: i64,
    /// A project user of the other company only.
    foreign_user: i64,
}

async fn seed(client: &tokio_postgres::Client, orm: &Orm) -> Seed {
    let company = xid(client, "base", "main_company").await;
    let group_system = xid(client, "base", "group_system").await;
    let group_user = xid(client, "base", "group_user").await;
    let group_project_user = xid(client, "project", "group_project_user").await;
    let group_project_manager = xid(client, "project", "group_project_manager").await;
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let foreign_company = other_company(&mut tx, company, "Other Company").await;
    let own_project = create(
        &mut tx,
        "project.project",
        named("Own Project").set_cell("company_id", Cell::Id(company)),
    )
    .await;
    let shared_project = create(&mut tx, "project.project", named("Shared Project")).await;
    let task = |name: &str, project: i64| named(name).set_cell("project_id", Cell::Id(project));
    let own_task = create(&mut tx, "project.task", task("Own Task", own_project)).await;
    let shared_task = create(&mut tx, "project.task", task("Shared Task", shared_project)).await;
    // The base addon refuses a user table without an administrator, so one comes first.
    let users = tx
        .env()
        .records("res.users")
        .unwrap()
        .create(
            &mut tx,
            vec![
                user("project-admin", company, &[group_system, group_user]),
                user("project-user", company, &[group_user, group_project_user]),
                user(
                    "project-manager",
                    company,
                    &[group_user, group_project_manager],
                ),
                user(
                    "project-foreign-user",
                    foreign_company,
                    &[group_user, group_project_user],
                ),
            ],
        )
        .await
        .unwrap()
        .ids();
    tx.commit().await.unwrap();
    Seed {
        own_project,
        shared_project,
        own_task,
        shared_task,
        project_user: users[1],
        manager: users[2],
        foreign_user: users[3],
    }
}

/// A transaction of `uid` with an operation identity, as a served call has one: deleting a
/// record with a chatter is an eventful mutation.
async fn begin(orm: &Orm, uid: i64) -> Transaction {
    orm.begin_opts(
        BeginOptions::new(UserId::new(uid).unwrap())
            .operation_identity(OperationIdentity::generate().unwrap()),
    )
    .await
    .unwrap()
}

/// `outcome` is an access refusal; `what` names the attempt.
fn refused<T: std::fmt::Debug>(outcome: erp_fields::Result<T>, what: &str) {
    match outcome {
        Err(err) => assert_eq!(err.kind(), ErrorKind::Access, "{what}: {err}"),
        Ok(value) => panic!("{what} was allowed: {value:?}"),
    }
}

async fn search(tx: &mut Transaction, model: &str, among: &[i64]) -> Vec<i64> {
    let mut found = tx
        .env()
        .records(model)
        .unwrap()
        .search(
            tx,
            &Domain::condition("id", Operator::In, among.to_vec()).unwrap(),
        )
        .await
        .unwrap_or_else(|e| panic!("searching {model}: {e}"))
        .ids();
    found.sort_unstable();
    found
}

/// The stages of `project`, or the templates for `None`.
async fn stage_ids(client: &tokio_postgres::Client, project: Option<i64>) -> Vec<i64> {
    client
        .query(
            "SELECT id::int8 FROM project_task_type WHERE project_id IS NOT DISTINCT FROM $1::int8 \
             ORDER BY id",
            &[&project],
        )
        .await
        .unwrap()
        .iter()
        .map(|row| row.get(0))
        .collect()
}

#[test]
fn a_project_user_works_on_tasks_and_a_manager_configures_projects() {
    run(|| async {
        let (db, installed) = cloned_installed().await;
        let client = db.client().await.unwrap();
        let seed = seed(&client, &installed).await;
        let orm = policy_orm(&db, &installed).await;

        // The project user creates and edits a task, creates a tag and edits a project.
        let mut tx = begin(&orm, seed.project_user).await;
        let task = create(
            &mut tx,
            "project.task",
            named("Written by a user").set_cell("project_id", Cell::Id(seed.own_project)),
        )
        .await;
        records(&tx, "project.task", task)
            .write(&mut tx, named("Renamed by a user"))
            .await
            .unwrap();
        records(&tx, "project.task", seed.shared_task)
            .write(
                &mut tx,
                RawVals::new().set_cell("priority", Cell::Str("1".into())),
            )
            .await
            .unwrap();
        create(&mut tx, "project.tags", named("Created by a user")).await;
        records(&tx, "project.project", seed.own_project)
            .write(&mut tx, named("Own Project (renamed)"))
            .await
            .unwrap();
        tx.commit().await.unwrap();
        drop(tx);

        // ... but creates no project and no stage, and deletes no task.
        let mut tx = begin(&orm, seed.project_user).await;
        let outcome = tx
            .env()
            .records("project.project")
            .unwrap()
            .create(&mut tx, vec![named("Refused Project")])
            .await;
        refused(outcome, "a project user creating a project");
        tx.rollback().await.unwrap();
        drop(tx);
        let mut tx = begin(&orm, seed.project_user).await;
        let outcome = tx
            .env()
            .records("project.task.type")
            .unwrap()
            .create(
                &mut tx,
                vec![named("Refused Stage").set_cell("project_id", Cell::Id(seed.own_project))],
            )
            .await;
        refused(outcome, "a project user creating a stage");
        tx.rollback().await.unwrap();
        drop(tx);
        let mut tx = begin(&orm, seed.project_user).await;
        let outcome = records(&tx, "project.task", task).unlink(&mut tx).await;
        refused(outcome, "a project user deleting a task");
        tx.rollback().await.unwrap();
        drop(tx);

        // The manager does all three; the new project gets its own four stages.
        let mut tx = begin(&orm, seed.manager).await;
        let project = create(&mut tx, "project.project", named("Manager Project")).await;
        let stages = ids(&cell(&mut tx, "project.project", project, "type_ids").await);
        assert_eq!(stages.len(), 4, "a manager's new project has four stages");
        create(
            &mut tx,
            "project.task.type",
            named("Review").set_cell("project_id", Cell::Id(seed.own_project)),
        )
        .await;
        records(&tx, "project.task", task)
            .unlink(&mut tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        drop(tx);
        let left: i64 = client
            .query_one(
                "SELECT count(*) FROM project_task WHERE id = $1::int8",
                &[&task],
            )
            .await
            .unwrap()
            .get(0);
        assert_eq!(left, 0, "the manager deleted the task");

        drop(orm);
        drop(installed);
        drop(client);
        db.cleanup().await.unwrap();
    });
}

#[test]
fn a_user_of_another_company_does_not_reach_the_project_its_tasks_or_stages() {
    run(|| async {
        let (db, installed) = cloned_installed().await;
        let client = db.client().await.unwrap();
        let seed = seed(&client, &installed).await;
        let orm = policy_orm(&db, &installed).await;
        let templates = stage_ids(&client, None).await;
        let own_stages = stage_ids(&client, Some(seed.own_project)).await;
        let shared_stages = stage_ids(&client, Some(seed.shared_project)).await;
        assert_eq!(templates.len(), 4);
        assert_eq!(own_stages.len(), 4);
        assert_eq!(shared_stages.len(), 4);
        let all_stages: Vec<i64> = templates
            .iter()
            .chain(&own_stages)
            .chain(&shared_stages)
            .copied()
            .collect();

        // The user of the main company reaches both projects, both tasks and every stage.
        let mut tx = begin(&orm, seed.project_user).await;
        let projects = [seed.own_project, seed.shared_project];
        let tasks = [seed.own_task, seed.shared_task];
        let mut every_project = projects.to_vec();
        every_project.sort_unstable();
        let mut every_task = tasks.to_vec();
        every_task.sort_unstable();
        let mut every_stage = all_stages.clone();
        every_stage.sort_unstable();
        assert_eq!(
            search(&mut tx, "project.project", &projects).await,
            every_project
        );
        assert_eq!(search(&mut tx, "project.task", &tasks).await, every_task);
        assert_eq!(
            search(&mut tx, "project.task.type", &all_stages).await,
            every_stage
        );
        tx.rollback().await.unwrap();
        drop(tx);

        // The user of the other company reaches the shared project, its task, its stages and the
        // templates only.
        let mut tx = begin(&orm, seed.foreign_user).await;
        assert_eq!(
            search(&mut tx, "project.project", &projects).await,
            [seed.shared_project]
        );
        assert_eq!(
            search(&mut tx, "project.task", &tasks).await,
            [seed.shared_task]
        );
        let mut visible: Vec<i64> = templates.iter().chain(&shared_stages).copied().collect();
        visible.sort_unstable();
        assert_eq!(
            search(&mut tx, "project.task.type", &all_stages).await,
            visible,
            "the stages of a hidden project are hidden too"
        );
        tx.rollback().await.unwrap();
        drop(tx);

        // Reading the hidden project or its task by id is refused, not answered empty.
        for (model, id) in [
            ("project.project", seed.own_project),
            ("project.task", seed.own_task),
        ] {
            let mut tx = begin(&orm, seed.foreign_user).await;
            let outcome = records(&tx, model, id).read(&mut tx, &["name"]).await;
            refused(
                outcome,
                &format!("a user of another company reading {model} {id}"),
            );
            tx.rollback().await.unwrap();
        }

        drop(orm);
        drop(installed);
        drop(client);
        db.cleanup().await.unwrap();
    });
}

/// Try `work` as `uid` in its own transaction, rolled back; returns the outcome.
async fn attempt<T, F>(orm: &Orm, uid: i64, work: F) -> erp_fields::Result<T>
where
    F: AsyncFnOnce(&mut Transaction) -> erp_fields::Result<T>,
{
    let mut tx = begin(orm, uid).await;
    let outcome = match work(&mut tx).await {
        Ok(value) => tx.flush().await.map(|()| value),
        Err(err) => Err(err),
    };
    tx.rollback().await.unwrap();
    outcome
}

#[test]
fn a_project_user_configures_nothing_through_another_door() {
    run(|| async {
        let (db, installed) = cloned_installed().await;
        let client = db.client().await.unwrap();
        let seed = seed(&client, &installed).await;
        let stages = stage_ids(&client, Some(seed.own_project)).await;
        let foreign_company: i64 = client
            .query_one(
                "SELECT id::int8 FROM res_company WHERE name = 'Other Company'",
                &[],
            )
            .await
            .unwrap()
            .get(0);
        let mut tx = installed.begin(SUPERUSER_ID).await.unwrap();
        let tag = create(&mut tx, "project.tags", named("Managed Tag")).await;
        tx.commit().await.unwrap();
        drop(tx);
        let orm = policy_orm(&db, &installed).await;
        let user = seed.project_user;
        let project = seed.own_project;

        // stages through the project's Stages page: create, update, delete
        let stage = erp_fields::RecId::from_real(stages[0]).unwrap();
        for (what, command) in [
            (
                "creating a stage through type_ids",
                Command::create(named("Sneaky Stage")),
            ),
            (
                "renaming a stage through type_ids",
                Command::Update(stage, named("Renamed Stage")),
            ),
            ("deleting a stage through type_ids", Command::Delete(stage)),
        ] {
            let outcome = attempt(&orm, user, async |tx: &mut Transaction| {
                records(tx, "project.project", project)
                    .write(tx, RawVals::new().set_commands("type_ids", [command]))
                    .await
            })
            .await;
            refused(outcome, what);
        }
        // stages directly
        let outcome = attempt(&orm, user, async |tx: &mut Transaction| {
            records(tx, "project.task.type", stages[0])
                .write(tx, named("Renamed Stage"))
                .await
        })
        .await;
        refused(outcome, "a project user renaming a stage");
        let outcome = attempt(&orm, user, async |tx: &mut Transaction| {
            records(tx, "project.task.type", stages[3]).unlink(tx).await
        })
        .await;
        refused(outcome, "a project user deleting a stage");
        // tags: created, never changed
        let outcome = attempt(&orm, user, async |tx: &mut Transaction| {
            records(tx, "project.tags", tag)
                .write(tx, named("Renamed Tag"))
                .await
        })
        .await;
        refused(outcome, "a project user renaming a tag");
        // projects: updated, never deleted nor moved to a company the user is not in
        let outcome = attempt(&orm, user, async |tx: &mut Transaction| {
            records(tx, "project.project", seed.shared_project)
                .unlink(tx)
                .await
        })
        .await;
        refused(outcome, "a project user deleting a project");
        let outcome = attempt(&orm, user, async |tx: &mut Transaction| {
            records(tx, "project.project", project)
                .write(
                    tx,
                    RawVals::new().set_cell("company_id", Cell::Id(foreign_company)),
                )
                .await
        })
        .await;
        assert!(
            outcome.is_err(),
            "a project user moving a project to a company outside theirs was allowed"
        );

        // nothing changed
        assert_eq!(stage_ids(&client, Some(project)).await, stages);
        let names: Vec<String> = client
            .query(
                "SELECT name::text FROM project_task_type WHERE id = ANY($1::int8[]) ORDER BY id",
                &[&stages],
            )
            .await
            .unwrap()
            .iter()
            .map(|row| row.get(0))
            .collect();
        assert!(
            !names.iter().any(|name| name.contains("Renamed")),
            "{names:?}"
        );
        let company: Option<i64> = client
            .query_one(
                "SELECT company_id::int8 FROM project_project WHERE id = $1::int8",
                &[&project],
            )
            .await
            .unwrap()
            .get(0);
        assert_ne!(company, Some(foreign_company));

        drop(orm);
        drop(installed);
        drop(client);
        db.cleanup().await.unwrap();
    });
}

#[test]
fn tasks_stay_in_their_company() {
    run(|| async {
        let (db, installed) = cloned_installed().await;
        let client = db.client().await.unwrap();
        let seed = seed(&client, &installed).await;
        let company = xid(&client, "base", "main_company").await;
        let own_stages = stage_ids(&client, Some(seed.own_project)).await;
        let orm = policy_orm(&db, &installed).await;

        // a task without a project, created in the main company, belongs to it
        let mut tx = begin(&orm, seed.project_user).await;
        let loose = create(&mut tx, "project.task", named("Loose Task")).await;
        assert_eq!(
            cell(&mut tx, "project.task", loose, "company_id").await,
            Cell::Id(company)
        );
        tx.commit().await.unwrap();
        drop(tx);

        // ... and is hidden from the user of the other company
        let mut tx = begin(&orm, seed.foreign_user).await;
        assert!(
            search(&mut tx, "project.task", &[loose]).await.is_empty(),
            "a project-less task of the main company is hidden from the other company"
        );
        tx.rollback().await.unwrap();
        drop(tx);
        let outcome = attempt(&orm, seed.foreign_user, async |tx: &mut Transaction| {
            records(tx, "project.task", loose)
                .write(tx, named("Taken over"))
                .await
        })
        .await;
        refused(
            outcome,
            "a user of another company writing a project-less task",
        );

        // the other company's user creates no task in a hidden project
        let outcome = attempt(&orm, seed.foreign_user, async |tx: &mut Transaction| {
            tx.env()
                .records("project.task")?
                .create(
                    tx,
                    vec![named("Smuggled").set_cell("project_id", Cell::Id(seed.own_project))],
                )
                .await
        })
        .await;
        assert!(
            outcome.is_err(),
            "a task in a hidden project was created: {outcome:?}"
        );
        // ... and moves no task into a hidden project or its stage
        for vals in [
            RawVals::new().set_cell("stage_id", Cell::Id(own_stages[1])),
            RawVals::new()
                .set_cell("project_id", Cell::Id(seed.own_project))
                .set_cell("stage_id", Cell::Id(own_stages[1])),
            RawVals::new().set_cell("project_id", Cell::Id(seed.own_project)),
        ] {
            let outcome = attempt(&orm, seed.foreign_user, async |tx: &mut Transaction| {
                records(tx, "project.task", seed.shared_task)
                    .write(tx, vals.clone())
                    .await
            })
            .await;
            assert!(
                outcome.is_err(),
                "the shared task moved into a hidden project with {vals:?}"
            );
        }
        let row = client
            .query_one(
                "SELECT project_id::int8, stage_id::int8 FROM project_task WHERE id = $1::int8",
                &[&seed.shared_task],
            )
            .await
            .unwrap();
        assert_eq!(row.get::<_, Option<i64>>(0), Some(seed.shared_project));
        assert!(
            !own_stages.contains(&row.get::<_, i64>(1)),
            "the shared task stays in its own stages"
        );

        drop(orm);
        drop(installed);
        drop(client);
        db.cleanup().await.unwrap();
    });
}

#[test]
fn the_tasks_of_an_archived_project_keep_their_stage() {
    run(|| async {
        let (db, installed) = cloned_installed().await;
        let client = db.client().await.unwrap();
        let seed = seed(&client, &installed).await;
        let own_stages = stage_ids(&client, Some(seed.own_project)).await;
        let orm = policy_orm(&db, &installed).await;

        let mut tx = begin(&orm, seed.manager).await;
        records(&tx, "project.project", seed.own_project)
            .write(
                &mut tx,
                RawVals::new().set_cell("active", Cell::Bool(false)),
            )
            .await
            .unwrap();
        tx.commit().await.unwrap();
        drop(tx);

        let mut tx = begin(&orm, seed.project_user).await;
        let task = records(&tx, "project.task", seed.own_task);
        let stage = task.related(&mut tx, "stage_id").await.unwrap();
        assert_eq!(stage.ids(), [own_stages[0]]);
        assert_eq!(
            stage.cell(&mut tx, "name").await.unwrap(),
            Cell::Str("New".into()),
            "the stage of an archived project's task is readable"
        );
        assert_eq!(
            search(&mut tx, "project.task.type", &own_stages).await,
            own_stages,
            "the stages of an archived project stay readable"
        );
        // and the task still moves between them
        task.write(
            &mut tx,
            RawVals::new().set_cell("stage_id", Cell::Id(own_stages[1])),
        )
        .await
        .unwrap();
        tx.rollback().await.unwrap();
        drop(tx);

        drop(orm);
        drop(installed);
        drop(client);
        db.cleanup().await.unwrap();
    });
}
