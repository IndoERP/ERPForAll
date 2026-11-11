//! Snapshot files: expected text checked into the repository, produced by our code.
//!
//! [`assert_snapshot`] compares what the code under test produced with a committed file. The
//! expected text is never written in the test: to create or change a file, run the test once with
//! `ERP_BLESS=1`, read the diff, and commit it. A missing file fails the test (also in CI), so a
//! snapshot cannot appear unreviewed.

use std::path::Path;

/// The environment variable that rewrites snapshot files instead of comparing with them.
pub const BLESS_ENV: &str = "ERP_BLESS";

/// Compare `actual` with the file at `path`.
///
/// With `ERP_BLESS=1` in the environment the file is written (its directory created) and the
/// comparison passes. Otherwise the file must exist and hold exactly `actual`.
///
/// # Panics
///
/// When the file is missing, differs from `actual`, or cannot be read or written; the message
/// names the file and the first differing line.
///
/// ```no_run
/// use erp_testkit::assert_snapshot;
///
/// let actual = "SELECT 1";
/// assert_snapshot(
///     concat!(env!("CARGO_MANIFEST_DIR"), "/tests/snapshots/one.sql"),
///     actual,
/// );
/// ```
#[allow(
    clippy::panic,
    reason = "test support: a snapshot mismatch must fail the test with a readable message"
)]
pub fn assert_snapshot(path: impl AsRef<Path>, actual: &str) {
    let path = path.as_ref();
    if std::env::var(BLESS_ENV).is_ok_and(|v| v == "1") {
        if let Some(dir) = path.parent()
            && let Err(e) = std::fs::create_dir_all(dir)
        {
            panic!(
                "snapshot {}: cannot create the directory: {e}",
                path.display()
            );
        }
        if let Err(e) = std::fs::write(path, actual) {
            panic!("snapshot {}: cannot write: {e}", path.display());
        }
        return;
    }
    let expected = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => panic!(
            "snapshot {} is missing; run the test with {BLESS_ENV}=1 to create it, then review \
             and commit the file",
            path.display()
        ),
        Err(e) => panic!("snapshot {}: cannot read: {e}", path.display()),
    };
    if expected != actual {
        let line = expected
            .lines()
            .zip(actual.lines())
            .position(|(e, a)| e != a)
            .unwrap_or_else(|| expected.lines().count().min(actual.lines().count()));
        panic!(
            "snapshot {} differs from the produced text at line {}:\n  file:     {:?}\n  produced: \
             {:?}\nrun the test with {BLESS_ENV}=1 to rewrite it if the change is intended",
            path.display(),
            line + 1,
            expected.lines().nth(line).unwrap_or("<end of file>"),
            actual.lines().nth(line).unwrap_or("<end of text>"),
        );
    }
}
