// SPDX-License-Identifier: MIT OR Apache-2.0
#![allow(clippy::unwrap_used, reason = "tests")]
use clap::Parser;
use erp_cli::{Cli, Commands};
#[test]
fn attachment_peer_cli_requires_explicit_peer_config_store_and_action() {
    let cli = Cli::try_parse_from([
        "erp",
        "attachments",
        "probe-store",
        "--peer",
        "--config",
        "private.toml",
        "--store",
        "main",
        "start",
    ])
    .unwrap();
    assert!(matches!(cli.command, Commands::Attachments(_)));
    let cases = [
        (
            vec!["erp", "attachments", "probe-store", "start"],
            clap::error::ErrorKind::MissingRequiredArgument,
            "error: the following required arguments were not provided:\n  --peer\n  --config <CONFIG>\n  --store <STORE>\n\nUsage: erp attachments probe-store --peer --config <CONFIG> --store <STORE> <COMMAND>\n\nFor more information, try '--help'.\n",
        ),
        (
            vec![
                "erp",
                "attachments",
                "probe-store",
                "--peer",
                "--config",
                "private.toml",
                "--store",
                "main",
                "join",
                "bad-uuid",
            ],
            clap::error::ErrorKind::ValueValidation,
            "error: invalid value 'bad-uuid' for '<SESSION>': peer session must be a canonical UUID\n\nFor more information, try '--help'.\n",
        ),
    ];
    for (args, kind, message) in cases {
        let e = Cli::try_parse_from(args).unwrap_err();
        assert_eq!(e.kind(), kind);
        assert_eq!(e.to_string(), message);
    }
}

#[test]
fn real_binary_refuses_unshared_store_before_database_work() {
    use std::{fs, os::unix::fs::PermissionsExt, process::Command};
    let dir = tempfile::tempdir().unwrap();
    for (name, text) in [
        (
            "owner-url",
            "postgresql://fixture_owner:fixture-password@127.0.0.1/fixture",
        ),
        (
            "runtime-url",
            "postgresql://fixture_runtime:fixture-password@127.0.0.1/fixture",
        ),
    ] {
        let p = dir.path().join(name);
        fs::write(&p, text).unwrap();
        fs::set_permissions(p, fs::Permissions::from_mode(0o600)).unwrap();
    }
    let config = dir.path().join("server.toml");
    fs::write(&config,"database='fixture'\ndatabase_url_file='runtime-url'\nowner_database_url_file='owner-url'\nbind='127.0.0.1:8765'\npublic_origin='http://127.0.0.1:8765'\nproxy_mode='direct'\naddons_path=['.']\nassets_manifest='manifest.json'\n[attachments]\nwrite_store='main'\n[attachments.stores.main]\nkind='local'\npath='unused'\nshared_filesystem=false\n").unwrap();
    let text = fs::read_to_string(&config).unwrap().replace(
        "path='unused'",
        &format!(
            "path={}",
            toml::Value::String(dir.path().join("unused").to_str().unwrap().into())
        ),
    );
    fs::write(&config, text).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_erp"))
        .args(["attachments", "probe-store", "--peer", "--config"])
        .arg(&config)
        .args(["--store", "main", "start"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        "Error: Execution error: Peer probing requires shared_filesystem = true on a path mounted by both hosts\n"
    );
    assert!(output.stdout.is_empty());
    assert!(!dir.path().join("unused").exists());
}
