//! The keys of encrypted fields for commands that do not read a server configuration.
//!
//! `erp server --config` installs the keys of `field_key_file` while it loads the
//! configuration. Commands such as `erp module install`/`upgrade` or `erp db init`
//! build an ORM from `DATABASE_URL` alone; when the linked addons declare an
//! encrypted field, the ORM refuses to build without keys. The global `--field-key-file`
//! option gives those commands the same file, under the same rules as the server: a
//! regular file of mode 600, not a symlink, at most 4096 bytes
//! ([`erp_server::config::read_secret`]).

use std::path::Path;

use erp_orm::field_encryption::{self, FieldKeys};

use crate::error::{CliError, Result};

const ARG: &str = "--field-key-file";

/// Read the key file at `path` and install its keys for this process, before any ORM is built.
///
/// Installing the key set the process already has (from a server configuration naming the same
/// file) is a no-op; another key set is refused.
///
/// # Errors
///
/// [`CliError::InvalidInput`] when the file is missing, a symlink, not mode 600, too large or
/// empty, when its keys do not parse, or when the process already has other keys. The message
/// never contains the file's contents.
pub fn install_from_file(path: &Path) -> Result<()> {
    let text = erp_server::config::read_secret(path).map_err(|_| CliError::InvalidInput {
        arg: ARG.into(),
        reason: format!(
            "'{}' must be a regular file (not a symlink) of mode 600, at most 4096 bytes, \
             with at least one key",
            path.display()
        ),
    })?;
    let keys = FieldKeys::parse(&text).map_err(|error| invalid(path, &error))?;
    field_encryption::install(keys).map_err(|error| invalid(path, &error))
}

fn invalid(path: &Path, error: &erp_fields::Error) -> CliError {
    CliError::InvalidInput {
        arg: ARG.into(),
        reason: format!("'{}': {error}", path.display()),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "tests")]

    use std::fs;
    use std::os::unix::fs::{PermissionsExt as _, symlink};

    use super::*;

    const KEY_LINE: &str = "k2026a AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=\n";

    fn key_file(dir: &Path, mode: u32) -> std::path::PathBuf {
        let path = dir.join("field-keys");
        fs::write(&path, KEY_LINE).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
        path
    }

    fn refused(result: Result<()>) -> String {
        match result {
            Err(CliError::InvalidInput { arg, reason }) => {
                assert_eq!(arg, ARG);
                assert!(!reason.contains("AAECAwQF"), "the key leaked: {reason}");
                reason
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    // The accepted case installs keys for the whole test process; it lives in
    // `tests/field_key_file.rs`, which runs the `erp` binary.

    #[test]
    fn a_key_file_readable_by_others_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = key_file(dir.path(), 0o644);
        assert!(refused(install_from_file(&path)).contains("mode 600"));
    }

    #[test]
    fn a_symlinked_key_file_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let target = key_file(dir.path(), 0o600);
        let link = dir.path().join("link");
        symlink(&target, &link).unwrap();
        assert!(refused(install_from_file(&link)).contains("not a symlink"));
    }

    #[test]
    fn a_missing_key_file_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        refused(install_from_file(&dir.path().join("absent")));
    }

    #[test]
    fn a_key_file_without_a_valid_key_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("field-keys");
        fs::write(&path, "k2026a not-base64\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let reason = refused(install_from_file(&path));
        assert!(reason.contains("field_key_file"), "{reason}");
    }
}
