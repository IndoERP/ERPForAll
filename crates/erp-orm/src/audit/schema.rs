//! The fixed definition of `erp_event.ir_audit_entry` (design `m3-audit` §3.1). The installer and
//! the checks are the ones of the outbox ([`crate::event_schema`]), parameterized by this table
//! definition: the table is never created by generic schema sync and an existing one is checked
//! against the definition, never adopted.

use crate::event_schema::{Check, Column, TableDef, column, generated};

use super::AUDIT_PREMAKE_DAYS;

/// The columns, in order: the generated four, then the columns the INSERT supplies.
const COLUMNS: &[Column] = &[
    generated(
        "id",
        "bigint",
        "nextval('erp_event.ir_audit_entry_id_seq'::regclass)",
        "pg_catalog.nextval('\"erp_event\".\"ir_audit_entry_id_seq\"'::pg_catalog.regclass)",
    ),
    generated("entry_id", "uuid", "uuidv7()", "pg_catalog.uuidv7()"),
    generated(
        "create_date",
        "timestamp with time zone",
        "transaction_timestamp()",
        "pg_catalog.transaction_timestamp()",
    ),
    generated(
        "xid",
        "xid8",
        "pg_current_xact_id()",
        "pg_catalog.pg_current_xact_id()",
    ),
    column("schema_version", "smallint", true),
    column("entry_kind", "text", true),
    column("origin", "text", true),
    column("outcome", "text", true),
    column("error_class", "text", false),
    column("attempt", "smallint", true),
    column("operation_id", "uuid", true),
    column("correlation_id", "uuid", false),
    column("causation_id", "uuid", false),
    column("original_operation_id", "uuid", false),
    column("principal_kind", "text", false),
    column("subject_id", "bigint", false),
    column("actor_kind", "text", false),
    column("actor_id", "bigint", false),
    column("delegating_user_id", "bigint", false),
    column("company_ids", "bigint[]", true),
    column("registry_generation", "bigint", false),
    column("policy_generation", "bigint", false),
    column("model", "text", false),
    column("method", "text", false),
    column("effect", "text", false),
    column("target_ids", "jsonb", true),
    column("args_digest", "bytea", false),
    column("redaction_version", "smallint", false),
    column("events", "jsonb", true),
    column("encoded_bytes", "integer", true),
];

/// The audit table: today plus [`AUDIT_PREMAKE_DAYS`] daily partitions.
pub(crate) const AUDIT_TABLE: TableDef = TableDef {
    table: "ir_audit_entry",
    sequence: "ir_audit_entry_id_seq",
    primary_key: "ir_audit_entry_pkey",
    key_name: "ir_audit_entry_entry_key",
    key_column: "entry_id",
    index_names: &[
        "ir_audit_entry_pkey",
        "ir_audit_entry_entry_key",
        "ir_audit_entry_cursor_idx",
        "ir_audit_entry_time_brin",
        "ir_audit_entry_correlation_idx",
    ],
    indexes: &[
        "CREATE INDEX ir_audit_entry_correlation_idx ON ONLY erp_event.ir_audit_entry USING btree (correlation_id, create_date)",
        "CREATE INDEX ir_audit_entry_cursor_idx ON ONLY erp_event.ir_audit_entry USING btree (xid, id)",
        "CREATE INDEX ir_audit_entry_time_brin ON ONLY erp_event.ir_audit_entry USING brin (create_date)",
        "CREATE UNIQUE INDEX ir_audit_entry_entry_key ON ONLY erp_event.ir_audit_entry USING btree (entry_id, create_date)",
        "CREATE UNIQUE INDEX ir_audit_entry_pkey ON ONLY erp_event.ir_audit_entry USING btree (id, create_date)",
    ],
    extra_index_ddl: &[
        "CREATE INDEX \"ir_audit_entry_correlation_idx\" ON \"erp_event\".\"ir_audit_entry\" (correlation_id, create_date)",
    ],
    checks: &[
        Check {
            name: "ir_audit_entry_kind_check",
            ddl: "entry_kind IN ('change', 'attempt')",
            printed: "CHECK ((entry_kind = ANY (ARRAY['change'::text, 'attempt'::text])))",
        },
        Check {
            name: "ir_audit_entry_subject_check",
            ddl: "subject_id IS NOT NULL OR (entry_kind = 'attempt' AND origin = 'auth')",
            printed: "CHECK (((subject_id IS NOT NULL) OR ((entry_kind = 'attempt'::text) AND (origin = 'auth'::text))))",
        },
        Check {
            name: "ir_audit_entry_shape_check",
            ddl: "jsonb_typeof(events) = 'array' AND jsonb_typeof(target_ids) = 'array' \
                  AND (args_digest IS NULL) = (redaction_version IS NULL) \
                  AND (args_digest IS NULL OR octet_length(args_digest) = 32)",
            printed: "CHECK (((jsonb_typeof(events) = 'array'::text) AND (jsonb_typeof(target_ids) = 'array'::text) AND ((args_digest IS NULL) = (redaction_version IS NULL)) AND ((args_digest IS NULL) OR (octet_length(args_digest) = 32))))",
        },
    ],
    columns: COLUMNS,
    premake_days: AUDIT_PREMAKE_DAYS,
};

/// The names of the columns the INSERT supplies (everything without a database default).
#[cfg(test)]
pub(crate) fn supplied_columns() -> Vec<&'static str> {
    COLUMNS
        .iter()
        .filter(|c| c.default.is_none())
        .map(|c| c.name)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// columns of the definition, in order.
    #[test]
    fn insert_names_exactly_the_supplied_columns() {
        let supplied = supplied_columns();
        let sql = super::super::persist::INSERT;
        let start = sql.find('(').unwrap_or(0) + 1;
        let end = sql[start..].find(')').map_or(sql.len(), |i| start + i);
        let named: Vec<&str> = sql[start..end].split(',').map(str::trim).collect();
        assert_eq!(named, supplied);
        assert_eq!(supplied.len(), 26);
        for generated in ["id", "entry_id", "create_date", "xid"] {
            assert!(
                !named.contains(&generated),
                "{generated} must stay generated"
            );
        }
    }
}
