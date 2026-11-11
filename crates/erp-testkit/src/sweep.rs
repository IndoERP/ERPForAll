//! Removal of leftover throwaway databases.
//!
//! Only databases that match all of the following are ever considered:
//! the exact name shape `erpforall_test_<pid>_<n>_<created>`, owned by the connecting role
//! (`datdba = current_user`), and not a template (`NOT datistemplate`). The creation time comes
//! from the name, so no lock against concurrent creators is needed: a database that is younger
//! than [`MIN_ORPHAN_AGE`] is never dropped unless its tag names the sweeping run.

use std::time::Duration;

use crate::db::{admin_config, connect, drop_database_with};
use crate::error::{Result, error_chain, pg_error};
use crate::tag::{Comment, Tag, TestDbName, unix_now};

/// Minimum age before a database without a readable tag may be dropped. Covers the window
/// between `CREATE DATABASE` and `COMMENT ON DATABASE`, and tags written by a newer testkit.
pub const MIN_ORPHAN_AGE: Duration = Duration::from_secs(10 * 60);

/// Which leftovers to remove.
#[derive(Debug, Clone)]
pub struct SweepOptions {
    /// Tagged databases at least this old are dropped. `Duration::ZERO` drops every tagged test
    /// database on the server that this role owns, including those of other runs in progress.
    pub max_age: Duration,
    /// Tagged databases with this run id are dropped regardless of age.
    pub run_id: Option<String>,
    /// List what would be dropped without dropping it.
    pub dry_run: bool,
}

/// Why a database was selected for removal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SweepReason {
    /// Its run id matches [`SweepOptions::run_id`].
    RunId,
    /// It is older than [`SweepOptions::max_age`].
    Age,
    /// It has no tag (its creator died before tagging it) and is older than [`MIN_ORPHAN_AGE`].
    Untagged,
    /// Its tag is unreadable (newer format) and it is older than both thresholds.
    Unreadable,
}

/// Outcome of a sweep.
#[derive(Debug, Default)]
pub struct SweepReport {
    /// Databases dropped (or that would be, on a dry run).
    pub dropped: Vec<(String, SweepReason)>,
    /// Test databases left in place.
    pub kept: Vec<String>,
    /// Databases that could not be dropped, with the error chain.
    pub failed: Vec<(String, String)>,
}

impl SweepReport {
    /// True when every selected database was dropped.
    pub fn is_clean(&self) -> bool {
        self.failed.is_empty()
    }
}

fn classify(
    name: &TestDbName,
    comment: &Comment,
    options: &SweepOptions,
    now: u64,
) -> Option<SweepReason> {
    let age = now.saturating_sub(name.created_at);
    let max_age = options.max_age.as_secs();
    let orphan_age = MIN_ORPHAN_AGE.as_secs();
    match comment {
        Comment::Tag(tag) => {
            if options.run_id.is_some() && tag.run_id == options.run_id {
                Some(SweepReason::RunId)
            } else {
                (age >= max_age).then_some(SweepReason::Age)
            }
        }
        Comment::Missing => (age >= orphan_age).then_some(SweepReason::Untagged),
        Comment::Unreadable => (age >= max_age.max(orphan_age)).then_some(SweepReason::Unreadable),
    }
}

/// Drop leftover test databases on the server in `database_url`.
///
/// Individual drop failures are collected in [`SweepReport::failed`]; connection and listing
/// failures are returned as errors.
pub async fn sweep(database_url: &str, options: &SweepOptions) -> Result<SweepReport> {
    let client = connect(&admin_config(database_url)?).await?;
    let rows = client
        .query(
            "SELECT d.datname::text, shobj_description(d.oid, 'pg_database') \
             FROM pg_database d \
             WHERE d.datname LIKE $1 \
               AND d.datdba = (SELECT oid FROM pg_roles WHERE rolname = current_user) \
               AND NOT d.datistemplate \
             ORDER BY d.datname",
            &[&"erpforall\\_test\\_%"],
        )
        .await
        .map_err(pg_error("listing test databases"))?;
    let now = unix_now()?;

    let mut report = SweepReport::default();
    for row in rows {
        let name: String = row.get(0);
        let comment: Option<String> = row.get(1);
        let Some(parsed) = TestDbName::parse(&name) else {
            continue;
        };
        let comment = Tag::parse(comment.as_deref());
        match classify(&parsed, &comment, options, now) {
            None => report.kept.push(name),
            Some(reason) if options.dry_run => report.dropped.push((name, reason)),
            Some(reason) => match drop_database_with(&client, &name).await {
                Ok(()) => report.dropped.push((name, reason)),
                Err(error) => report.failed.push((name, error_chain(&error))),
            },
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_000_000;

    fn options(max_age_secs: u64, run_id: Option<&str>) -> SweepOptions {
        SweepOptions {
            max_age: Duration::from_secs(max_age_secs),
            run_id: run_id.map(str::to_owned),
            dry_run: false,
        }
    }

    fn name(age: u64) -> TestDbName {
        TestDbName {
            pid: 1,
            n: 0,
            created_at: NOW - age,
        }
    }

    fn tagged(run_id: Option<&str>) -> Comment {
        Comment::Tag(Tag {
            pid: 1,
            run_id: run_id.map(str::to_owned),
        })
    }

    #[test]
    fn untagged_databases_wait_for_the_orphan_age() {
        let opts = options(3600, None);
        assert_eq!(classify(&name(599), &Comment::Missing, &opts, NOW), None);
        assert_eq!(
            classify(&name(600), &Comment::Missing, &opts, NOW),
            Some(SweepReason::Untagged)
        );
    }

    #[test]
    fn unreadable_tags_wait_for_both_thresholds() {
        let short = options(0, None);
        assert_eq!(
            classify(&name(599), &Comment::Unreadable, &short, NOW),
            None
        );
        assert_eq!(
            classify(&name(600), &Comment::Unreadable, &short, NOW),
            Some(SweepReason::Unreadable)
        );
        let long = options(3600, None);
        assert_eq!(
            classify(&name(3599), &Comment::Unreadable, &long, NOW),
            None
        );
    }

    #[test]
    fn matching_run_id_is_dropped_at_any_age() {
        assert_eq!(
            classify(
                &name(0),
                &tagged(Some("run-a")),
                &options(3600, Some("run-a")),
                NOW
            ),
            Some(SweepReason::RunId)
        );
    }

    #[test]
    fn other_runs_are_kept_until_old() {
        let opts = options(3600, Some("run-a"));
        assert_eq!(
            classify(&name(3599), &tagged(Some("run-b")), &opts, NOW),
            None
        );
        assert_eq!(
            classify(&name(3600), &tagged(Some("run-b")), &opts, NOW),
            Some(SweepReason::Age)
        );
    }

    #[test]
    fn no_run_id_never_matches_untagged_runs() {
        assert_eq!(
            classify(&name(1), &tagged(None), &options(3600, None), NOW),
            None
        );
    }

    #[test]
    fn future_creation_times_do_not_underflow() {
        let future = TestDbName {
            pid: 1,
            n: 0,
            created_at: NOW + 50,
        };
        assert_eq!(
            classify(&future, &Comment::Missing, &options(0, None), NOW),
            None
        );
    }
}
