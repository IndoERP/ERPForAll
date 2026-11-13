//! Upgrading a database from base 1.8 to 1.9:
//! `res.users.share` and `res.partner.partner_share` become required, and every
//! `partner_share` (a placeholder before 1.9: true unless the partner id was the superuser id)
//! is recomputed.
//!
//! The old shape is made by taking the new schema back: both columns lose their NOT NULL and their
//! database default, users get NULL `share`, partners NULL or placeholder values. The ordinary
//! schema step then runs, as the module lifecycle orders it, fills the NULLs with the fields'
//! static default before it sets NOT NULL, and gives both columns that default as their database
//! default (`db_default`); the 1.9 post-migration body runs last. This is a simulation
//! of the old schema, not a genuine old-release upgrade.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod users_support;

use erp_addon_base::migrations::recompute_shares;
use erp_sdk::{Transaction, Value};
use users_support::{create, harness, int, map, records, s, set_cmd, user_types_and_admin};

async fn partner(tx: &mut Transaction, name: &str) -> i64 {
    create(tx, "res.partner", &[("name", s(name))]).await
}

async fn linked_user(tx: &mut Transaction, login: &str, partner: i64, groups: &[i64]) -> i64 {
    create(
        tx,
        "res.users",
        &[
            ("name", s(login)),
            ("login", s(login)),
            ("partner_id", int(partner)),
            ("group_ids", set_cmd(groups)),
        ],
    )
    .await
}

async fn stored(tx: &mut Transaction, table: &str, column: &str) -> Vec<(i64, Option<bool>)> {
    tx.query(
        &format!("SELECT id, {column} FROM {table} ORDER BY id"),
        &[],
    )
    .await
    .unwrap()
    .iter()
    .map(|row| (row.get(0), row.get(1)))
    .collect()
}

/// The catalog's default expression of `table.column` (`pg_attrdef`), `None` without one.
async fn column_default(tx: &mut Transaction, table: &str, column: &str) -> Option<String> {
    tx.query(
        "SELECT pg_get_expr(d.adbin, d.adrelid) FROM pg_attrdef d \
           JOIN pg_attribute a ON a.attrelid = d.adrelid AND a.attnum = d.adnum \
          WHERE d.adrelid = to_regclass($1) AND a.attname = $2",
        &[&table, &column],
    )
    .await
    .unwrap()
    .first()
    .map(|row| row.get(0))
}

async fn not_null(tx: &mut Transaction, table: &str, column: &str) -> bool {
    tx.query(
        "SELECT a.attnotnull FROM pg_attribute a \
          WHERE a.attrelid = to_regclass($1) AND a.attname = $2 AND NOT a.attisdropped",
        &[&table, &column],
    )
    .await
    .unwrap()[0]
        .get(0)
}

#[tokio::test]
async fn an_old_database_gets_required_escape_columns_with_computed_values() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let t = user_types_and_admin(&mut tx).await;
    let a = partner(&mut tx, "A").await;
    let b = partner(&mut tx, "B").await;
    let c = partner(&mut tx, "C").await;
    let d = partner(&mut tx, "D").await;
    let su = partner(&mut tx, "System").await;
    linked_user(&mut tx, "ub", b, &[t.internal]).await;
    linked_user(&mut tx, "uc", c, &[t.portal]).await;
    let ud = linked_user(&mut tx, "ud", d, &[t.internal]).await;
    records(&tx, "res.users", &[ud])
        .write_values(&mut tx, &map(&[("active", Value::Bool(false))]))
        .await
        .unwrap();
    records(&tx, "res.users", &[erp_sdk::SUPERUSER_ID.get()])
        .write_values(&mut tx, &map(&[("partner_id", int(su))]))
        .await
        .unwrap();
    tx.flush().await.unwrap();
    // what the ORM computed, record by record, on the new schema
    let mut users_want = stored(&mut tx, "res_users", "share").await;
    // the superuser row is the harness's own insert with the column default, never computed: the
    // recompute makes it what the compute gives, never a share user
    let superuser = erp_sdk::SUPERUSER_ID.get();
    users_want
        .iter_mut()
        .find(|row| row.0 == superuser)
        .unwrap()
        .1 = Some(false);
    let partners_want = stored(&mut tx, "res_partner", "partner_share").await;
    let of = |rows: &[(i64, Option<bool>)], id: i64| rows.iter().find(|r| r.0 == id).unwrap().1;
    // not vacuous: the placeholder (true unless id = 1) differs for B and the superuser's partner
    assert_eq!(
        [a, b, c, d, su].map(|id| of(&partners_want, id)),
        [Some(true), Some(false), Some(true), Some(true), Some(false)]
    );
    assert!(su != erp_sdk::SUPERUSER_ID.get() && b != erp_sdk::SUPERUSER_ID.get());

    // the new shape keeps the fail-closed value as the database default of both columns
    assert_eq!(
        column_default(&mut tx, "res_users", "share")
            .await
            .as_deref(),
        Some("true")
    );
    assert_eq!(
        column_default(&mut tx, "res_partner", "partner_share")
            .await
            .as_deref(),
        Some("true")
    );

    // the old shape: nullable columns without a database default, NULL users, NULL or
    // placeholder partners
    tx.batch(
        "ALTER TABLE res_users ALTER COLUMN share DROP NOT NULL, \
                               ALTER COLUMN share DROP DEFAULT; \
         ALTER TABLE res_partner ALTER COLUMN partner_share DROP NOT NULL, \
                                 ALTER COLUMN partner_share DROP DEFAULT; \
         UPDATE res_users SET share = NULL; \
         UPDATE res_partner SET partner_share = (id <> 1)",
    )
    .await
    .unwrap();
    tx.execute(
        "UPDATE res_partner SET partner_share = NULL WHERE id = ANY($1)",
        &[&vec![a, b]],
    )
    .await
    .unwrap();
    tx.invalidate_all().await.unwrap();
    assert!(!not_null(&mut tx, "res_users", "share").await);
    assert!(!not_null(&mut tx, "res_partner", "partner_share").await);
    assert_eq!(column_default(&mut tx, "res_users", "share").await, None);
    assert_eq!(
        column_default(&mut tx, "res_partner", "partner_share").await,
        None
    );

    // the schema step the lifecycle runs next fills the NULLs with the default, sets NOT NULL and
    // gives both columns their database default
    tx.init_models(&[]).await.unwrap();
    assert!(not_null(&mut tx, "res_users", "share").await);
    assert!(not_null(&mut tx, "res_partner", "partner_share").await);
    assert_eq!(
        column_default(&mut tx, "res_users", "share")
            .await
            .as_deref(),
        Some("true")
    );
    assert_eq!(
        column_default(&mut tx, "res_partner", "partner_share")
            .await
            .as_deref(),
        Some("true")
    );
    let filled = stored(&mut tx, "res_partner", "partner_share").await;
    assert_eq!([of(&filled, a), of(&filled, b)], [Some(true), Some(true)]);
    assert!(
        stored(&mut tx, "res_users", "share")
            .await
            .iter()
            .all(|r| r.1 == Some(true))
    );

    // the 1.9 post-migration: every user and every partner gets the computed value back
    recompute_shares(&mut tx).await.unwrap();
    assert_eq!(stored(&mut tx, "res_users", "share").await, users_want);
    assert_eq!(
        stored(&mut tx, "res_partner", "partner_share").await,
        partners_want
    );
    // the ORM reads the new values, not a cached old one
    let read = records(&tx, "res.partner", &[b, su])
        .values(&mut tx, "partner_share")
        .await
        .unwrap();
    assert_eq!(read, [Value::Bool(false), Value::Bool(false)]);
    // running it again changes nothing
    recompute_shares(&mut tx).await.unwrap();
    assert_eq!(
        stored(&mut tx, "res_partner", "partner_share").await,
        partners_want
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// and base's manifest is at 1.9 or later; the lifecycle selecting and running it is not exercised here)
#[test]
fn the_share_recompute_is_linked_as_the_post_step_of_base_1_9() {
    let steps: Vec<(&str, &str)> = erp_data::migration::MIGRATIONS
        .iter()
        .filter(|m| m.addon() == "base" && m.version() == "1.9")
        .map(|m| (m.stage().as_str(), m.name()))
        .collect();
    assert_eq!(steps, [("post", "recompute_shares_base_1_9")]);
    let manifest = include_str!("../../../addons/base/manifest.toml");
    let version: Vec<u32> = manifest
        .lines()
        .find_map(|line| line.strip_prefix("version = "))
        .unwrap()
        .trim_matches('"')
        .split('.')
        .map(|part| part.parse().unwrap())
        .collect();
    assert!(version >= vec![1, 9], "{version:?}");
}
