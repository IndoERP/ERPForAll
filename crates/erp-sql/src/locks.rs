//! Advisory lock classes this crate registers next to the partition locks (`1701`, `1702`, see
//! [`crate::partition`]). A class is the first key of a two-key advisory lock; keeping every
//! class in one searchable place stops two features from sharing one by accident.

/// First key of the transaction-level advisory lock a singleton job takes: the
/// second key is `hashtext(singleton_key)`. Used only with `pg_try_advisory_xact_lock` in the
/// claim and `pg_advisory_xact_lock` in every chunk, never as a session lock.
pub const JOBS_LOCK_CLASS: i32 = 1703;

/// First key of the transaction-level advisory lock that serializes the schedulers of one record
/// (`mail.scheduled.message`): the second key is
/// `hashtext('<model>:<res_id>')`, taken with `pg_advisory_xact_lock` before the per-record count.
/// The `mail` addon cannot depend on this crate, so it repeats the number
/// (`erp_addon_mail::scheduled::THREAD_LOCK_CLASS`); a test pins the two equal. Classes in use:
/// 1701, 1702 (partitions), 1703 (singleton jobs), 1704 (this one) and `0x45525046` (`ERPF`, the
/// node and install locks, `erp_orm::node::LOCK_CLASS`).
pub const MAIL_SCHEDULED_THREAD_LOCK_CLASS: i32 = 1704;

/// `pg_try_advisory_xact_lock` of the singleton key in `$1` (`text`).
pub const JOBS_TRY_LOCK: &str = "pg_try_advisory_xact_lock(1703, hashtext($1::text))";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_lock_text_names_the_registered_class() {
        assert!(JOBS_TRY_LOCK.contains(&JOBS_LOCK_CLASS.to_string()));
    }
}
