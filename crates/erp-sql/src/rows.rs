//! Statements on explicit rows of one table: existence checks and row locks (RSET §4.7–4.8).
//!
//! Ids are always one `int8[]` parameter (`= ANY($1::int8[])`), so one prepared
//! statement serves every recordset size.

use crate::quote;
use crate::select::select_ids_where;

/// The row lock taken by `lock_for_update` / `try_lock_for_update`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LockMode {
    /// `FOR UPDATE`: exclusive, also blocks inserts of rows referencing the locked ones.
    Update,
    /// `FOR NO KEY UPDATE` (`allow_referencing=True`): other transactions may still insert rows
    /// that reference the locked ones.
    NoKeyUpdate,
}

impl LockMode {
    pub fn new(allow_referencing: bool) -> Self {
        if allow_referencing {
            Self::NoKeyUpdate
        } else {
            Self::Update
        }
    }

    fn sql(self) -> &'static str {
        match self {
            Self::Update => "FOR UPDATE",
            Self::NoKeyUpdate => "FOR NO KEY UPDATE",
        }
    }
}

/// `SELECT "t"."id" FROM "t" WHERE "t"."id" = ANY($1::int8[])`: which of the ids exist (`exists()`).
/// No `ORDER BY`: callers keep their own order.
pub fn existing_ids(table: &str) -> String {
    select_ids_where(table, &[], "", None, 1)
}

/// `… WHERE "t"."id" = ANY($1::int8[]) FOR [NO KEY] UPDATE SKIP LOCKED`: lock the rows that are
/// free, skip the ones another transaction holds, never wait (`lock_for_update()` and
/// `try_lock_for_update()` without a limit).
pub fn lock_ids(table: &str, mode: LockMode) -> String {
    format!("{} {} SKIP LOCKED", existing_ids(table), mode.sql())
}

/// ```sql
/// SELECT "t"."id" FROM "t"
/// JOIN unnest($1::int8[]) WITH ORDINALITY AS "__ids"("id", "ord") ON "t"."id" = "__ids"."id"
/// ORDER BY "__ids"."ord" LIMIT $2::int8 FOR UPDATE OF "t" SKIP LOCKED
/// ```
///
/// Rows skipped because they are locked do not count toward the limit (PostgreSQL applies the
/// limit above the row-locking step).
pub fn lock_ids_ordered(table: &str, mode: LockMode) -> String {
    let t = quote(table);
    format!(
        "SELECT {t}.\"id\" FROM {t} JOIN unnest($1::int8[]) WITH ORDINALITY AS \"__ids\"(\"id\", \
         \"ord\") ON {t}.\"id\" = \"__ids\".\"id\" ORDER BY \"__ids\".\"ord\" LIMIT $2::int8 {} OF \
         {t} SKIP LOCKED",
        mode.sql()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_rset_024_025_028_statement_shapes() {
        assert_eq!(
            existing_ids("p"),
            "SELECT \"p\".\"id\" FROM \"p\" WHERE \"p\".\"id\" = ANY($1::int8[])"
        );
        assert_eq!(
            lock_ids("p", LockMode::new(false)),
            "SELECT \"p\".\"id\" FROM \"p\" WHERE \"p\".\"id\" = ANY($1::int8[]) FOR UPDATE SKIP \
             LOCKED"
        );
        assert_eq!(
            lock_ids("p", LockMode::new(true)),
            "SELECT \"p\".\"id\" FROM \"p\" WHERE \"p\".\"id\" = ANY($1::int8[]) FOR NO KEY \
             UPDATE SKIP LOCKED"
        );
        assert_eq!(
            lock_ids_ordered("p", LockMode::Update),
            "SELECT \"p\".\"id\" FROM \"p\" JOIN unnest($1::int8[]) WITH ORDINALITY AS \
             \"__ids\"(\"id\", \"ord\") ON \"p\".\"id\" = \"__ids\".\"id\" ORDER BY \
             \"__ids\".\"ord\" LIMIT $2::int8 FOR UPDATE OF \"p\" SKIP LOCKED"
        );
    }
}
