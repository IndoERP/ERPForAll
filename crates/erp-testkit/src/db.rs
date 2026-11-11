//! Throwaway databases with a connection pool.

use std::fmt;
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use deadpool_postgres::{Object, Pool, Runtime, Timeouts};
use tokio_postgres::error::SqlState;
use tokio_postgres::{Client, Config, NoTls};

use crate::cluster::{
    Chosen, Cluster, Clusters, PgProber, UsageThreshold, cluster_log_from_env, parse_url_list,
    probed_unreachable_from_env, record_choice, select, stderr_log,
};
use crate::error::{Result, TestkitError, error_chain, pg_error};
use crate::tag::{RUN_ID_ENV, Tag, TestDbName, unix_now, validate_run_id, validate_template};

/// How many names to try when one already exists (only possible after pid reuse within a second).
const NAME_ATTEMPTS: u32 = 32;

/// Default pool size; tests rarely need more.
const DEFAULT_POOL_SIZE: usize = 4;

/// TCP connect timeout for every connection the testkit opens.
pub(crate) const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Pool timeouts: a test waiting longer than this for a connection has a bug (a leaked client).
const POOL_TIMEOUTS: Timeouts = Timeouts {
    wait: Some(Duration::from_secs(30)),
    create: Some(Duration::from_secs(10)),
    recycle: Some(Duration::from_secs(5)),
};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// Read `DATABASE_URL` from the environment (the single-cluster setting; see [`crate::cluster`]
/// for `ERP_TEST_DATABASE_URLS`).
pub fn database_url_from_env() -> Result<String> {
    match std::env::var("DATABASE_URL") {
        Ok(url) if !url.trim().is_empty() => Ok(url),
        _ => Err(TestkitError::MissingDatabaseUrl),
    }
}

/// Parse a connection string, check it names an admin database, and apply the connect timeout.
pub(crate) fn admin_config(database_url: &str) -> Result<Config> {
    let mut config = Config::from_str(database_url).map_err(TestkitError::InvalidDatabaseUrl)?;
    if config.get_dbname().is_none_or(str::is_empty) {
        return Err(TestkitError::MissingAdminDatabase);
    }
    // A comma-joined URL is accepted by libpq parsing as part of the database name.
    // Reject it before a label or server error can echo the embedded credentials.
    if config.get_dbname().is_some_and(|name| name.contains("://")) {
        return Err(TestkitError::NestedDatabaseUrl);
    }
    config.connect_timeout(CONNECT_TIMEOUT);
    Ok(config)
}

/// Open a single connection and drive it on the current tokio runtime.
pub(crate) async fn connect(config: &Config) -> Result<Client> {
    let (client, connection) = config.connect(NoTls).await.map_err(TestkitError::Connect)?;
    tokio::spawn(async move {
        if let Err(error) = connection.await {
            eprintln!(
                "erp-testkit: connection closed with error: {}",
                error_chain(&error)
            );
        }
    });
    Ok(client)
}

/// Quote an identifier for SQL. Callers only pass names produced or validated by this crate, but
/// quoting keeps the statement correct even if that ever changes.
pub(crate) fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// Quote a string literal for utility statements that cannot take bind parameters.
pub(crate) fn quote_literal(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

/// `DROP DATABASE IF EXISTS … WITH (FORCE)` through a fresh admin connection.
pub(crate) async fn drop_database(admin: &Config, name: &str) -> Result<()> {
    let client = connect(admin).await?;
    drop_database_with(&client, name).await
}

/// Drop a database on the server in `database_url`.
pub async fn drop_database_url(database_url: &str, name: &str) -> Result<()> {
    let admin = admin_config(database_url)?;
    drop_database(&admin, name).await
}

pub(crate) async fn drop_database_with(client: &Client, name: &str) -> Result<()> {
    let sql = format!("DROP DATABASE IF EXISTS {} WITH (FORCE)", quote_ident(name));
    client
        .batch_execute(&sql)
        .await
        .map_err(pg_error(format!("dropping database {name}")))
}

/// Whether a database with this exact name exists on the server in `database_url`. To ask the
/// server a [`TestDb`] was created on, use [`TestDb::cluster`] and [`Cluster::database_exists`].
pub async fn database_exists(database_url: &str, name: &str) -> Result<bool> {
    Cluster::parse(1, 1, database_url)?
        .database_exists(name)
        .await
}

/// A server the caller chose for a database, bypassing the cluster selection.
#[derive(Clone)]
enum Pin {
    Url(String),
    Urls(String),
    Cluster(Cluster),
}

/// Create a fresh ERPForAll database with the `C` collation, install base extensions
/// (`pg_trgm`), and return a connection pool guarded by the PG18 version gate.
///
/// This provides the foundation for `erp db create` (see CLI wiring follow-up note).
// TODO: Wire 'erp db create' in erp-cli to erp_testkit::db::create_database
pub async fn create_database(database_url: &str, name: &str) -> Result<Pool> {
    create_database_with_size(database_url, name, DEFAULT_POOL_SIZE).await
}

/// Like [`create_database`], but with a custom maximum pool size.
pub async fn create_database_with_size(
    database_url: &str,
    name: &str,
    pool_size: usize,
) -> Result<Pool> {
    create_database_with_base_install(database_url, name, pool_size, |client| {
        Box::pin(async move {
            client
                .batch_execute("CREATE EXTENSION IF NOT EXISTS pg_trgm")
                .await
                .map_err(|e| format!("installing base extension pg_trgm: {e}"))
        })
    })
    .await
}

/// Create a new database, configure connection pool with version gate, and execute a base install hook.
///
/// If pool creation, connection establishment, or the base install hook fails, the newly created
/// database is automatically dropped to prevent leaks.
pub async fn create_database_with_base_install<F>(
    database_url: &str,
    name: &str,
    pool_size: usize,
    base_install: F,
) -> Result<Pool>
where
    F: for<'a> FnOnce(
        &'a tokio_postgres::Client,
    ) -> erp_orm::BoxFuture<'a, std::result::Result<(), String>>,
{
    if pool_size == 0 {
        return Err(TestkitError::Configuration(
            "pool_size must be greater than zero; got 0".into(),
        ));
    }
    let admin = admin_config(database_url)?;
    let client = connect(&admin).await?;
    let sql = format!(
        "CREATE DATABASE {} TEMPLATE template0 ENCODING 'UTF8' LC_COLLATE 'C'",
        quote_ident(name)
    );
    client
        .batch_execute(&sql)
        .await
        .map_err(pg_error(format!("creating database {name}")))?;

    let mut db_config = admin.clone();
    db_config.dbname(name);

    let build_and_init = async {
        let pool = erp_orm::pool::create_pool(db_config, pool_size)
            .map_err(|e| TestkitError::Configuration(e.to_string()))?;
        let client_conn = pool.get().await?;
        base_install(&client_conn)
            .await
            .map_err(TestkitError::Configuration)?;
        Ok::<Pool, TestkitError>(pool)
    };

    match build_and_init.await {
        Ok(pool) => Ok(pool),
        Err(err) => {
            if let Err(drop_error) = drop_database_with(&client, name).await {
                eprintln!(
                    "erp-testkit: could not drop failed database {name}: {}",
                    error_chain(&drop_error)
                );
            }
            Err(err)
        }
    }
}

/// Options for [`TestDb`]. Start with [`TestDb::builder`].
#[derive(Clone)]
#[must_use = "call `.create().await` to make the database"]
pub struct TestDbBuilder {
    pin: Option<Pin>,
    max_usage: Option<UsageThreshold>,
    pool_size: usize,
    template: Option<String>,
    run_id: Option<String>,
}

impl fmt::Debug for TestDbBuilder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The URL can carry a password: show only whether one was given, or the cluster label.
        let pin = match &self.pin {
            None => None,
            Some(Pin::Url(_)) => Some("<redacted url>".to_owned()),
            Some(Pin::Urls(_)) => Some("<redacted url list>".to_owned()),
            Some(Pin::Cluster(cluster)) => Some(cluster.to_string()),
        };
        f.debug_struct("TestDbBuilder")
            .field("database_url", &pin)
            .field("max_usage", &self.max_usage)
            .field("pool_size", &self.pool_size)
            .field("template", &self.template)
            .field("run_id", &self.run_id)
            .finish()
    }
}

impl Default for TestDbBuilder {
    fn default() -> Self {
        Self {
            pin: None,
            max_usage: None,
            pool_size: DEFAULT_POOL_SIZE,
            template: None,
            run_id: None,
        }
    }
}

impl TestDbBuilder {
    /// Server to use instead of the environment (`ERP_TEST_DATABASE_URLS` / `DATABASE_URL`), with
    /// no headroom check. The database named in the URL is used for administrative statements
    /// only. Replaces an earlier [`TestDbBuilder::cluster`].
    pub fn database_url(mut self, url: impl Into<String>) -> Self {
        self.pin = Some(Pin::Url(url.into()));
        self
    }

    /// Choose among these clusters instead of the environment: a whitespace-separated list of
    /// admin URLs in the format of `ERP_TEST_DATABASE_URLS` (see [`crate::cluster`]).
    pub fn database_urls(mut self, urls: impl Into<String>) -> Self {
        self.pin = Some(Pin::Urls(urls.into()));
        self
    }

    /// Usage threshold for choosing among several clusters, instead of
    /// `ERP_TEST_DB_MAX_USAGE_PCT`. Ignored when the server is pinned.
    pub fn max_usage(mut self, threshold: UsageThreshold) -> Self {
        self.max_usage = Some(threshold);
        self
    }

    /// Create the database on this cluster (e.g. `other.cluster().clone()`, to clone a test
    /// database as a template on the server that holds it), with no headroom check. Replaces an
    /// earlier [`TestDbBuilder::database_url`].
    pub fn cluster(mut self, cluster: Cluster) -> Self {
        self.pin = Some(Pin::Cluster(cluster));
        self
    }

    /// Maximum connections in the returned pool (at least 1).
    pub fn pool_size(mut self, size: usize) -> Self {
        self.pool_size = size.max(1);
        self
    }

    pub fn template(mut self, template: impl Into<String>) -> Self {
        self.template = Some(template.into());
        self
    }

    /// Run id recorded in the database tag. Defaults to `ERP_TESTKIT_RUN_ID` when set.
    pub fn run_id(mut self, run_id: impl Into<String>) -> Self {
        self.run_id = Some(run_id.into());
        self
    }

    /// Create the database and its pool.
    ///
    /// Without a pinned server, the cluster comes from the environment: the first entry of
    /// `ERP_TEST_DATABASE_URLS` with connection headroom, else `DATABASE_URL` (see
    /// [`crate::cluster`]). The choice is printed to stderr and, with `ERP_TESTKIT_CLUSTER_LOG`,
    /// appended to that file.
    ///
    /// # Errors
    /// Invalid settings or names, [`TestkitError::NoClusterAvailable`] when every listed cluster
    /// is full or unreachable, and connection or SQL failures.
    pub async fn create(self) -> Result<TestDb> {
        let plan = match self.pin {
            Some(Pin::Url(url)) => Clusters::Pinned(Cluster::parse(1, 1, &url)?),
            Some(Pin::Urls(urls)) => Clusters::Ordered(parse_url_list(&urls)?),
            Some(Pin::Cluster(cluster)) => Clusters::Pinned(cluster),
            None => Clusters::from_env()?,
        };
        let run_id = match self.run_id {
            Some(id) => Some(id),
            None => std::env::var(RUN_ID_ENV).ok().filter(|id| !id.is_empty()),
        };
        if let Some(id) = &run_id {
            validate_run_id(id)?;
        }
        if let Some(template) = &self.template {
            validate_template(template)?;
        }
        // Read before any connection is opened, so a bad setting touches no server.
        let log_path = cluster_log_from_env()?;

        let (cluster, client, usage) = match plan {
            Clusters::Ordered(list) => {
                // Read before any connection is opened, so a bad setting touches no server.
                let threshold = match self.max_usage {
                    Some(threshold) => threshold,
                    None => UsageThreshold::from_env()?,
                };
                // The pool opens up to `pool_size` connections after the probe; count them now. A
                // pool larger than u32::MAX connections cannot fit any cluster, so saturating is
                // exact for the comparison (it can only make the cluster look full).
                let reserve = u32::try_from(self.pool_size).unwrap_or(u32::MAX);
                let reported = probed_unreachable_from_env()?;
                let Chosen {
                    cluster,
                    client,
                    usage,
                } = select(
                    &list,
                    threshold,
                    reserve,
                    &reported,
                    &mut PgProber,
                    &mut stderr_log,
                )
                .await?;
                (cluster, client, Some(usage))
            }
            Clusters::Pinned(cluster) => {
                let client = connect(cluster.config()).await?;
                (cluster, client, None)
            }
        };
        let now = unix_now()?;
        let name = create_tagged(&client, self.template.as_deref(), run_id.clone()).await?;
        if let Err(error) = record_choice(
            log_path.as_deref(),
            &cluster,
            &name,
            usage,
            run_id.as_deref(),
            now,
        ) {
            if let Err(drop_error) = drop_database_with(&client, &name).await {
                eprintln!(
                    "erp-testkit: could not drop {name} after the cluster log failed: {}",
                    error_chain(&drop_error)
                );
            }
            return Err(error);
        }

        let mut db_config = cluster.config().clone();
        db_config.dbname(&name);
        let pool = match erp_orm::pool::pool_builder(db_config)
            .max_size(self.pool_size)
            .timeouts(POOL_TIMEOUTS)
            .runtime(Runtime::Tokio1)
            .build()
        {
            Ok(pool) => pool,
            Err(error) => {
                if let Err(drop_error) = drop_database_with(&client, &name).await {
                    eprintln!(
                        "erp-testkit: could not drop {name} after pool failure: {}",
                        error_chain(&drop_error)
                    );
                }
                return Err(error.into());
            }
        };
        // Verify the initial connection and version gate, and install base extension
        let client_conn = match pool.get().await {
            Ok(c) => c,
            Err(error) => {
                if let Err(drop_error) = drop_database_with(&client, &name).await {
                    eprintln!(
                        "erp-testkit: could not drop {name} after connection failure: {}",
                        error_chain(&drop_error)
                    );
                }
                return Err(error.into());
            }
        };
        if let Err(ext_err) = client_conn
            .batch_execute("CREATE EXTENSION IF NOT EXISTS pg_trgm")
            .await
        {
            if let Err(drop_error) = drop_database_with(&client, &name).await {
                eprintln!(
                    "erp-testkit: could not drop {name} after extension failure: {}",
                    error_chain(&drop_error)
                );
            }
            return Err(pg_error("installing base extension pg_trgm")(ext_err));
        }
        drop(client_conn);
        Ok(TestDb {
            name,
            pool,
            cluster,
            cleaned: false,
        })
    }
}

/// Create a uniquely named database and tag it with the creator's pid and run id.
async fn create_tagged(
    client: &Client,
    template: Option<&str>,
    run_id: Option<String>,
) -> Result<String> {
    let pid = std::process::id();
    // A given template brings its own locale. Note: LC_CTYPE is not set to 'C'
    // so case-folding / ILIKE on UTF8 text works correctly.
    let template_clause = template.map_or_else(
        || " TEMPLATE template0 ENCODING 'UTF8' LC_COLLATE 'C'".to_owned(),
        |t| format!(" TEMPLATE {}", quote_ident(t)),
    );
    let mut created = None;
    for _ in 0..NAME_ATTEMPTS {
        let name = TestDbName {
            pid,
            n: COUNTER.fetch_add(1, Ordering::Relaxed),
            created_at: unix_now()?,
        }
        .render();
        let sql = format!("CREATE DATABASE {}{template_clause}", quote_ident(&name));
        match client.batch_execute(&sql).await {
            Ok(()) => {
                created = Some(name);
                break;
            }
            Err(error) if error.code() == Some(&SqlState::DUPLICATE_DATABASE) => {}
            Err(error) => return Err(pg_error(format!("creating database {name}"))(error)),
        }
    }
    let name = created.ok_or(TestkitError::NamesExhausted {
        attempts: NAME_ATTEMPTS,
    })?;

    let tag = Tag { pid, run_id };
    let comment = format!(
        "COMMENT ON DATABASE {} IS {}",
        quote_ident(&name),
        quote_literal(&tag.render())
    );
    if let Err(error) = client.batch_execute(&comment).await {
        let error = pg_error(format!("tagging database {name}"))(error);
        if let Err(drop_error) = drop_database_with(client, &name).await {
            eprintln!(
                "erp-testkit: could not drop untagged {name}: {}",
                error_chain(&drop_error)
            );
        }
        return Err(error);
    }
    Ok(name)
}

/// A throwaway PostgreSQL database with a connection pool.
///
/// The database is dropped by [`TestDb::cleanup`] or, failing that, when the value is dropped.
/// Prefer `cleanup().await`: it reports errors. The drop path runs on a helper thread, can only
/// print errors to stderr, and anything it misses is removed later by [`crate::sweep`].
pub struct TestDb {
    name: String,
    pool: Pool,
    cluster: Cluster,
    cleaned: bool,
}

impl fmt::Debug for TestDb {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Deliberately omits the connection config: it carries the password.
        f.debug_struct("TestDb")
            .field("name", &self.name)
            .field("cluster", &self.cluster.label())
            .finish_non_exhaustive()
    }
}

impl TestDb {
    /// Create a database on the server in `DATABASE_URL` with default options.
    pub async fn create() -> Result<Self> {
        TestDbBuilder::default().create().await
    }

    /// Options for a new database.
    pub fn builder() -> TestDbBuilder {
        TestDbBuilder::default()
    }

    /// Database name, `erpforall_test_<pid>_<n>_<created>`.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The cluster this database lives on (for checks against the same server, or to create
    /// another database next to it with [`TestDbBuilder::cluster`]).
    pub fn cluster(&self) -> &Cluster {
        &self.cluster
    }

    /// Pool of connections to this database.
    pub fn pool(&self) -> &Pool {
        &self.pool
    }

    /// One pooled connection.
    pub async fn client(&self) -> Result<Object> {
        Ok(self.pool.get().await?)
    }

    /// Return the database connection configuration for this test database.
    #[doc(hidden)]
    pub fn config(&self) -> Config {
        let mut cfg = self.cluster.config().clone();
        cfg.dbname(&self.name);
        cfg
    }

    /// Close the pool and drop the database, reporting any failure.
    pub async fn cleanup(mut self) -> Result<()> {
        self.pool.close();
        drop_database(self.cluster.config(), &self.name).await?;
        self.cleaned = true;
        Ok(())
    }
}

impl Drop for TestDb {
    fn drop(&mut self) {
        if self.cleaned {
            return;
        }
        self.pool.close();
        let admin = self.cluster.config().clone();
        let name = self.name.clone();
        // Drop cannot await, and blocking on the caller's runtime would deadlock a current-thread
        // runtime, so run the DROP on a short-lived thread with its own runtime.
        let worker = std::thread::Builder::new()
            .name("erp-testkit-drop".into())
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(TestkitError::Runtime)?;
                runtime.block_on(drop_database(&admin, &name))
            });
        let outcome = match worker {
            Ok(handle) => match handle.join() {
                Ok(Ok(())) => return,
                Ok(Err(error)) => error_chain(&error),
                Err(_) => "cleanup thread panicked".to_owned(),
            },
            Err(error) => format!("could not spawn cleanup thread: {error}"),
        };
        eprintln!(
            "erp-testkit: failed to drop database {}: {outcome}; run `erp-testkit sweep` to remove it",
            self.name
        );
    }
}
