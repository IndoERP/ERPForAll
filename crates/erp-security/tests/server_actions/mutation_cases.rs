//! Regression coverage for mutation guard ordering, width and defaulted relationships.

use super::*;
use erp_addon_base::server_action::MAX_CHAIN;
use erp_orm::Operation;

/// Denied model and record permissions are checked before any tree row is locked.
#[tokio::test]
async fn unlink_checks_access_before_tree_locks() {
    for record_denial in [false, true] {
        let h = harness().await;
        let client = connect(&h).await;
        let seeded = seed(&client).await;
        let id = action(&h, "Denied", seeded.partner_model, "multi", RawVals::new()).await;
        let (operation, domain) = if record_denial {
            ("crud", Some("[('name', '!=', 'Denied')]"))
        } else {
            ("cru", None)
        };
        client
            .execute(
                "UPDATE ir_access SET operation = $1, domain = $2 WHERE name = 'actions'",
                &[&operation, &domain],
            )
            .await
            .unwrap();
        let orm = policy_orm(&h).await;
        let mut tx = orm.begin(UserId::new(seeded.demo).unwrap()).await.unwrap();
        let records = tx
            .env()
            .records("ir.actions.server")
            .unwrap()
            .browse([id])
            .unwrap();
        // The full expected message comes from the same access API base.unlink uses,
        // including its dynamic user/record evidence, before the competing lock exists.
        let expected = records
            .check_access(&mut tx, Operation::Unlink)
            .await
            .unwrap_err();
        assert_eq!(expected.kind(), ErrorKind::Access);
        let mut holder = h.begin().await.unwrap();
        holder
            .query(
                "SELECT id FROM ir_act_server WHERE id = $1 FOR UPDATE",
                &[&id],
            )
            .await
            .unwrap();
        tx.batch("SET LOCAL lock_timeout = '100ms'").await.unwrap();
        let error = records.unlink(&mut tx).await.unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Access, "{error:?}");
        assert_eq!(error.message(), expected.message());
        tx.rollback().await.unwrap();
        drop(tx);
        holder.rollback().await.unwrap();
        drop(holder);
        assert_eq!(
            read(&h, "ir.actions.server", id, "name").await,
            Value::str("Denied")
        );
        h.cleanup().await.unwrap();
    }
}

/// A complete boundary tree is checked; an over-limit or much wider tree is refused
/// before locks, even when its last descendant carries the elevated flag.
#[tokio::test]
async fn unlink_bounded_tree_never_accepts_a_partial_security_check() {
    let h = harness().await;
    let client = connect(&h).await;
    let seeded = seed(&client).await;
    let orm = policy_orm(&h).await;
    for total in [MAX_ACTIONS, MAX_ACTIONS + 1, MAX_ACTIONS * 64] {
        let root = action(&h, "Root", seeded.partner_model, "multi", RawVals::new()).await;
        let children: Vec<i64> = client
            .query(
            "INSERT INTO ir_act_server (name, model_id, type, binding_type, usage, state, parent_id) \
             SELECT 'Child', $1, 'ir.actions.server', 'action', 'ir_actions_server', 'multi', $2 \
             FROM generate_series(1, $3::int8) RETURNING id::int8",
                &[
                    &seeded.partner_model,
                    &root,
                    &i64::try_from(total - 1).unwrap(),
                ],
            )
            .await
            .unwrap()
            .iter()
            .map(|row| row.get(0))
            .collect();
        if total == MAX_ACTIONS {
            let mut tx = orm.begin(UserId::new(seeded.demo).unwrap()).await.unwrap();
            tx.env()
                .records("ir.actions.server")
                .unwrap()
                .browse([root])
                .unwrap()
                .unlink(&mut tx)
                .await
                .unwrap();
            tx.rollback().await.unwrap();
        }
        client
            .execute(
                "UPDATE ir_act_server SET run_as_superuser = true WHERE id = $1",
                &[children.last().unwrap()],
            )
            .await
            .unwrap();
        let mut holder = h.begin().await.unwrap();
        if total > MAX_ACTIONS {
            holder
                .query(
                    "SELECT id FROM ir_act_server WHERE id = $1 FOR UPDATE",
                    &[&root],
                )
                .await
                .unwrap();
        }
        let mut tx = orm.begin(UserId::new(seeded.demo).unwrap()).await.unwrap();
        tx.batch("SET LOCAL lock_timeout = '100ms'").await.unwrap();
        let error = tx
            .env()
            .records("ir.actions.server")
            .unwrap()
            .browse([root])
            .unwrap()
            .unlink(&mut tx)
            .await
            .unwrap_err();
        if total == MAX_ACTIONS {
            assert_typed(
                &error,
                ErrorKind::Access,
                "Only administrators (Settings) can set Run as Superuser on a server action or change an action that has it.",
                &ServerActionError::SuperuserAdminOnly,
            );
        } else {
            assert_typed(
                &error,
                ErrorKind::UserError,
                "The server action has more than 256 actions in one mutation; the run is refused.",
                &ServerActionError::Limit {
                    kind: RunLimit::MutationActions,
                },
            );
        }
        tx.rollback().await.unwrap();
        drop(tx);
        holder.rollback().await.unwrap();
        drop(holder);
        let remaining: i64 = client
            .query_one(
                "SELECT count(*) FROM ir_act_server WHERE id = $1 OR parent_id = $1",
                &[&root],
            )
            .await
            .unwrap()
            .get(0);
        assert_eq!(remaining, i64::try_from(total).unwrap());
    }
    h.cleanup().await.unwrap();
}

/// Stored default_parent_id links obey the chain bound in loader and sudo environments.
#[tokio::test]
async fn create_default_parent_checks_chain_in_loader_and_sudo() {
    let h = harness().await;
    let client = connect(&h).await;
    let seeded = seed(&client).await;
    let orm = policy_orm(&h).await;
    for sudo in [false, true] {
        let ids: Vec<i64> = client
            .query(
                "INSERT INTO ir_act_server (name, model_id, type, binding_type, usage, state) \
             SELECT 'Chain', $1, 'ir.actions.server', 'action', 'ir_actions_server', 'multi' \
             FROM generate_series(1, $2::int8) RETURNING id::int8",
                &[
                    &seeded.partner_model,
                    &i64::try_from(MAX_CHAIN - 1).unwrap(),
                ],
            )
            .await
            .unwrap()
            .iter()
            .map(|row| row.get(0))
            .collect();
        client.execute(
            "UPDATE ir_act_server a SET parent_id = p.parent FROM \
             (SELECT id, lag(id) OVER (ORDER BY id) AS parent FROM ir_act_server WHERE id = ANY($1)) p \
             WHERE a.id = p.id", &[&ids],
        ).await.unwrap();
        let mut tx = if sudo {
            orm.begin(UserId::new(seeded.demo).unwrap()).await.unwrap()
        } else {
            h.begin().await.unwrap()
        };
        let servers = tx.env().records("ir.actions.server").unwrap().sudo(true);
        let values = || {
            vals(&[
                ("name", s("Defaulted")),
                ("model_id", Cell::Id(seeded.partner_model)),
                ("state", s("multi")),
            ])
        };
        let boundary = servers
            .with_context([("default_parent_id", Value::Int(*ids.last().unwrap()))])
            .create(&mut tx, vec![values()])
            .await
            .unwrap()
            .ids()[0];
        let parent = servers
            .browse([boundary])
            .unwrap()
            .value(&mut tx, "parent_id")
            .await
            .unwrap();
        assert_eq!(parent, Value::Int(*ids.last().unwrap()));
        let error = servers
            .with_context([("default_parent_id", Value::Int(boundary))])
            .create(&mut tx, vec![values()])
            .await
            .unwrap_err();
        assert_typed(
            &error,
            ErrorKind::UserError,
            "The server action has more than 256 parent actions in a chain; the run is refused.",
            &ServerActionError::Limit {
                kind: RunLimit::Chain,
            },
        );
        tx.rollback().await.unwrap();
        drop(tx);
    }
    h.cleanup().await.unwrap();
}
