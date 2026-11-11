//! Naming and tagging of throwaway databases.
//!
//! Every test database is named `erpforall_test_<pid>_<n>_<created>`, where `<created>` is the
//! creation time in seconds since the Unix epoch. PostgreSQL keeps no creation time for databases,
//! so the name carries it: the sweeper always knows a database's age, even when its creator died
//! before tagging it, and no lock between creators and the sweeper is needed.
//!
//! Each database also carries a comment (`COMMENT ON DATABASE`) recording the creator's pid and run
//! id, which lets `scripts/ci.sh` drop exactly the databases of its own run.

use std::time::{SystemTime, UNIX_EPOCH};

use crate::error::{Result, TestkitError};

/// Prefix shared by every throwaway database. The sweeper never touches anything else.
pub const TEST_DB_PREFIX: &str = "erpforall_test_";

/// Environment variable naming the current test run (set by `scripts/ci.sh`).
pub const RUN_ID_ENV: &str = "ERP_TESTKIT_RUN_ID";

const TAG_MAGIC: &str = "erpforall-testkit";
const TAG_VERSION: &str = "v1";
const NO_RUN_ID: &str = "-";

/// Seconds since the Unix epoch.
pub(crate) fn unix_now() -> Result<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .map_err(TestkitError::Clock)
}

/// The parts of a test database name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TestDbName {
    /// Creator's process id.
    pub pid: u32,
    /// Per-process counter.
    pub n: u64,
    /// Creation time, seconds since the Unix epoch.
    pub created_at: u64,
}

impl TestDbName {
    /// Render as `erpforall_test_<pid>_<n>_<created_at>`.
    pub fn render(&self) -> String {
        format!(
            "{TEST_DB_PREFIX}{}_{}_{}",
            self.pid, self.n, self.created_at
        )
    }

    /// Parse a name of exactly that shape; anything else is `None`.
    pub fn parse(name: &str) -> Option<Self> {
        let rest = name.strip_prefix(TEST_DB_PREFIX)?;
        let mut parts = rest.split('_');
        let mut number = || -> Option<&str> {
            let part = parts.next()?;
            (!part.is_empty() && part.bytes().all(|b| b.is_ascii_digit())).then_some(part)
        };
        let pid = number()?.parse().ok()?;
        let n = number()?.parse().ok()?;
        let created_at = number()?.parse().ok()?;
        if parts.next().is_some() {
            return None;
        }
        Some(Self { pid, n, created_at })
    }
}

/// True when `name` is a test database name this crate could have produced.
pub fn is_test_db_name(name: &str) -> bool {
    TestDbName::parse(name).is_some()
}

/// Check a run id before it is written into a database comment.
pub(crate) fn validate_run_id(run_id: &str) -> Result<()> {
    let ok = run_id != NO_RUN_ID
        && !run_id.is_empty()
        && run_id.len() <= 128
        && run_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b':'));
    if ok {
        Ok(())
    } else {
        Err(TestkitError::InvalidName {
            what: "run id",
            value: run_id.to_owned(),
            rule: "1-128 characters from [A-Za-z0-9_.:-], and not \"-\"",
        })
    }
}

/// Check a template database name before it is spliced into `CREATE DATABASE`.
pub(crate) fn validate_template(template: &str) -> Result<()> {
    let mut bytes = template.bytes();
    let first_ok = bytes
        .next()
        .is_some_and(|b| b.is_ascii_lowercase() || b == b'_');
    let ok = first_ok
        && template.len() <= 63
        && bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
    if ok {
        Ok(())
    } else {
        Err(TestkitError::InvalidName {
            what: "template database",
            value: template.to_owned(),
            rule: "a lowercase identifier [a-z_][a-z0-9_]*, at most 63 bytes",
        })
    }
}

/// Metadata stored in the database comment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tag {
    /// Process id of the creator.
    pub pid: u32,
    /// Run id of the creator, if one was set.
    pub run_id: Option<String>,
}

/// What a database comment says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Comment {
    /// No comment: the creator died between `CREATE DATABASE` and `COMMENT ON DATABASE`.
    Missing,
    /// A tag this version understands.
    Tag(Tag),
    /// A comment this version cannot read (a future tag version, or someone else's note).
    Unreadable,
}

impl Tag {
    /// Render the tag as the comment text. `run_id` must already be validated.
    pub(crate) fn render(&self) -> String {
        format!(
            "{TAG_MAGIC} {TAG_VERSION} pid={} run_id={}",
            self.pid,
            self.run_id.as_deref().unwrap_or(NO_RUN_ID),
        )
    }

    /// Parse a comment written by `Tag::render`.
    pub fn parse(comment: Option<&str>) -> Comment {
        let Some(comment) = comment else {
            return Comment::Missing;
        };
        Self::parse_v1(comment).map_or(Comment::Unreadable, Comment::Tag)
    }

    fn parse_v1(comment: &str) -> Option<Self> {
        let mut parts = comment.split_whitespace();
        if parts.next()? != TAG_MAGIC || parts.next()? != TAG_VERSION {
            return None;
        }
        let mut pid = None;
        let mut run_id = None;
        for part in parts {
            let (key, value) = part.split_once('=')?;
            match key {
                "pid" => pid = Some(value.parse().ok()?),
                "run_id" => run_id = Some((value != NO_RUN_ID).then(|| value.to_owned())),
                _ => return None,
            }
        }
        Some(Self {
            pid: pid?,
            run_id: run_id?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_round_trip() {
        let name = TestDbName {
            pid: 4242,
            n: 7,
            created_at: 1_760_500_000,
        };
        assert_eq!(name.render(), "erpforall_test_4242_7_1760500000");
        assert_eq!(TestDbName::parse(&name.render()), Some(name));
        assert!(is_test_db_name(&name.render()));
    }

    #[test]
    fn foreign_names_are_not_test_databases() {
        for name in [
            "erpforall",
            "erpforall_test_",
            "erpforall_test_12_3",
            "erpforall_test_12_3_",
            "erpforall_test__3_4",
            "erpforall_test_1_2_3_4",
            "erpforall_test_a_1_2",
            "erpforall_test_99999999999_1_2",
            "legacy_test_1_2_3",
        ] {
            assert!(!is_test_db_name(name), "{name} must not match");
        }
    }

    #[test]
    fn tag_round_trips() {
        let tag = Tag {
            pid: 99,
            run_id: Some("ci-abc:1".into()),
        };
        assert_eq!(Tag::parse(Some(&tag.render())), Comment::Tag(tag));
        let no_run = Tag {
            pid: 1,
            run_id: None,
        };
        assert_eq!(Tag::parse(Some(&no_run.render())), Comment::Tag(no_run));
    }

    #[test]
    fn comments_are_classified() {
        assert_eq!(Tag::parse(None), Comment::Missing);
        for text in [
            "",
            "my notes",
            "erpforall-testkit v1 pid=x run_id=-",
            "erpforall-testkit v1 run_id=-",
            "erpforall-testkit v1 pid=1 run_id=- extra=1",
            "erpforall-testkit v2 pid=1 run_id=-",
        ] {
            assert_eq!(Tag::parse(Some(text)), Comment::Unreadable, "{text:?}");
        }
    }

    #[test]
    fn run_ids_are_validated() {
        assert!(validate_run_id("ci-0123abc-20261015T010101Z-77").is_ok());
        assert!(validate_run_id("").is_err());
        assert!(validate_run_id("-").is_err());
        assert!(validate_run_id("x'; DROP DATABASE erpforall; --").is_err());
        assert!(validate_run_id(&"a".repeat(129)).is_err());
    }

    #[test]
    fn templates_are_validated() {
        assert!(validate_template("template0").is_ok());
        assert!(validate_template("_bench_tpl").is_ok());
        assert!(validate_template("Template0").is_err());
        assert!(validate_template("0abc").is_err());
        assert!(validate_template("a\"b").is_err());
        assert!(validate_template("").is_err());
    }
}
