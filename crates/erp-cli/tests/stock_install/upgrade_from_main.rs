// SPDX-License-Identifier: MIT OR Apache-2.0
//! Current-binary upgrade of the relevant stock 1.0.0 shape at main revision
//! 1c93ac1c64ae137b5e7275b3b9bd47731bec5b34: no stock security data or SQL objects,
//! nullable reservation days and legacy locations without policies. Unrelated current metadata
//! and prerequisite addons remain installed; this is not a previous-binary rehearsal.

use super::*;
use erp_testkit::tokio_postgres::Client;

const GROUPS: &[&str] = &[
    "group_stock_user",
    "group_stock_manager",
    "group_stock_multi_locations",
    "group_stock_multi_warehouses",
    "group_stock_picking_batch",
    "group_production_lot",
    "group_stock_lot_print_gs1",
    "group_lot_on_delivery_slip",
    "group_tracking_lot",
    "group_adv_location",
    "group_tracking_owner",
    "group_warning_stock",
    "group_stock_sign_delivery",
];

// The shared error envelope is already covered by the ORM's database-error tests. Read its
// private constant from our source rather than duplicate that compatibility wording here;
// the stock policy text below remains an independent oracle for the selected constraint.
fn constraint_message(policy: &str) -> String {
    let source = include_str!("../../../erp-orm/src/dberror.rs");
    let literal = source
        .lines()
        .find_map(|line| line.strip_prefix("pub(crate) const PREFIX: &str = "))
        .expect("the ORM declares its database-error envelope")
        .strip_suffix(';')
        .expect("the envelope declaration ends in a semicolon");
    let prefix: String = serde_json::from_str(literal).unwrap();
    format!("{prefix}{policy}")
}

async fn image(client: &Client, sql: &str) -> Vec<String> {
    client
        .query(sql, &[])
        .await
        .unwrap()
        .iter()
        .map(|row| row.get(0))
        .collect()
}

async fn policies(client: &Client) -> Vec<String> {
    image(
        client,
        "SELECT row_to_json(p)::text FROM stock_location_policy p ORDER BY id",
    )
    .await
}

async fn identities(client: &Client) -> Vec<String> {
    image(
        client,
        "SELECT json_build_array(id,name,model,res_id,noupdate)::text
         FROM ir_model_data WHERE module='stock' ORDER BY name",
    )
    .await
}

// Compare upgrade with fresh-install security semantics, independent of newly allocated IDs
// and reload audit fields. The explicit GROUPS list also detects missing fresh-install groups.
async fn security_contents(client: &Client) -> Vec<String> {
    image(
        client,
        "SELECT json_build_array('group',d.name,g.name,g.sequence,p.name)::text
         FROM ir_model_data d JOIN res_groups g ON g.id=d.res_id AND d.model='res.groups'
         LEFT JOIN res_groups_privilege p ON p.id=g.privilege_id WHERE d.module='stock'
         UNION ALL
         SELECT json_build_array('access',d.name,a.name,m.model,gd.module,gd.name,a.operation,a.domain)::text
         FROM ir_model_data d JOIN ir_access a ON a.id=d.res_id AND d.model='ir.access'
         JOIN ir_model m ON m.id=a.model_id
         LEFT JOIN ir_model_data gd ON gd.model='res.groups' AND gd.res_id=a.group_id
         WHERE d.module='stock' ORDER BY 1",
    )
    .await
}

async fn stable_state(client: &Client) -> Vec<Vec<String>> {
    vec![
        policies(client).await,
        identities(client).await,
        security_contents(client).await,
        image(client, "SELECT row_to_json(t)::text FROM stock_picking_type t ORDER BY id").await,
        image(client, "SELECT json_build_array(gid,hid)::text FROM res_groups_implied_rel ORDER BY gid,hid").await,
        image(client, "SELECT json_build_array(key,physical_identity,shape,encode(declaration_sha256,'hex'),encode(installed_sha256,'hex'))::text FROM erp_schema_sql_object WHERE addon='stock' ORDER BY key").await,
    ]
}

async fn operation_types(orm: &Orm) -> Vec<i64> {
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let company: i64 = tx
        .query(
            "SELECT res_id FROM ir_model_data WHERE module='base' AND name='main_company'",
            &[],
        )
        .await
        .unwrap()[0]
        .get(0);
    let location: i64 = tx.query("SELECT res_id FROM ir_model_data WHERE module='stock' AND name='stock_location_suppliers'", &[]).await.unwrap()[0].get(0);
    let mut ids = Vec::new();
    for name in ["Legacy null days", "Legacy configured days"] {
        ids.push(
            tx.env()
                .records("stock.picking.type")
                .unwrap()
                .create(
                    &mut tx,
                    vec![
                        RawVals::new()
                            .set_cell("name", Cell::Str(name.into()))
                            .set_cell("company_id", Cell::Id(company))
                            .set_cell("default_location_src_id", Cell::Id(location))
                            .set_cell("default_location_dest_id", Cell::Id(location)),
                    ],
                )
                .await
                .unwrap()
                .ids()[0],
        );
    }
    tx.commit().await.unwrap();
    ids
}

async fn restore_legacy_shape(client: &Client, operations: &[i64]) -> Vec<i64> {
    // These objects are all absent from the pinned main. Remove the physical objects as well
    // as their provenance, so schema reconciliation must install them, rather than admit drift.
    client.batch_execute("BEGIN;
        DROP TRIGGER stock_move_done_at_guard ON stock_move;
        DROP TRIGGER stock_line_done_at_guard ON stock_move_line;
        DROP TRIGGER stock_correction_update_guard ON stock_move_line_correction;
        DROP TRIGGER stock_correction_delete_guard ON stock_move_line_correction;
        DROP TRIGGER stock_correction_truncate_guard ON stock_move_line_correction;
        DROP TRIGGER stock_location_policy_insert ON stock_location;
        DROP TRIGGER stock_policy_update_guard ON stock_location_policy;
        DROP TRIGGER stock_policy_delete_guard ON stock_location_policy;
        DROP TRIGGER stock_policy_truncate_guard ON stock_location_policy;
        DROP FUNCTION stock_guard_done_at();
        DROP FUNCTION stock_guard_correction();
        DROP FUNCTION stock_create_location_policies();
        DROP FUNCTION stock_guard_location_policy();
        DELETE FROM erp_schema_sql_object WHERE addon='stock';
        CREATE TEMP TABLE later_stock_security ON COMMIT DROP AS
            SELECT id,model,res_id FROM ir_model_data WHERE module='stock'
            AND model IN ('ir.access','res.groups','res.groups.privilege');
        DELETE FROM ir_access WHERE id IN (SELECT res_id FROM later_stock_security WHERE model='ir.access');
        DELETE FROM res_groups_implied_rel WHERE gid IN
            (SELECT res_id FROM later_stock_security WHERE model='res.groups') OR hid IN
            (SELECT res_id FROM later_stock_security WHERE model='res.groups');
        DELETE FROM res_groups WHERE id IN (SELECT res_id FROM later_stock_security WHERE model='res.groups');
        DELETE FROM res_groups_privilege WHERE id IN
            (SELECT res_id FROM later_stock_security WHERE model='res.groups.privilege');
        DELETE FROM ir_model_data WHERE id IN (SELECT id FROM later_stock_security);
        ALTER TABLE stock_picking_type DROP CONSTRAINT stock_picking_type_reservation_days_bounds;
        ALTER TABLE stock_picking_type DROP CONSTRAINT stock_picking_type_reservation_days_method;
        ALTER TABLE stock_picking_type ALTER COLUMN reservation_days_before DROP NOT NULL;
        ALTER TABLE stock_picking_type ALTER COLUMN reservation_days_before_priority DROP NOT NULL;
        UPDATE ir_module_module SET latest_version='1.0.0' WHERE name='stock';").await.unwrap();
    client.execute("UPDATE stock_picking_type SET reservation_days_before=NULL,reservation_days_before_priority=NULL WHERE id=$1", &[&operations[0]]).await.unwrap();
    client.execute("UPDATE stock_picking_type SET reservation_method='by_date',reservation_days_before=365,reservation_days_before_priority=7 WHERE id=$1", &[&operations[1]]).await.unwrap();
    let gaps = client.query("INSERT INTO stock_location(name,usage,active) VALUES ('Legacy active gap','view',true),('Legacy archived gap','view',false) RETURNING id", &[]).await.unwrap().iter().map(|row| row.get(0)).collect();
    client.batch_execute("COMMIT").await.unwrap();
    gaps
}

async fn assert_security(client: &Client, expected: &[String]) {
    let rows = client.query("SELECT d.name,d.res_id,g.id FROM ir_model_data d LEFT JOIN res_groups g ON g.id=d.res_id AND d.model='res.groups' WHERE d.module='stock' AND d.name=ANY($1) ORDER BY d.name", &[&GROUPS]).await.unwrap();
    let mut names: Vec<&str> = GROUPS.to_vec();
    names.sort_unstable();
    assert_eq!(rows.len(), names.len());
    let mut ids = std::collections::BTreeSet::new();
    for (row, name) in rows.iter().zip(names) {
        assert_eq!(row.get::<_, String>(0), name);
        let id: i64 = row.get(1);
        assert_eq!(row.get::<_, Option<i64>>(2), Some(id), "stock.{name}");
        assert!(
            ids.insert(id),
            "stock.{name} shares another group's identity"
        );
    }
    assert_eq!(security_contents(client).await, expected);
    for (from_module, from, to_module, to) in [
        ("stock", "group_stock_user", "base", "group_user"),
        ("stock", "group_stock_manager", "stock", "group_stock_user"),
        ("base", "group_system", "stock", "group_stock_manager"),
    ] {
        let count: i64 = client.query_one("SELECT count(*) FROM res_groups_implied_rel r JOIN ir_model_data f ON f.model='res.groups' AND f.res_id=r.gid JOIN ir_model_data t ON t.model='res.groups' AND t.res_id=r.hid WHERE f.module=$1 AND f.name=$2 AND t.module=$3 AND t.name=$4", &[&from_module,&from,&to_module,&to]).await.unwrap().get(0);
        assert_eq!(count, 1, "{from_module}.{from} -> {to_module}.{to}");
    }
    let rows = client.query("SELECT d.name,p.id FROM ir_model_data d JOIN res_groups_privilege p ON p.id=d.res_id AND d.model='res.groups.privilege' WHERE d.module='stock'", &[]).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].get::<_, String>(0),
        "res_groups_privilege_inventory"
    );
    let privilege: i64 = rows[0].get(1);
    let roles = client.query("SELECT g.privilege_id FROM res_groups g JOIN ir_model_data d ON d.model='res.groups' AND d.res_id=g.id WHERE d.module='stock' AND d.name IN ('group_stock_user','group_stock_manager')", &[]).await.unwrap();
    assert_eq!(roles.len(), 2);
    for row in roles {
        assert_eq!(row.get::<_, Option<i64>>(0), Some(privilege));
    }
}

async fn assert_reservation_checks(orm: &Orm, id: i64) {
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let checks = tx.query("SELECT conname,convalidated FROM pg_constraint WHERE conrelid='stock_picking_type'::regclass AND conname IN ('stock_picking_type_reservation_days_bounds','stock_picking_type_reservation_days_method') ORDER BY conname", &[]).await.unwrap();
    assert_eq!(checks.len(), 2);
    assert!(checks.iter().all(|row| row.get::<_, bool>(1)));
    let before: String = tx
        .query(
            "SELECT row_to_json(t)::text FROM stock_picking_type t WHERE id=$1",
            &[&id],
        )
        .await
        .unwrap()[0]
        .get(0);
    for field in [
        "reservation_days_before",
        "reservation_days_before_priority",
    ] {
        for value in [-1, 366] {
            let sql = format!(
                "UPDATE stock_picking_type SET reservation_method='by_date',{field}=$1 WHERE id=$2"
            );
            let error = tx
                .savepoint(async |tx| tx.execute(&sql, &[&value, &id]).await)
                .await
                .unwrap_err();
            assert_eq!(error.kind(), ErrorKind::Validation, "{error:?}");
            assert_eq!(
                error.constraint(),
                Some("stock_picking_type_reservation_days_bounds")
            );
            assert_eq!(
                error.message(),
                constraint_message(
                    "stock.picking.type reservation lead times must be between 0 and 365 days."
                )
            );
        }
        for method in ["manual", "at_confirm"] {
            let sql = format!(
                "UPDATE stock_picking_type SET reservation_method=$1,{field}=1 WHERE id=$2"
            );
            let error = tx
                .savepoint(async |tx| tx.execute(&sql, &[&method, &id]).await)
                .await
                .unwrap_err();
            assert_eq!(error.kind(), ErrorKind::Validation, "{error:?}");
            assert_eq!(
                error.constraint(),
                Some("stock_picking_type_reservation_days_method")
            );
            assert_eq!(
                error.message(),
                constraint_message(
                    "stock.picking.type reservation lead times must be zero unless reservation_method is by_date."
                )
            );
        }
    }
    let after: String = tx
        .query(
            "SELECT row_to_json(t)::text FROM stock_picking_type t WHERE id=$1",
            &[&id],
        )
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(after, before);
    for value in [0, 365] {
        tx.execute("UPDATE stock_picking_type SET reservation_method='by_date',reservation_days_before=$1,reservation_days_before_priority=$1 WHERE id=$2", &[&value, &id]).await.unwrap();
    }
    tx.rollback().await.unwrap();
}

#[test]
fn stock_upgrade_from_main_1_0_preserves_rows_and_repeats() {
    run(async {
        let db = TestDb::create().await.unwrap();
        let installed = install(&db).await;
        let operations = operation_types(&installed).await;
        let client = db.client().await.unwrap();
        let expected_security = security_contents(&client).await;
        assert_security(&client, &expected_security).await;
        let kept: i64 = client
            .query_one(
                "SELECT id FROM stock_location_policy ORDER BY id LIMIT 1",
                &[],
            )
            .await
            .unwrap()
            .get(0);
        client.execute("UPDATE stock_location_policy SET forbid_negative=true,create_date=TIMESTAMP '2001-02-03 04:05:06',write_date=TIMESTAMP '2002-03-04 05:06:07' WHERE id=$1", &[&kept]).await.unwrap();
        let kept_image: String = client
            .query_one(
                "SELECT row_to_json(p)::text FROM stock_location_policy p WHERE id=$1",
                &[&kept],
            )
            .await
            .unwrap()
            .get(0);
        let gaps = restore_legacy_shape(&client, &operations).await;
        let legacy_ids = identities(&client).await;
        assert!(!legacy_ids.is_empty());
        assert_eq!(
            client
                .query_one(
                    "SELECT latest_version FROM ir_module_module WHERE name='stock'",
                    &[]
                )
                .await
                .unwrap()
                .get::<_, String>(0),
            "1.0.0"
        );
        for sql in [
            "SELECT count(*) FROM erp_schema_sql_object WHERE addon='stock'",
            "SELECT count(*) FROM ir_model_data WHERE module='stock' AND model IN ('ir.access','res.groups','res.groups.privilege')",
            "SELECT count(*) FROM pg_trigger WHERE tgname='stock_location_policy_insert'",
        ] {
            assert_eq!(
                client.query_one(sql, &[]).await.unwrap().get::<_, i64>(0),
                0,
                "{sql}"
            );
        }
        assert_eq!(client.query_one("SELECT count(*) FROM stock_location l WHERE l.id=ANY($1) AND NOT EXISTS (SELECT FROM stock_location_policy p WHERE p.location_id=l.id)", &[&gaps]).await.unwrap().get::<_, i64>(0), i64::try_from(gaps.len()).unwrap());
        assert_eq!(gaps.len(), 2);
        drop(installed);

        let manifest: toml::Table = toml::from_str(
            &std::fs::read_to_string(
                Path::new(env!("CARGO_MANIFEST_DIR")).join("../../addons/stock/manifest.toml"),
            )
            .unwrap(),
        )
        .unwrap();
        let target = manifest["version"].as_str().unwrap();
        let lower: String = client
            .query_one("SELECT (clock_timestamp() AT TIME ZONE 'UTC')::text", &[])
            .await
            .unwrap()
            .get(0);
        let sys = system(&db);
        let modules = [
            ModuleName::new("base").unwrap(),
            ModuleName::new("stock").unwrap(),
        ];
        let upgraded = sys
            .button_immediate_upgrade(&sys.system_env(Context::new()), &modules)
            .await
            .unwrap();
        assert!(
            upgraded.post_commit.is_clean(),
            "{:?}",
            upgraded.post_commit
        );
        let upper: String = client
            .query_one("SELECT (clock_timestamp() AT TIME ZONE 'UTC')::text", &[])
            .await
            .unwrap()
            .get(0);
        assert_eq!(
            client
                .query_one(
                    "SELECT latest_version FROM ir_module_module WHERE name='stock'",
                    &[]
                )
                .await
                .unwrap()
                .get::<_, String>(0),
            target
        );
        let rows = client.query("SELECT location_id,forbid_negative,create_uid,write_uid,create_date=write_date AND create_date BETWEEN $2::text::timestamp AND $3::text::timestamp AS utc_stamp FROM stock_location_policy WHERE location_id=ANY($1) ORDER BY location_id", &[&gaps, &lower, &upper]).await.unwrap();
        assert_eq!(rows.len(), gaps.len());
        for row in rows {
            assert!(!row.get::<_, bool>(1));
            assert_eq!(row.get::<_, Option<i64>>(2), None);
            assert_eq!(row.get::<_, Option<i64>>(3), None);
            assert_eq!(row.get::<_, Option<bool>>(4), Some(true));
        }
        assert_eq!(client.query_one("SELECT count(*) FROM stock_location l WHERE (SELECT count(*) FROM stock_location_policy p WHERE p.location_id=l.id)<>1", &[]).await.unwrap().get::<_, i64>(0), 0);
        assert_eq!(
            client
                .query_one(
                    "SELECT row_to_json(p)::text FROM stock_location_policy p WHERE id=$1",
                    &[&kept]
                )
                .await
                .unwrap()
                .get::<_, String>(0),
            kept_image
        );
        let current_ids = identities(&client).await;
        for row in legacy_ids {
            assert!(current_ids.contains(&row), "legacy identity changed: {row}");
        }
        let days = client.query("SELECT id,reservation_days_before,reservation_days_before_priority FROM stock_picking_type WHERE id=ANY($1) ORDER BY id", &[&operations]).await.unwrap();
        assert_eq!(days.len(), operations.len());
        for ((row, id), expected) in days.iter().zip(&operations).zip([(0, 0), (365, 7)]) {
            assert_eq!(row.get::<_, i64>(0), *id);
            assert_eq!((row.get::<_, i32>(1), row.get::<_, i32>(2)), expected);
        }
        assert_security(&client, &expected_security).await;
        assert_reservation_checks(&upgraded.orm, operations[0]).await;
        let snapshot = stable_state(&client).await;
        drop(upgraded);
        let repeated = sys
            .button_immediate_upgrade(&sys.system_env(Context::new()), &modules)
            .await
            .unwrap();
        assert!(
            repeated.post_commit.is_clean(),
            "{:?}",
            repeated.post_commit
        );
        assert_eq!(stable_state(&client).await, snapshot);
        assert_eq!(
            client
                .query_one(
                    "SELECT latest_version FROM ir_module_module WHERE name='stock'",
                    &[]
                )
                .await
                .unwrap()
                .get::<_, String>(0),
            target
        );
        let fresh: i64 = client.query_one("INSERT INTO stock_location(name,usage) VALUES ('After upgrade','view') RETURNING id", &[]).await.unwrap().get(0);
        assert_eq!(
            client
                .query_one(
                    "SELECT count(*) FROM stock_location_policy WHERE location_id=$1",
                    &[&fresh]
                )
                .await
                .unwrap()
                .get::<_, i64>(0),
            1
        );
        drop(repeated);
        drop(sys);
        drop(client);
        db.cleanup().await.unwrap();
    });
}
