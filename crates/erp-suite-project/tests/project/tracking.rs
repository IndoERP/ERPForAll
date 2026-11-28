// SPDX-License-Identifier: MIT OR Apache-2.0
//! The task chatter on an installed `project` (mail installed with it): a project user who moves
//! a task to another stage of its project writes one history (`tracking`) message whose changes
//! name `stage_id` with the old and the new stage; the move stamps the last stage update and
//! puts the task back in progress (D3).

use erp_fields::Cell;
use erp_orm::{RawVals, SUPERUSER_ID, UserId};

use crate::support::{cell, cloned_installed, create, named, policy_orm, records, run, user, xid};

/// The `tracking` changes of every history message of a task, oldest first.
async fn history(client: &tokio_postgres::Client, task: i64) -> Vec<serde_json::Value> {
    client
        .query(
            "SELECT tracking::text FROM mail_message \
             WHERE model = 'project.task' AND res_id = $1::int8 AND message_type = 'tracking' \
             ORDER BY id",
            &[&task],
        )
        .await
        .unwrap()
        .iter()
        .map(|row| {
            row.get::<_, Option<String>>(0)
                .map_or(serde_json::Value::Null, |text| {
                    serde_json::from_str(&text).unwrap()
                })
        })
        .collect()
}

/// The change of `field` in a message's `tracking` array, if any.
fn change<'a>(tracking: &'a serde_json::Value, field: &str) -> Option<&'a serde_json::Value> {
    tracking
        .as_array()?
        .iter()
        .find(|entry| entry["n"].as_str() == Some(field))
}

#[test]
fn a_stage_move_writes_a_tracking_message() {
    run(|| async {
        let (db, installed) = cloned_installed().await;
        let client = db.client().await.unwrap();
        let company = xid(&client, "base", "main_company").await;
        let group_system = xid(&client, "base", "group_system").await;
        let group_user = xid(&client, "base", "group_user").await;
        let group_project_user = xid(&client, "project", "group_project_user").await;

        let mut tx = installed.begin(SUPERUSER_ID).await.unwrap();
        let project = create(&mut tx, "project.project", named("Tracked Project")).await;
        let users = tx
            .env()
            .records("res.users")
            .unwrap()
            .create(
                &mut tx,
                vec![
                    user("tracking-admin", company, &[group_system, group_user]),
                    user("tracking-user", company, &[group_user, group_project_user]),
                ],
            )
            .await
            .unwrap()
            .ids();
        tx.commit().await.unwrap();
        drop(tx);
        let project_user = UserId::new(users[1]).unwrap();
        let stages: Vec<i64> = client
            .query(
                "SELECT id::int8 FROM project_task_type WHERE project_id = $1::int8 \
                 ORDER BY sequence, id",
                &[&project],
            )
            .await
            .unwrap()
            .iter()
            .map(|row| row.get(0))
            .collect();
        assert_eq!(stages.len(), 4, "the project has its four stages");

        let orm = policy_orm(&db, &installed).await;
        // The user creates a task (it lands in the project's first stage) and approves it.
        let mut tx = orm.begin(project_user).await.unwrap();
        let task = create(
            &mut tx,
            "project.task",
            named("Tracked Task").set_cell("project_id", Cell::Id(project)),
        )
        .await;
        assert_eq!(
            cell(&mut tx, "project.task", task, "stage_id").await,
            Cell::Id(stages[0])
        );
        records(&tx, "project.task", task)
            .write(
                &mut tx,
                RawVals::new().set_cell("state", Cell::Str("approved".into())),
            )
            .await
            .unwrap();
        tx.commit().await.unwrap();
        drop(tx);
        let before = history(&client, task).await;
        let stamped_new: Option<String> = client
            .query_one(
                "SELECT date_last_stage_update::text FROM project_task WHERE id = $1::int8",
                &[&task],
            )
            .await
            .unwrap()
            .get(0);
        assert!(
            stamped_new.is_some(),
            "a new task carries its last stage update"
        );
        // back-date the stamp, so the move must write a strictly newer one
        client
            .execute(
                "UPDATE project_task SET date_last_stage_update = '2020-01-01 00:00:00' \
                 WHERE id = $1::int8",
                &[&task],
            )
            .await
            .unwrap();

        // The user moves it to the second stage.
        let mut tx = orm.begin(project_user).await.unwrap();
        records(&tx, "project.task", task)
            .write(
                &mut tx,
                RawVals::new().set_cell("stage_id", Cell::Id(stages[1])),
            )
            .await
            .unwrap();
        tx.commit().await.unwrap();
        drop(tx);

        let after = history(&client, task).await;
        assert_eq!(
            after.len(),
            before.len() + 1,
            "one history message for the move: {after:?}"
        );
        let moved = change(&after[after.len() - 1], "stage_id")
            .unwrap_or_else(|| panic!("the history message names stage_id: {after:?}"));
        assert_eq!(moved["o"], serde_json::json!(stages[0]), "{moved}");
        assert_eq!(moved["v"], serde_json::json!(stages[1]), "{moved}");

        // The move put the task back in progress and stamped the last stage update.
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        assert_eq!(
            cell(&mut tx, "project.task", task, "state").await,
            Cell::Str("in_progress".into()),
            "a real stage change resets the state (D3)"
        );
        tx.rollback().await.unwrap();
        drop(tx);
        let newer: bool = client
            .query_one(
                "SELECT date_last_stage_update > '2020-01-01 00:00:00' FROM project_task \
                 WHERE id = $1::int8",
                &[&task],
            )
            .await
            .unwrap()
            .get(0);
        assert!(newer, "the move stamps the last stage update anew");

        drop(orm);
        drop(installed);
        drop(client);
        db.cleanup().await.unwrap();
    });
}
