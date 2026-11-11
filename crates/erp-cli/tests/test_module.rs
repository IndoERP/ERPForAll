#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

use erp_cli::args::{DatabaseName, ModuleAction, ModuleOpArgs};
use erp_cli::error::CliError;
use erp_cli::module::{
    ExecutionReport, LifecycleRequest, ModuleLifecycleExecutor, ModuleOperation,
    execute_module_command, execute_module_command_with_executor,
};
use erp_modules::ModuleName;
use std::fs;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tempfile::tempdir;

fn create_mock_addon(dir: &std::path::Path, name: &str, depends: &[&str]) {
    let addon_dir = dir.join(name);
    fs::create_dir_all(&addon_dir).expect("create addon dir");

    let dep_str = depends
        .iter()
        .map(|d| format!("\"{}\"", d))
        .collect::<Vec<_>>()
        .join(", ");

    let manifest_content = format!(
        r#"name = "{name}"
summary = "Test addon {name}"
version = "1.0.0"
license = "MIT OR Apache-2.0"
author = "Tester"
depends = [{dep_str}]
"#
    );

    fs::write(addon_dir.join("manifest.toml"), manifest_content).expect("write manifest");
}

type Call = (Vec<ModuleName>, Vec<ModuleName>);

#[derive(Default, Clone)]
struct RecordingExecutor {
    install_count: Arc<AtomicUsize>,
    upgrade_count: Arc<AtomicUsize>,
    uninstall_count: Arc<AtomicUsize>,
    /// `(requested, plan)` of each call.
    requests: Arc<Mutex<Vec<Call>>>,
}

impl ModuleLifecycleExecutor for RecordingExecutor {
    fn execute(
        &self,
        operation: ModuleOperation,
        request: &LifecycleRequest<'_>,
    ) -> erp_cli::Result<ExecutionReport> {
        let count = match operation {
            ModuleOperation::Install => &self.install_count,
            ModuleOperation::Upgrade => &self.upgrade_count,
            ModuleOperation::Uninstall => &self.uninstall_count,
        };
        count.fetch_add(1, Ordering::SeqCst);
        self.requests
            .lock()
            .expect("lock")
            .push((request.requested.to_vec(), request.plan.to_vec()));
        Ok(ExecutionReport::default())
    }
}

#[test]
fn test_module_install_dry_run_never_invokes_executor() {
    let tmp = tempdir().expect("tempdir");
    let addons_dir = tmp.path().to_path_buf();
    create_mock_addon(&addons_dir, "base", &[]);

    let executor = RecordingExecutor::default();
    let args = ModuleOpArgs::new(
        DatabaseName::new("test_db").expect("db"),
        vec![ModuleName::new("base").expect("name")],
    )
    .with_addons_path(vec![addons_dir])
    .with_dry_run(true);

    let plan = execute_module_command_with_executor(&ModuleAction::Install(args), &executor)
        .expect("plan");
    assert_eq!(plan, vec![ModuleName::new("base").expect("name")]);

    // Architectural guarantee: dry-run NEVER calls the executor
    assert_eq!(executor.install_count.load(Ordering::SeqCst), 0);
    assert_eq!(executor.upgrade_count.load(Ordering::SeqCst), 0);
    assert_eq!(executor.uninstall_count.load(Ordering::SeqCst), 0);
}

#[test]
fn test_module_install_live_invokes_executor_once() {
    let tmp = tempdir().expect("tempdir");
    let addons_dir = tmp.path().to_path_buf();
    create_mock_addon(&addons_dir, "base", &[]);

    let executor = RecordingExecutor::default();
    let args = ModuleOpArgs::new(
        DatabaseName::new("test_db").expect("db"),
        vec![ModuleName::new("base").expect("name")],
    )
    .with_addons_path(vec![addons_dir])
    .with_dry_run(false); // live execution

    let plan = execute_module_command_with_executor(&ModuleAction::Install(args), &executor)
        .expect("plan");
    assert_eq!(plan, vec![ModuleName::new("base").expect("name")]);

    // Live execution invokes install executor exactly once
    assert_eq!(executor.install_count.load(Ordering::SeqCst), 1);
    assert_eq!(executor.upgrade_count.load(Ordering::SeqCst), 0);
    assert_eq!(executor.uninstall_count.load(Ordering::SeqCst), 0);
}

#[test]
fn test_module_install_unknown_module_fails() {
    let tmp = tempdir().expect("tempdir");

    let args = ModuleOpArgs::new(
        DatabaseName::new("test_db").expect("db"),
        vec![ModuleName::new("nonexistent_mod").expect("name")],
    )
    .with_addons_path(vec![tmp.path().to_path_buf()])
    .with_dry_run(true);

    let result = execute_module_command(&ModuleAction::Install(args));
    match result {
        Err(CliError::ModuleNotFound(ref name)) => {
            assert_eq!(name.as_str(), "nonexistent_mod");
            assert_eq!(
                result.unwrap_err().to_string(),
                "Module 'nonexistent_mod' not found in addon search paths"
            );
        }
        other => panic!("expected ModuleNotFound, got {other:?}"),
    }
}

#[test]
fn test_module_install_dependency_resolution_topological_order() {
    let tmp = tempdir().expect("tempdir");
    let addons_dir = tmp.path().to_path_buf();

    create_mock_addon(&addons_dir, "base", &[]);
    create_mock_addon(&addons_dir, "addon_a", &["base"]);
    create_mock_addon(&addons_dir, "addon_b", &["addon_a"]);

    let args = ModuleOpArgs::new(
        DatabaseName::new("test_db").expect("db"),
        vec![ModuleName::new("addon_b").expect("name")],
    )
    .with_addons_path(vec![addons_dir])
    .with_dry_run(true);

    let result = execute_module_command(&ModuleAction::Install(args));
    assert!(result.is_ok(), "expected Ok, got {:?}", result.err());

    let plan = result.expect("plan");
    let names: Vec<&str> = plan.iter().map(|m| m.as_str()).collect();

    // Must resolve base first, then addon_a, then addon_b
    assert_eq!(names, vec!["base", "addon_a", "addon_b"]);
}

#[test]
fn test_module_install_missing_dependency_fails_loudly() {
    let tmp = tempdir().expect("tempdir");
    let addons_dir = tmp.path().to_path_buf();

    create_mock_addon(&addons_dir, "addon_orphan", &["nonexistent_parent"]);

    let args = ModuleOpArgs::new(
        DatabaseName::new("test_db").expect("db"),
        vec![ModuleName::new("addon_orphan").expect("name")],
    )
    .with_addons_path(vec![addons_dir])
    .with_dry_run(true);

    let result = execute_module_command(&ModuleAction::Install(args));
    match result {
        Err(CliError::MissingDependency {
            ref module,
            ref dependency,
        }) => {
            assert_eq!(module.as_str(), "addon_orphan");
            assert_eq!(dependency.as_str(), "nonexistent_parent");
            assert_eq!(
                result.unwrap_err().to_string(),
                "Module 'addon_orphan' requires missing dependency 'nonexistent_parent'"
            );
        }
        other => panic!("expected MissingDependency, got {other:?}"),
    }
}

#[test]
fn test_module_install_cycle_fails_loudly() {
    let tmp = tempdir().expect("tempdir");
    let addons_dir = tmp.path().to_path_buf();

    create_mock_addon(&addons_dir, "cycle_x", &["cycle_y"]);
    create_mock_addon(&addons_dir, "cycle_y", &["cycle_x"]);

    let args = ModuleOpArgs::new(
        DatabaseName::new("test_db").expect("db"),
        vec![ModuleName::new("cycle_x").expect("name")],
    )
    .with_addons_path(vec![addons_dir])
    .with_dry_run(true);

    let result = execute_module_command(&ModuleAction::Install(args));
    match result {
        Err(CliError::DependencyCycle(ref members)) => {
            assert!(members.contains(&ModuleName::new("cycle_x").expect("name")));
            assert!(members.contains(&ModuleName::new("cycle_y").expect("name")));
            let err_str = result.unwrap_err().to_string();
            assert!(err_str.starts_with("Dependency cycle detected among modules:"));
            assert!(err_str.contains("cycle_x"));
            assert!(err_str.contains("cycle_y"));
        }
        other => panic!("expected DependencyCycle, got {other:?}"),
    }
}

#[test]
fn test_module_command_invalid_database_name() {
    match DatabaseName::new("") {
        Err(CliError::InvalidDatabaseName(ref name)) => {
            assert_eq!(name, "");
        }
        other => panic!("expected InvalidDatabaseName for empty string, got {other:?}"),
    }

    match DatabaseName::new("invalid-db-name") {
        Err(CliError::InvalidDatabaseName(ref name)) => {
            assert_eq!(name, "invalid-db-name");
            assert_eq!(
                CliError::InvalidDatabaseName(name.clone()).to_string(),
                "Invalid database name 'invalid-db-name': must be 1..=63 ASCII alphanumeric or underscore characters"
            );
        }
        other => panic!("expected InvalidDatabaseName for dashes, got {other:?}"),
    }

    match DatabaseName::new("db with spaces") {
        Err(CliError::InvalidDatabaseName(ref name)) => {
            assert_eq!(name, "db with spaces");
        }
        other => panic!("expected InvalidDatabaseName for spaces, got {other:?}"),
    }

    assert!(DatabaseName::new("valid_db_123").is_ok());
}

#[test]
fn test_module_uninstall_refuses_when_dependents_exist() {
    let tmp = tempdir().expect("tempdir");
    let addons_dir = tmp.path().to_path_buf();

    create_mock_addon(&addons_dir, "base", &[]);
    create_mock_addon(&addons_dir, "dependent_mod", &["base"]);

    let args = ModuleOpArgs::new(
        DatabaseName::new("test_db").expect("db"),
        vec![ModuleName::new("base").expect("name")],
    )
    .with_addons_path(vec![addons_dir])
    .with_dry_run(true);

    let result = execute_module_command(&ModuleAction::Uninstall(args));
    match result {
        Err(CliError::DependencyResolution(ref msg)) => {
            assert!(msg.contains(
                "cannot uninstall module 'base' because module 'dependent_mod' depends on it"
            ));
            assert!(msg.contains("conservative plan-only check on available addons; a live run cascades to the installed dependents"));
        }
        other => panic!("expected DependencyResolution refusing uninstall, got {other:?}"),
    }
}

#[test]
fn test_module_upgrade_and_uninstall_dry_run_success() {
    let tmp = tempdir().expect("tempdir");
    let addons_dir = tmp.path().to_path_buf();

    create_mock_addon(&addons_dir, "base", &[]);
    create_mock_addon(&addons_dir, "standalone_mod", &["base"]);

    let upgrade_args = ModuleOpArgs::new(
        DatabaseName::new("test_db").expect("db"),
        vec![ModuleName::new("standalone_mod").expect("name")],
    )
    .with_addons_path(vec![addons_dir.clone()])
    .with_dry_run(true);

    let upgrade_plan =
        execute_module_command(&ModuleAction::Upgrade(upgrade_args)).expect("upgrade");
    assert_eq!(
        upgrade_plan,
        vec![
            ModuleName::new("base").expect("base"),
            ModuleName::new("standalone_mod").expect("mod")
        ]
    );

    let uninstall_args = ModuleOpArgs::new(
        DatabaseName::new("test_db").expect("db"),
        vec![ModuleName::new("standalone_mod").expect("name")],
    )
    .with_addons_path(vec![addons_dir])
    .with_dry_run(true);

    let uninstall_plan =
        execute_module_command(&ModuleAction::Uninstall(uninstall_args)).expect("uninstall");
    assert_eq!(
        uninstall_plan,
        vec![ModuleName::new("standalone_mod").expect("mod")]
    );
}

#[test]
fn test_discovery_manifest_error_mapped_real_manifest_path() {
    let tmp = tempdir().expect("tempdir");
    let addon_dir = tmp.path().join("broken_addon");
    fs::create_dir_all(&addon_dir).expect("mkdir");
    let manifest_file = addon_dir.join("manifest.toml");
    fs::write(&manifest_file, "invalid toml ::: =").expect("write broken manifest");

    let args = ModuleOpArgs::new(
        DatabaseName::new("test_db").expect("db"),
        vec![ModuleName::new("broken_addon").expect("name")],
    )
    .with_addons_path(vec![tmp.path().to_path_buf()])
    .with_dry_run(true);

    let result = execute_module_command(&ModuleAction::Install(args));
    match result {
        Err(err) => {
            assert_eq!(err.exit_code(), std::process::ExitCode::from(1));
            match err {
                CliError::Manifest { path, source } => {
                    // Must be the REAL manifest path, not just the bare module name
                    assert_eq!(path, manifest_file);
                    assert!(!source.to_string().is_empty());
                }
                other => panic!("expected CliError::Manifest, got {other:?}"),
            }
        }
        Ok(_) => panic!("expected error"),
    }
}

#[test]
fn test_map_discovery_error_io_maps_to_exit_code_two() {
    use erp_cli::module::map_discovery_error;
    use std::path::PathBuf;
    use std::process::ExitCode;

    let io_err = erp_modules::Error::Io {
        path: PathBuf::from("/nonexistent/manifest.toml"),
        source: std::io::Error::new(std::io::ErrorKind::PermissionDenied, "permission denied"),
    };

    let mapped = map_discovery_error(io_err, &[PathBuf::from("/nonexistent")]);
    match mapped {
        CliError::Io {
            ref path,
            ref source,
        } => {
            assert_eq!(path, &PathBuf::from("/nonexistent/manifest.toml"));
            assert_eq!(source.kind(), std::io::ErrorKind::PermissionDenied);
            assert_eq!(mapped.exit_code(), ExitCode::from(2));
        }
        other => panic!("expected CliError::Io, got {other:?}"),
    }
}

#[cfg(unix)]
#[test]
fn test_discovery_io_error_filesystem_maps_to_exit_code_two() {
    use std::os::unix::fs::PermissionsExt;
    use std::process::ExitCode;

    let tmp = tempdir().expect("tempdir");
    let addon_dir = tmp.path().join("unreadable_addon");
    fs::create_dir_all(&addon_dir).expect("mkdir");
    let manifest_file = addon_dir.join("manifest.toml");
    fs::write(&manifest_file, "name = \"unreadable_addon\"\n").expect("write");

    // Make manifest unreadable to force std::io::Error on read_to_string
    fs::set_permissions(&manifest_file, fs::Permissions::from_mode(0o000)).expect("chmod 000");

    let args = ModuleOpArgs::new(
        DatabaseName::new("test_db").expect("db"),
        vec![ModuleName::new("unreadable_addon").expect("name")],
    )
    .with_addons_path(vec![tmp.path().to_path_buf()])
    .with_dry_run(true);

    let result = execute_module_command(&ModuleAction::Install(args));

    // Restore permissions so tempdir cleanup succeeds
    let _ = fs::set_permissions(&manifest_file, fs::Permissions::from_mode(0o644));

    match result {
        Err(err) => {
            assert_eq!(err.exit_code(), ExitCode::from(2));
            match err {
                CliError::Io { path, .. } => {
                    assert_eq!(path, manifest_file);
                }
                other => panic!("expected CliError::Io, got {other:?}"),
            }
        }
        Ok(_) => panic!("expected error"),
    }
}

#[test]
fn test_map_discovery_error_preserves_underlying_errors() {
    use erp_cli::module::map_discovery_error;
    use std::path::PathBuf;

    let errors = vec![
        erp_modules::Error::DiscoveryError {
            path: PathBuf::from("/test/path_a"),
            message: "custom issue a".to_string(),
        },
        erp_modules::Error::DiscoveryError {
            path: PathBuf::from("/test/path_b"),
            message: "custom issue b".to_string(),
        },
    ];

    let compound_err = erp_modules::Error::DiscoveryErrors {
        count: errors.len(),
        errors,
    };

    let mapped = map_discovery_error(compound_err, &[PathBuf::from("/test")]);
    match mapped {
        CliError::MultipleDiscoveryErrors(ref errors) => {
            assert_eq!(errors.len(), 2);
            let msg = mapped.to_string();
            assert!(msg.contains("Addon discovery failed with 2 error(s):"));
            assert!(msg.contains("custom issue a"));
            assert!(msg.contains("custom issue b"));
        }
        other => panic!("expected MultipleDiscoveryErrors preserving errors, got {other:?}"),
    }
}

/// MUST-FIX: When DiscoveryErrors contains both an Io error and a manifest error,
/// ALL errors must appear in the message and the exit code must be 2 (because of the Io).
/// The original io::Error must be moved, not rebuilt.
#[test]
fn test_map_discovery_errors_mixed_io_and_manifest_preserves_all_exit_code_two() {
    use erp_cli::module::map_discovery_error;
    use std::path::PathBuf;
    use std::process::ExitCode;

    let tmp = tempdir().expect("tempdir");
    create_mock_addon(tmp.path(), "broken_addon", &[]);
    let errors = vec![
        erp_modules::Error::Io {
            path: PathBuf::from("/addons/unreadable/manifest.toml"),
            source: std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "permission denied on unreadable manifest",
            ),
        },
        erp_modules::Error::InvalidManifest {
            name: ModuleName::new("broken_addon").expect("name"),
            message: "invalid syntax in manifest".to_string(),
        },
    ];

    let compound_err = erp_modules::Error::DiscoveryErrors {
        count: errors.len(),
        errors,
    };

    let mapped = map_discovery_error(compound_err, &[tmp.path().to_path_buf()]);
    match mapped {
        CliError::MultipleDiscoveryErrors(ref errors) => {
            assert_eq!(errors.len(), 2);
            // First error should be Io
            assert!(
                matches!(&errors[0], CliError::Io { .. }),
                "expected first error to be Io, got {:?}",
                errors[0]
            );
            // Second error should be Manifest
            assert!(
                matches!(&errors[1], CliError::Manifest { path, .. }
                    if *path == tmp.path().join("broken_addon").join("manifest.toml")),
                "expected second error to be Manifest, got {:?}",
                errors[1]
            );
            // Both errors appear in the formatted message
            let msg = mapped.to_string();
            assert!(
                msg.contains("permission denied on unreadable manifest"),
                "Io error missing from message: {msg}"
            );
            assert!(
                msg.contains("invalid syntax in manifest"),
                "Manifest error missing from message: {msg}"
            );
            // Exit code must be 2 because there is an Io error
            assert_eq!(mapped.exit_code(), ExitCode::from(2));
        }
        other => panic!("expected MultipleDiscoveryErrors, got {other:?}"),
    }
}

/// SHOULD-FIX: `uninstall A B` where B depends on A should deduplicate and order B first
/// (dependents before their dependencies in reverse topological order).
#[test]
fn test_uninstall_deduplicates_and_orders_dependents_first() {
    let tmp = tempdir().expect("tempdir");
    let addons_dir = tmp.path().to_path_buf();

    create_mock_addon(&addons_dir, "base", &[]);
    create_mock_addon(&addons_dir, "addon_a", &["base"]);
    create_mock_addon(&addons_dir, "addon_b", &["addon_a"]);

    // Request uninstall of addon_a then addon_b (wrong order: addon_b depends on addon_a)
    // Also include a duplicate addon_a to test deduplication.
    let args = ModuleOpArgs::new(
        DatabaseName::new("test_db").expect("db"),
        vec![
            ModuleName::new("addon_a").expect("name"),
            ModuleName::new("addon_b").expect("name"),
            ModuleName::new("addon_a").expect("name"), // duplicate
        ],
    )
    .with_addons_path(vec![addons_dir])
    .with_dry_run(true);

    let result = execute_module_command(&ModuleAction::Uninstall(args));
    assert!(result.is_ok(), "expected Ok, got {:?}", result.err());

    let plan = result.expect("plan");
    let names: Vec<&str> = plan.iter().map(|m| m.as_str()).collect();

    // addon_b depends on addon_a → addon_b must come first; duplicate addon_a removed
    assert_eq!(names, vec!["addon_b", "addon_a"]);
}

/// HIGH: `uninstall` of a 3-module dependency chain c -> b -> a given in scrambled order
/// must produce reverse topological order [c, b, a] without relying on a non-total sort_by comparator.
#[test]
fn test_uninstall_three_module_chain_scrambled_order() {
    let tmp = tempdir().expect("tempdir");
    let addons_dir = tmp.path().to_path_buf();

    create_mock_addon(&addons_dir, "base", &[]);
    create_mock_addon(&addons_dir, "chain_a", &["base"]);
    create_mock_addon(&addons_dir, "chain_b", &["chain_a"]);
    create_mock_addon(&addons_dir, "chain_c", &["chain_b"]);

    // Scrambled order: a, c, b
    let args1 = ModuleOpArgs::new(
        DatabaseName::new("test_db").expect("db"),
        vec![
            ModuleName::new("chain_a").expect("name"),
            ModuleName::new("chain_c").expect("name"),
            ModuleName::new("chain_b").expect("name"),
        ],
    )
    .with_addons_path(vec![addons_dir.clone()])
    .with_dry_run(true);

    let plan1 = execute_module_command(&ModuleAction::Uninstall(args1)).expect("uninstall 1");
    let names1: Vec<&str> = plan1.iter().map(|m| m.as_str()).collect();
    assert_eq!(names1, vec!["chain_c", "chain_b", "chain_a"]);

    // Another scrambled order: b, a, c
    let args2 = ModuleOpArgs::new(
        DatabaseName::new("test_db").expect("db"),
        vec![
            ModuleName::new("chain_b").expect("name"),
            ModuleName::new("chain_a").expect("name"),
            ModuleName::new("chain_c").expect("name"),
        ],
    )
    .with_addons_path(vec![addons_dir])
    .with_dry_run(true);

    let plan2 = execute_module_command(&ModuleAction::Uninstall(args2)).expect("uninstall 2");
    let names2: Vec<&str> = plan2.iter().map(|m| m.as_str()).collect();
    assert_eq!(names2, vec!["chain_c", "chain_b", "chain_a"]);
}

/// A manifest error for a module whose manifest is under no search path has no file to name:
/// the error says so in words, with no placeholder path.
#[test]
fn test_map_discovery_error_manifest_not_found_names_no_placeholder_path() {
    use erp_cli::module::map_discovery_error;
    use std::path::PathBuf;
    use std::process::ExitCode;

    let err = erp_modules::Error::ManifestNotFound {
        name: ModuleName::new("nonexistent_mod").expect("name"),
    };

    let mapped = map_discovery_error(err, &[PathBuf::from("/nonexistent/addons/path")]);
    assert!(
        matches!(&mapped, CliError::DependencyResolution(_)),
        "{mapped:?}"
    );
    assert_eq!(
        mapped.to_string(),
        "Module dependency resolution failed: Module 'nonexistent_mod': manifest not found"
    );
    assert_eq!(mapped.exit_code(), ExitCode::from(1));
}

/// Looking for the manifest file of a failed module must not hide an I/O error as "not there":
/// a search path that cannot be read is reported as an I/O error (exit code 2).
#[cfg(unix)]
#[test]
fn test_map_discovery_error_unreadable_search_path_is_an_io_error() {
    use erp_cli::module::map_discovery_error;
    use std::os::unix::fs::PermissionsExt;
    use std::process::ExitCode;

    let tmp = tempdir().expect("tempdir");
    let locked = tmp.path().join("locked");
    fs::create_dir_all(&locked).expect("mkdir");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).expect("chmod 000");
    // The permission bits do not apply to a superuser: nothing to check then.
    let enforced = fs::read_dir(&locked).is_err();

    let err = erp_modules::Error::InvalidManifest {
        name: ModuleName::new("hidden_mod").expect("name"),
        message: "bad".to_owned(),
    };
    let mapped = map_discovery_error(err, std::slice::from_ref(&locked));
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).expect("restore");
    if !enforced {
        return;
    }
    match &mapped {
        CliError::Io { path, source } => {
            assert_eq!(path, &locked.join("hidden_mod").join("manifest.toml"));
            assert_eq!(source.kind(), std::io::ErrorKind::PermissionDenied);
            assert_eq!(mapped.exit_code(), ExitCode::from(2));
        }
        other => panic!("expected CliError::Io, got {other:?}"),
    }
}

/// The executor gets the modules as named (without duplicates) next to the offline plan, so
/// the module system resolves dependencies against the database, not against the disk.
#[test]
fn live_run_passes_requested_modules_and_plan_to_the_executor() {
    let tmp = tempdir().expect("tempdir");
    let addons_dir = tmp.path().to_path_buf();
    create_mock_addon(&addons_dir, "base", &[]);
    create_mock_addon(&addons_dir, "addon_a", &["base"]);
    create_mock_addon(&addons_dir, "addon_b", &["addon_a"]);

    let executor = RecordingExecutor::default();
    let b = ModuleName::new("addon_b").expect("name");
    let args = ModuleOpArgs::new(
        DatabaseName::new("test_db").expect("db"),
        vec![b.clone(), b.clone()],
    )
    .with_addons_path(vec![addons_dir]);
    let plan = execute_module_command_with_executor(&ModuleAction::Install(args), &executor)
        .expect("plan");

    let names = |v: &[ModuleName]| v.iter().map(|m| m.to_string()).collect::<Vec<_>>();
    assert_eq!(names(&plan), ["base", "addon_a", "addon_b"]);
    let calls = executor.requests.lock().expect("lock");
    assert_eq!(calls.len(), 1);
    assert_eq!(names(&calls[0].0), ["addon_b"]);
    assert_eq!(names(&calls[0].1), ["base", "addon_a", "addon_b"]);
}

/// A live uninstall leaves dependents to the module system (which cascades to the installed
/// ones); the plan-only dry run keeps its conservative refusal.
#[test]
fn live_uninstall_defers_the_dependents_check_to_the_executor() {
    let tmp = tempdir().expect("tempdir");
    let addons_dir = tmp.path().to_path_buf();
    create_mock_addon(&addons_dir, "base", &[]);
    create_mock_addon(&addons_dir, "dependent_mod", &["base"]);

    let executor = RecordingExecutor::default();
    let args = ModuleOpArgs::new(
        DatabaseName::new("test_db").expect("db"),
        vec![ModuleName::new("base").expect("name")],
    )
    .with_addons_path(vec![addons_dir]);
    execute_module_command_with_executor(&ModuleAction::Uninstall(args.clone()), &executor)
        .expect("live uninstall reaches the executor");
    assert_eq!(executor.uninstall_count.load(Ordering::SeqCst), 1);

    let err = execute_module_command_with_executor(
        &ModuleAction::Uninstall(args.with_dry_run(true)),
        &executor,
    )
    .unwrap_err();
    assert!(matches!(&err, CliError::DependencyResolution(_)), "{err:?}");
    assert_eq!(
        err.to_string(),
        "Module dependency resolution failed: cannot uninstall module 'base' because module 'dependent_mod' depends on it (conservative plan-only check on available addons; a live run cascades to the installed dependents)"
    );
    assert_eq!(executor.uninstall_count.load(Ordering::SeqCst), 1);
}

#[test]
fn restore_generated_restrictions_is_refused_outside_the_upgrade_command() {
    let tmp = tempdir().expect("tempdir");
    let addons_dir = tmp.path().to_path_buf();
    create_mock_addon(&addons_dir, "base", &[]);
    create_mock_addon(&addons_dir, "standalone_mod", &["base"]);

    let args = || {
        ModuleOpArgs::new(
            DatabaseName::new("test_db").expect("db"),
            vec![ModuleName::new("standalone_mod").expect("name")],
        )
        .with_addons_path(vec![addons_dir.clone()])
        .with_dry_run(true)
        .with_restore_generated_restrictions(true)
    };
    for action in [
        ModuleAction::Install(args()),
        ModuleAction::Uninstall(args()),
    ] {
        match execute_module_command(&action) {
            Err(CliError::InvalidInput { arg, reason }) => {
                assert_eq!(arg, "--restore-generated-restrictions");
                assert_eq!(
                    reason,
                    "only the upgrade command rewrites generated company restrictions"
                );
            }
            other => panic!("expected InvalidInput, got {other:?}"),
        }
    }
    // the upgrade command accepts it
    execute_module_command(&ModuleAction::Upgrade(args())).expect("upgrade accepts the flag");
}
