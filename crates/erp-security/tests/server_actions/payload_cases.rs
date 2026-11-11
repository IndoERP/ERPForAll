//! Stored payload bounds: SQL must refuse oversized data before the driver receives it.

use super::*;

// Fixed contract values, deliberately independent of the implementation constants.
const FIELD_BYTES: usize = 65_536;
const GROUPS: i64 = 256;
const TREE_BYTES: usize = 1_048_576;

fn payload_limit(error: &Error, message: &str, kind: RunLimit) {
    assert_eq!(error.kind(), ErrorKind::UserError);
    assert_eq!(error.message(), message);
    assert_eq!(typed(error), &ServerActionError::Limit { kind });
}

#[tokio::test]
async fn stored_field_payload_is_bounded_for_allowed_and_denied_callers() {
    let h = harness().await;
    let client = connect(&h).await;
    let seeded = seed(&client).await;
    let orm = policy_orm(&h).await;
    let id = action(
        &h,
        "Bounded",
        seeded.partner_model,
        "x_open_window",
        RawVals::new(),
    )
    .await;

    // UTF-8 byte count, not character count; the boundary is accepted without truncation.
    let boundary = "é".repeat(FIELD_BYTES / 2);
    client
        .execute(
            "UPDATE ir_act_server SET value = $2 WHERE id = $1",
            &[&id, &boundary],
        )
        .await
        .unwrap();
    assert_eq!(
        run(&orm, seeded.demo, id, "x.partner", &[])
            .await
            .unwrap()
            .unwrap()
            .action_type(),
        "ir.actions.act_window"
    );
    let over = format!("{boundary}x");
    client
        .execute(
            "UPDATE ir_act_server SET value = $2 WHERE id = $1",
            &[&id, &over],
        )
        .await
        .unwrap();
    payload_limit(
        &run(&orm, seeded.demo, id, "x.partner", &[])
            .await
            .unwrap_err(),
        "The server action has more than 65536 bytes in a stored field; the run is refused.",
        RunLimit::FieldBytes,
    );

    // A caller outside the action's groups receives the same bounded, payload-free refusal.
    client
        .execute(
            "INSERT INTO ir_act_server_group_rel (act_id, gid) VALUES ($1, $2)",
            &[&id, &seeded.group_runner],
        )
        .await
        .unwrap();
    payload_limit(
        &run(&orm, seeded.demo, id, "x.partner", &[])
            .await
            .unwrap_err(),
        "The server action has more than 65536 bytes in a stored field; the run is refused.",
        RunLimit::FieldBytes,
    );
    client
        .execute(
            "UPDATE ir_act_server SET value = NULL WHERE id = $1",
            &[&id],
        )
        .await
        .unwrap();
    assert_typed(
        &run(&orm, seeded.demo, id, "x.partner", &[])
            .await
            .unwrap_err(),
        ErrorKind::Access,
        ACCESS,
        &ServerActionError::AccessDenied { detail: None },
    );

    // Every action text field is covered, including values irrelevant to this runner.
    for field in [
        "name",
        "code",
        "state",
        "update_path",
        "evaluation_type",
        "update_m2m_operation",
        "update_boolean_value",
    ] {
        let old: Option<String> = client
            .query_one(
                &format!("SELECT {field} FROM ir_act_server WHERE id = $1"),
                &[&id],
            )
            .await
            .unwrap()
            .get(0);
        client
            .execute(
                &format!("UPDATE ir_act_server SET {field} = $2 WHERE id = $1"),
                &[&id, &over],
            )
            .await
            .unwrap();
        payload_limit(
            &run(&orm, seeded.runner, id, "x.partner", &[])
                .await
                .unwrap_err(),
            "The server action has more than 65536 bytes in a stored field; the run is refused.",
            RunLimit::FieldBytes,
        );
        client
            .execute(
                &format!("UPDATE ir_act_server SET {field} = $2 WHERE id = $1"),
                &[&id, &old],
            )
            .await
            .unwrap();
    }
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn stored_group_payload_is_bounded_without_truncating_authorization() {
    let h = harness().await;
    let client = connect(&h).await;
    let seeded = seed(&client).await;
    let id = action(
        &h,
        "BoundedGroups",
        seeded.partner_model,
        "x_open_window",
        RawVals::new(),
    )
    .await;
    let mut groups: Vec<i64> = client.query("INSERT INTO res_groups (name) SELECT 'payload-' || i::text FROM generate_series(1, $1::int8) i RETURNING id::int8", &[&(GROUPS + 1)]).await.unwrap().iter().map(|row| row.get(0)).collect();
    groups.sort_unstable();
    let last = groups[usize::try_from(GROUPS - 1).unwrap()];
    client
        .execute(
            "INSERT INTO res_groups_users_rel (uid, gid) VALUES ($1, $2)",
            &[&seeded.runner, &last],
        )
        .await
        .unwrap();
    client
        .execute(
            "INSERT INTO ir_act_server_group_rel (act_id, gid) SELECT $1, unnest($2::int8[])",
            &[&id, &&groups[..usize::try_from(GROUPS).unwrap()]],
        )
        .await
        .unwrap();
    let orm = policy_orm(&h).await;
    // The only matching membership is the last group; it must not be truncated away.
    assert_eq!(
        run(&orm, seeded.runner, id, "x.partner", &[])
            .await
            .unwrap()
            .unwrap()
            .action_type(),
        "ir.actions.act_window"
    );
    assert_typed(
        &run(&orm, seeded.demo, id, "x.partner", &[])
            .await
            .unwrap_err(),
        ErrorKind::Access,
        ACCESS,
        &ServerActionError::AccessDenied { detail: None },
    );
    client
        .execute(
            "INSERT INTO ir_act_server_group_rel (act_id, gid) VALUES ($1, $2)",
            &[&id, groups.last().unwrap()],
        )
        .await
        .unwrap();
    for uid in [seeded.runner, seeded.demo] {
        payload_limit(
            &run(&orm, uid, id, "x.partner", &[]).await.unwrap_err(),
            "The server action has more than 256 groups on one action; the run is refused.",
            RunLimit::Groups,
        );
    }
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn stored_tree_payload_budget_counts_all_actions() {
    let h = harness().await;
    let client = connect(&h).await;
    let seeded = seed(&client).await;
    let orm = policy_orm(&h).await;
    let top = action(
        &h,
        "TreeBudget",
        seeded.partner_model,
        "multi",
        RawVals::new(),
    )
    .await;
    // Each individual value fits; their aggregate already equals the tree budget before
    // model names, states and other fields are counted.
    let mut tx = h.begin().await.unwrap();
    let children = (0..TREE_BYTES / FIELD_BYTES)
        .map(|_| {
            vals(&[
                ("name", s("child")),
                ("state", s("x_open_window")),
                ("model_id", Cell::Id(seeded.partner_model)),
                ("parent_id", Cell::Id(top)),
            ])
        })
        .collect();
    tx.env()
        .records("ir.actions.server")
        .unwrap()
        .create(&mut tx, children)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    client
        .execute(
            "UPDATE ir_act_server SET value = repeat('x', $2::int4) WHERE parent_id = $1",
            &[&top, &i32::try_from(FIELD_BYTES).unwrap()],
        )
        .await
        .unwrap();
    payload_limit(
        &run(&orm, seeded.demo, top, "x.partner", &[])
            .await
            .unwrap_err(),
        "The server action has more than 1048576 bytes in the action tree payload; the run is refused.",
        RunLimit::TreeBytes,
    );
    client
        .execute(
            "UPDATE ir_act_server SET value = '' WHERE parent_id = $1",
            &[&top],
        )
        .await
        .unwrap();
    assert_eq!(
        run(&orm, seeded.demo, top, "x.partner", &[])
            .await
            .unwrap()
            .unwrap()
            .action_type(),
        "ir.actions.act_window"
    );
    h.cleanup().await.unwrap();
}
