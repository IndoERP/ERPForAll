#![allow(clippy::unwrap_used, reason = "tests")]
use clap::Parser;
use erp_cli::Cli;
#[test]
fn human_server_and_database_bootstrap_have_explicit_operator_commands() {
    Cli::try_parse_from(["erp", "server", "--config", "/private/server.toml"]).unwrap();
    Cli::try_parse_from([
        "erp",
        "db",
        "init",
        "--config",
        "/private/server.toml",
        "--admin-login",
        "operator",
        "--admin-password-file",
        "/private/password",
    ])
    .unwrap();
}

#[test]
fn runtime_grant_publication_requires_explicit_config() {
    Cli::try_parse_from([
        "erp",
        "db",
        "publish-grants",
        "--config",
        "/private/server.toml",
    ])
    .unwrap();
    let error = Cli::try_parse_from(["erp", "db", "publish-grants"]).unwrap_err();
    assert_eq!(
        error.kind(),
        clap::error::ErrorKind::MissingRequiredArgument
    );
    assert!(error.to_string().contains("--config"));
}

/// The publisher contract is a no-effect binary capability, independent of deployment config.
#[test]
fn runtime_grant_contract_needs_no_config_database_or_async_runtime() {
    let root = tempfile::tempdir().unwrap();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_erp"))
        .args(["db", "runtime-grant-contract"])
        .current_dir(root.path())
        .env_clear()
        .output()
        .unwrap();
    assert!(output.status.success(), "{:?}", output);
    assert!(output.stderr.is_empty(), "{:?}", output.stderr);
    let contract: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        contract,
        serde_json::json!({"version": 6, "ui_assets": "read-only", "node_features": "execute-only", "event_outbox": "insert-only", "uom_references": "guarded-monotone", "job_queue": "column-grants", "audit_trail": "insert-only"})
    );
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
}

#[test]
fn runtime_grant_contract_rejects_config_or_unknown_options() {
    for option in ["--config", "--unknown"] {
        let error =
            Cli::try_parse_from(["erp", "db", "runtime-grant-contract", option]).unwrap_err();
        assert_eq!(error.kind(), clap::error::ErrorKind::UnknownArgument);
        assert_eq!(
            error.to_string(),
            format!(
                "error: unexpected argument '{option}' found\n\nUsage: erp db runtime-grant-contract\n\nFor more information, try '--help'.\n"
            )
        );
    }
}
