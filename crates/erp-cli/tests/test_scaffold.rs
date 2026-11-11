#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

use erp_cli::args::ScaffoldArgs;
use erp_cli::error::CliError;
use erp_cli::scaffold::generate_scaffold;
use erp_modules::{ModuleName, read_manifest_from_dir};
use std::fs;
use std::path::PathBuf;
use tempfile::tempdir;

#[test]
fn test_scaffold_native_creates_valid_structure() {
    let tmp = tempdir().expect("tempdir");
    let target = tmp.path().join("my_addon");

    let args = ScaffoldArgs::new("my_addon")
        .with_dest(target.clone())
        .with_data_only(false)
        .with_author("Test Author")
        .with_summary("Test summary");

    let result = generate_scaffold(&args);
    assert!(result.is_ok(), "scaffold failed: {:?}", result.err());

    // Check directory structure
    assert!(target.join("manifest.toml").is_file());
    assert!(target.join("Cargo.toml").is_file());
    assert!(target.join("src/lib.rs").is_file());
    assert!(target.join("src/models.rs").is_file());
    assert!(target.join("views/my_addon_views.xml").is_file());
    assert!(target.join("data/my_addon_data.xml").is_file());
    assert!(target.join("tests/test_my_addon.rs").is_file());

    // Verify manifest parses through erp_modules
    let mod_name = ModuleName::new("my_addon").expect("valid name");
    let manifest = read_manifest_from_dir(&target, &mod_name)
        .expect("manifest read")
        .expect("manifest exists");

    assert_eq!(manifest.name().as_str(), "my_addon");
    assert_eq!(manifest.summary(), "Test summary");
    assert_eq!(manifest.author(), "Test Author");
    assert_eq!(manifest.license(), "MIT OR Apache-2.0");
    assert_eq!(manifest.version().raw(), "1.0.0");
    assert_eq!(
        manifest.depends(),
        &[ModuleName::new("base").expect("base")]
    );

    assert!(
        manifest
            .data()
            .contains(&"data/my_addon_data.xml".to_string())
    );
    assert!(
        manifest
            .data()
            .contains(&"views/my_addon_views.xml".to_string())
    );

    let data_xml =
        fs::read_to_string(target.join("data/my_addon_data.xml")).expect("read data xml");
    assert!(data_xml.contains("<erp>"));
    let views_xml =
        fs::read_to_string(target.join("views/my_addon_views.xml")).expect("read views xml");
    assert!(views_xml.contains("<erp>"));
    for (name, xml) in [("data.xml", data_xml), ("views.xml", views_xml)] {
        assert!(
            erp_data::parse_xml_str(&xml, name, "my_addon", false)
                .expect("generated XML is valid ERP data")
                .is_empty()
        );
    }

    // Verify native addon code and Cargo.toml
    let cargo_toml = fs::read_to_string(target.join("Cargo.toml")).expect("read cargo toml");
    assert!(cargo_toml.contains("erp-sdk = \"0.1\""));
    assert!(cargo_toml.contains("edition = \"2024\""));
    assert!(!cargo_toml.contains("workspace"));

    let lib_rs = fs::read_to_string(target.join("src/lib.rs")).expect("read lib.rs");
    assert!(lib_rs.contains("erp_sdk::addon!(name = \"my_addon\", depends = [\"base\"]);"));
    assert!(lib_rs.contains("pub mod models;"));

    let models_rs = fs::read_to_string(target.join("src/models.rs")).expect("read models.rs");
    assert!(models_rs.contains("use erp_sdk::prelude::*;"));
    assert!(
        models_rs.contains("#[model(name = \"my_addon.record\", description = \"Test summary\")]")
    );
}

#[test]
fn test_scaffold_data_only_creates_valid_structure() {
    let tmp = tempdir().expect("tempdir");
    let target = tmp.path().join("my_data_addon");

    let args = ScaffoldArgs::new("my_data_addon")
        .with_dest(target.clone())
        .with_data_only(true)
        .with_author("Data Author")
        .with_summary("Data-only addon");

    let result = generate_scaffold(&args);
    assert!(result.is_ok(), "scaffold failed: {:?}", result.err());

    // Check directory structure: no Cargo.toml, no src/, no models/ directory
    assert!(target.join("manifest.toml").is_file());
    assert!(!target.join("Cargo.toml").exists());
    assert!(!target.join("src").exists());
    assert!(!target.join("models").exists());
    assert!(target.join("views/my_data_addon_views.xml").is_file());
    assert!(target.join("data/my_data_addon_data.xml").is_file());

    // Verify manifest parses through erp_modules
    let mod_name = ModuleName::new("my_data_addon").expect("valid name");
    let manifest = read_manifest_from_dir(&target, &mod_name)
        .expect("manifest read")
        .expect("manifest exists");

    assert_eq!(manifest.name().as_str(), "my_data_addon");
    assert_eq!(manifest.summary(), "Data-only addon");
}

#[test]
fn test_scaffold_rejects_invalid_names() {
    let tmp = tempdir().expect("tempdir");

    let invalid_names = [
        "my-addon", // dashes forbidden in technical module name
        "my addon", // spaces forbidden
        "",         // empty forbidden
        "my.addon", // dots forbidden
        "addon!",   // exclamation mark forbidden
    ];

    for name in invalid_names {
        let target = tmp.path().join(name);
        let args = ScaffoldArgs::new(name).with_dest(target);

        let result = generate_scaffold(&args);
        match result {
            Err(CliError::InvalidModuleName {
                ref name,
                ref reason,
            }) => {
                assert!(!reason.is_empty(), "reason should not be empty for {name}");
            }
            other => panic!("expected InvalidModuleName for '{name}', got {other:?}"),
        }
    }
}

#[test]
fn test_scaffold_refuses_non_empty_directory() {
    let tmp = tempdir().expect("tempdir");
    let target = tmp.path().join("existing_dir");
    fs::create_dir_all(&target).expect("create dir");
    fs::write(target.join("dummy.txt"), "hello").expect("write file");

    let args = ScaffoldArgs::new("existing_dir").with_dest(target.clone());

    let result = generate_scaffold(&args);
    match result {
        Err(CliError::DestinationNotEmpty(ref p)) => {
            assert_eq!(p, &target);
            assert_eq!(
                result.unwrap_err().to_string(),
                format!(
                    "Destination directory '{}' already exists and is not empty",
                    target.display()
                )
            );
        }
        other => panic!("expected DestinationNotEmpty, got {other:?}"),
    }
}

#[test]
fn test_scaffold_refuses_file_destination() {
    let tmp = tempdir().expect("tempdir");
    let target = tmp.path().join("existing_file.txt");
    fs::write(&target, "content").expect("write file");

    let args = ScaffoldArgs::new("my_addon").with_dest(target.clone());

    let result = generate_scaffold(&args);
    match result {
        Err(CliError::DestinationIsFile(ref p)) => {
            assert_eq!(p, &target);
            assert_eq!(
                result.unwrap_err().to_string(),
                format!(
                    "Destination '{}' exists and is a file, not a directory",
                    target.display()
                )
            );
        }
        other => panic!("expected DestinationIsFile, got {other:?}"),
    }
}

#[cfg(unix)]
#[test]
fn test_scaffold_refuses_symlink_destination() {
    let tmp = tempdir().expect("tempdir");
    let real_dir = tmp.path().join("real_dir");
    fs::create_dir_all(&real_dir).expect("create real dir");
    let symlink = tmp.path().join("addon_symlink");
    std::os::unix::fs::symlink(&real_dir, &symlink).expect("create symlink");

    let args = ScaffoldArgs::new("my_addon").with_dest(symlink.clone());

    let result = generate_scaffold(&args);
    match result {
        Err(CliError::DestinationIsSymlink(ref p)) => {
            assert_eq!(p, &symlink);
            assert_eq!(
                result.unwrap_err().to_string(),
                format!(
                    "Destination '{}' is a symlink; symlinks are not allowed as scaffold destinations",
                    symlink.display()
                )
            );
        }
        other => panic!("expected DestinationIsSymlink, got {other:?}"),
    }
}

/// Verifies that a scaffolded native compiled addon compiles successfully with `cargo check`.
///
/// The scaffold generator emits `erp-sdk = "0.1"`, which is the planned release dependency on crates.io.
/// Because `erp-sdk` is an in-repo workspace crate not yet published during Milestone 2, this test patches
/// `Cargo.toml` to point to the local workspace crate directory (`erp-sdk = { path = "..." }`).
/// documents the path or git dependency required for users developing outside the repository.
#[test]
fn test_scaffold_build_cargo_check() {
    let tmp = tempdir().expect("tempdir");
    let target = tmp.path().join("buildable_addon");

    let args = ScaffoldArgs::new("buildable_addon")
        .with_dest(target.clone())
        .with_data_only(false)
        .with_author("CI Tester")
        .with_summary("Test compilation of scaffolded addon");

    generate_scaffold(&args).expect("scaffold");

    // erp-sdk and the workspace patches from this checkout
    use_local_workspace(&target.join("Cargo.toml"));

    let output = std::process::Command::new("cargo")
        .arg("check")
        .arg("--offline")
        .current_dir(&target)
        .output()
        .expect("run cargo check");

    assert!(
        output.status.success(),
        "offline cargo check failed on scaffolded addon (required registry crates must be in the local Cargo cache): stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Verifies that summaries containing double quotes and non-ASCII characters are properly escaped
/// in `models.rs` and compile cleanly with `cargo check`.
#[test]
fn test_scaffold_quoted_non_ascii_summary_cargo_check() {
    let tmp = tempdir().expect("tempdir");
    let target = tmp.path().join("quoted_summary_addon");

    let summary = "Custom \"business\" addon: café & ☕ (non-ascii: 日本語)";
    let args = ScaffoldArgs::new("quoted_summary_addon")
        .with_dest(target.clone())
        .with_data_only(false)
        .with_author("Quoted Author")
        .with_summary(summary);

    generate_scaffold(&args).expect("scaffold");

    // Verify models.rs properly escaped the summary attribute
    let models_rs = fs::read_to_string(target.join("src/models.rs")).expect("read models.rs");
    assert!(
        models_rs.contains(
            r#"description = "Custom \"business\" addon: café & ☕ (non-ascii: 日本語)""#
        ),
        "models.rs does not contain properly escaped summary: {models_rs}"
    );

    // erp-sdk and the workspace patches from this checkout
    use_local_workspace(&target.join("Cargo.toml"));

    let output = std::process::Command::new("cargo")
        .arg("check")
        .arg("--offline")
        .current_dir(&target)
        .output()
        .expect("run cargo check");

    assert!(
        output.status.success(),
        "offline cargo check failed on scaffolded addon with quoted non-ASCII summary (required registry crates must be in the local Cargo cache): stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
/// SHOULD-FIX: A FIFO (named pipe) destination should get its own error variant
/// (DestinationNotRegularFile), not DestinationNotEmpty.
#[cfg(unix)]
#[test]
fn test_scaffold_refuses_fifo_destination() {
    use std::process::Command;

    let tmp = tempdir().expect("tempdir");
    let fifo_path = tmp.path().join("my_fifo");

    // Create a FIFO (named pipe)
    let status = Command::new("mkfifo")
        .arg(&fifo_path)
        .status()
        .expect("mkfifo");
    assert!(status.success(), "mkfifo failed");

    let args = ScaffoldArgs::new("my_addon").with_dest(fifo_path.clone());

    let result = generate_scaffold(&args);
    match result {
        Err(CliError::DestinationNotRegularFile(ref p)) => {
            assert_eq!(p, &fifo_path);
            let msg = result.unwrap_err().to_string();
            assert!(msg.contains("special file"));
            assert!(msg.contains("FIFO"));
        }
        other => panic!("expected DestinationNotRegularFile for FIFO, got {other:?}"),
    }
}

/// Points a scaffolded addon's `Cargo.toml` at this checkout: `erp-sdk` by path, and the workspace's
/// `[patch.crates-io]` entries whole, with each `path` made absolute. A crate outside the workspace
/// does not inherit the workspace's patches, and `erp-orm` needs the vendored tokio-postgres (ADR
/// 0055; "Resolving `erp-sdk` Before Publication"). Paths are written
/// as TOML strings (escaped), and every other key of an entry is kept.
fn use_local_workspace(cargo_toml: &std::path::Path) {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let crates = manifest_dir.parent().expect("crates");
    let root = crates.parent().expect("workspace root");
    let toml_path = |path: PathBuf| toml::Value::String(path.display().to_string()).to_string();
    let content = fs::read_to_string(cargo_toml).expect("read");
    let mut patched = content.replace(
        "erp-sdk = \"0.1\"",
        &format!(
            "erp-sdk = {{ path = {} }}",
            toml_path(crates.join("erp-sdk"))
        ),
    );
    assert_ne!(
        patched, content,
        "the scaffold no longer depends on erp-sdk = \"0.1\""
    );
    let workspace: toml::Table = fs::read_to_string(root.join("Cargo.toml"))
        .expect("read the workspace manifest")
        .parse()
        .expect("parse the workspace manifest");
    let patches = workspace
        .get("patch")
        .and_then(|p| p.get("crates-io"))
        .and_then(toml::Value::as_table)
        .expect("the workspace has [patch.crates-io]");
    let mut entries = toml::Table::new();
    for (name, entry) in patches {
        let mut entry = entry.clone();
        let table = entry.as_table_mut();
        assert!(table.is_some(), "[patch.crates-io] {name} is not a table");
        let table = table.expect("checked above");
        let path = table.get("path").and_then(toml::Value::as_str);
        assert!(
            path.is_some(),
            "[patch.crates-io] {name} is not a path entry"
        );
        let absolute = root.join(path.expect("checked above"));
        table.insert(
            "path".to_owned(),
            toml::Value::String(absolute.display().to_string()),
        );
        entries.insert(name.clone(), entry);
    }
    let mut patch = toml::Table::new();
    patch.insert("crates-io".to_owned(), toml::Value::Table(entries));
    let mut section = toml::Table::new();
    section.insert("patch".to_owned(), toml::Value::Table(patch));
    patched.push('\n');
    patched.push_str(&section.to_string());
    fs::write(cargo_toml, patched).expect("write patched Cargo.toml");
}
