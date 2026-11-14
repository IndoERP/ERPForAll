// SPDX-License-Identifier: MIT OR Apache-2.0
//! Synthetic models have no default records. Real PostgreSQL roles, no runtime SQL registration.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "fixture assertions"
)]
use super::*;
use erp_orm::{Orm, SUPERUSER_ID, StaticPolicy};
use erp_registry::{
    AddonDescriptor, FieldDecl, ModelDecl, RegistryBuilder, SqlBinding, SqlObjectDecl,
    SqlObjectManifest, SqlTableGrantDecl, SqlTableOp,
};
use erp_test_models::base_users;
use erp_testkit::Harness;
use std::sync::Arc;

static ADDON: AddonDescriptor = AddonDescriptor {
    name: "publish_fixture",
    depends: &["base"],
    exact_only: false,
};
static PRIVATE: ModelDecl =
    ModelDecl::new("publish_fixture", "publish_fixture.private").fields(&[FieldDecl::char("name")]);
static ORDINARY: ModelDecl = ModelDecl::new("publish_fixture", "publish_fixture.ordinary")
    .fields(&[FieldDecl::char("name")]);
static POLICY: SqlObjectDecl = SqlObjectDecl::table_grant(
    "publish_fixture",
    "insert",
    SqlTableGrantDecl::new(
        "publish_fixture.private",
        SqlBinding::Request,
        &[SqlTableOp::Insert],
    ),
);
static EXPECTED: SqlObjectManifest = SqlObjectManifest::new("publish_fixture", &["insert"]);

#[tokio::test]
async fn ordinary_grants_preserve_restricted_tables_and_owned_sequences() {
    assert_grants(false).await;
}

#[tokio::test]
async fn ordinary_grants_preserve_restricted_partition_children() {
    assert_grants(true).await;
}

async fn assert_grants(partitioned: bool) {
    let h = Harness::builder([&base_users::ADDON])
        .build()
        .await
        .unwrap();
    let owner = format!("{}_owner", h.db().name());
    let role = format!("{}_request", h.db().name());
    let admin = h.db().client().await.unwrap();
    let o = erp_sql::quote(&owner);
    let r = erp_sql::quote(&role);
    let protected = if partitioned {
        "CREATE TABLE publish_fixture_private(id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY, name varchar) PARTITION BY RANGE(id); CREATE TABLE publish_fixture_child PARTITION OF publish_fixture_private FOR VALUES FROM (MINVALUE) TO (MAXVALUE);"
    } else {
        "CREATE TABLE publish_fixture_private(id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY, name varchar); CREATE TABLE publish_fixture_child () INHERITS (publish_fixture_private);"
    };
    admin.batch_execute(&format!("CREATE ROLE {o} NOLOGIN; CREATE ROLE {r} NOLOGIN; GRANT USAGE, CREATE ON SCHEMA public TO {o}; {protected} CREATE TABLE publish_fixture_ordinary(id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY, name varchar); ALTER TABLE publish_fixture_private OWNER TO {o}; ALTER TABLE publish_fixture_ordinary OWNER TO {o}; GRANT ALL ON ALL TABLES IN SCHEMA public TO {o} WITH GRANT OPTION; GRANT ALL ON ALL SEQUENCES IN SCHEMA public TO {o} WITH GRANT OPTION")).await.unwrap();
    let registry = Arc::new(
        RegistryBuilder::new([&base_users::ADDON, &ADDON])
            .with_decls([&PRIVATE, &ORDINARY])
            .require_sql_objects([&EXPECTED])
            .with_sql_objects([&POLICY])
            .build()
            .unwrap(),
    );
    let mut config = h.db().config();
    config.options(format!("-c role={owner}"));
    let orm = Orm::builder(
        registry.clone(),
        erp_orm::pool::create_pool(config, 2).unwrap(),
    )
    .policy(StaticPolicy::AllowAll)
    .build()
    .unwrap();
    let options =
        SchemaSyncOptions::new().sql_roles(SqlRoleBindings::new().request_owned(role.clone()));
    for _ in 0..2 {
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        tx.reconcile_sql_object_grants(&registry, &options)
            .await
            .unwrap();
        grant_ordinary(&mut tx, &role, &registry).await.unwrap();
        tx.verify_sql_object_publication(&registry, &options)
            .await
            .unwrap();
        tx.commit().await.unwrap();
    }
    let row = admin.query_one("SELECT has_table_privilege($1::name,'publish_fixture_private','INSERT'), has_table_privilege($1::name,'publish_fixture_private','SELECT'), has_table_privilege($1::name,'publish_fixture_private','UPDATE'), has_table_privilege($1::name,'publish_fixture_private','DELETE'), (has_table_privilege($1::name,'publish_fixture_ordinary','SELECT') AND has_table_privilege($1::name,'publish_fixture_ordinary','INSERT') AND has_table_privilege($1::name,'publish_fixture_ordinary','UPDATE') AND has_table_privilege($1::name,'publish_fixture_ordinary','DELETE')), has_sequence_privilege($1::name,'publish_fixture_private_id_seq','USAGE,SELECT'), (has_sequence_privilege($1::name,'publish_fixture_ordinary_id_seq','USAGE') AND has_sequence_privilege($1::name,'publish_fixture_ordinary_id_seq','SELECT'))", &[&role]).await.unwrap();
    assert!(row.get::<_, bool>(0));
    for index in 1..4 {
        assert!(!row.get::<_, bool>(index));
    }
    assert!(row.get::<_, bool>(4));
    assert!(!row.get::<_, bool>(5));
    assert!(row.get::<_, bool>(6));
    let child: bool = admin.query_one("SELECT has_table_privilege($1::name,'publish_fixture_child','SELECT,INSERT,UPDATE,DELETE,TRUNCATE,REFERENCES,TRIGGER')", &[&role]).await.unwrap().get(0);
    assert!(
        !child,
        "generic grants widened a restricted table through its inheritance child"
    );
    for grant in ["SELECT", "SELECT(name)"] {
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        tx.batch(&format!("GRANT {grant} ON publish_fixture_child TO {r}"))
            .await
            .unwrap();
        let error = tx
            .verify_sql_object_publication(&registry, &options)
            .await
            .unwrap_err();
        assert_eq!(error.kind(), erp_fields::ErrorKind::Access);
        assert!(error.message().contains("undeclared descendant"), "{error}");
        assert!(error.message().contains("SELECT"), "{error}");
        tx.rollback().await.unwrap();
    }
    // Execute through the restricted identity, including the identity-column INSERT.
    admin.batch_execute(&format!("SET ROLE {r}; INSERT INTO publish_fixture_private(name) VALUES ('accepted'); RESET ROLE")).await.unwrap();
    admin
        .batch_execute(
            "DROP TABLE publish_fixture_child, publish_fixture_private, publish_fixture_ordinary",
        )
        .await
        .unwrap();
    admin
        .batch_execute(&format!("DROP OWNED BY {r}, {o}; DROP ROLE {r}, {o}"))
        .await
        .unwrap();
    drop(admin);
    h.cleanup().await.unwrap();
}
