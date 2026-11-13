// SPDX-License-Identifier: MIT OR Apache-2.0
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]
use clap::Parser;
use erp_cli::{
    Cli,
    args::{AttachmentAction, Commands},
};
#[test]
fn attachment_claim_requires_explicit_operator_decision() {
    let base = [
        "erp",
        "attachments",
        "claim",
        "--config",
        "/tmp/server.toml",
        "--store",
        "main",
        "--force",
        "--reason",
        "restore-primary",
        "--confirm",
        "snapshot",
    ];
    let cli = Cli::try_parse_from(base).unwrap();
    assert!(
        matches!(cli.command,Commands::Attachments(ref c) if matches!(c.action,AttachmentAction::Claim(_)))
    );
    for option in ["--force", "--reason", "--confirm"] {
        let mut a = base.to_vec();
        let at = a.iter().position(|s| *s == option).unwrap();
        a.remove(at);
        if option != "--force" {
            a.remove(at);
        }
        let e = Cli::try_parse_from(a).unwrap_err();
        assert_eq!(e.kind(), clap::error::ErrorKind::MissingRequiredArgument);
        let missing = match option {
            "--force" => "--force",
            "--reason" => "--reason <REASON>",
            "--confirm" => "--confirm <CONFIRM>",
            _ => panic!("unknown fixture option"),
        };
        let usage = if option == "--reason" {
            "erp attachments claim --config <CONFIG> --store <STORE> --force --confirm <CONFIRM> --reason <REASON>"
        } else {
            "erp attachments claim --config <CONFIG> --store <STORE> --force --reason <REASON> --confirm <CONFIRM>"
        };
        assert_eq!(
            e.to_string(),
            format!(
                "error: the following required arguments were not provided:\n  {missing}\n\nUsage: {usage}\n\nFor more information, try '--help'.\n"
            )
        );
    }
    let e = Cli::try_parse_from([
        "erp",
        "attachments",
        "claim",
        "--config",
        "x",
        "--store",
        "main",
        "--force",
        "--resume",
        "bad",
        "--reason",
        "retire-clone",
    ])
    .unwrap_err();
    assert_eq!(e.kind(), clap::error::ErrorKind::ArgumentConflict);
    assert_eq!(
        e.to_string(),
        "error: the argument '--resume <RESUME>' cannot be used with '--reason <REASON>'\n\nUsage: erp attachments claim --config <CONFIG> --store <STORE> --force --resume <RESUME>\n\nFor more information, try '--help'.\n"
    );
}

#[tokio::test]
async fn configured_binary_claim_and_resume_use_real_store_and_audit() {
    use erp_addon_base::attachments::{
        ownership::{self, Purpose},
        recovery,
    };
    use erp_orm::{BeginOptions, Isolation, SUPERUSER_ID};
    use erp_sdk::blob::{AttachmentLimits, AttachmentRuntime, AttachmentStore, LocalStore};
    use std::{os::unix::fs::PermissionsExt, process::Command, sync::Arc};
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("store");
    let store = Arc::new(LocalStore::open(&path).await.unwrap());
    let rt = Arc::new(AttachmentRuntime::single(
        "main",
        store.clone(),
        AttachmentLimits::default(),
    ));
    let h = erp_testkit::Harness::builder([
        &erp_test_models::base_users::ADDON,
        &erp_addon_base::ADDON,
    ])
    .attach(rt)
    .build()
    .await
    .unwrap();
    h.orm().check_audit_store().await.unwrap();
    let mut tx = h
        .orm()
        .begin_opts(BeginOptions::new(SUPERUSER_ID).isolation(Isolation::ReadCommitted))
        .await
        .unwrap();
    let p = ownership::prepare(&mut tx, "main", Purpose::Heartbeat)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let a = p.execute().await;
    let mut tx = h
        .orm()
        .begin_opts(BeginOptions::new(SUPERUSER_ID).isolation(Isolation::ReadCommitted))
        .await
        .unwrap();
    ownership::settle(&mut tx, &p, a).await.unwrap();
    tx.commit().await.unwrap();
    store
        .create_contested(p.namespace(), p.marker())
        .await
        .unwrap();
    let c = h.db().config();
    let erp_testkit::tokio_postgres::config::Host::Tcp(host) = &c.get_hosts()[0] else {
        panic!("TCP fixture")
    };
    fn enc(b: &[u8]) -> String {
        b.iter()
            .map(|b| {
                if b.is_ascii_alphanumeric() {
                    char::from(*b).to_string()
                } else {
                    format!("%{b:02X}")
                }
            })
            .collect()
    }
    let url = format!(
        "postgresql://{}:{}@{}:{}/{}",
        enc(c.get_user().unwrap().as_bytes()),
        enc(c.get_password().unwrap()),
        host,
        c.get_ports()[0],
        c.get_dbname().unwrap()
    );
    let request = h.read_only_role().await.unwrap();
    let runtime_url = format!(
        "postgresql://{}:{}@{}:{}/{}",
        enc(request.get_user().unwrap().as_bytes()),
        enc(request.get_password().unwrap()),
        host,
        request.get_ports()[0],
        request.get_dbname().unwrap()
    );
    let runtime_secret = root.path().join("runtime-url");
    std::fs::write(&runtime_secret, runtime_url).unwrap();
    std::fs::set_permissions(&runtime_secret, std::fs::Permissions::from_mode(0o600)).unwrap();
    let config = root.path().join("server.toml");
    let secret = root.path().join("owner-url");
    std::fs::write(&secret, &url).unwrap();
    std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o600)).unwrap();
    std::fs::write(&config,format!("database = {:?}\ndatabase_url_file = \"runtime-url\"\nowner_database_url_file = \"owner-url\"\nbind = \"127.0.0.1:18080\"\npublic_origin = \"http://127.0.0.1:18080\"\nproxy_mode = \"direct\"\naddons_path = [\".\"]\nassets_manifest = \"manifest.json\"\n[attachments]\nwrite_store = \"main\"\n[attachments.stores.main]\nkind = \"local\"\npath = {:?}\n",c.get_dbname().unwrap(),path.to_str().unwrap())).unwrap();
    std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o600)).unwrap();
    let confirmation = recovery::inspect(h.orm(), "main")
        .await
        .unwrap()
        .confirmation();
    let output = Command::new(env!("CARGO_BIN_EXE_erp"))
        .args([
            "attachments",
            "claim",
            "--config",
            config.to_str().unwrap(),
            "--store",
            "main",
            "--force",
            "--reason",
            "restore-primary",
            "--confirm",
            &confirmation,
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "claim stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(store.contested(p.namespace()).await.unwrap().is_none());
    let owner = h.db().client().await.unwrap();
    let op: String = owner
        .query_one("SELECT operation::text FROM ir_blob_owner_claim", &[])
        .await
        .unwrap()
        .get(0);
    let output = Command::new(env!("CARGO_BIN_EXE_erp"))
        .args([
            "attachments",
            "claim",
            "--config",
            config.to_str().unwrap(),
            "--store",
            "main",
            "--force",
            "--resume",
            &op,
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "resume stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    h.cleanup().await.unwrap();
}
