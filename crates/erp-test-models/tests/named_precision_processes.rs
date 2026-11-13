//! Two OS processes verify transaction-visible precision and rollback after node loss.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stdout,
    reason = "subprocess tests"
)]
use erp_fields::{Cell, ErrorKind};
use erp_orm::{Orm, RawVals, StaticPolicy};
use erp_registry::RegistryBuilder;
use erp_test_models::{base_users, test_defaults};
use erp_testkit::{Harness, tokio_postgres};
use std::{process::Stdio, sync::Arc, time::Duration};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
const PREFIX: &str = "ERP_NAMED_PRECISION_";
const LIMIT: Duration = Duration::from_secs(120);

fn row(value: &str) -> RawVals {
    RawVals::new().set_cell("price", Cell::Decimal(value.parse().unwrap()))
}

#[tokio::test]
#[ignore = "protocol worker explicitly executed by named_precision_survives_process_loss"]
async fn named_precision_child_worker() {
    let need = |name: &str| std::env::var(format!("{PREFIX}{name}")).expect("worker configuration");
    let mut config = tokio_postgres::Config::new();
    let host = need("HOST");
    if host.starts_with('/') {
        config.host_path(&host);
    } else {
        config.host(host);
    }
    config
        .port(need("PORT").parse().unwrap())
        .user(need("USER"))
        .dbname(need("DB"));
    if let Ok(password) = std::env::var(format!("{PREFIX}PASSWORD")) {
        config.password(password);
    }
    let registry = Arc::new(
        RegistryBuilder::new([
            &base_users::ADDON,
            &erp_addon_base::ADDON,
            &test_defaults::ADDON,
        ])
        .build()
        .unwrap(),
    );
    let orm = Orm::builder(registry, erp_orm::pool::create_pool(config, 2).unwrap())
        .policy(StaticPolicy::AllowAll)
        .trust_caller_resolutions()
        .build()
        .unwrap();
    let mut retained = None;
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    println!("@@ ready {}", std::process::id());
    while let Some(line) = lines.next_line().await.unwrap() {
        let scale = match line.as_str() {
            "commit4" => 4,
            "stage8" => 8,
            _ => panic!("invalid worker command"),
        };
        let mut tx = orm.begin(erp_orm::SUPERUSER_ID).await.unwrap();
        tx.batch(&format!(
            "UPDATE decimal_precision SET digits = {scale} WHERE name = 'Product Price'"
        ))
        .await
        .unwrap();
        if line == "commit4" {
            tx.commit().await.unwrap();
            println!("@@ committed");
        } else {
            retained = Some(tx);
            println!("@@ staged");
        }
    }
    drop(retained);
}

async fn response(lines: &mut tokio::io::Lines<BufReader<tokio::process::ChildStdout>>) -> String {
    tokio::time::timeout(LIMIT, async {
        loop {
            let line = lines.next_line().await.unwrap().expect("worker response");
            // libtest can prefix the worker's first line with "test <name> ... ".
            if let Some((_, text)) = line.split_once("@@ ") {
                return text.to_owned();
            }
        }
    })
    .await
    .expect("worker response deadline")
}

#[tokio::test]
async fn named_precision_survives_process_loss() {
    let h = Harness::builder([
        &base_users::ADDON,
        &erp_addon_base::ADDON,
        &test_defaults::ADDON,
    ])
    .build()
    .await
    .unwrap();
    let mut setup = h.begin().await.unwrap();
    setup
        .env()
        .records("decimal.precision")
        .unwrap()
        .create(
            &mut setup,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Product Price".into()))
                    .set_cell("digits", Cell::Int(2)),
            ],
        )
        .await
        .unwrap();
    setup.commit().await.unwrap();
    let config = h.db().config();
    assert_eq!(config.get_hosts().len(), 1);
    assert_eq!(config.get_ports().len(), 1);
    assert!(config.get_options().is_none() && config.get_application_name().is_none());
    let host = match &config.get_hosts()[0] {
        tokio_postgres::config::Host::Tcp(host) => host.clone(),
        tokio_postgres::config::Host::Unix(path) => path.to_string_lossy().into_owned(),
    };
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--ignored",
            "--exact",
            "named_precision_child_worker",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(format!("{PREFIX}HOST"), host)
        .env(format!("{PREFIX}PORT"), config.get_ports()[0].to_string())
        .env(format!("{PREFIX}USER"), config.get_user().unwrap())
        .env(format!("{PREFIX}DB"), config.get_dbname().unwrap())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);
    if let Some(password) = config.get_password() {
        command.env(
            format!("{PREFIX}PASSWORD"),
            String::from_utf8(password.to_vec()).unwrap(),
        );
    } else {
        command.env_remove(format!("{PREFIX}PASSWORD"));
    }
    let mut child = command.spawn().unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let ready = response(&mut lines).await;
    assert_eq!(ready, format!("ready {}", child.id().unwrap()));
    assert_ne!(child.id().unwrap(), std::process::id());
    let mut before = h.begin().await.unwrap();
    let items = before
        .env()
        .with_company(1)
        .unwrap()
        .records("test_defaults.item")
        .unwrap();
    let error = items
        .create(&mut before, vec![row("1.234")])
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "Excess decimal scale for test_defaults.item.price (precision 'Product Price'): maximum 2 fractional digits."
    );
    stdin.write_all(b"commit4\n").await.unwrap();
    stdin.flush().await.unwrap();
    assert_eq!(response(&mut lines).await, "committed");
    // An already established REPEATABLE READ transaction retains its old policy snapshot.
    let error = items
        .create(&mut before, vec![row("1.234")])
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "Excess decimal scale for test_defaults.item.price (precision 'Product Price'): maximum 2 fractional digits."
    );
    before.rollback().await.unwrap();
    let mut after = h.begin().await.unwrap();
    let items = after
        .env()
        .with_company(1)
        .unwrap()
        .records("test_defaults.item")
        .unwrap();
    items.create(&mut after, vec![row("1.2345")]).await.unwrap();
    after.rollback().await.unwrap();
    stdin.write_all(b"stage8\n").await.unwrap();
    stdin.flush().await.unwrap();
    assert_eq!(response(&mut lines).await, "staged");
    child.kill().await.unwrap();
    let status = tokio::time::timeout(LIMIT, child.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(!status.success());
    let mut recovered = h.begin().await.unwrap();
    let items = recovered
        .env()
        .with_company(1)
        .unwrap()
        .records("test_defaults.item")
        .unwrap();
    let error = items
        .create(&mut recovered, vec![row("1.23456")])
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "Excess decimal scale for test_defaults.item.price (precision 'Product Price'): maximum 4 fractional digits."
    );
    recovered.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}
