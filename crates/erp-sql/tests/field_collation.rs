// SPDX-License-Identifier: MIT OR Apache-2.0
//! Column SQL carries validated collation metadata.
use erp_fields::{ColumnType, TextCollation};
use erp_sql::{
    ddl::{self, ColumnDef},
    partition,
};
#[test]
fn create_add_partition_and_generated_render_collation() {
    let col = ColumnDef::new("code", ColumnType::Varchar(Some(20)))
        .collation(TextCollation::C)
        .not_null(true);
    for sql in [
        ddl::create_table("item", "", std::slice::from_ref(&col), None, None).join("\n"),
        ddl::add_column("item", &col, Some("'seed'")).join("\n"),
        ddl::add_column_keeping_default("item", &col, "'seed'").join("\n"),
        partition::create_partitioned_table("item", "", std::slice::from_ref(&col), "code")
            .join("\n"),
    ] {
        assert!(
            sql.contains("varchar(20) COLLATE \"pg_catalog\".\"C\""),
            "{sql}"
        );
    }
    let generated = ColumnDef::new("g", ColumnType::Text)
        .collation(TextCollation::C)
        .generated(erp_fields::ColumnGenerated::new(
            erp_fields::GeneratedStorage::Stored,
            "code",
        ));
    assert!(
        ddl::create_table("item", "", &[generated], None, None)[0]
            .contains("text COLLATE \"pg_catalog\".\"C\" GENERATED")
    );
    assert!(
        !ddl::create_table(
            "plain",
            "",
            &[ColumnDef::new("code", ColumnType::Text)],
            None,
            None
        )[0]
        .contains("COLLATE")
    );
}
#[test]
fn alteration_uses_explicit_collation_identity() {
    let sql = ddl::alter_column_collation(
        "item",
        "code",
        ColumnType::Varchar(Some(30)),
        TextCollation::DatabaseDefault,
    );
    assert_eq!(
        sql,
        "ALTER TABLE \"item\" ALTER COLUMN \"code\" TYPE varchar(30) COLLATE \"pg_catalog\".\"default\""
    );
}
