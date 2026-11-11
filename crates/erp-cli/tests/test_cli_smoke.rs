#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

use clap::Parser;
use erp_cli::args::{Cli, Commands, ModuleAction};
use erp_cli::run;
use erp_modules::ModuleName;
use std::fs;
use std::path::PathBuf;
use std::process::{Command, ExitCode};
use tempfile::tempdir;

#[test]
fn test_cli_parsing_scaffold() {
    let cli = Cli::try_parse_from(["erp", "scaffold", "sale_order", "--dest", "/tmp/sale"])
        .expect("parse");
    match cli.command {
        Commands::Scaffold(args) => {
            assert_eq!(args.name, "sale_order");
            assert_eq!(args.dest, Some(PathBuf::from("/tmp/sale")));
            assert!(!args.data_only);
            assert_eq!(args.author, "ERPForAll Contributor");
        }
        _ => panic!("expected Scaffold command"),
    }
}

#[test]
fn test_cli_parsing_scaffold_data_only() {
    let cli = Cli::try_parse_from([
        "erp",
        "scaffold",
        "custom_tags",
        "--data-only",
        "--author",
        "Alice",
        "--summary",
        "Custom tags module",
    ])
    .expect("parse");

    match cli.command {
        Commands::Scaffold(args) => {
            assert_eq!(args.name, "custom_tags");
            assert!(args.data_only);
            assert_eq!(args.author, "Alice");
            assert_eq!(args.summary.as_deref(), Some("Custom tags module"));
        }
        _ => panic!("expected Scaffold command"),
    }
}

#[test]
fn test_cli_parsing_module_install() {
    let cli = Cli::try_parse_from([
        "erp",
        "module",
        "install",
        "-d",
        "production_db",
        "account",
        "sale",
        "--addons-path",
        "/custom/addons",
        "--dry-run",
    ])
    .expect("parse");

    match cli.command {
        Commands::Module(cmd) => match cmd.action {
            ModuleAction::Install(args) => {
                assert_eq!(args.database.as_str(), "production_db");
                assert_eq!(
                    args.modules,
                    vec![
                        ModuleName::new("account").expect("mod"),
                        ModuleName::new("sale").expect("mod")
                    ]
                );
                assert_eq!(args.addons_path, vec![PathBuf::from("/custom/addons")]);
                assert!(args.dry_run);
            }
            _ => panic!("expected Install action"),
        },
        _ => panic!("expected Module command"),
    }
}

#[test]
fn test_cli_parsing_module_missing_database_fails() {
    let result = Cli::try_parse_from(["erp", "module", "install", "account"]);
    match result {
        Err(err) => {
            assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
            let msg = err.to_string();
            assert!(msg.contains("--database"));
        }
        Ok(_) => panic!("expected failure without -d/--database"),
    }
}

#[test]
fn test_cli_parsing_dev() {
    let cli = Cli::try_parse_from(["erp", "dev", "-d", "dev_db", "-p", "9000"]).expect("parse");
    match cli.command {
        Commands::Dev(args) => {
            assert_eq!(args.database.as_ref().map(|d| d.as_str()), Some("dev_db"));
            assert_eq!(args.port, 9000);
        }
        _ => panic!("expected Dev command"),
    }
}

#[test]
fn test_run_scaffold_end_to_end() {
    let tmp = tempdir().expect("tempdir");
    let dest = tmp.path().join("test_addon");

    let exit_code = run([
        "erp",
        "scaffold",
        "test_addon",
        "--dest",
        dest.to_str().expect("str"),
    ]);

    assert_eq!(exit_code, ExitCode::SUCCESS);
    assert!(dest.join("manifest.toml").is_file());
    assert!(dest.join("data/test_addon_data.xml").is_file());
}

#[test]
fn test_run_dev_port_zero_exits_code_one() {
    let exit_code = run(["erp", "dev", "-p", "0"]);
    // Invalid argument/input must exit with code 1
    assert_eq!(exit_code, ExitCode::from(1));
}

#[test]
fn test_run_dev_unimplemented_runtime_exits_code_two() {
    let tmp = tempdir().expect("tempdir");
    let exit_code = run([
        "erp",
        "dev",
        "--addons-path",
        tmp.path().to_str().expect("str"),
    ]);
    // Unimplemented runtime error exits with code 2
    assert_eq!(exit_code, ExitCode::from(2));
}

#[test]
fn test_binary_smoke_version() {
    let output = Command::new(env!("CARGO_BIN_EXE_erp"))
        .arg("--version")
        .output()
        .expect("run erp binary");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("erp"));
}

#[test]
fn test_binary_smoke_module_not_found() {
    let tmp = tempdir().expect("tempdir");
    let output = Command::new(env!("CARGO_BIN_EXE_erp"))
        .args([
            "module",
            "install",
            "nonexistent_mod",
            "-d",
            "test_db",
            "--addons-path",
            tmp.path().to_str().expect("str"),
            "--dry-run",
        ])
        .output()
        .expect("run erp binary");

    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("Module 'nonexistent_mod' not found in addon search paths"));
}

#[test]
fn test_binary_smoke_dependency_cycle() {
    let tmp = tempdir().expect("tempdir");
    let dir = tmp.path();

    // Create cyclic addons
    let mod_x = dir.join("mod_x");
    fs::create_dir_all(&mod_x).expect("mkdir");
    fs::write(
        mod_x.join("manifest.toml"),
        r#"name = "mod_x"
summary = "Cycle X"
version = "1.0.0"
license = "MIT OR Apache-2.0"
author = "Tester"
depends = ["mod_y"]
"#,
    )
    .expect("write");

    let mod_y = dir.join("mod_y");
    fs::create_dir_all(&mod_y).expect("mkdir");
    fs::write(
        mod_y.join("manifest.toml"),
        r#"name = "mod_y"
summary = "Cycle Y"
version = "1.0.0"
license = "MIT OR Apache-2.0"
author = "Tester"
depends = ["mod_x"]
"#,
    )
    .expect("write");

    let output = Command::new(env!("CARGO_BIN_EXE_erp"))
        .args([
            "module",
            "install",
            "mod_x",
            "-d",
            "test_db",
            "--addons-path",
            dir.to_str().expect("str"),
            "--dry-run",
        ])
        .output()
        .expect("run erp binary");

    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("Dependency cycle detected among modules"));
    assert!(stderr.contains("mod_x"));
    assert!(stderr.contains("mod_y"));
}

#[test]
fn test_binary_smoke_missing_database() {
    let output = Command::new(env!("CARGO_BIN_EXE_erp"))
        .args(["module", "install", "base"])
        .output()
        .expect("run erp binary");

    // Clap usage error exits with code 2
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("--database"));
}

#[test]
fn test_binary_smoke_live_execution_needs_database_url() {
    let tmp = tempdir().expect("tempdir");
    let base_dir = tmp.path().join("base");
    fs::create_dir_all(&base_dir).expect("mkdir");
    fs::write(
        base_dir.join("manifest.toml"),
        r#"name = "base"
summary = "Base addon"
version = "1.0.0"
license = "MIT OR Apache-2.0"
author = "Tester"
depends = []
"#,
    )
    .expect("write");

    let output = Command::new(env!("CARGO_BIN_EXE_erp"))
        .args([
            "module",
            "install",
            "base",
            "-d",
            "test_db",
            "--addons-path",
            tmp.path().to_str().expect("str"),
        ])
        .env_remove("DATABASE_URL")
        .output()
        .expect("run erp binary");

    // The credentials come from the environment only: an input error (exit code 1)
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        String::from_utf8_lossy(&output.stderr),
        "Error: Invalid argument 'DATABASE_URL': not set: the connection credentials come from the environment\n"
    );
}

#[test]
fn test_binary_smoke_invalid_database_name() {
    let output = Command::new(env!("CARGO_BIN_EXE_erp"))
        .args(["module", "install", "base", "-d", "invalid!name"])
        .output()
        .expect("run erp binary");

    // Invalid database name value validation exits with code 1
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("Invalid database name 'invalid!name'"));
}
