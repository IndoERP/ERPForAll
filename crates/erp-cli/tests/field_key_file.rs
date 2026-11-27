//! `erp --field-key-file`: module commands load the keys of encrypted fields under the rules
//! of the server's `field_key_file` (mode 600, not a symlink), before any ORM is built.

#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

use std::fs;
use std::os::unix::fs::{PermissionsExt as _, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use clap::Parser;
use erp_cli::args::{Cli, Commands, ModuleAction};
use tempfile::tempdir;

/// Synthetic key (bytes 0..32), never a real one.
const KEY_LINE: &str = "test2026a AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=\n";

fn addons() -> String {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../addons")
        .display()
        .to_string()
}

fn key_file(dir: &Path, mode: u32) -> PathBuf {
    let path = dir.join("field-keys");
    fs::write(&path, KEY_LINE).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
    path
}

/// `erp module <op> --field-key-file <keys> -d keys_db base --dry-run`
fn module_dry_run(op: &str, keys: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_erp"))
        .args(["module", op, "--field-key-file"])
        .arg(keys)
        .args([
            "-d",
            "keys_db",
            "base",
            "--addons-path",
            &addons(),
            "--dry-run",
        ])
        .output()
        .expect("run erp binary")
}

fn assert_refused(output: &Output, expected: &str) {
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("--field-key-file"), "{stderr}");
    assert!(stderr.contains(expected), "{stderr}");
    assert!(!stderr.contains("AAECAwQF"), "the key leaked: {stderr}");
}

#[test]
fn the_option_is_accepted_before_and_after_the_subcommand() {
    for args in [
        &[
            "erp",
            "--field-key-file",
            "/k",
            "module",
            "upgrade",
            "-d",
            "db",
            "base",
        ][..],
        &[
            "erp",
            "module",
            "upgrade",
            "-d",
            "db",
            "base",
            "--field-key-file",
            "/k",
        ][..],
    ] {
        let cli = Cli::try_parse_from(args).expect("parse");
        assert_eq!(cli.field_key_file.as_deref(), Some(Path::new("/k")));
        assert!(matches!(
            cli.command,
            Commands::Module(ref cmd) if matches!(cmd.action, ModuleAction::Upgrade(_))
        ));
    }
    let cli = Cli::try_parse_from(["erp", "module", "install", "-d", "db", "base"]).unwrap();
    assert_eq!(cli.field_key_file, None);
}

#[test]
fn install_and_upgrade_accept_a_protected_key_file() {
    let dir = tempdir().unwrap();
    let keys = key_file(dir.path(), 0o600);
    for op in ["install", "upgrade"] {
        let output = module_dry_run(op, &keys);
        assert!(output.status.success(), "{op}: {output:?}");
        assert!(String::from_utf8_lossy(&output.stdout).contains("plan resolved"));
    }
}

#[test]
fn a_key_file_readable_by_others_is_refused() {
    let dir = tempdir().unwrap();
    let keys = key_file(dir.path(), 0o640);
    assert_refused(&module_dry_run("upgrade", &keys), "mode 600");
}

#[test]
fn a_symlinked_key_file_is_refused() {
    let dir = tempdir().unwrap();
    let target = key_file(dir.path(), 0o600);
    let link = dir.path().join("keys-link");
    symlink(&target, &link).unwrap();
    assert_refused(&module_dry_run("install", &link), "not a symlink");
}

#[test]
fn a_key_file_with_a_malformed_key_is_refused() {
    let dir = tempdir().unwrap();
    let keys = dir.path().join("field-keys");
    fs::write(&keys, "test2026a c2hvcnQ=\n").unwrap();
    fs::set_permissions(&keys, fs::Permissions::from_mode(0o600)).unwrap();
    assert_refused(&module_dry_run("upgrade", &keys), "32 bytes");
}
