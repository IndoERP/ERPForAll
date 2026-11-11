//! Statement-level `AFTER DELETE` triggers that keep a key table and a named cache in step with
//! deletions the database makes on its own ([`TableObjectKind::FollowDelete`]
//! (erp_registry::TableObjectKind::FollowDelete), design): the schema
//! sync creates them for a model's declarations, after every table exists.
//!
//! One function for every declaration, in the schema the tables live in; each trigger passes the
//! key table (or `''`), the cache name and the model whose rows the key rows name (its `type`, or
//! `''`) as arguments and reads the deleted rows from its transition table `erp_old`. The
//! function deletes the key rows with the deleted ids and that type (one statement per firing,
//! through `format('%I')`, so the table name is quoted by PostgreSQL) and bumps `cache:<name>` at
//! most once per transaction, as the signal bump functions do ([`super::bump_trigger`]): a
//! transaction-local setting remembers the transaction that bumped. A firing that deleted nothing
//! does nothing. Its `search_path` is pinned with `pg_temp` last.
//!
//! `TRUNCATE` cannot be followed (it has no transition table), so every followed table also gets
//! a `BEFORE TRUNCATE` trigger running the same function, which refuses it (`0A000`, naming the
//! table): a `TRUNCATE … CASCADE` of a table whose rows a followed table references is refused
//! too, since PostgreSQL truncates the followed table with it.
//!
//! Cost: one firing per statement that deletes from a followed table, also per cascade statement
//! PostgreSQL runs for a foreign key into it: an `EXISTS` on the transition table, at most one
//! key-row `DELETE` and one generation row per transaction.

use erp_registry::FollowDeleteDecl;

use super::SIGNAL_TABLE;

/// The function every follow-delete trigger runs (the catalog reads it by this name).
pub(crate) const FUNCTION: &str = "erp_follow_delete";

/// The name of the transition table holding the deleted rows.
const OLD_TABLE: &str = "erp_old";

/// The comment schema sync compares to decide whether the function must be (re)created.
pub(crate) fn function_comment(schema: &str) -> String {
    format!("erpforall follow delete v3: search_path pg_catalog, {schema}, pg_temp")
}

/// `CREATE OR REPLACE FUNCTION` in `schema`, with its comment.
pub(crate) fn create_function(schema: &str) -> String {
    format!(
        "CREATE OR REPLACE FUNCTION {FUNCTION}() RETURNS trigger LANGUAGE plpgsql\n    \
         SET search_path = pg_catalog, {schema_q}, pg_temp AS $erp$\n\
         DECLARE\n    \
         flag text := 'erp.cache_' || TG_ARGV[1] || '_bumped';\n\
         BEGIN\n    \
         IF TG_OP = 'TRUNCATE' THEN\n        \
         RAISE EXCEPTION USING ERRCODE = '0A000', MESSAGE = format('TRUNCATE of %I is refused: \
         its deletions are followed (%scache %L), and TRUNCATE cannot be followed; delete the \
         rows instead', TG_TABLE_NAME, CASE WHEN TG_ARGV[0] = '' THEN '' ELSE format('key table \
         %L, ', TG_ARGV[0]) END, TG_ARGV[1]);\n    \
         END IF;\n    \
         IF NOT EXISTS (SELECT FROM {OLD_TABLE}) THEN\n        \
         RETURN NULL;\n    \
         END IF;\n    \
         IF TG_ARGV[0] <> '' THEN\n        \
         EXECUTE format('DELETE FROM %I AS k USING {OLD_TABLE} AS o WHERE k.id = o.id AND \
         k.type = $1', TG_ARGV[0]) USING TG_ARGV[2];\n    \
         END IF;\n    \
         IF current_setting(flag, true) IS DISTINCT FROM pg_current_xact_id()::text THEN\n        \
         INSERT INTO {SIGNAL_TABLE} (key) VALUES ('cache:' || TG_ARGV[1]);\n        \
         PERFORM set_config(flag, pg_current_xact_id()::text, true);\n    \
         END IF;\n    \
         RETURN NULL;\n\
         END\n\
         $erp$;\n\
         COMMENT ON FUNCTION {FUNCTION}() IS {comment}",
        schema_q = erp_sql::quote(schema),
        comment = erp_sql::literal(&function_comment(schema)),
    )
}

/// How every follow-delete trigger's comment starts, whatever its version: schema sync removes
/// the triggers carrying it that no declaration names any more (review C3-R3).
pub(crate) const TRIGGER_COMMENT_PREFIX: &str = "erpforall follow delete trigger ";

/// The comment schema sync compares to decide whether the triggers of a declaration on `model`
/// must be (re)created (both carry it).
pub(crate) fn trigger_comment(decl: &FollowDeleteDecl, model: &str) -> String {
    format!(
        "{TRIGGER_COMMENT_PREFIX}v2: keys {}, cache {}, type {}",
        decl.key_table.unwrap_or("-"),
        decl.cache,
        if decl.key_table.is_some() { model } else { "-" },
    )
}

/// The name of the `BEFORE TRUNCATE` trigger of the follow-delete trigger `name`.
pub(crate) fn truncate_trigger_name(name: &str) -> String {
    erp_sql::shorten_identifier(&format!("{name}_truncate"))
}

/// `CREATE OR REPLACE TRIGGER` `name` (`AFTER DELETE`) and its `BEFORE TRUNCATE` companion on
/// `table` for `decl` of `model`, with their comments.
pub(crate) fn create_trigger(
    table: &str,
    name: &str,
    decl: &FollowDeleteDecl,
    model: &str,
) -> String {
    let truncate = erp_sql::quote(&truncate_trigger_name(name));
    let (name, table) = (erp_sql::quote(name), erp_sql::quote(table));
    let args = format!(
        "{}, {}, {}",
        erp_sql::literal(decl.key_table.unwrap_or("")),
        erp_sql::literal(decl.cache),
        erp_sql::literal(if decl.key_table.is_some() { model } else { "" }),
    );
    let comment = erp_sql::literal(&trigger_comment(decl, model));
    format!(
        "CREATE OR REPLACE TRIGGER {name} AFTER DELETE ON {table} REFERENCING OLD TABLE AS \
         {OLD_TABLE} FOR EACH STATEMENT EXECUTE FUNCTION {FUNCTION}({args});\n\
         COMMENT ON TRIGGER {name} ON {table} IS {comment};\n\
         CREATE OR REPLACE TRIGGER {truncate} BEFORE TRUNCATE ON {table} FOR EACH STATEMENT \
         EXECUTE FUNCTION {FUNCTION}({args});\n\
         COMMENT ON TRIGGER {truncate} ON {table} IS {comment}",
    )
}

/// The statement removing the follow-delete trigger `name` of `table` (one no declaration names
/// any more, [`TRIGGER_COMMENT_PREFIX`]).
pub(crate) fn drop_trigger(table: &str, name: &str) -> String {
    format!(
        "DROP TRIGGER IF EXISTS {} ON {}",
        erp_sql::quote(name),
        erp_sql::quote(table)
    )
}

#[cfg(test)]
mod tests {
    use erp_registry::TableObjectKind;

    use super::*;

    #[test]
    fn function_text_deletes_keys_and_bumps_once_per_transaction() {
        let sql = create_function("public");
        assert!(sql.starts_with("CREATE OR REPLACE FUNCTION erp_follow_delete() RETURNS trigger"));
        assert!(sql.contains("SET search_path = pg_catalog, \"public\", pg_temp AS $erp$"));
        assert!(sql.contains("IF TG_OP = 'TRUNCATE' THEN"));
        assert!(sql.contains("RAISE EXCEPTION USING ERRCODE = '0A000'"));
        assert!(sql.contains("IF NOT EXISTS (SELECT FROM erp_old) THEN"));
        assert!(sql.contains(
            "EXECUTE format('DELETE FROM %I AS k USING erp_old AS o WHERE k.id = o.id AND \
             k.type = $1', TG_ARGV[0]) USING TG_ARGV[2];"
        ));
        assert!(
            sql.contains(
                "INSERT INTO erp_signal_generation (key) VALUES ('cache:' || TG_ARGV[1]);"
            )
        );
        assert!(sql.contains("PERFORM set_config(flag, pg_current_xact_id()::text, true);"));
        // review C3-R5: a bump-only trigger (no key table) names no key table when it refuses
        assert!(sql.contains(
            "followed (%scache %L), and TRUNCATE cannot be followed; delete the rows instead', \
             TG_TABLE_NAME, CASE WHEN TG_ARGV[0] = '' THEN '' ELSE format('key table %L, ', \
             TG_ARGV[0]) END, TG_ARGV[1]);"
        ));
        assert!(sql.ends_with(
            "COMMENT ON FUNCTION erp_follow_delete() IS 'erpforall follow delete v3: search_path \
             pg_catalog, public, pg_temp'"
        ));
        assert_ne!(function_comment("a"), function_comment("b"));
    }

    #[test]
    fn trigger_text_names_the_key_table_the_cache_and_the_type() {
        let keys = TableObjectKind::follow_delete("actions").deleting_keys("ir_actions");
        assert_eq!(
            create_trigger(
                "ir_act_url",
                "ir_act_url_keys_follow",
                &keys,
                "ir.actions.act_url"
            ),
            "CREATE OR REPLACE TRIGGER \"ir_act_url_keys_follow\" AFTER DELETE ON \"ir_act_url\" \
             REFERENCING OLD TABLE AS erp_old FOR EACH STATEMENT EXECUTE FUNCTION \
             erp_follow_delete('ir_actions', 'actions', 'ir.actions.act_url');\nCOMMENT ON \
             TRIGGER \"ir_act_url_keys_follow\" ON \"ir_act_url\" IS 'erpforall follow delete \
             trigger v2: keys ir_actions, cache actions, type ir.actions.act_url';\nCREATE OR \
             REPLACE TRIGGER \"ir_act_url_keys_follow_truncate\" BEFORE TRUNCATE ON \
             \"ir_act_url\" FOR EACH STATEMENT EXECUTE FUNCTION erp_follow_delete('ir_actions', \
             'actions', 'ir.actions.act_url');\nCOMMENT ON TRIGGER \
             \"ir_act_url_keys_follow_truncate\" ON \"ir_act_url\" IS 'erpforall follow delete \
             trigger v2: keys ir_actions, cache actions, type ir.actions.act_url'"
        );
        let groups = TableObjectKind::follow_delete("actions").on_relation("group_ids");
        assert!(
            create_trigger("rel", "n", &groups, "x.a")
                .contains("EXECUTE FUNCTION erp_follow_delete('', 'actions', '');")
        );
        assert_ne!(
            trigger_comment(&keys, "x.a"),
            trigger_comment(&groups, "x.a")
        );
        assert_ne!(trigger_comment(&keys, "x.a"), trigger_comment(&keys, "x.b"));
        assert!(trigger_comment(&keys, "x.a").starts_with(TRIGGER_COMMENT_PREFIX));
        assert!(trigger_comment(&groups, "x.a").starts_with(TRIGGER_COMMENT_PREFIX));
        assert_eq!(
            drop_trigger("ir_act_url", "old_follow"),
            "DROP TRIGGER IF EXISTS \"old_follow\" ON \"ir_act_url\""
        );
    }
}
