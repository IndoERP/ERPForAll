//! Statement-level triggers that bump a signal generation (
//! ): the DDL schema sync runs for a model's
//! [`TableObjectKind::SignalTrigger`](erp_registry::TableObjectKind::SignalTrigger).
//!
//! One function per signal, in the schema the tables live in; one trigger per declaration. The
//! function inserts the generation row and sends the wake-up at most once per transaction: a
//! transaction-local setting remembers the transaction id that bumped (a value left at session
//! level by `SET` can never match a later transaction, so it cannot suppress a bump), and it is
//! reverted with the row by `ROLLBACK TO SAVEPOINT`. The function pins its `search_path` with
//! `pg_temp` last, so a temporary table cannot take the row.

use erp_registry::{SignalTriggerDecl, TriggerSignal};

use super::{SIGNAL_CHANNEL, SIGNAL_TABLE};

/// Prefix of the bump functions' names (the catalog reads them by it).
pub(crate) const FUNCTION_PREFIX: &str = "erp_signal_bump_";

/// The bump function of `signal` (`erp_signal_bump_policy`).
pub(crate) fn function_name(signal: TriggerSignal) -> String {
    format!("{FUNCTION_PREFIX}{}", signal.key())
}

/// The transaction-local setting holding the id of the transaction that bumped `signal`.
pub(crate) fn flag(signal: TriggerSignal) -> String {
    format!("erp.{}_bumped", signal.key())
}

/// The comment schema sync compares to decide whether the function must be (re)created.
pub(crate) fn function_comment(signal: TriggerSignal, schema: &str) -> String {
    format!(
        "erpforall signal bump v1: key {}, search_path pg_catalog, {schema}, pg_temp",
        signal.key()
    )
}

/// `CREATE OR REPLACE FUNCTION` for `signal` in `schema`, with its comment.
pub(crate) fn create_function(signal: TriggerSignal, schema: &str) -> String {
    let name = function_name(signal);
    let flag = erp_sql::literal(&flag(signal));
    let key = erp_sql::literal(signal.key());
    // `policy` and `membership` keep their wake-up (SIGNAL: registry, policy and membership notify,
    // named caches do not): a listener applies the new generation, so a lagging replica is seen
    let notify = match signal {
        TriggerSignal::Policy | TriggerSignal::Membership => format!(
            "\n        PERFORM pg_notify({}, '');",
            erp_sql::literal(SIGNAL_CHANNEL)
        ),
    };
    format!(
        "CREATE OR REPLACE FUNCTION {name}() RETURNS trigger LANGUAGE plpgsql\n    \
         SET search_path = pg_catalog, {schema_q}, pg_temp AS $erp$\n\
         BEGIN\n    \
         IF current_setting({flag}, true) IS DISTINCT FROM pg_current_xact_id()::text THEN\n        \
         INSERT INTO {SIGNAL_TABLE} (key) VALUES ({key});\n        \
         PERFORM set_config({flag}, pg_current_xact_id()::text, true);{notify}\n    \
         END IF;\n    \
         RETURN NULL;\n\
         END\n\
         $erp$;\n\
         COMMENT ON FUNCTION {name}() IS {comment}",
        schema_q = erp_sql::quote(schema),
        comment = erp_sql::literal(&function_comment(signal, schema)),
    )
}

/// The comment schema sync compares to decide whether a trigger must be (re)created.
pub(crate) fn trigger_comment(decl: &SignalTriggerDecl) -> String {
    format!(
        "erpforall signal trigger v1: bump {} on {}",
        decl.signal.key(),
        decl.events_sql()
    )
}

/// `CREATE OR REPLACE TRIGGER` `name` on `table` for `decl`, with its comment.
pub(crate) fn create_trigger(table: &str, name: &str, decl: &SignalTriggerDecl) -> String {
    let (name, table) = (erp_sql::quote(name), erp_sql::quote(table));
    format!(
        "CREATE OR REPLACE TRIGGER {name} AFTER {events} ON {table} FOR EACH STATEMENT \
         EXECUTE FUNCTION {function}();\n\
         COMMENT ON TRIGGER {name} ON {table} IS {comment}",
        events = decl.events_sql(),
        function = function_name(decl.signal),
        comment = erp_sql::literal(&trigger_comment(decl)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_attestation_rejects_changed_function_body_or_signal() {
        let sql = create_function(TriggerSignal::Policy, "public");
        let Some((_, source)) = sql.split_once("AS $erp$") else {
            panic!("missing function body")
        };
        let Some((body, _)) = source.split_once("$erp$;") else {
            panic!("missing function terminator")
        };
        assert!(super::super::builtin_signal_function_matches(
            TriggerSignal::Policy,
            "public",
            body
        ));
        assert!(!super::super::builtin_signal_function_matches(
            TriggerSignal::Policy,
            "public",
            "BEGIN RETURN NULL; END"
        ));
        assert!(!super::super::builtin_signal_function_matches(
            TriggerSignal::Membership,
            "public",
            body
        ));
        // The body uses the function's configured search_path, so its source is identical
        // across schemas. The engine separately attests the exact canonical proconfig in SQL.
        assert!(super::super::builtin_signal_function_matches(
            TriggerSignal::Policy,
            "other_schema",
            body
        ));
        let changed_return = body.replace("RETURN NULL", "RETURN NEW");
        assert!(!super::super::builtin_signal_function_matches(
            TriggerSignal::Policy,
            "public",
            &changed_return
        ));
    }

    #[test]
    fn function_text_guards_by_transaction_and_pins_the_search_path() {
        let sql = create_function(TriggerSignal::Policy, "public");
        assert!(
            sql.starts_with("CREATE OR REPLACE FUNCTION erp_signal_bump_policy() RETURNS trigger")
        );
        assert!(sql.contains("SET search_path = pg_catalog, \"public\", pg_temp AS $erp$"));
        assert!(sql.contains(
            "IF current_setting('erp.policy_bumped', true) IS DISTINCT FROM \
             pg_current_xact_id()::text THEN"
        ));
        assert!(sql.contains("INSERT INTO erp_signal_generation (key) VALUES ('policy');"));
        assert!(sql.contains(
            "PERFORM set_config('erp.policy_bumped', pg_current_xact_id()::text, true);"
        ));
        assert!(sql.contains("PERFORM pg_notify('erp_signal', '');"));
        assert!(sql.ends_with(
            "COMMENT ON FUNCTION erp_signal_bump_policy() IS 'erpforall signal bump v1: key \
             policy, search_path pg_catalog, public, pg_temp'"
        ));
    }

    #[test]
    fn schema_names_are_quoted_and_compared() {
        let sql = create_function(TriggerSignal::Policy, "we\"ird");
        assert!(sql.contains("SET search_path = pg_catalog, \"we\"\"ird\", pg_temp"));
        assert_ne!(
            function_comment(TriggerSignal::Policy, "a"),
            function_comment(TriggerSignal::Policy, "b")
        );
    }

    #[test]
    fn trigger_text_names_events_and_function() {
        let all = SignalTriggerDecl::new(TriggerSignal::Policy);
        assert_eq!(
            create_trigger("ir_access", "ir_access_policy_bump", &all),
            "CREATE OR REPLACE TRIGGER \"ir_access_policy_bump\" AFTER INSERT OR UPDATE OR DELETE \
             OR TRUNCATE ON \"ir_access\" FOR EACH STATEMENT EXECUTE FUNCTION \
             erp_signal_bump_policy();\nCOMMENT ON TRIGGER \"ir_access_policy_bump\" ON \
             \"ir_access\" IS 'erpforall signal trigger v1: bump policy on INSERT OR UPDATE OR \
             DELETE OR TRUNCATE'"
        );
        let no_updates = all.without_updates();
        assert!(
            create_trigger("t", "n", &no_updates)
                .contains("AFTER INSERT OR DELETE OR TRUNCATE ON \"t\"")
        );
        assert_ne!(trigger_comment(&all), trigger_comment(&no_updates));
    }
}
