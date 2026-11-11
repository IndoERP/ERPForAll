//! Transactional provenance catalog for database-generated columns.
//!
//! Stores compiler format version, canonical declaration signature, and the installed
//! catalog expression, type, kind, and collation in `erp_schema_generated`.

use erp_fields::{ColumnType, Error, GeneratedStorage, Result};

/// The provenance table name.
pub const TABLE_NAME: &str = "erp_schema_generated";

/// Current compiler format version.
pub const GENERATED_FORMAT_VERSION: i32 = 2;

/// DDL creating `erp_schema_generated`.
pub const GENERATED_DDL: &str = "\
CREATE TABLE IF NOT EXISTS erp_schema_generated (
    schema_name           text    NOT NULL DEFAULT current_schema(),
    table_name            text    NOT NULL,
    column_name           text    NOT NULL,
    format_version        int4    NOT NULL,
    declaration_signature text    NOT NULL,
    catalog_expression    text    NOT NULL,
    catalog_type          text    NOT NULL,
    catalog_kind          text    NOT NULL,
    catalog_collation     text,
    PRIMARY KEY (schema_name, table_name, column_name)
);";

/// Core DDL entry for `erp-orm` registration.
pub(crate) const GENERATED_TABLES: crate::CoreDdl = crate::CoreDdl {
    owner: "erp-orm",
    name: "erp_schema_generated",
    purpose: "the generated columns provenance catalog",
    tables: &[TABLE_NAME],
    sql: GENERATED_DDL,
    after_models: None,
};

/// Record of an installed generated column in `erp_schema_generated`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProvenanceInfo {
    /// Compiler format version.
    pub format_version: i32,
    /// Canonical declaration signature.
    pub declaration_signature: String,
    /// Installed catalog expression from `pg_get_expr`.
    pub catalog_expression: String,
    /// Qualified native type identity and raw typmod (format 2).
    pub catalog_type: String,
    /// Catalog generation kind ("v" or "s").
    pub catalog_kind: String,
    /// Qualified catalog collation identity if collatable.
    pub catalog_collation: Option<String>,
}

/// Actual catalog definition of a generated column from PostgreSQL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GeneratedColumnInfo {
    /// Generation kind in pg_attribute: "v" for VIRTUAL, "s" for STORED.
    pub kind: String,
    /// Server-normalized expression from `pg_get_expr`.
    pub expression: String,
    /// UDT name in pg_type.
    pub udt_name: String,
    /// Namespace of the UDT in pg_type.
    pub type_namespace: String,
    /// Exact pg_attribute.atttypmod, including -1 for no modifier.
    pub typmod: i32,
    /// Qualified collation identity from pg_collation.
    pub collation: Option<String>,
}

impl GeneratedColumnInfo {
    /// Decode the catalog query's fixed-shape JSON payload. PostgreSQL identifiers can contain
    /// separator characters, so they must not be decoded with a delimiter or silent defaults.
    pub(crate) fn from_catalog(kind: String, expression: String, payload: &str) -> Result<Self> {
        if !matches!(kind.as_str(), "v" | "s") {
            return Err(Error::programming(format!(
                "unknown generated column kind {kind:?}"
            )));
        }
        let (type_namespace, udt_name, typmod, collation): (
            String,
            String,
            i32,
            Option<(String, String)>,
        ) = serde_json::from_str(payload).map_err(|source| {
            Error::new(
                erp_fields::ErrorKind::Database,
                "unreadable generated column catalog identity",
            )
            .with_source(source)
        })?;
        Ok(Self {
            kind,
            expression,
            type_namespace,
            udt_name,
            typmod,
            collation: collation.map(|(schema, name)| {
                format!("{}.{}", erp_sql::quote(&schema), erp_sql::quote(&name))
            }),
        })
    }

    /// Stable across restore: qualified names and a raw typmod, never database-local OIDs.
    pub(crate) fn catalog_type(&self) -> String {
        format!(
            "{}.{}:{}",
            erp_sql::quote(&self.type_namespace),
            erp_sql::quote(&self.udt_name),
            self.typmod
        )
    }
}

/// Compute the canonical declaration signature for a generated column.
pub fn compute_signature(storage: GeneratedStorage, ty: ColumnType, sql: &str) -> String {
    format!(
        "v{GENERATED_FORMAT_VERSION}:{}:{}:{}",
        storage.as_str(),
        ty.sql(),
        sql
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]
mod tests {
    use super::*;

    #[test]
    fn catalog_identity_keeps_identifier_boundaries() {
        let payload = serde_json::to_string(&(
            "schema\u{1f}left",
            "odd\"type",
            14,
            Some(("collation\u{1f}schema", "名\"name")),
        ))
        .unwrap();
        let info =
            GeneratedColumnInfo::from_catalog("s".into(), "source".into(), &payload).unwrap();
        assert_eq!(
            info.catalog_type(),
            "\"schema\u{1f}left\".\"odd\"\"type\":14"
        );
        assert_eq!(
            info.collation.as_deref(),
            Some("\"collation\u{1f}schema\".\"名\"\"name\"")
        );
    }

    #[test]
    fn catalog_identity_refuses_wrong_shapes_and_generation_kinds() {
        for payload in [
            "null",
            "[]",
            "[1,2,3,4]",
            "[\"pg_catalog\",\"int4\",-1]",
            "[\"pg_catalog\",\"int4\",-1,null,0]",
            "[\"pg_catalog\",\"int4\",-1,[\"only-schema\"]]",
        ] {
            let error = GeneratedColumnInfo::from_catalog("v".into(), "source".into(), payload)
                .unwrap_err();
            assert_eq!(error.kind(), erp_fields::ErrorKind::Database);
            assert_eq!(
                error.message(),
                "unreadable generated column catalog identity"
            );
        }
        let error =
            GeneratedColumnInfo::from_catalog("stored".into(), "source".into(), "[]").unwrap_err();
        assert_eq!(error.kind(), erp_fields::ErrorKind::Programming);
        assert_eq!(error.message(), "unknown generated column kind \"stored\"");
    }
}
