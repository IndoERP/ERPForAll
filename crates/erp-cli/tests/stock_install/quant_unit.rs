// SPDX-License-Identifier: MIT OR Apache-2.0
//! Stock runner, persisted policy and caller boundary through the real installer.
use super::*;
use erp_addon_base::ir_config_setting::{read_values, write_values};
use erp_addon_stock::{
    QuantChange, QuantFilter, QuantKey, QuantUnit, apply_quant_changes, available_quantity,
    run_quant_unit,
};
use erp_sdk::views::wire::SettingsScope;
use erp_sdk::{Error, OperationIdentity, Value};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

async fn install_unit(db: &TestDb) -> Orm {
    let orm = install(db).await;
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    tx.batch("CREATE TABLE stock_unit_marker(value int NOT NULL)")
        .await
        .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    orm
}

// Construct the future on its test thread: the existing quant engine's delta parameters
// include non-Send boxed SQL values. This fixture does not transfer an in-flight transaction.
fn run_local<F>(make: impl FnOnce() -> F + Send + 'static)
where
    F: std::future::Future<Output = ()>,
{
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(make());
        })
        .unwrap()
        .join()
        .unwrap();
}

fn unit() -> QuantUnit {
    QuantUnit::new(SUPERUSER_ID, OperationIdentity::generate().unwrap(), "P").unwrap()
}

/// and the unit's lock_timeout)
#[test]
fn tcp_connection_restores_policy_after_commit_and_rollback() {
    run_local(|| async {
        let db = TestDb::create().await.unwrap();
        let installed = install_unit(&db).await;
        let pool = erp_orm::pool::create_pool(db.config(), 1).unwrap();
        let orm = Orm::builder(Arc::clone(installed.registry()), pool)
            .policy(erp_orm::StaticPolicy::AllowAll)
            .build()
            .unwrap();
        let sql = "SELECT pg_backend_pid(), inet_client_addr() IS NOT NULL, current_setting('idle_in_transaction_session_timeout'), current_setting('tcp_keepalives_idle'), current_setting('tcp_keepalives_interval'), current_setting('tcp_keepalives_count'), current_setting('lock_timeout')";
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let rows = tx.query(sql, &[]).await.unwrap();
        assert!(rows[0].get::<_, bool>(1), "the restore proof must use TCP");
        let pid = rows[0].get::<_, i32>(0);
        let baseline: Vec<String> = (2..7).map(|i| rows[0].get(i)).collect();
        tx.commit().await.unwrap();
        drop(tx);
        for commit in [true, false] {
            let result = run_quant_unit(&orm, unit(), async |tx| {
                let rows = tx.query(sql, &[]).await?;
                assert_eq!(rows[0].get::<_, i32>(0), pid, "same pooled connection");
                assert!(rows[0].get::<_, bool>(1));
                let actual: Vec<String> = (2..7).map(|i| rows[0].get(i)).collect();
                assert_eq!(actual, ["1min", "30", "10", "3", "1s"]);
                if commit {
                    Ok(())
                } else {
                    Err(Error::validation("rollback fixture"))
                }
            })
            .await;
            if commit {
                result.unwrap();
            } else {
                let error = result.unwrap_err();
                assert_eq!(error.kind(), ErrorKind::Validation);
                assert_eq!(error.message(), "rollback fixture");
            }
            let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
            let rows = tx.query(sql, &[]).await.unwrap();
            assert_eq!(
                rows[0].get::<_, i32>(0),
                pid,
                "restoration must not be hidden by a replacement connection"
            );
            let actual: Vec<String> = (2..7).map(|i| rows[0].get(i)).collect();
            assert_eq!(actual, baseline);
            tx.commit().await.unwrap();
            drop(tx);
        }
        drop(orm);
        drop(installed);
        db.cleanup().await.unwrap();
    });
}

#[test]
fn stock_runner_refuses_outer_transaction_before_policy_or_work() {
    run_local(|| async {
        let db = TestDb::create().await.unwrap();
        let orm = install_unit(&db).await;
        let calls = AtomicUsize::new(0);
        let error = erp_orm::unit_of_work(async {
            let mut outer = orm.begin(SUPERUSER_ID).await.unwrap();
            outer.query("SELECT 1", &[]).await.unwrap();
            let error = run_quant_unit(&orm, unit(), async |_tx| {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })
            .await
            .unwrap_err();
            outer.rollback().await.unwrap();
            error
        })
        .await;
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(error.kind(), ErrorKind::Programming);
        assert_eq!(
            error.message(),
            "stock.run_quant_unit cannot run inside another unit of work (including an RPC transaction)."
        );
        drop(orm);
        db.cleanup().await.unwrap();
    });
}

async fn policy(orm: &Orm, values: Vec<(&str, i64)>) {
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let env = tx.env();
    let snapshot = read_values(
        &env,
        &mut tx,
        SettingsScope::Global {},
        vec!["stock.quant_max_attempts".into()],
    )
    .await
    .unwrap();
    write_values(
        &env,
        &mut tx,
        SettingsScope::Global {},
        snapshot.revision,
        values
            .into_iter()
            .map(|(key, value)| (key.to_owned(), Value::Int(value)))
            .collect(),
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
}

async fn seed(orm: &Orm) -> QuantKey {
    let company = super::security::xid(orm, "base", "main_company").await;
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let env = tx.env().with_company(company).unwrap();
    let template = env
        .records("product.template")
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("P".into()))
                    .set_cell("is_storable", Cell::Bool(true)),
            ],
        )
        .await
        .unwrap()
        .rec_ids()[0]
        .get();
    let product = tx
        .query(
            "SELECT id FROM product_product WHERE product_tmpl_id=$1",
            &[&template],
        )
        .await
        .unwrap()[0]
        .get(0);
    let location = env
        .records("stock.location")
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Unit test location".into()))
                    .set_cell("usage", Cell::Str("internal".into())),
            ],
        )
        .await
        .unwrap()
        .rec_ids()[0]
        .get();
    let key = QuantKey::new(company, product, location);
    apply_quant_changes(
        &mut tx,
        &[QuantChange::new(key, Decimal::TEN, Decimal::ZERO)],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    key
}

#[test]
fn lock_timeout_restarts_probe_and_saves_one_complete_change() {
    run_local(|| async {
        let db = TestDb::create().await.unwrap();
        let orm = install_unit(&db).await;
        let key = seed(&orm).await;
        policy(
            &orm,
            vec![
                ("stock.quant_lock_timeout_ms", 20),
                ("stock.quant_retry_base_ms", 1),
                ("stock.quant_retry_cap_ms", 1),
            ],
        )
        .await;
        let mut holder = orm.begin(SUPERUSER_ID).await.unwrap();
        holder
            .query(
                "SELECT id FROM stock_quant WHERE product_id=$1 FOR UPDATE",
                &[&key.product_id],
            )
            .await
            .unwrap();
        let calls = AtomicUsize::new(0);
        let hooks = Arc::new(AtomicUsize::new(0));
        run_quant_unit(&orm, unit(), async |tx| {
            let attempt = calls.fetch_add(1, Ordering::SeqCst);
            assert_eq!(
                available_quantity(
                    tx,
                    &QuantFilter::new(key.company_id, key.product_id, key.location_id),
                    false
                )
                .await?,
                Decimal::TEN
            );
            if attempt == 1 {
                holder.rollback().await?;
            }
            tx.execute("INSERT INTO stock_unit_marker(value) VALUES (1)", &[])
                .await?;
            let hooks = Arc::clone(&hooks);
            tx.after_commit(move || {
                hooks.fetch_add(1, Ordering::SeqCst);
            });
            apply_quant_changes(
                tx,
                &[QuantChange::new(key, Decimal::ZERO, Decimal::from(6))],
            )
            .await?;
            Ok(())
        })
        .await
        .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(hooks.load(Ordering::SeqCst), 1);
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        assert_eq!(
            available_quantity(
                &mut tx,
                &QuantFilter::new(key.company_id, key.product_id, key.location_id),
                false
            )
            .await
            .unwrap(),
            Decimal::from(4)
        );
        assert_eq!(
            tx.query("SELECT count(*) FROM stock_unit_marker", &[])
                .await
                .unwrap()[0]
                .get::<_, i64>(0),
            1
        );
        tx.rollback().await.unwrap();
        drop(tx);
        drop(holder);
        drop(orm);
        db.cleanup().await.unwrap();
    });
}

#[test]
fn exhaustion_is_named_and_policy_changes_apply_on_next_invocation() {
    run_local(|| async {
        let db = TestDb::create().await.unwrap();
        let orm = install_unit(&db).await;
        policy(
            &orm,
            vec![
                ("stock.quant_max_attempts", 2),
                ("stock.quant_retry_base_ms", 1),
                ("stock.quant_retry_cap_ms", 1),
            ],
        )
        .await;
        let calls = AtomicUsize::new(0);
        let error = run_quant_unit(&orm, unit(), async |tx| {
            if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                // A committed policy change must not shorten this invocation's snapshot.
                policy(&orm, vec![("stock.quant_max_attempts", 1)]).await;
            }
            tx.execute("INSERT INTO stock_unit_marker(value) VALUES (1)", &[])
                .await?;
            Err::<(), _>(
                Error::new(ErrorKind::Concurrency, "retry fixture")
                    .with_sqlstate(Some("40001".into())),
            )
        })
        .await
        .unwrap_err();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(error.kind(), ErrorKind::Lock);
        assert_eq!(
            std::error::Error::source(&error)
                .unwrap()
                .downcast_ref::<erp_addon_stock::QuantRetryExhausted>()
                .unwrap()
                .attempts(),
            2
        );
        assert_eq!(
            error.message(),
            "Stock of P is being changed by other operations. The operation was tried 2 times and stopped; nothing was saved. Try again."
        );
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        assert_eq!(
            tx.query("SELECT count(*) FROM stock_unit_marker", &[])
                .await
                .unwrap()[0]
                .get::<_, i64>(0),
            0
        );
        tx.rollback().await.unwrap();
        drop(tx);
        let error = run_quant_unit(&orm, unit(), async |_tx| {
            calls.fetch_add(1, Ordering::SeqCst);
            Err::<(), _>(
                Error::new(ErrorKind::Concurrency, "retry fixture")
                    .with_sqlstate(Some("40001".into())),
            )
        })
        .await
        .unwrap_err();
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        assert_eq!(error.kind(), ErrorKind::Lock);
        assert_eq!(
            error.message(),
            "Stock of P is being changed by other operations. The operation was tried 1 times and stopped; nothing was saved. Try again."
        );
        drop(orm);
        db.cleanup().await.unwrap();
    });
}

#[test]
fn idle_unit_loses_its_session_and_rolls_back() {
    run_local(|| async {
        let db = TestDb::create().await.unwrap();
        let orm = install_unit(&db).await;
        policy(
            &orm,
            vec![("stock.quant_idle_transaction_timeout_ms", 1000)],
        )
        .await;
        let calls = AtomicUsize::new(0);
        let error = run_quant_unit(&orm, unit(), async |tx| {
            calls.fetch_add(1, Ordering::SeqCst);
            let rows = tx.query("SELECT current_setting('idle_in_transaction_session_timeout'), current_setting('tcp_keepalives_idle'), current_setting('tcp_keepalives_interval'), current_setting('tcp_keepalives_count')", &[]).await?;
            assert_eq!(rows[0].get::<_, String>(0), "1s");
            assert_eq!(rows[0].get::<_, String>(1), "30");
            assert_eq!(rows[0].get::<_, String>(2), "10");
            assert_eq!(rows[0].get::<_, String>(3), "3");
            tx.execute("INSERT INTO stock_unit_marker(value) VALUES (1)", &[]).await?;
            // Deliberately leave the transaction idle; this exercises a safety limit, not latency.
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            tx.query("SELECT 1", &[]).await?;
            Ok(())
        }).await.unwrap_err();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(error.kind(), ErrorKind::Database);
        // Driver timing decides whether it reports the server's fatal idle error or the
        // already-closed connection; both must name the failed connection, never be retried.
        assert!(error.message().contains("connection"), "{error}");
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        assert_eq!(
            tx.query("SELECT count(*) FROM stock_unit_marker", &[])
                .await
                .unwrap()[0]
                .get::<_, i64>(0),
            0
        );
        tx.rollback().await.unwrap();
        drop(tx);
        drop(orm);
        db.cleanup().await.unwrap();
    });
}

#[test]
fn policy_access_validation_and_work_principal() {
    run_local(|| async {
        let db = TestDb::create().await.unwrap();
        let installed = install(&db).await;
        let actors = super::security::roles(&installed).await;
        let orm = super::security::secure(&db, &installed).await;
        let stock_user = erp_orm::UserId::new(actors[2]).unwrap();
        let mut tx = orm.begin(stock_user).await.unwrap();
        let env = tx.env();
        let snapshot = read_values(
            &env,
            &mut tx,
            SettingsScope::Global {},
            vec!["stock.quant_lock_timeout_ms".into()],
        )
        .await
        .unwrap();
        let error = write_values(
            &env,
            &mut tx,
            SettingsScope::Global {},
            snapshot.revision,
            vec![("stock.quant_lock_timeout_ms".into(), Value::Int(20))],
        )
        .await
        .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Access);
        assert_eq!(
            error.message(),
            "Setting stock.quant_lock_timeout_ms is unavailable or access is denied."
        );
        tx.rollback().await.unwrap();
        drop(tx);
        let work_calls = AtomicUsize::new(0);
        let error = run_quant_unit(
            &orm,
            QuantUnit::new(stock_user, OperationIdentity::generate().unwrap(), "P").unwrap(),
            async |tx| {
                work_calls.fetch_add(1, Ordering::SeqCst);
                assert!(!tx.env().su());
                Err::<(), _>(Error::validation("work refusal"))
            },
        )
        .await
        .unwrap_err();
        assert_eq!(work_calls.load(Ordering::SeqCst), 1);
        assert_eq!(error.kind(), ErrorKind::Validation);
        assert_eq!(error.message(), "work refusal");
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let env = tx.env();
        let before = read_values(
            &env,
            &mut tx,
            SettingsScope::Global {},
            vec![
                "stock.quant_retry_base_ms".into(),
                "stock.quant_retry_cap_ms".into(),
            ],
        )
        .await
        .unwrap();
        let error = write_values(
            &env,
            &mut tx,
            SettingsScope::Global {},
            before.revision.clone(),
            vec![
                ("stock.quant_retry_base_ms".into(), Value::Int(100)),
                ("stock.quant_retry_cap_ms".into(), Value::Int(99)),
            ],
        )
        .await
        .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Validation);
        assert_eq!(
            error.message(),
            "stock.quant_retry_base_ms cannot exceed stock.quant_retry_cap_ms."
        );
        let after = read_values(
            &env,
            &mut tx,
            SettingsScope::Global {},
            vec![
                "stock.quant_retry_base_ms".into(),
                "stock.quant_retry_cap_ms".into(),
            ],
        )
        .await
        .unwrap();
        assert_eq!(
            serde_json::to_value(&before).unwrap(),
            serde_json::to_value(&after).unwrap()
        );
        tx.rollback().await.unwrap();
        drop(tx);
        policy(&orm, vec![("stock.quant_max_attempts", 2)]).await;
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        // Simulate corrupt storage without using the validated Settings writer.
        assert_eq!(tx.execute("UPDATE ir_config_setting_value SET value_text='0' WHERE setting_key='stock.quant_max_attempts'", &[]).await.unwrap(), 1);
        tx.commit().await.unwrap();
        drop(tx);
        let error = run_quant_unit(&orm, unit(), async |_tx| {
            work_calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
        .await
        .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Validation);
        assert_eq!(
            error.message(),
            "Stored setting stock.quant_max_attempts is invalid."
        );
        assert_eq!(
            work_calls.load(Ordering::SeqCst),
            1,
            "corrupt policy never enters work"
        );
        drop(orm);
        drop(installed);
        db.cleanup().await.unwrap();
    });
}

// A real process owns the abandoned connection. No credential is written to the rendezvous file.
#[test]
fn process_lock_holder() {
    let Ok(database) = std::env::var("STOCK_UNIT_CHILD_DATABASE") else {
        return;
    };
    run_local(move || async move {
        let mut config: erp_testkit::tokio_postgres::Config =
            std::env::var("DATABASE_URL").unwrap().parse().unwrap();
        config.dbname(&database);
        let (client, connection) = config
            .connect(erp_testkit::tokio_postgres::NoTls)
            .await
            .unwrap();
        let connection = tokio::spawn(connection);
        // Bound the helper even if its parent disappears before sending the termination.
        client.batch_execute("BEGIN; SET LOCAL idle_in_transaction_session_timeout='30s'; SELECT id FROM stock_quant FOR UPDATE; INSERT INTO stock_unit_marker(value) VALUES(1)").await.unwrap();
        let ready = std::path::PathBuf::from(std::env::var("STOCK_UNIT_CHILD_READY").unwrap());
        let temporary = ready.with_extension("tmp");
        std::fs::write(&temporary, "locked").unwrap();
        std::fs::rename(temporary, ready).unwrap();
        // The parent terminates this process after observing the database-backed lock.
        connection.await.unwrap().unwrap();
    });
}

#[test]
fn client_process_death_releases_quant_lock() {
    run_local(|| async {
        let db = TestDb::create().await.unwrap();
        let orm = install_unit(&db).await;
        let key = seed(&orm).await;
        let directory = tempfile::tempdir().unwrap();
        let ready = directory.path().join("ready");
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "quant_unit::process_lock_holder", "--nocapture"])
            .env("STOCK_UNIT_CHILD_DATABASE", db.name())
            .env("STOCK_UNIT_CHILD_READY", &ready)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
        while !ready.exists() {
            if let Some(status) = child.try_wait().unwrap() {
                panic!("lock holder exited before readiness: {status}");
            }
            if tokio::time::Instant::now() >= deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("lock holder did not become ready");
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        assert_eq!(std::fs::read_to_string(&ready).unwrap(), "locked");
        child.kill().unwrap();
        let status = child.wait().unwrap();
        assert!(!status.success(), "parent terminated the holder");
        run_quant_unit(&orm, unit(), async |tx| {
            apply_quant_changes(tx, &[QuantChange::new(key, Decimal::ZERO, Decimal::ONE)]).await?;
            Ok(())
        })
        .await
        .unwrap();
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        assert_eq!(
            tx.query("SELECT count(*) FROM stock_unit_marker", &[])
                .await
                .unwrap()[0]
                .get::<_, i64>(0),
            0
        );
        assert_eq!(
            available_quantity(
                &mut tx,
                &QuantFilter::new(key.company_id, key.product_id, key.location_id),
                false
            )
            .await
            .unwrap(),
            Decimal::from(9)
        );
        tx.rollback().await.unwrap();
        drop(tx);
        drop(orm);
        db.cleanup().await.unwrap();
    });
}
