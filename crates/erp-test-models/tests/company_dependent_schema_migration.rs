// SPDX-License-Identifier: MIT OR Apache-2.0
//! Genuine scalar predecessor adoption through an explicit native migration policy.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]
use erp_registry::{AddonDescriptor, FieldDecl, ModelDecl, RegistryBuilder};
use erp_sdk::migrations::CompanyMigrationPolicy;
use erp_sdk::{Digits, ErrorKind, IndexKind, Orm, SUPERUSER_ID, StaticPolicy, Transaction};
use erp_test_models::base_users;
use erp_testkit::Harness;
use serde_json::{Value, json};
use std::sync::Arc;

static ADDON: AddonDescriptor = AddonDescriptor {
    name: "company_migration",
    depends: &["base"],
    exact_only: false,
};
fn decl(fields: Vec<FieldDecl>) -> ModelDecl {
    let company_scoped = fields.iter().any(|f| f.name == "company_id");
    let mut model = ModelDecl::new("company_migration", "migration.property")
        .table("migration_property")
        .fields(Box::leak(fields.into_boxed_slice()));
    if company_scoped {
        model = model.company_access(erp_registry::CompanyAccessDecl::unrestricted(
            "Trusted migration fixture exercises shared predecessor rows",
        ));
    }
    model
}
async fn predecessor(fields: Vec<FieldDecl>) -> Harness {
    let h = Harness::builder([&base_users::ADDON, &erp_addon_base::ADDON, &ADDON])
        .with_decls([decl(fields)])
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    let currency = erp_testkit::fixtures::seed_currency_in(&mut tx)
        .await
        .unwrap();
    tx.execute("INSERT INTO res_company(id,name,currency_id) VALUES (1,'One',$1),(2,'Two',$1),(3,'Three',$1) ON CONFLICT DO NOTHING", &[&currency]).await.unwrap();
    tx.commit().await.unwrap();
    h
}
fn successor(h: &Harness, fields: Vec<FieldDecl>) -> Orm {
    let target = Box::leak(Box::new(decl(fields)));
    let registry = RegistryBuilder::new([&base_users::ADDON, &erp_addon_base::ADDON, &ADDON])
        .with_decls([&*target])
        .build()
        .unwrap();
    Orm::builder(Arc::new(registry), h.db().pool().clone())
        .policy(StaticPolicy::AllowAll)
        .build()
        .unwrap()
}
async fn map(tx: &mut Transaction, field: &str) -> Vec<Option<Value>> {
    tx.query(
        &format!(
            "SELECT {} FROM migration_property ORDER BY id",
            erp_sql::quote(field)
        ),
        &[],
    )
    .await
    .unwrap()
    .iter()
    .map(|r| r.get(0))
    .collect()
}
fn property(field: FieldDecl) -> FieldDecl {
    field.company_dependent(true)
}

#[tokio::test]
async fn explicit_set_preserves_exact_scalars_and_catalog() {
    let fields = || {
        vec![
            FieldDecl::char("note").index(Some(IndexKind::BtreeNotNull)),
            FieldDecl::float_digits("cost", Digits::Unrounded),
            FieldDecl::boolean("flag"),
            FieldDecl::integer("count"),
        ]
    };
    let h = predecessor(fields()).await;
    let mut tx = h.begin().await.unwrap();
    tx.batch("INSERT INTO migration_property(id,note,cost,flag,count) VALUES (1,'kept',9007199254740993.123456789,true,7),(2,'',-0.0000000000000000000000000001,false,0),(3,NULL,NULL,NULL,NULL); COMMENT ON COLUMN migration_property.note IS 'legacy note'; COMMENT ON INDEX migration_property__note_index IS 'legacy index'").await.unwrap();
    let oid: i64 = tx
        .query(
            "SELECT oid::int8 FROM pg_class WHERE oid='migration_property'::regclass",
            &[],
        )
        .await
        .unwrap()[0]
        .get(0);
    tx.commit().await.unwrap();
    let orm = successor(&h, fields().into_iter().map(property).collect());
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let policy = CompanyMigrationPolicy::companies([1, 2]).unwrap();
    for field in ["note", "cost", "flag", "count"] {
        tx.migrate_company_dependent("migration.property", field, &policy)
            .await
            .unwrap();
    }
    assert_eq!(
        map(&mut tx, "note").await,
        vec![
            Some(json!({"1":"kept","2":"kept"})),
            Some(json!({"1":"","2":""})),
            None
        ]
    );
    assert_eq!(
        map(&mut tx, "cost").await,
        vec![
            Some(json!({"1":"9007199254740993.123456789","2":"9007199254740993.123456789"})),
            Some(
                json!({"1":"-0.0000000000000000000000000001","2":"-0.0000000000000000000000000001"})
            ),
            None
        ]
    );
    assert_eq!(
        map(&mut tx, "flag").await,
        vec![
            Some(json!({"1":true,"2":true})),
            Some(json!({"1":false,"2":false})),
            None
        ]
    );
    assert_eq!(
        map(&mut tx, "count").await,
        vec![Some(json!({"1":7,"2":7})), Some(json!({"1":0,"2":0})), None]
    );
    let meta=tx.query("SELECT c.oid::int8,col_description(c.oid,a.attnum),a.attnotnull FROM pg_class c JOIN pg_attribute a ON a.attrelid=c.oid WHERE c.oid='migration_property'::regclass AND a.attname='note'",&[]).await.unwrap();
    assert_eq!(meta[0].get::<_, i64>(0), oid);
    assert_eq!(meta[0].get::<_, String>(1), "legacy note");
    assert!(!meta[0].get::<_, bool>(2));
    assert_eq!(
        tx.query(
            "SELECT obj_description('migration_property__note_index'::regclass)",
            &[]
        )
        .await
        .unwrap()[0]
            .get::<_, String>(0),
        "legacy index"
    );
    let before = map(&mut tx, "note").await;
    tx.init_models(&[]).await.unwrap();
    tx.migrate_company_dependent(
        "migration.property",
        "note",
        &CompanyMigrationPolicy::companies([3]).unwrap(),
    )
    .await
    .unwrap();
    tx.init_models(&[]).await.unwrap();
    assert_eq!(map(&mut tx, "note").await, before);
    let indexes=tx.query("SELECT indexdef FROM pg_indexes WHERE tablename='migration_property' AND indexname='migration_property__note_index'",&[]).await.unwrap();
    assert_eq!(indexes.len(), 1);
    assert!(
        indexes[0]
            .get::<_, String>(0)
            .contains("WHERE (note IS NOT NULL)")
    );
    tx.commit().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn row_company_is_explicit_and_unassigned_values_refuse() {
    let fields = || {
        vec![
            FieldDecl::char("note"),
            FieldDecl::many2one("company_id", "res.company"),
        ]
    };
    let h = predecessor(fields()).await;
    let mut tx = h.begin().await.unwrap();
    tx.batch("INSERT INTO migration_property(id,note,company_id) VALUES (1,'one',1),(2,'two',2),(3,'shared',NULL)").await.unwrap();
    tx.commit().await.unwrap();
    let orm = successor(
        &h,
        vec![
            property(FieldDecl::char("note")),
            FieldDecl::many2one("company_id", "res.company"),
        ],
    );
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let policy = CompanyMigrationPolicy::row_company("company_id").unwrap();
    let err = tx
        .savepoint(async |tx| {
            tx.migrate_company_dependent("migration.property", "note", &policy)
                .await
        })
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.message(),
        "migration.property.note: non-null predecessor value has no valid company owner"
    );
    tx.execute("UPDATE migration_property SET note=NULL WHERE id=3", &[])
        .await
        .unwrap();
    tx.migrate_company_dependent("migration.property", "note", &policy)
        .await
        .unwrap();
    assert_eq!(
        map(&mut tx, "note").await,
        vec![Some(json!({"1":"one"})), Some(json!({"2":"two"})), None]
    );
    tx.commit().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn implicit_adoption_refuses_and_later_failure_restores_predecessor() {
    let h = predecessor(vec![FieldDecl::char("note"), FieldDecl::char("empty")]).await;
    let mut tx = h.begin().await.unwrap();
    tx.batch("INSERT INTO migration_property(id,note) VALUES (1,'keep')")
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let orm = successor(
        &h,
        vec![
            property(FieldDecl::char("note")),
            property(FieldDecl::char("empty")),
        ],
    );
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let err = tx
        .savepoint(async |tx| tx.init_models(&[]).await)
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "migration.property.note: populated scalar conversion requires an explicit company migration policy"
    );
    let policy = CompanyMigrationPolicy::companies([2]).unwrap();
    let err = tx
        .savepoint(async |tx| {
            tx.migrate_company_dependent("migration.property", "note", &policy)
                .await?;
            tx.batch("SELECT 1/0").await
        })
        .await
        .unwrap_err();
    assert_eq!(err.sqlstate(), Some("22012"));
    assert_eq!(
        tx.query("SELECT note FROM migration_property", &[])
            .await
            .unwrap()[0]
            .get::<_, String>(0),
        "keep"
    );
    tx.migrate_company_dependent("migration.property", "note", &policy)
        .await
        .unwrap();
    tx.init_models(&[]).await.unwrap();
    assert_eq!(map(&mut tx, "empty").await, vec![None]);
    tx.commit().await.unwrap();
    drop(tx);
    let reverse = successor(
        &h,
        vec![FieldDecl::char("note"), property(FieldDecl::char("empty"))],
    );
    let mut tx = reverse.begin(SUPERUSER_ID).await.unwrap();
    let err = tx.init_models(&[]).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.message(),
        "cannot update the schema: conversion of jsonb field 'migration.property.note' to varchar on populated table \"migration_property\" refused: converting jsonb column to varchar would destroy existing JSON data; manual migration required"
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn policy_admission_is_bounded_and_never_uses_installer_company() {
    for ids in [vec![], vec![0], vec![-1], vec![1, 1]] {
        let err = CompanyMigrationPolicy::companies(ids.clone()).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Programming);
        assert_eq!(
            err.message(),
            if ids.is_empty() {
                "invalid company migration policy: at least one company is required"
            } else {
                "invalid company migration policy: owners must be unique positive IDs within MAX_OWNERS"
            }
        );
    }
    let err = CompanyMigrationPolicy::row_company("x\"; DROP TABLE res_company;--").unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "invalid company migration policy: row-company field must be a PostgreSQL field identifier"
    );
    let h = predecessor(vec![FieldDecl::char("note")]).await;
    let orm = successor(&h, vec![property(FieldDecl::char("note"))]);
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let err = tx
        .migrate_company_dependent(
            "migration.property",
            "note",
            &CompanyMigrationPolicy::companies([999999]).unwrap(),
        )
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.message(),
        "migration.property.note: company migration policy names an unknown company"
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn temporal_and_relation_migration_keeps_types_and_unrelated_objects() {
    let fields = || {
        vec![
            FieldDecl::date("day"),
            FieldDecl::datetime("moment"),
            FieldDecl::float("ratio"),
            FieldDecl::text("body"),
            FieldDecl::html("html"),
            FieldDecl::many2one("link", "res.company"),
        ]
    };
    let h = predecessor(fields()).await;
    let mut tx = h.begin().await.unwrap();
    tx.batch("INSERT INTO migration_property(id,day,moment,ratio,body,html,link) VALUES (1,'2024-02-29','2024-02-29 12:34:56.123456',1.25,'text','<p>original</p>',2); CREATE INDEX unrelated_id ON migration_property(id)").await.unwrap();
    tx.commit().await.unwrap();
    let orm = successor(&h, fields().into_iter().map(property).collect());
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    tx.batch("SET LOCAL DateStyle TO 'SQL, DMY'; SET LOCAL TIME ZONE 'Asia/Jakarta'")
        .await
        .unwrap();
    let policy = CompanyMigrationPolicy::companies([1]).unwrap();
    for name in ["day", "moment", "ratio", "body", "html", "link"] {
        tx.migrate_company_dependent("migration.property", name, &policy)
            .await
            .unwrap();
    }
    assert_eq!(
        map(&mut tx, "day").await,
        vec![Some(json!({"1":"2024-02-29"}))]
    );
    assert_eq!(
        map(&mut tx, "moment").await,
        vec![Some(json!({"1":"2024-02-29 12:34:56.123456"}))]
    );
    assert_eq!(map(&mut tx, "ratio").await, vec![Some(json!({"1":1.25}))]);
    assert_eq!(
        map(&mut tx, "html").await,
        vec![Some(json!({"1":"<p>original</p>"}))]
    );
    assert_eq!(map(&mut tx, "link").await, vec![Some(json!({"1":2}))]);
    tx.init_models(&[]).await.unwrap();
    let rows=tx.query("SELECT count(*) FROM pg_constraint WHERE conrelid='migration_property'::regclass AND contype='f' AND conname='migration_property_link_fkey'",&[]).await.unwrap();
    assert_eq!(rows[0].get::<_, i64>(0), 0);
    assert!(
        tx.query("SELECT to_regclass('unrelated_id') IS NOT NULL", &[])
            .await
            .unwrap()[0]
            .get::<_, bool>(0)
    );
    tx.commit().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn unsafe_dependencies_and_invalid_values_refuse_before_mutation() {
    let h = predecessor(vec![
        FieldDecl::char("note"),
        FieldDecl::float_digits("cost", Digits::Unrounded),
    ])
    .await;
    let mut tx = h.begin().await.unwrap();
    tx.batch("INSERT INTO migration_property(id,note,cost) VALUES (1,'keep',123); CREATE UNIQUE INDEX custom_unique_note ON migration_property(note)").await.unwrap();
    tx.commit().await.unwrap();
    let orm = successor(
        &h,
        vec![
            property(FieldDecl::char("note")),
            property(FieldDecl::float_digits("cost", Digits::Unrounded)),
        ],
    );
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let policy = CompanyMigrationPolicy::companies([1]).unwrap();
    let err = tx
        .savepoint(async |tx| {
            tx.migrate_company_dependent("migration.property", "note", &policy)
                .await
        })
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "migration.property.note: company migration refuses dependent index 'custom_unique_note'; migrate it explicitly"
    );
    assert_eq!(
        tx.query("SELECT note FROM migration_property", &[])
            .await
            .unwrap()[0]
            .get::<_, String>(0),
        "keep"
    );
    tx.batch("DROP INDEX custom_unique_note; ALTER TABLE migration_property ADD CONSTRAINT custom_check CHECK(length(note)>0)").await.unwrap();
    let err = tx
        .savepoint(async |tx| {
            tx.migrate_company_dependent("migration.property", "note", &policy)
                .await
        })
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "migration.property.note: company migration refuses dependent constraint 'custom_check'; migrate it explicitly"
    );
    tx.batch("ALTER TABLE migration_property DROP CONSTRAINT custom_check; UPDATE migration_property SET cost='NaN'").await.unwrap();
    let err = tx
        .savepoint(async |tx| {
            tx.migrate_company_dependent("migration.property", "cost", &policy)
                .await
        })
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.message(),
        "migration.property.cost: invalid predecessor value"
    );
    tx.batch("ALTER TABLE migration_property ALTER COLUMN note TYPE jsonb USING '{\"1\":\"valid\",\"3\":{\"secret\":true}}'::jsonb").await.unwrap();
    let err = tx
        .migrate_company_dependent("migration.property", "note", &policy)
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.message(),
        "migration.property.note: invalid company-dependent JSONB storage; an explicit migration is required"
    );
    assert!(!err.message().contains("secret"));
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn monetary_selection_and_required_fields_keep_legacy_values() {
    let fields = || {
        vec![
            FieldDecl::many2one("currency_id", "res.currency"),
            FieldDecl::new(
                "amount",
                erp_registry::KindDecl::Monetary {
                    currency_field: Some("currency_id"),
                },
            ),
            FieldDecl::selection("choice", &[("a", "First"), ("b", "Second")]),
            FieldDecl::char("required_note").required(true),
        ]
    };
    let h = predecessor(fields()).await;
    let mut tx = h.begin().await.unwrap();
    tx.batch("INSERT INTO migration_property(id,amount,choice,required_note) VALUES (1,9007199254740993.125,'a','present')").await.unwrap();
    tx.commit().await.unwrap();
    let mut target = fields();
    for field in &mut target {
        if field.name != "currency_id" {
            *field = (*field).company_dependent(true);
        }
    }
    let orm = successor(&h, target);
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let policy = CompanyMigrationPolicy::companies([2]).unwrap();
    for field in ["amount", "choice", "required_note"] {
        tx.migrate_company_dependent("migration.property", field, &policy)
            .await
            .unwrap();
    }
    tx.init_models(&[]).await.unwrap();
    assert_eq!(
        map(&mut tx, "amount").await,
        vec![Some(json!({"2":"9007199254740993.125"}))]
    );
    assert_eq!(map(&mut tx, "choice").await, vec![Some(json!({"2":"a"}))]);
    assert!(tx.query("SELECT NOT attnotnull FROM pg_attribute WHERE attrelid='migration_property'::regclass AND attname='required_note'",&[]).await.unwrap()[0].get::<_,bool>(0));
    tx.commit().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn migration_lock_serializes_competing_schema_work() {
    let h = predecessor(vec![FieldDecl::char("note")]).await;
    let orm = successor(&h, vec![property(FieldDecl::char("note"))]);
    let mut first = orm.begin(SUPERUSER_ID).await.unwrap();
    first
        .migrate_company_dependent(
            "migration.property",
            "note",
            &CompanyMigrationPolicy::companies([1]).unwrap(),
        )
        .await
        .unwrap();
    let other = successor(&h, vec![property(FieldDecl::char("note"))]);
    let mut second = other.begin(SUPERUSER_ID).await.unwrap();
    second.batch("SET LOCAL lock_timeout='50ms'").await.unwrap();
    let err = second.init_models(&[]).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Lock);
    assert_eq!(err.sqlstate(), Some("55P03"));
    // Database lock diagnostics contain server-selected relation/context text; SQLSTATE is stable.
    assert!(err.message().contains("lock"));
    second.rollback().await.unwrap();
    drop(second);
    first.rollback().await.unwrap();
    drop(first);
    let mut retry = orm.begin(SUPERUSER_ID).await.unwrap();
    retry.init_models(&[]).await.unwrap();
    retry.commit().await.unwrap();
    drop(retry);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn limits_and_authority_refuse_without_changing_scalar_data() {
    let error = CompanyMigrationPolicy::companies(1..=257).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.message(),
        "invalid company migration policy: owners must be unique positive IDs within MAX_OWNERS"
    );
    let h = predecessor(vec![FieldDecl::text("note")]).await;
    let mut tx = h.begin().await.unwrap();
    tx.batch("INSERT INTO migration_property(id,note) VALUES(1,repeat('x',16777216))")
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let orm = successor(&h, vec![property(FieldDecl::text("note"))]);
    let policy = CompanyMigrationPolicy::companies([1]).unwrap();
    let mut denied = orm.begin(erp_orm::UserId::new(2).unwrap()).await.unwrap();
    let error = denied
        .migrate_company_dependent("migration.property", "note", &policy)
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Access);
    assert_eq!(
        error.message(),
        "company migration requires an elevated native migration transaction"
    );
    drop(denied);
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let error = tx
        .migrate_company_dependent("migration.property", "note", &policy)
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "migration.property.note: invalid predecessor value or company-map byte limit exceeded"
    );
    assert_eq!(
        tx.query("SELECT length(note) FROM migration_property", &[])
            .await
            .unwrap()[0]
            .get::<_, i32>(0),
        16777216
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn custom_reference_key_under_orm_name_is_never_dropped() {
    let h = predecessor(vec![FieldDecl::many2one("link", "res.company")]).await;
    let mut tx = h.begin().await.unwrap();
    tx.batch("ALTER TABLE res_company ADD COLUMN external_key bigint UNIQUE; UPDATE res_company SET external_key=id; ALTER TABLE migration_property DROP CONSTRAINT migration_property_link_fkey; ALTER TABLE migration_property ADD CONSTRAINT migration_property_link_fkey FOREIGN KEY(link) REFERENCES res_company(external_key) ON DELETE SET NULL; INSERT INTO migration_property(id,link) VALUES(1,1)").await.unwrap();
    tx.commit().await.unwrap();
    let orm = successor(
        &h,
        vec![property(FieldDecl::many2one("link", "res.company"))],
    );
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let error = tx
        .migrate_company_dependent(
            "migration.property",
            "link",
            &CompanyMigrationPolicy::companies([1]).unwrap(),
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.message(),
        "migration.property.link: company migration refuses dependent constraint 'migration_property_link_fkey'; migrate it explicitly"
    );
    assert_eq!(
        tx.query("SELECT link FROM migration_property", &[])
            .await
            .unwrap()[0]
            .get::<_, i64>(0),
        1
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn maximum_owner_set_builds_every_override_without_argument_overflow() {
    let h = predecessor(vec![FieldDecl::char("note")]).await;
    let mut tx = h.begin().await.unwrap();
    tx.batch("INSERT INTO res_company(id,name,currency_id) SELECT n,'Owner '||n,(SELECT currency_id FROM res_company WHERE id=1) FROM generate_series(4,256) n; INSERT INTO migration_property(id,note) VALUES(1,'same')").await.unwrap();
    tx.commit().await.unwrap();
    let orm = successor(&h, vec![property(FieldDecl::char("note"))]);
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    tx.migrate_company_dependent(
        "migration.property",
        "note",
        &CompanyMigrationPolicy::companies(1..=256).unwrap(),
    )
    .await
    .unwrap();
    let values = map(&mut tx, "note").await;
    let object = values[0].as_ref().unwrap().as_object().unwrap();
    assert_eq!(object.len(), 256);
    for id in 1..=256 {
        assert_eq!(object.get(&id.to_string()), Some(&json!("same")));
    }
    drop(tx);
    h.cleanup().await.unwrap();
}
