//! SQL text builders for ERPForAll (layer L4). Everything here returns SQL text with `$n`
//! placeholders; nothing is executed. Identifiers are always quoted; values never go into the text
//! except string literals in `COMMENT` statements, which are escaped.
//!
//! - [`ddl`], [`dml`] (S1): tables, columns, table objects, indexes, foreign keys; create/read/write
//!   by ids.
//! - [`filter`] (S2): FULL-optimized `erp-domain` domains to a parameterized `WHERE` predicate.
//! - [`order`] (S2): the order-specification grammar and `ORDER BY` compilation.
//! - [`select`] (S2): search, count and fetch statements; [`param`]: driver-independent parameters.
//! - [`relational`] (S3): many2many relation rows and tables, deletion, hierarchy expansion.
//! - [`rows`] (S6): existence checks and row locks on explicit ids.
//! - [`keyset`] (S7a): the "after this row" predicate of keyset pagination.
//! - [`partition`]: partitioned leaf tables, partition upkeep and its catalog reads.
//! - [`upsert`], [`version`]: business keys and the upsert statement; row-version
//!   trigger DDL and the versioned flush `UPDATE`.
//! - [`constant_filter`], [`exclusion`]: constant domain filters as literal SQL
//!   predicates; exclusion constraints, period checks and `WITHOUT OVERLAPS` keys.
//! - [`sqlobj`]: DDL text and canonical hash payloads of addon-owned
//!   functions, triggers and conditions.
//! - [`delta`]: validated projection shapes for transactional deltas, canonical
//!   nullable grain order, exact measure admission and their SQL fragments.

pub mod company;
pub mod conflict;
pub mod constant_filter;
pub mod ddl;
pub mod delta;
pub mod dml;
pub mod exclusion;
pub mod filter;
pub mod keyset;
pub mod locks;
pub mod order;
pub mod param;
pub mod partition;
pub mod reference;
pub mod relational;
pub mod rows;
pub mod select;
pub mod sqlobj;
pub mod upsert;
pub mod version;

pub use erp_registry::ident::{index_name, quote, quote_arc, shorten_identifier};

/// Quote a string literal: `it's` → `'it''s'`. Text with a backslash becomes an escape string
/// (`E'a\\b'`, backslashes and quotes doubled), which means the same whatever the server's
/// `standard_conforming_strings` is; text without one is the plain form.
pub fn literal(text: &str) -> String {
    debug_assert!(!text.contains('\0'), "NUL byte in literal {text:?}");
    let quoted = text.replace('\'', "''");
    if text.contains('\\') {
        format!("E'{}'", quoted.replace('\\', "\\\\"))
    } else {
        format!("'{quoted}'")
    }
}

pub const NOW_UTC: &str = "(now() AT TIME ZONE 'UTC')";

/// Reject unsafe or unbounded language text before SQL generation or shape-key allocation.
/// Active-language membership belongs to the ORM transaction.
pub(crate) fn validate_language(lang: Option<&str>) -> erp_fields::Result<()> {
    if let Some(code) = lang
        && (code.len() > 64
            || !code
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_@.-".contains(&b)))
    {
        return Err(erp_fields::Error::user(format!(
            "Invalid language code: {code}"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting() {
        assert_eq!(quote("res_partner"), "\"res_partner\"");
        assert_eq!(quote("a\"b"), "\"a\"\"b\"");
        assert_eq!(literal("it's"), "'it''s'");
        // a backslash selects the escape-string form, doubled, so the text is the same with
        // standard_conforming_strings on or off
        assert_eq!(literal("a\\b"), "E'a\\\\b'");
        assert_eq!(literal("it's a\\"), "E'it''s a\\\\'");
        assert_eq!(literal("plain"), "'plain'");
    }
}
