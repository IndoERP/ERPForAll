// SPDX-License-Identifier: MIT OR Apache-2.0
//! The served maintenance runner uses the configured owner role, never the request role.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    dead_code,
    reason = "integration fixture assertions"
)]
#[path = "support/native_http.rs"]
mod native_http;
use native_http::*;
use std::{
    fs,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};
struct Running(Child);
impl Drop for Running {
    fn drop(&mut self) {
        if self.0.try_wait().unwrap().is_none() {
            self.0.kill().unwrap();
            self.0.wait().unwrap();
        }
    }
}

/// The test's LOGIN owner role, dropped after its database whether the test passes or fails, so a
/// failed run leaves no login role on the shared test cluster.
struct OwnerRole {
    admin: erp_testkit::tokio_postgres::Config,
    database: String,
    role: String,
}
impl Drop for OwnerRole {
    fn drop(&mut self) {
        let (admin, database, role) =
            (self.admin.clone(), self.database.clone(), self.role.clone());
        // Drop cannot await, and the test's runtime may be the caller: use a short-lived one.
        let dropped = std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async move {
                    let (client, connection) =
                        admin.connect(erp_testkit::tokio_postgres::NoTls).await?;
                    tokio::spawn(connection);
                    // two statements: PostgreSQL refuses DROP DATABASE inside the implicit
                    // transaction block of a multi-statement query
                    client
                        .batch_execute(&format!(
                            "DROP DATABASE IF EXISTS {} WITH (FORCE)",
                            erp_sql::quote(&database)
                        ))
                        .await?;
                    client
                        .batch_execute(&format!("DROP ROLE IF EXISTS {}", erp_sql::quote(&role)))
                        .await
                })
        })
        .join();
        if !matches!(dropped, Ok(Ok(()))) {
            eprintln!("could not drop the maintenance test role {}", self.role);
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn isolated_served_owner_runs_attachment_maintenance_without_general_jobs() {
    let mut f = NativeFixture::start().await;
    f.server.terminate();
    // Seed through our uploader/linker, but execute all successful cleanup through the served runner.
    let config_path = f.dir.path().join("server.toml");
    let settings = erp_server::config::ServerConfig::load(&config_path).unwrap();
    let runtime = std::sync::Arc::new(
        erp_sdk::blob::AttachmentRuntime::open(
            erp_sdk::blob::AttachmentConfig::from_toml(settings.attachments_toml()).unwrap(),
        )
        .await
        .unwrap(),
    );
    let registry = std::sync::Arc::new(
        erp_registry::RegistryBuilder::new([
            &erp_test_models::base_users::ADDON,
            &erp_addon_base::ADDON,
        ])
        .build()
        .unwrap(),
    );
    let orm = erp_orm::Orm::builder(
        std::sync::Arc::clone(&registry),
        erp_orm::pool::create_pool(f.db.config(), 4).unwrap(),
    )
    .generation_attachment(erp_orm::GenerationAttachment::new(registry, runtime))
    .build()
    .unwrap();
    let mut tx = orm.begin(erp_orm::SUPERUSER_ID).await.unwrap();
    let env = tx.env();
    tx.rollback().await.unwrap();
    drop(tx);
    let payload = vec![b'x'; 100_000];
    let up = erp_addon_base::attachments::Uploader::new(&orm)
        .stage(
            &env,
            &mut std::io::Cursor::new(&payload),
            erp_addon_base::attachments::StageRequest::named("retained.txt")
                .with_declared_length(payload.len() as u64),
        )
        .await
        .unwrap();
    let upload = up.id.as_str().to_owned();
    let mut tx = orm.begin(erp_orm::SUPERUSER_ID).await.unwrap();
    erp_addon_base::attachments::create(
        &mut tx,
        vec![erp_addon_base::attachments::NewAttachment::new(
            "retained.txt",
            erp_addon_base::attachments::AttachmentContent::Upload(up.id),
        )],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    drop(orm);
    drop(env);
    assert_eq!(
        f.client
            .query_one(
                "SELECT reserved_bytes FROM ir_blob_upload WHERE id=$1::text::uuid",
                &[&upload]
            )
            .await
            .unwrap()
            .get::<_, i64>(0),
        payload.len() as i64
    );

    let owner = format!("{}_maintenance", f.db.name());
    let q = erp_sql::quote(&owner);
    let mut admin = f.db.config();
    admin.dbname("postgres");
    let _owner_role = OwnerRole {
        admin,
        database: f.db.name().to_owned(),
        role: owner.clone(),
    };
    f.client.batch_execute(&format!("CREATE ROLE {q} LOGIN PASSWORD 'fixture-runtime-password' NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION; ALTER DATABASE {} OWNER TO {q}; GRANT EXECUTE ON FUNCTION pg_control_system() TO {q};",erp_sql::quote(f.db.name()))).await.unwrap();
    // Transfer only this test database's public/event objects; never REASSIGN OWNED (it includes shared objects).
    let objects=f.client.query("SELECT c.relname,c.relkind::text,n.nspname FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname IN ('public','erp_event') AND c.relkind IN ('r','p','v','m','S') ORDER BY CASE WHEN c.relkind='S' THEN 1 ELSE 0 END",&[]).await.unwrap();
    for row in objects {
        let name: String = row.get(0);
        let kind: String = row.get(1);
        let schema: String = row.get(2);
        let object = match kind.as_str() {
            "S" => "SEQUENCE",
            "v" => "VIEW",
            "m" => "MATERIALIZED VIEW",
            _ => "TABLE",
        };
        f.client
            .batch_execute(&format!(
                "ALTER {object} {}.{} OWNER TO {q}",
                erp_sql::quote(&schema),
                erp_sql::quote(&name)
            ))
            .await
            .unwrap();
    }
    f.client
        .batch_execute(&format!("ALTER SCHEMA erp_event OWNER TO {q}"))
        .await
        .unwrap();
    for row in f.client.query("SELECT p.oid::regprocedure::text FROM pg_proc p JOIN pg_namespace n ON n.oid=p.pronamespace WHERE n.nspname='public' AND p.prokind='f' AND p.proowner=(SELECT oid FROM pg_roles WHERE rolname=current_user)",&[]).await.unwrap(){
        let function:String=row.get(0);
        f.client.batch_execute(&format!("ALTER FUNCTION {function} OWNER TO {q}")).await.unwrap();
    }
    // Keep the narrowly published NOLOGIN SECURITY DEFINER owners intact. The new database
    // owner may invoke their existing native services, but cannot turn them into login-owned helpers.
    f.client
        .batch_execute(&format!(
            "GRANT EXECUTE ON ALL FUNCTIONS IN SCHEMA public TO {q}"
        ))
        .await
        .unwrap();
    private(
        &f.dir.path().join("owner-url"),
        &url(&f.db.config(), Some(&owner)),
    );
    let config = f.dir.path().join("server.toml");
    let mut text = fs::read_to_string(&config).unwrap();
    text.push_str("attachment_maintenance_run_in_app=true\n");
    private(&config, &text);
    f.client.batch_execute("INSERT INTO ir_blob_usage_delta(bytes) SELECT 0 FROM generate_series(1,7); UPDATE erp_job SET run_after=clock_timestamp() WHERE kind='base.attachment_usage_compact'").await.unwrap();
    let mut child = Running(
        Command::new(env!("CARGO_BIN_EXE_erp"))
            .args(["server", "--config"])
            .arg(&config)
            .stdout(Stdio::null())
            .stderr(fs::File::create(&f.stderr_path).unwrap())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            panic!(
                "maintenance server exited {status}: {}",
                fs::read_to_string(&f.stderr_path).unwrap()
            )
        }
        let n:i64=f.client.query_one("SELECT count(*) FROM erp_job WHERE kind='base.attachment_usage_compact' AND state='ready' AND started_at IS NOT NULL AND last_error_code IS NULL AND run_after>clock_timestamp()",&[]).await.unwrap().get(0);
        if n > 0 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "maintenance did not run: {}",
            fs::read_to_string(&f.stderr_path).unwrap()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    // The scheduled heartbeat establishes ownership. Only the test clock is matured; no
    // coordinator/cleaner is called by this fixture after the server has started.
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let healthy:i64=f.client.query_one("SELECT count(*) FROM ir_blob_owner_state WHERE mode='healthy' AND observation_id IS NOT NULL",&[]).await.unwrap().get(0);
        if healthy > 0 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "scheduled heartbeat did not establish ownership: {}",
            fs::read_to_string(&f.stderr_path).unwrap()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    f.client.batch_execute("UPDATE ir_blob_owner_state SET observation_started_at=clock_timestamp()-interval '700 seconds', observation_not_before=clock_timestamp()-interval '1 second'; UPDATE erp_job SET run_after=clock_timestamp() WHERE kind='base.attachment_clean_staging' AND state='ready'").await.unwrap();
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let n: i64 = f
            .client
            .query_one(
                "SELECT count(*) FROM ir_blob_upload WHERE id=$1::text::uuid",
                &[&upload],
            )
            .await
            .unwrap()
            .get(0);
        if n == 0 {
            break;
        }
        let dead=f.client.query("SELECT kind,last_error_code,last_error FROM erp_job WHERE kind='base.attachment_clean_staging' AND state='dead'",&[]).await.unwrap();
        assert!(
            dead.is_empty(),
            "cleanup dead letter: {:?}",
            dead.iter()
                .map(|r| (
                    r.get::<_, String>(0),
                    r.get::<_, Option<String>>(1),
                    r.get::<_, Option<String>>(2)
                ))
                .collect::<Vec<_>>()
        );
        assert!(
            Instant::now() < deadline,
            "scheduled cleanup did not release retained staging: {}",
            fs::read_to_string(&f.stderr_path).unwrap()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(
        f.client
            .query_one(
                "SELECT count(*) FROM ir_attachment WHERE name='retained.txt'",
                &[]
            )
            .await
            .unwrap()
            .get::<_, i64>(0),
        1
    );
    // Cleanup creates a release delta; make the compactor due once more and wait for its fold.
    f.client.batch_execute("UPDATE erp_job SET run_after=clock_timestamp() WHERE kind='base.attachment_usage_compact' AND state='ready'").await.unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    while f
        .client
        .query_one("SELECT count(*) FROM ir_blob_usage_delta", &[])
        .await
        .unwrap()
        .get::<_, i64>(0)
        > 0
    {
        assert!(
            Instant::now() < deadline,
            "scheduled compaction did not fold the cleanup release"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(
        f.client
            .query_one("SELECT count(*) FROM ir_blob_usage_delta", &[])
            .await
            .unwrap()
            .get::<_, i64>(0),
        0
    );
    // Bump the actual registry signal and exercise the request-generation refresh. The next
    // due compaction must use a fresh admitted owner ORM, since old ones fence themselves.
    f.client.batch_execute("INSERT INTO erp_signal_generation(key) VALUES ('registry'); INSERT INTO ir_blob_usage_delta(bytes) VALUES (0)").await.unwrap();
    let reply = request(f.port, "GET", "/erp", &[("cookie", &f.cookies)], "");
    assert_eq!(reply.status, 200, "generation refresh: {}", reply.body);
    let deadline = Instant::now() + Duration::from_secs(120);
    while f
        .client
        .query_one("SELECT count(*) FROM ir_blob_usage_delta", &[])
        .await
        .unwrap()
        .get::<_, i64>(0)
        > 0
    {
        f.client.batch_execute("UPDATE erp_job SET run_after=clock_timestamp() WHERE kind='base.attachment_usage_compact' AND state='ready'").await.unwrap();
        assert!(
            Instant::now() < deadline,
            "replacement generation did not resume owner compaction: {}",
            fs::read_to_string(&f.stderr_path).unwrap()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    // A policy-only change (any write to ir.access, ir.capability or res.groups) is applied in
    // place by the request ORM, so no replacement generation is spawned: the owner runner must
    // keep running on its own ORM.
    f.client.batch_execute("INSERT INTO erp_signal_generation(key) VALUES ('policy'); INSERT INTO ir_blob_usage_delta(bytes) VALUES (0)").await.unwrap();
    let reply = request(f.port, "GET", "/erp", &[("cookie", &f.cookies)], "");
    assert_eq!(reply.status, 200, "policy refresh: {}", reply.body);
    let deadline = Instant::now() + Duration::from_secs(120);
    while f
        .client
        .query_one("SELECT count(*) FROM ir_blob_usage_delta", &[])
        .await
        .unwrap()
        .get::<_, i64>(0)
        > 0
    {
        f.client.batch_execute("UPDATE erp_job SET run_after=clock_timestamp() WHERE kind='base.attachment_usage_compact' AND state='ready'").await.unwrap();
        assert!(
            Instant::now() < deadline,
            "a policy-only generation change stopped owner compaction: {}",
            fs::read_to_string(&f.stderr_path).unwrap()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        !f.client
            .query_one("SELECT rolsuper FROM pg_roles WHERE rolname=$1", &[&owner])
            .await
            .unwrap()
            .get::<_, bool>(0)
    );
    let bad:bool=f.client.query_one("SELECT has_table_privilege($1,'ir_blob_upload','DELETE') OR has_table_privilege($1,'ir_blob','TRUNCATE') OR pg_has_role($1,$2,'MEMBER')",&[&f.role,&owner]).await.unwrap().get(0);
    assert!(!bad, "request role inherited destructive owner privileges");
    assert_eq!(
        f.client
            .query_one("SELECT count(*) FROM erp_job WHERE kind LIKE 'core.%'", &[])
            .await
            .unwrap()
            .get::<_, i64>(0),
        0,
        "maintenance silently enabled the ordinary runner"
    );
    assert!(
        Command::new("/bin/kill")
            .arg("-TERM")
            .arg(child.0.id().to_string())
            .status()
            .unwrap()
            .success()
    );
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            assert!(
                status.success(),
                "owner runner shutdown {status}: {}",
                fs::read_to_string(&f.stderr_path).unwrap()
            );
            break;
        }
        assert!(
            Instant::now() < deadline,
            "owner runners did not drain after TERM"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(
        f.client
            .query_one(
                "SELECT count(*) FROM erp_node WHERE state<>'stopped' AND expires_at>now()",
                &[]
            )
            .await
            .unwrap()
            .get::<_, i64>(0),
        0
    );
    drop(child);
    // A maintenance owner without EXECUTE on pg_control_system() (cleanup admission reads it) is
    // refused when the server starts, not at the first cleanup.
    f.client
        .batch_execute(&format!(
            "REVOKE EXECUTE ON FUNCTION pg_control_system() FROM PUBLIC; REVOKE EXECUTE ON FUNCTION pg_control_system() FROM {q}"
        ))
        .await
        .unwrap();
    assert!(
        !f.client
            .query_one(
                "SELECT has_function_privilege($1,'pg_control_system()','EXECUTE')",
                &[&owner]
            )
            .await
            .unwrap()
            .get::<_, bool>(0)
    );
    let refused_path = f.dir.path().join("refused.stderr");
    let mut refused = Running(
        Command::new(env!("CARGO_BIN_EXE_erp"))
            .args(["server", "--config"])
            .arg(&config)
            .stdout(Stdio::null())
            .stderr(fs::File::create(&refused_path).unwrap())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(120);
    let status = loop {
        if let Some(status) = refused.0.try_wait().unwrap() {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "a maintenance owner without pg_control_system() was not refused at start"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    let stderr = fs::read_to_string(&refused_path).unwrap();
    assert!(!status.success(), "{stderr}");
    assert!(
        stderr.contains("could not start attachment maintenance")
            && stderr.contains("gc=Attachment maintenance owner lacks required blob, ownership, accounting or control-data privileges."),
        "{stderr}"
    );
    drop(refused);
    f.cleanup().await;
    // `_owner_role` drops the role now that its database has gone.
}
