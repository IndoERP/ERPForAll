//! Addon scaffolding implementation for ERPForAll.

use crate::args::ScaffoldArgs;
use crate::error::{CliError, Result};
use erp_modules::validate_module_name;
use serde::Serialize;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Serialize)]
struct GeneratedManifest<'a> {
    name: &'a str,
    summary: &'a str,
    version: &'a str,
    license: &'a str,
    author: &'a str,
    depends: &'a [&'a str],
    data: &'a [String],
}

/// Generate a new addon directory tree from the provided scaffolding arguments.
///
/// Validates module naming rules, ensures destination directory safety (refusing symlinks,
/// existing files, and non-empty directories), and generates files atomically via a temporary
/// staging directory so that failures mid-way never leave partial trees on disk.
///
/// # Errors
///
/// Returns [`CliError::InvalidModuleName`] if the addon name violates technical naming rules.
/// Returns [`CliError::DestinationIsSymlink`] if the destination is a symlink.
/// Returns [`CliError::DestinationIsFile`] if the destination exists and is a file.
/// Returns [`CliError::DestinationNotEmpty`] if the destination directory exists and is not empty.
/// Returns [`CliError::Io`] on filesystem write failure.
///
/// # Examples
///
/// ```
/// use erp_cli::args::ScaffoldArgs;
/// use erp_cli::scaffold::generate_scaffold;
/// use tempfile::tempdir;
///
/// let tmp = tempdir().map_err(|e| erp_cli::CliError::Io {
///     path: std::path::PathBuf::from("/tmp"),
///     source: e,
/// })?;
/// let args = ScaffoldArgs::new("my_addon")
///     .with_dest(tmp.path().join("my_addon"))
///     .with_data_only(false)
///     .with_author("Developer")
///     .with_summary("My addon");
/// let path = generate_scaffold(&args)?;
/// assert!(path.join("manifest.toml").exists());
/// # Ok::<(), erp_cli::CliError>(())
/// ```
pub fn generate_scaffold(args: &ScaffoldArgs) -> Result<PathBuf> {
    // 1. Validate technical module name
    validate_module_name(&args.name).map_err(|e| CliError::InvalidModuleName {
        name: args.name.clone(),
        reason: e.to_string(),
    })?;

    // 2. Resolve destination directory
    let dest = args
        .dest
        .clone()
        .unwrap_or_else(|| PathBuf::from("addons").join(&args.name));

    // 3. Inspect destination safety using symlink_metadata (refuse symlinks, files, non-empty dirs)
    let dest_exists_and_empty = match dest.symlink_metadata() {
        Ok(meta) => {
            let ft = meta.file_type();
            if ft.is_symlink() {
                return Err(CliError::DestinationIsSymlink(dest));
            }
            if ft.is_file() {
                return Err(CliError::DestinationIsFile(dest));
            }
            if ft.is_dir() {
                let mut entries = fs::read_dir(&dest).map_err(|e| CliError::Io {
                    path: dest.clone(),
                    source: e,
                })?;
                if entries.next().is_some() {
                    return Err(CliError::DestinationNotEmpty(dest));
                }
                true
            } else {
                // Special file (FIFO, socket, device node, etc.) — not a valid scaffold destination.
                return Err(CliError::DestinationNotRegularFile(dest));
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => {
            return Err(CliError::Io {
                path: dest,
                source: e,
            });
        }
    };

    let summary = args
        .summary
        .as_deref()
        .unwrap_or("Custom business addon for ERPForAll");

    // 4. Ensure parent directory exists for atomic staging
    let parent = dest.parent().unwrap_or_else(|| Path::new("."));
    let parent_exists = parent.try_exists().map_err(|e| CliError::Io {
        path: parent.to_path_buf(),
        source: e,
    })?;
    if !parent.as_os_str().is_empty() && !parent_exists {
        fs::create_dir_all(parent).map_err(|e| CliError::Io {
            path: parent.to_path_buf(),
            source: e,
        })?;
    }

    // 5. Stage generation inside a temporary directory sibling to prevent partial trees on failure
    let tmp_dir = tempfile::Builder::new()
        .prefix(&format!(".tmp_scaffold_{}_", args.name))
        .tempdir_in(parent)
        .map_err(|e| CliError::Io {
            path: parent.to_path_buf(),
            source: e,
        })?;
    let stage = tmp_dir.path();

    let data_dir = stage.join("data");
    let views_dir = stage.join("views");
    fs::create_dir_all(&data_dir).map_err(|e| CliError::Io {
        path: data_dir.clone(),
        source: e,
    })?;
    fs::create_dir_all(&views_dir).map_err(|e| CliError::Io {
        path: views_dir.clone(),
        source: e,
    })?;

    // 6. Generate manifest.toml
    let data_files = vec![
        format!("data/{}_data.xml", args.name),
        format!("views/{}_views.xml", args.name),
    ];
    let manifest_dto = GeneratedManifest {
        name: &args.name,
        summary,
        version: "1.0.0",
        license: "MIT OR Apache-2.0",
        author: &args.author,
        depends: &["base"],
        data: &data_files,
    };
    let manifest_content = toml::to_string_pretty(&manifest_dto)
        .map_err(|e| CliError::Execution(format!("Failed to serialize manifest.toml: {e}")))?;

    let manifest_path = stage.join("manifest.toml");
    write_file(&manifest_path, &manifest_content)?;

    // 7. Generate data and view files with the canonical <erp> root.
    let data_file = data_dir.join(format!("{}_data.xml", args.name));
    let data_content = format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<erp>
    <!-- Initial data records for {name} -->
</erp>
"#,
        name = args.name
    );
    write_file(&data_file, &data_content)?;

    let views_file = views_dir.join(format!("{}_views.xml", args.name));
    let views_content = format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<erp>
    <!-- Views and menus for {name} -->
</erp>
"#,
        name = args.name
    );
    write_file(&views_file, &views_content)?;

    // 8. Generate native compiled addon files if not data-only
    if !args.data_only {
        let src_dir = stage.join("src");
        let tests_dir = stage.join("tests");
        fs::create_dir_all(&src_dir).map_err(|e| CliError::Io {
            path: src_dir.clone(),
            source: e,
        })?;
        fs::create_dir_all(&tests_dir).map_err(|e| CliError::Io {
            path: tests_dir.clone(),
            source: e,
        })?;

        let cargo_toml_path = stage.join("Cargo.toml");
        let cargo_toml_content = format!(
            r#"[package]
name = "addon-{name}"
version = "0.1.0"
edition = "2024"
license = "MIT OR Apache-2.0"
publish = false

[dependencies]
erp-sdk = "0.1"
"#,
            name = args.name
        );
        write_file(&cargo_toml_path, &cargo_toml_content)?;

        let lib_path = src_dir.join("lib.rs");
        let escaped_addon_name = format!("{:?}", args.name);
        let lib_content = format!(
            r#"//! Addon `{name}` for ERPForAll.

erp_sdk::addon!(name = {escaped_addon_name}, depends = ["base"]);

pub mod models;
"#,
            name = args.name,
            escaped_addon_name = escaped_addon_name,
        );
        write_file(&lib_path, &lib_content)?;

        let models_path = src_dir.join("models.rs");
        let model_name = format!("{}.record", args.name);
        let escaped_model_name = format!("{model_name:?}");
        let escaped_summary = format!("{summary:?}");
        let models_content = format!(
            r#"//! Model definitions for addon `{name}`.

use erp_sdk::prelude::*;

#[model(name = {escaped_model_name}, description = {escaped_summary})]
pub struct Record {{
    #[field(char, default = "")]
    pub name: Option<Arc<str>>,
}}
"#,
            name = args.name,
            escaped_model_name = escaped_model_name,
            escaped_summary = escaped_summary,
        );
        write_file(&models_path, &models_content)?;

        let test_path = tests_dir.join(format!("test_{}.rs", args.name));
        let test_content = format!(
            r#"//! Integration tests for addon `{name}`.

#[test]
fn test_addon_initialization() {{
    // Verify addon functionality here.
}}
"#,
            name = args.name
        );
        write_file(&test_path, &test_content)?;
    }

    // 9. Commit atomic rename from stage to dest
    if dest_exists_and_empty {
        fs::remove_dir(&dest).map_err(|e| CliError::Io {
            path: dest.clone(),
            source: e,
        })?;
    }

    let stage_path = tmp_dir.keep();
    if let Err(e) = fs::rename(&stage_path, &dest) {
        // Best-effort cleanup of temporary staging directory on rename failure.
        // We ignore any cleanup error here to ensure the primary I/O rename error is preserved.
        let _ = fs::remove_dir_all(&stage_path);
        return Err(CliError::Io {
            path: dest,
            source: e,
        });
    }

    Ok(dest)
}

fn write_file(path: &Path, content: &str) -> Result<()> {
    fs::write(path, content).map_err(|e| CliError::Io {
        path: path.to_path_buf(),
        source: e,
    })
}
