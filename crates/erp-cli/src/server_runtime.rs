//! Served membership and supervised maintenance around the existing HTTP application.
use std::{sync::Arc, time::Duration};

use erp_modules::ModuleSystem;
use erp_orm::node::{NodeIdentity, NodeMembership, NodeOptions, NodeRole};
use erp_security::auth::gc_schedule::{GcRun, GcSchedule};
use erp_server::{config::ServerConfig, web::WebApp};
use tokio::{net::TcpListener, time::MissedTickBehavior};

use crate::{CliError, Result, access::pool_with_size};

fn failed(message: impl Into<String>) -> CliError {
    CliError::Execution(message.into())
}

fn failed_with(context: &str, cause: &(dyn std::error::Error + 'static)) -> CliError {
    failed(format!("{context}: {}", sanitized_cause(cause)))
}

// Source messages can contain connection URLs, SQL values or addon-supplied text. Preserve typed
// causes and only a small vocabulary of our own fixed GC messages; never render arbitrary Display.
pub(crate) fn sanitized_cause(error: &(dyn std::error::Error + 'static)) -> String {
    let mut parts = Vec::new();
    let mut source = Some(error);
    for _ in 0..8 {
        let Some(error) = source else { break };
        if let Some(error) = error.downcast_ref::<erp_fields::Error>() {
            parts.push(format!("erp={:?}", error.kind()));
            if let Some(message) = known_gc_cause(error.message()) {
                parts.push(format!("gc={message}"));
            }
            if let Some(condition) = error.condition() {
                parts.push(format!("condition={condition:?}"));
            }
            if let Some(state) = error.sqlstate() {
                parts.push(format!("sqlstate={}", safe_sqlstate(state)));
            }
        } else if let Some(error) = error.downcast_ref::<tokio_postgres::Error>() {
            if let Some(db) = error.as_db_error() {
                parts.push(format!("sqlstate={}", safe_sqlstate(db.code().code())));
            } else {
                parts.push(
                    if error.is_closed() {
                        "postgres=closed"
                    } else {
                        "postgres=client"
                    }
                    .to_owned(),
                );
            }
        } else if let Some(error) = error.downcast_ref::<deadpool_postgres::PoolError>() {
            use deadpool_postgres::PoolError;
            parts.push(match error {
                PoolError::Timeout(kind) => format!("pool_timeout={kind:?}"),
                PoolError::Backend(_) => "pool=backend".to_owned(),
                PoolError::Closed => "pool=closed".to_owned(),
                PoolError::NoRuntimeSpecified => "pool=missing_runtime".to_owned(),
                PoolError::PostCreateHook(_) => "pool=post_create_hook".to_owned(),
            });
        } else if let Some(error) = error.downcast_ref::<std::io::Error>() {
            parts.push(format!("io={:?}", error.kind()));
        } else if let Some(error) = error.downcast_ref::<tokio::task::JoinError>() {
            parts.push(
                if error.is_panic() {
                    "task=panicked"
                } else {
                    "task=cancelled"
                }
                .to_owned(),
            );
        } else if error.is::<std::str::Utf8Error>() {
            parts.push("utf8=invalid".to_owned());
        } else {
            // Unknown causes are still failures; their untrusted text is explicitly redacted.
            parts.push("cause=redacted".to_owned());
        }
        source = error.source();
    }
    if source.is_some() {
        parts.push("source_chain=truncated".to_owned());
    }
    parts.join("; ")
}

fn safe_sqlstate(state: &str) -> &str {
    if state.len() == 5
        && state
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit())
    {
        state
    } else {
        "redacted"
    }
}

fn known_gc_cause(message: &str) -> Option<&'static str> {
    const KNOWN: &[&str] = &[
        "authentication collection state is missing; run schema sync before serving login",
        "authentication collection needs SELECT and UPDATE on erp_auth_gc_state before serving login",
        "authentication collection run exceeded its deadline",
        "authentication collection control deadline exceeded",
        "authentication collection control connection deadline exceeded",
        "authentication collection control statement failed",
        "could not get authentication collection control connection",
        "authentication collection lease expired or was superseded",
        "authentication collection lease token is invalid or exhausted",
        "authentication collection cursor is outside PostgreSQL's block range",
        "authentication collection sweep range overflowed",
        "authentication collection lease is implausibly far in the future",
        "authentication collection due time is implausibly far in the future",
        crate::human::MAINTENANCE_PRIVILEGES_MISSING,
    ];
    KNOWN.iter().copied().find(|known| {
        message == *known
            || message
                .strip_prefix(*known)
                .is_some_and(|rest| rest.starts_with(" (and "))
    })
}

/// A membership in either direction (including NOINHERIT/SET/ADMIN paths) crosses the role boundary.
pub(crate) const NODE_ROLE_MEMBERSHIP_SQL: &str = "SELECT pg_has_role($1::text::name,$2::text::name,'MEMBER') OR pg_has_role($2::text::name,$1::text::name,'MEMBER')";

pub(crate) const REQUEST_NODE_PRIVILEGE_SQL: &str = "
WITH RECURSIVE reachable(oid) AS (
    SELECT oid FROM pg_roles WHERE rolname=$1
    UNION SELECT m.roleid FROM pg_auth_members m JOIN reachable r ON r.oid=m.member
)
SELECT EXISTS (SELECT 1 FROM reachable r CROSS JOIN pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace
WHERE n.nspname='public' AND c.relname IN ('erp_node','erp_node_detail','erp_node_rollout','erp_node_feature','erp_node_feature_digest')
AND (has_table_privilege(r.oid,c.oid,'SELECT') OR has_table_privilege(r.oid,c.oid,'INSERT')
 OR has_table_privilege(r.oid,c.oid,'UPDATE') OR has_table_privilege(r.oid,c.oid,'DELETE')
 OR has_table_privilege(r.oid,c.oid,'TRUNCATE') OR has_table_privilege(r.oid,c.oid,'REFERENCES')
 OR has_table_privilege(r.oid,c.oid,'TRIGGER') OR has_any_column_privilege(r.oid,c.oid,'SELECT')
 OR has_any_column_privilege(r.oid,c.oid,'INSERT') OR has_any_column_privilege(r.oid,c.oid,'UPDATE')
 OR has_any_column_privilege(r.oid,c.oid,'REFERENCES')))";

pub(crate) const HEARTBEAT_BUSINESS_PRIVILEGE_SQL: &str = "
WITH RECURSIVE reachable(oid) AS (
    SELECT oid FROM pg_roles WHERE rolname=$1
    UNION SELECT m.roleid FROM pg_auth_members m JOIN reachable r ON r.oid=m.member
)
SELECT EXISTS (SELECT 1 FROM reachable r JOIN pg_roles p ON p.oid=r.oid
WHERE p.rolsuper OR p.rolcreaterole OR p.rolreplication
OR EXISTS (SELECT 1 FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace
WHERE n.nspname='public' AND c.relkind IN ('r','p') AND c.relname NOT IN ('erp_node','erp_node_detail','erp_node_rollout')
AND (has_table_privilege(r.oid,c.oid,'SELECT') OR has_table_privilege(r.oid,c.oid,'INSERT')
 OR has_table_privilege(r.oid,c.oid,'UPDATE') OR has_table_privilege(r.oid,c.oid,'DELETE')
 OR has_table_privilege(r.oid,c.oid,'TRUNCATE') OR has_table_privilege(r.oid,c.oid,'REFERENCES')
 OR has_table_privilege(r.oid,c.oid,'TRIGGER') OR has_any_column_privilege(r.oid,c.oid,'SELECT')
 OR has_any_column_privilege(r.oid,c.oid,'INSERT') OR has_any_column_privilege(r.oid,c.oid,'UPDATE')
 OR has_any_column_privilege(r.oid,c.oid,'REFERENCES'))))";

/// The same shared RPC settings enter node admission and every serving generation.
pub(crate) fn rpc_config(config: &ServerConfig) -> erp_fields::Result<erp_rpc::RpcConfig> {
    erp_rpc::RpcConfig::builder()
        .body_bytes(config.startup_limits().rpc_body_bytes())
        .result_bytes(config.startup_limits().rpc_result_bytes())
        .read_audit_policy(config.read_audit_policy().clone())
        .build()
}

pub(crate) async fn register(
    config: &ServerConfig,
    system: Arc<ModuleSystem>,
) -> Result<NodeMembership> {
    let options = NodeOptions::default();
    let pool = erp_orm::node::heartbeat_pool(
        config
            .node_connection()
            .map_err(|error| failed(error.to_string()))?,
        &options,
    )?;
    let client = pool
        .get()
        .await
        .map_err(|error| failed_with("could not inspect heartbeat privileges", &error))?;
    let runtime_connection = config.runtime_connection();
    let node_connection = config
        .node_connection()
        .map_err(|error| failed(error.to_string()))?;
    let runtime_role = runtime_connection
        .get_user()
        .ok_or_else(|| failed("runtime role missing"))?;
    let node_role = node_connection
        .get_user()
        .ok_or_else(|| failed("heartbeat role missing"))?;
    for (sql, params, message) in [
        (
            NODE_ROLE_MEMBERSHIP_SQL,
            vec![runtime_role, node_role],
            "request and heartbeat roles must not be members of each other",
        ),
        (
            REQUEST_NODE_PRIVILEGE_SQL,
            vec![runtime_role],
            "request role must not access node membership tables",
        ),
        (
            HEARTBEAT_BUSINESS_PRIVILEGE_SQL,
            vec![node_role],
            "heartbeat role must not access business or authentication tables",
        ),
    ] {
        let params: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> = params
            .iter()
            .map(|value| value as &(dyn tokio_postgres::types::ToSql + Sync))
            .collect();
        let refused: bool = client
            .query_one(sql, &params)
            .await
            .map_err(|error| failed_with("could not inspect serving role isolation", &error))?
            .try_get(0)
            .map_err(|error| failed_with("invalid serving role isolation state", &error))?;
        if refused {
            return Err(erp_fields::Error::programming(message).into());
        }
    }
    drop(client);
    let addons = tokio::task::spawn_blocking(move || system.with_index(native_addon_identity))
        .await
        .map_err(|error| failed_with("addon identity task failed", &error))?
        .map_err(|error| CliError::ModuleOperation(Box::new(error)))?;
    let host = rustix::system::uname();
    let host = host
        .nodename()
        .to_str()
        .map_err(|error| failed_with("host name is not UTF-8", &error))?;
    let rpc = rpc_config(config)?;
    let identity = NodeIdentity::builder(
        NodeRole::App,
        config.node_name(),
        host,
        erp_orm::build_identity!(),
        addons,
    )
    .features(erp_orm::node::FeatureSet::new([(
        erp_orm::node::FeatureKey::new("auth.login_principal")?,
        erp_orm::node::FeatureVersion::new(1)?,
    )])?)
    .settings(
        &erp_addon_base::jobs_settings_source::DatabaseJobSettingsSource::compatibility_identity()?,
    )?
    .settings(config)?
    .settings(config.startup_limits())?
    .settings(&rpc)?
    .settings(&rpc.limits())?
    .options(options)
    .create(&pool)
    .await?;
    NodeMembership::register(Arc::new(identity), pool)
        .await
        .map_err(Into::into)
}

// ModuleSystem injects this intrinsic descriptor outside AddonIndex discovery. It is still
// actual native code held by this process and must appear in both registration and refresh
// catalogue observations. Its version label is the enclosing binary's package version; it has
// no standalone manifest and its implementation/content is covered by the native build digest.
pub(crate) fn native_addon_identity(
    index: &mut erp_modules::AddonIndex,
) -> erp_modules::Result<erp_orm::node::AddonsDigest> {
    use erp_orm::node::{AddonContent, AddonsDigest};
    let discovered = index.digest()?;
    let mut catalogue = AddonsDigest::builder();
    for addon in discovered.entries() {
        catalogue = catalogue.addon(addon.name(), addon.version(), addon.content())?;
    }
    Ok(catalogue
        .addon(
            erp_data::model_data::ADDON.name,
            env!("CARGO_PKG_VERSION"),
            AddonContent::Embedded,
        )?
        .build()?)
}

pub(crate) async fn run(
    app: Arc<WebApp>,
    listener: TcpListener,
    config: Arc<ServerConfig>,
    membership: NodeMembership,
    deadline: &mut Option<tokio::time::Instant>,
) -> Result<()> {
    let serving = serve_member(&app, listener, &config, &membership, deadline).await;
    let until =
        deadline.unwrap_or_else(|| tokio::time::Instant::now() + config.shutdown_deadline());
    *deadline = Some(until);
    // An expired drain budget never becomes another full wait for node departure.
    let leaving = tokio::time::timeout_at(until, membership.shutdown()).await;
    let leaving = match leaving {
        Ok(result) => result.map_err(CliError::from),
        Err(_) => Err(failed(
            "server shutdown deadline expired during node departure",
        )),
    };
    match (serving, leaving) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
        (Err(error), Err(leave)) => Err(failed(format!("{error}; {leave}"))),
    }
}

async fn serve_member(
    app: &Arc<WebApp>,
    listener: TcpListener,
    config: &ServerConfig,
    membership: &NodeMembership,
    deadline: &mut Option<tokio::time::Instant>,
) -> Result<()> {
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .map_err(|error| failed_with("could not install server TERM handler", &error))?;
    let mut interrupt =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
            .map_err(|error| failed_with("could not install server INT handler", &error))?;
    app.set_login_available(false);
    let schedule = GcSchedule::new(pool_with_size(config.runtime_connection(), 1)?);
    let event_owner = crate::human::owner_maintenance_orm(config)?;
    let generic_owner = pool_with_size(config.owner_connection(), 1)?;
    let initial_upkeep_period =
        crate::partition_upkeep::policy(app.current_auth().await.orm(), &generic_owner)
            .await
            .map_err(|error| {
                failed_with(
                    "could not read generic partition upkeep policy at startup",
                    &error,
                )
            })?;
    membership.request_ready().await?;
    let (stop, mut stopped) = tokio::sync::watch::channel(false);
    let job_stop = stopped.clone();
    let http = Arc::clone(app).serve_until(listener, async {
        stopped
            .wait_for(|stop| *stop)
            .await
            .map_err(|_| std::io::Error::other("server shutdown control ended"))?;
        Ok(())
    });
    let jobs = maintenance(app, config, membership, job_stop);
    tokio::pin!(jobs);
    let mut jobs_finished = false;
    let trigger = async {
        tokio::select! {
            received = term.recv() => signal_received(received),
            received = interrupt.recv() => signal_received(received),
            result = &mut jobs => { jobs_finished=true; result },
            result = gc_maintenance(app, &schedule) => result,
            result = event_upkeep_maintenance(app, &event_owner) => result,
            result = crate::partition_upkeep::supervise(initial_upkeep_period,
                async || crate::partition_upkeep::policy(app.current_auth().await.orm(), &generic_owner).await,
                async || crate::partition_upkeep::maintain(app.current_auth().await.orm(), &generic_owner).await.map(|_report| ()),
            ) => result,
        }
    };
    let drain = async { membership.drain().await.map_err(std::io::Error::other) };
    let outcome = supervise_shutdown(
        http,
        trigger,
        drain,
        stop,
        config.shutdown_deadline(),
        || app.shutdown_deadline(),
    )
    .await;
    *deadline = Some(outcome.deadline);
    if !jobs_finished {
        tokio::time::timeout_at(outcome.deadline, &mut jobs)
            .await
            .map_err(|_| failed("attachment job runners exceeded the shutdown deadline"))?
            .map_err(|e| failed(format!("attachment job shutdown failed: {e}")))?;
    }
    outcome
        .result
        .map_err(|error| failed(format!("HTTP server stopped: {error}")))
}

struct ShutdownOutcome {
    result: std::io::Result<()>,
    deadline: tokio::time::Instant,
}

// This is the complete runtime ordering seam, shared by the real server and the blocked-ack
// regression: trigger -> immediate HTTP stop -> concurrent membership/HTTP drain -> one deadline.
async fn supervise_shutdown(
    http: impl std::future::Future<Output = std::io::Result<()>>,
    trigger: impl std::future::Future<Output = std::io::Result<()>>,
    drain: impl std::future::Future<Output = std::io::Result<()>>,
    stop: tokio::sync::watch::Sender<bool>,
    limit: Duration,
    http_deadline: impl FnOnce() -> Option<tokio::time::Instant>,
) -> ShutdownOutcome {
    tokio::pin!(http, trigger, drain);
    let (cause, completed_http) = tokio::select! {
        result = &mut trigger => (result, None),
        result = &mut http => (Ok(()), Some(result)),
    };
    let deadline = http_deadline().unwrap_or_else(|| tokio::time::Instant::now() + limit);
    stop.send_replace(true);
    let cleanup = tokio::time::timeout_at(deadline, async {
        let (http_result, drain_result) = tokio::join!(
            async {
                match completed_http {
                    Some(result) => result,
                    None => http.as_mut().await,
                }
            },
            drain.as_mut(),
        );
        combine(http_result, drain_result)
    })
    .await;
    let cleanup = match cleanup {
        Ok(result) => result,
        Err(_) => Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "server shutdown deadline expired",
        )),
    };
    ShutdownOutcome {
        result: combine(cause, cleanup),
        deadline,
    }
}

fn combine(first: std::io::Result<()>, second: std::io::Result<()>) -> std::io::Result<()> {
    match (first, second) {
        (Ok(()), result) | (result, Ok(())) => result,
        (Err(first), Err(second)) => Err(std::io::Error::other(format!("{first}; {second}"))),
    }
}

fn signal_received(value: Option<()>) -> std::io::Result<()> {
    value.ok_or_else(|| std::io::Error::other("server signal stream ended"))
}

const GC_POLL: Duration = Duration::from_secs(30);
const GC_BACKOFF: [Duration; 4] = [
    Duration::from_secs(5),
    Duration::from_secs(10),
    Duration::from_secs(20),
    Duration::from_secs(30),
];
const GC_PROGRESS_WARNING: Duration = Duration::from_secs(360);

#[derive(Default)]
enum GcHealth {
    #[default]
    Unknown,
    Healthy,
    Failed(u8),
}
impl GcHealth {
    fn available(&self) -> bool {
        matches!(self, Self::Healthy)
    }
    fn failures(&self) -> u8 {
        match self {
            Self::Unknown | Self::Healthy => 0,
            Self::Failed(count) => *count,
        }
    }
    fn collected(&mut self) {
        *self = Self::Healthy;
    }
    // Only called after the real scheduler's privilege/state probe and validated run result.
    // Failed and lock-stopped runs never advance the shared due time, so NotDue is proof of a
    // still-current successful collection even when a peer completed it.
    fn not_due(&mut self) {
        *self = Self::Healthy;
    }
    fn skipped(&self) {
        tracing::debug!(
            available = self.available(),
            failures = self.failures(),
            "authentication collection contention preserves admission state"
        );
    }
    fn failed(&mut self) -> std::io::Result<Duration> {
        let prior = self.failures();
        let count = prior.checked_add(1).ok_or_else(|| {
            std::io::Error::other("authentication collection failure count overflowed")
        })?;
        *self = Self::Failed(count);
        GC_BACKOFF.get(usize::from(prior)).copied().ok_or_else(|| {
            std::io::Error::other("authentication collection failed 5 consecutive times")
        })
    }
}

enum GcObservation {
    Collected,
    NotDue,
    Busy,
}
fn gc_observation(run: GcRun) -> erp_fields::Result<GcObservation> {
    match run {
        GcRun::Collected(report) => {
            if let Some(phase) = report.stopped_by_lock {
                tracing::debug!(
                    phase = phase.as_str(),
                    "scheduled authentication collection deferred by batch lock"
                );
                Ok(GcObservation::Busy)
            } else {
                tracing::info!(?report, "scheduled authentication collection completed");
                Ok(GcObservation::Collected)
            }
        }
        GcRun::NotDue => {
            tracing::debug!("scheduled authentication collection not due");
            Ok(GcObservation::NotDue)
        }
        GcRun::Busy => {
            tracing::debug!("scheduled authentication collection busy");
            Ok(GcObservation::Busy)
        }
        // Future scheduler outcomes cannot silently become healthy admission.
        _ => Err(erp_fields::Error::programming(
            "unsupported authentication collection scheduler outcome",
        )),
    }
}

async fn gc_maintenance(app: &WebApp, schedule: &GcSchedule) -> std::io::Result<()> {
    supervise_collection(
        async || {
            schedule.probe().await?;
            let auth = app.current_auth().await;
            schedule.run(&auth).await
        },
        |available| app.set_login_available(available),
    )
    .await
}

// Kept independent of registry refresh and signal supervision: a bounded, slow collection must
// neither stop their polling nor delay TERM/INT. Tests drive this same retry/admission loop.
async fn supervise_collection(
    collect: impl AsyncFn() -> erp_fields::Result<GcRun>,
    publish: impl Fn(bool),
) -> std::io::Result<()> {
    let mut health = GcHealth::default();
    let mut progress = tokio::time::Instant::now();
    let mut warned = progress;
    loop {
        let observed = collect().await.and_then(gc_observation);
        let delay = match observed {
            Ok(GcObservation::Collected) => {
                health.collected();
                progress = tokio::time::Instant::now();
                GC_POLL
            }
            Ok(GcObservation::NotDue) => {
                health.not_due();
                progress = tokio::time::Instant::now();
                GC_POLL
            }
            Ok(GcObservation::Busy) => {
                health.skipped();
                GC_POLL
            }
            Err(error) => {
                let retry = health.failed();
                tracing::error!(kind=?error.kind(), cause=%sanitized_cause(&error), failures=health.failures(), "scheduled authentication collection failed; login admission closed");
                match retry {
                    Ok(delay) => delay,
                    Err(terminal) => {
                        publish(false);
                        return Err(terminal);
                    }
                }
            }
        };
        publish(health.available());
        let now = tokio::time::Instant::now();
        if now.duration_since(progress) >= GC_PROGRESS_WARNING
            && now.duration_since(warned) >= GC_PROGRESS_WARNING
        {
            tracing::warn!(
                available = health.available(),
                "scheduled authentication collection has made no confirmed progress for six minutes"
            );
            warned = now;
        }
        tokio::time::sleep(delay).await;
    }
}

/// How often every node runs the event partition upkeep.
const EVENT_UPKEEP_PERIOD: Duration = Duration::from_secs(3_600);

/// The scheduled event partition upkeep of this node: every
/// [`EVENT_UPKEEP_PERIOD`], the upkeep on `owner` (a one-connection owner pool opened once at
/// start, never bound to a registry generation), then the audit store check on the runtime ORM of
/// the current generation, whose cached state would otherwise keep refusing after the window was
/// refreshed. Every node runs it; the namespace advisory lock lets one of them create a day. It
/// is a timer on the owner pool, not a queue job: the ordinary job runner holds the runtime role
/// and cannot run DDL, and the isolated runner for elevated kinds is not in place yet.
async fn event_upkeep_maintenance(app: &WebApp, owner: &erp_orm::Orm) -> std::io::Result<()> {
    supervise_event_upkeep(EVENT_UPKEEP_PERIOD, async || {
        let auth = app.current_auth().await;
        let tick = owner.event_upkeep_tick(auth.orm()).await?;
        log_event_upkeep(&tick.report, "scheduled")?;
        if tick.audit_store != erp_orm::audit::AuditStoreState::Ready {
            tracing::error!(
                state = %tick.audit_store.describe(),
                "audit store is not ready after the scheduled event partition upkeep"
            );
        }
        Ok(())
    })
    .await
}

/// Run `tick` every `period`, the first time one period after the call (the server start ran the
/// upkeep already). A failed tick is logged at error level with the full error and its sanitized
/// cause, and retried
/// at the next tick; it never stops the node, so this never returns.
async fn supervise_event_upkeep(
    period: Duration,
    tick: impl AsyncFn() -> erp_fields::Result<()>,
) -> std::io::Result<()> {
    let mut ticks = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
    ticks.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        ticks.tick().await;
        if let Err(error) = tick().await {
            // the operator's log keeps the full error, as the startup refusal does
            tracing::error!(
                %error,
                kind = ?error.kind(),
                cause = %sanitized_cause(&error),
                "scheduled event partition upkeep failed; it runs again at the next tick"
            );
        }
    }
}

/// Log what an event partition upkeep did (`when`: "startup" or "scheduled"): a busy table at
/// warning level, created partitions at info, and the window of each installed table, at info
/// with today covered and two days or more after it, at error otherwise (inserts fail within two
/// UTC midnights or already do). A table that is not installed is logged at debug level only: it is the normal
/// state of the outbox until a model opts into events.
///
/// # Errors
///
/// `Programming` for an outcome this build does not know.
pub(crate) fn log_event_upkeep(
    report: &erp_orm::EventUpkeepReport,
    when: &'static str,
) -> erp_fields::Result<()> {
    use erp_orm::EventTableUpkeep;
    for (table, outcome) in [
        (erp_orm::OUTBOX_TABLE_NAME, &report.outbox),
        (erp_orm::AUDIT_TABLE_NAME, &report.audit),
    ] {
        match outcome {
            EventTableUpkeep::NotInstalled => {
                tracing::debug!(
                    table,
                    when,
                    "event table not installed; no partition upkeep"
                );
            }
            EventTableUpkeep::Busy => tracing::warn!(
                table,
                when,
                "event partition upkeep busy: another session held a lock past the partition \
                 lock timeout; it runs again at the next tick"
            ),
            EventTableUpkeep::Maintained { created } if created.is_empty() => {
                tracing::debug!(table, when, "event partition window complete");
            }
            EventTableUpkeep::Maintained { created } => tracing::info!(
                table,
                when,
                created = ?created,
                "event partitions created"
            ),
            other => {
                return Err(erp_fields::Error::programming(format!(
                    "unknown event partition upkeep outcome for {table}: {other:?}"
                )));
            }
        }
    }
    for window in report.health.iter().filter(|w| w.installed) {
        if window.is_short() {
            tracing::error!(
                table = window.table,
                today_covered = window.today_covered,
                days_left = window.days_left,
                when,
                "event partition window is short: inserts fail within two UTC midnights or \
                 already do; run `erp audit maintain` as the owner role"
            );
        } else {
            tracing::info!(
                table = window.table,
                today_covered = window.today_covered,
                days_left = window.days_left,
                when,
                "event partition window"
            );
        }
    }
    Ok(())
}

/// The job runner of this process for `orm`, when the configuration asks for one (4):
/// `jobs_run_in_app = true`. It claims only while the node is ready and divides the database cap
/// by the live workers of the registry.
struct JobRunners {
    ordinary: Option<erp_orm::jobs::JobRunnerHandle>,
    attachments: Option<erp_orm::jobs::JobRunnerHandle>,
}
impl JobRunners {
    async fn shutdown(self) {
        async fn stop(handle: Option<erp_orm::jobs::JobRunnerHandle>) {
            if let Some(handle) = handle {
                handle.shutdown().await;
            }
        }
        tokio::join!(stop(self.ordinary), stop(self.attachments));
    }
    /// The runner whose task ended, if one did.
    fn finished(&self) -> Option<&'static str> {
        if self.ordinary.as_ref().is_some_and(|r| r.is_finished()) {
            Some("job runner")
        } else if self.attachments.as_ref().is_some_and(|r| r.is_finished()) {
            Some("attachment maintenance runner")
        } else {
            None
        }
    }
}
async fn spawn_job_runners(
    orm: &erp_orm::Orm,
    config: &ServerConfig,
    membership: &NodeMembership,
) -> std::io::Result<JobRunners> {
    let process = Arc::new(tokio::sync::Semaphore::new(
        orm.job_node_settings().workers() as usize,
    ));
    let database = Arc::new(tokio::sync::Semaphore::new(
        orm.job_node_settings().workers_per_db() as usize,
    ));
    let build_error = |e: erp_fields::Error| {
        std::io::Error::other(format!(
            "could not build job runner: {}",
            sanitized_cause(&e)
        ))
    };
    let ordinary = if config.jobs_run_in_app() {
        let view = membership.subscribe();
        Some(
            erp_orm::jobs::JobRunner::builder(
                orm.clone(),
                erp_security::jobs::SecurityJobPrincipalResolver,
            )
            .node_view(move || view.borrow().clone())
            .process_slots(Arc::clone(&process))
            .database_slots(Arc::clone(&database))
            .exclude_elevated()
            .build()
            .map_err(build_error)?,
        )
    } else {
        None
    };
    // Construct and admit both before spawning either: partial startup cannot leave a runner detached.
    let attachments = if config.attachment_maintenance_run_in_app() {
        let owner = crate::human::attachment_maintenance_orm(config, orm)
            .await
            .map_err(|e| {
                // The ORM's error is boxed inside `CliError::Access`: hand it over itself, so its
                // kind and a known fixed message survive sanitizing.
                let cause = match &e {
                    CliError::Access(error) => sanitized_cause(error.as_ref()),
                    other => sanitized_cause(other),
                };
                std::io::Error::other(format!("could not start attachment maintenance: {cause}"))
            })?;
        let view = membership.subscribe();
        Some(
            erp_orm::jobs::JobRunner::builder(
                owner,
                erp_security::jobs::SecurityJobPrincipalResolver,
            )
            .node_view(move || view.borrow().clone())
            .process_slots(process)
            .database_slots(database)
            .maintenance_kinds(&erp_addon_base::attachments::gc::maintenance_kinds())
            .build()
            .map_err(build_error)?,
        )
    } else {
        tracing::warn!(
            "attachment maintenance execution is disabled on this node; schedule rows alone do not run cleanup"
        );
        None
    };
    Ok(JobRunners {
        ordinary: ordinary.map(|r| r.spawn()),
        attachments: attachments.map(|r| r.spawn()),
    })
}

async fn maintenance(
    app: &WebApp,
    config: &ServerConfig,
    membership: &NodeMembership,
    mut stopped: tokio::sync::watch::Receiver<bool>,
) -> std::io::Result<()> {
    let mut auth = app.current_auth().await;
    let options = erp_orm::signal::ListenerOptions::default();
    // the runner subscribes to its wake-up channel when it is built: before the listener starts
    let mut runners = spawn_job_runners(auth.orm(), config, membership).await?;
    let mut listener = auth
        .orm()
        .spawn_signal_listener(config.runtime_connection(), tokio_postgres::NoTls, options)
        .map_err(|error| {
            std::io::Error::other(format!(
                "could not start registry signal listener: {}",
                sanitized_cause(&error)
            ))
        })?;
    let mut refresh = tokio::time::interval(Duration::from_secs(1));
    refresh.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = stopped.changed() => {
                listener.shutdown().await;
                runners.shutdown().await;
                return Ok(());
            }
            _ = refresh.tick() => {
                if matches!(membership.readiness(), erp_orm::node::Readiness::Unready(erp_orm::node::UnreadyReason::IdentityConflict)) {
                    return Err(std::io::Error::other("node identity conflict; restart the process"));
                }
                if !app.refresh_generation().await { continue; }
                let current = app.current_auth().await;
                if !Arc::ptr_eq(&auth, &current) {
                    listener.shutdown().await;
                    runners.shutdown().await;
                    auth = current;
                    runners = spawn_job_runners(auth.orm(), config, membership).await?;
                    listener = auth.orm().spawn_signal_listener(config.runtime_connection(), tokio_postgres::NoTls, options)
                        .map_err(|error| std::io::Error::other(format!("could not restart registry signal listener: {}", sanitized_cause(&error))))?;
                } else if listener.is_finished() {
                    return Err(std::io::Error::other("registry signal listener ended without a replacement generation"));
                } else if let Some(runner) = runners.finished() {
                    return Err(std::io::Error::other(format!("the {runner} ended without a replacement generation")));
                }
            }
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "shutdown coordination regression"
)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct Handler(Arc<AtomicBool>);
    impl Drop for Handler {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    #[test]
    fn gc_failure_budget_only_resets_on_proven_healthy_schedule() {
        let mut health = GcHealth::default();
        assert!(!health.available());
        assert_eq!(health.failed().unwrap(), Duration::from_secs(5));
        assert!(!health.available());
        health.skipped();
        assert!(!health.available(), "Busy must not heal a failed collector");
        assert_eq!(health.failed().unwrap(), Duration::from_secs(10));
        health.not_due();
        assert!(
            health.available(),
            "a probed, validated future due time proves healthy shared collection"
        );
        assert_eq!(health.failed().unwrap(), Duration::from_secs(5));
        for expected in [10, 20, 30] {
            assert_eq!(health.failed().unwrap(), Duration::from_secs(expected));
        }
        let error = health.failed().unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::Other);
        assert_eq!(
            error.to_string(),
            "authentication collection failed 5 consecutive times"
        );
        assert!(!health.available());
    }

    #[test]
    fn busy_collector_does_not_consume_a_healthy_replicas_failure_budget() {
        let mut health = GcHealth::default();
        health.collected();
        for _ in 0..100 {
            health.skipped();
        }
        assert!(health.available());
        assert_eq!(health.failed().unwrap(), Duration::from_secs(5));
    }

    #[tokio::test(start_paused = true)]
    async fn gc_supervisor_retries_recovers_and_only_stops_at_the_failure_bound() {
        use std::sync::Mutex;
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let changes = Mutex::new(Vec::new());
        let started = tokio::time::Instant::now();
        let result = supervise_collection(
            async || {
                let call = calls.fetch_add(1, Ordering::SeqCst);
                match call {
                    0 => Ok(erp_security::auth::gc_schedule::GcRun::NotDue),
                    1 | 4.. => Err(erp_fields::Error::db("private addon failure")),
                    2 => Ok(erp_security::auth::gc_schedule::GcRun::Busy),
                    3 => Ok(erp_security::auth::gc_schedule::GcRun::Collected(
                        Default::default(),
                    )),
                }
            },
            |available| changes.lock().unwrap().push(available),
        )
        .await;
        let error = result.unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::Other);
        assert_eq!(
            error.to_string(),
            "authentication collection failed 5 consecutive times"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 9);
        let changes = changes.into_inner().unwrap();
        assert_eq!(&changes[..4], &[true, false, false, true]);
        assert!(changes[4..].iter().all(|available| !available));
        assert!(
            tokio::time::Instant::now().duration_since(started)
                >= Duration::from_secs(30 * 3 + 5 + 5 + 10 + 20 + 30)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_partial_locked_collection_does_not_open_login() {
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let available = std::sync::Mutex::new(Vec::new());
        let error = supervise_collection(
            async || {
                if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    let mut report = erp_security::auth::gc::GcReport::default();
                    report.stopped_by_lock = Some(erp_security::auth::gc::GcPhase::Expired);
                    Ok(erp_security::auth::gc_schedule::GcRun::Collected(report))
                } else {
                    Err(erp_fields::Error::db("private cause"))
                }
            },
            |value| available.lock().unwrap().push(value),
        )
        .await
        .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::Other);
        assert_eq!(
            error.to_string(),
            "authentication collection failed 5 consecutive times"
        );
        assert!(available.into_inner().unwrap().iter().all(|value| !value));
    }

    #[derive(Clone)]
    struct CapturedLog(Arc<std::sync::Mutex<Vec<u8>>>);
    impl std::io::Write for CapturedLog {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[tokio::test(start_paused = true)]
    async fn every_collection_failure_logs_its_sanitized_cause() {
        let bytes = Arc::new(std::sync::Mutex::new(Vec::new()));
        let logger = erp_server::logging::LineLogger::new(
            erp_server::logging::LogFilter::parse("debug").unwrap(),
            CapturedLog(Arc::clone(&bytes)),
        );
        let _guard = tracing::subscriber::set_default(logger);
        let error = supervise_collection(
            async || {
                Err(
                    erp_fields::Error::db("private-password\nprivate SQL values")
                        .with_sqlstate(Some("42501".to_owned())),
                )
            },
            |_| {},
        )
        .await
        .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::Other);
        assert_eq!(
            error.to_string(),
            "authentication collection failed 5 consecutive times"
        );
        let log = String::from_utf8(bytes.lock().unwrap().clone()).unwrap();
        assert_eq!(
            log.lines()
                .filter(|line| line.contains("scheduled authentication collection failed"))
                .count(),
            5
        );
        assert!(log.contains("sqlstate=42501"), "{log}");
        assert!(!log.contains("private-password"), "{log}");
        assert!(!log.contains("private SQL"), "{log}");
    }

    #[tokio::test(start_paused = true)]
    async fn busy_and_not_due_are_logged_without_spamming_progress_warnings() {
        let bytes = Arc::new(std::sync::Mutex::new(Vec::new()));
        let logger = erp_server::logging::LineLogger::new(
            erp_server::logging::LogFilter::parse("debug").unwrap(),
            CapturedLog(Arc::clone(&bytes)),
        );
        let _guard = tracing::subscriber::set_default(logger);
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let error = supervise_collection(
            async || match calls.fetch_add(1, Ordering::SeqCst) {
                0..=29 | 31..=36 => Ok(GcRun::Busy),
                30 => Ok(GcRun::NotDue),
                _ => Err(erp_fields::Error::db("private failure")),
            },
            |_| {},
        )
        .await
        .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::Other);
        assert_eq!(
            error.to_string(),
            "authentication collection failed 5 consecutive times"
        );
        let log = String::from_utf8(bytes.lock().unwrap().clone()).unwrap();
        assert_eq!(
            log.lines()
                .filter(|line| line.contains("scheduled authentication collection busy"))
                .count(),
            36,
            "{log}"
        );
        assert_eq!(
            log.lines()
                .filter(|line| line.contains("scheduled authentication collection not due"))
                .count(),
            1,
            "{log}"
        );
        // Busy polls at 0..870 seconds warn at 360 and 720. After NotDue at 900,
        // six more Busy polls cross the old 1080 boundary. There must be no third warning:
        // the following failures finish at 1175, before the reset boundary of 1260.
        assert_eq!(
            log.lines()
                .filter(|line| line.contains("has made no confirmed progress"))
                .count(),
            2,
            "{log}"
        );
        assert!(!log.contains("private failure"), "{log}");
    }

    /// A failed scheduled event upkeep is logged at error level with the full error and its
    /// sanitized cause, and the next tick runs one period later; the supervisor never ends, so the
    /// node keeps serving.
    #[tokio::test(start_paused = true)]
    async fn event_upkeep_failure_is_logged_and_the_next_tick_runs() {
        let bytes = Arc::new(std::sync::Mutex::new(Vec::new()));
        let logger = erp_server::logging::LineLogger::new(
            erp_server::logging::LogFilter::parse("debug").unwrap(),
            CapturedLog(Arc::clone(&bytes)),
        );
        let _guard = tracing::subscriber::set_default(logger);
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let started = tokio::time::Instant::now();
        let outcome = tokio::time::timeout(
            EVENT_UPKEEP_PERIOD * 4 + Duration::from_secs(1),
            supervise_event_upkeep(EVENT_UPKEEP_PERIOD, async || {
                match calls.fetch_add(1, Ordering::SeqCst) {
                    0 | 2 => Err(erp_fields::Error::db(
                        "partition upkeep statement failed: FULL-CAUSE",
                    )
                    .with_sqlstate(Some("55P03".to_owned()))),
                    _ => Ok(()),
                }
            }),
        )
        .await;
        assert!(outcome.is_err(), "the supervisor ended: {outcome:?}");
        // the first tick waits one period (startup already ran the upkeep), then one per period
        assert_eq!(calls.load(Ordering::SeqCst), 4);
        assert!(tokio::time::Instant::now().duration_since(started) >= EVENT_UPKEEP_PERIOD * 4);
        let log = String::from_utf8(bytes.lock().unwrap().clone()).unwrap();
        let failures: Vec<&str> = log
            .lines()
            .filter(|line| line.contains("scheduled event partition upkeep failed"))
            .collect();
        assert_eq!(failures.len(), 2, "{log}");
        assert!(failures.iter().all(|line| line.contains("ERROR")), "{log}");
        assert!(
            failures.iter().all(|line| line.contains("sqlstate=55P03")),
            "{log}"
        );
        assert!(
            failures.iter().all(|line| line.contains("FULL-CAUSE")),
            "the full error is logged: {log}"
        );
    }

    #[test]
    fn startup_causes_are_useful_without_rendering_untrusted_error_text() {
        let error = std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "postgres://owner:private-password@private-host/db\nprivate-data",
        );
        let rendered = sanitized_cause(&error);
        assert_eq!(rendered, "io=PermissionDenied");
        assert!(!rendered.contains("password"));
        assert!(!rendered.contains("private-host"));
        assert!(!rendered.contains('\n'));
    }

    #[test]
    fn database_causes_keep_typed_state_but_drop_error_messages() {
        let error = erp_fields::Error::db("private query and parameters")
            .with_sqlstate(Some("42501".to_owned()))
            .with_source(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "private password",
            ));
        assert_eq!(
            sanitized_cause(&error),
            "erp=Database; sqlstate=42501; io=PermissionDenied"
        );
        let invalid =
            erp_fields::Error::db("private").with_sqlstate(Some("private\npassword".to_owned()));
        assert_eq!(sanitized_cause(&invalid), "erp=Database; sqlstate=redacted");
        let overflow =
            erp_fields::Error::programming("authentication collection sweep range overflowed");
        assert_eq!(
            sanitized_cause(&overflow),
            "erp=Programming; gc=authentication collection sweep range overflowed"
        );
    }

    #[test]
    fn compound_collection_error_keeps_safe_primary_cause() {
        let error = erp_fields::Error::new(
            erp_fields::ErrorKind::ResourceExhausted,
            "authentication collection run exceeded its deadline",
        )
        .and_also(&erp_fields::Error::db("private-password and private query"));
        assert_eq!(
            sanitized_cause(&error),
            "erp=ResourceExhausted; gc=authentication collection run exceeded its deadline"
        );
    }

    #[tokio::test]
    async fn stalled_membership_cannot_delay_http_stop_or_extend_shutdown_budget() {
        let (stop, mut stopped) = tokio::sync::watch::channel(false);
        let (started, active) = tokio::sync::oneshot::channel();
        let dropped = Arc::new(AtomicBool::new(false));
        let observed_stop = Arc::new(AtomicBool::new(false));
        let finished = Arc::clone(&dropped);
        let observed = Arc::clone(&observed_stop);
        let http = async move {
            let _handler = Handler(finished);
            started.send(()).unwrap();
            stopped.wait_for(|stop| *stop).await.unwrap();
            observed.store(true, Ordering::SeqCst);
            std::future::pending::<std::io::Result<()>>().await
        };
        let signal = async {
            active.await.unwrap();
            Ok(())
        };
        let member_ack = std::future::pending::<std::io::Result<()>>();
        let outcome = tokio::time::timeout(
            Duration::from_secs(1),
            supervise_shutdown(
                http,
                signal,
                member_ack,
                stop,
                Duration::from_millis(50),
                || None,
            ),
        )
        .await
        .unwrap();
        let error = outcome.result.unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        assert_eq!(error.to_string(), "server shutdown deadline expired");
        assert!(
            observed_stop.load(Ordering::SeqCst),
            "HTTP stop must not wait for membership acknowledgement"
        );
        assert!(
            dropped.load(Ordering::SeqCst),
            "stalled handler must be dropped by the same deadline"
        );
        // Final membership departure gets the remaining absolute budget, not a fresh interval.
        let leave = tokio::time::timeout(
            Duration::from_millis(100),
            tokio::time::timeout_at(outcome.deadline, std::future::pending::<()>()),
        )
        .await
        .unwrap();
        assert!(leave.is_err());
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "native catalogue regression")]
mod catalogue_tests {
    #[test]
    fn native_catalogue_includes_the_intrinsic_module_registry_descriptor() {
        use erp_orm::node::AddonContent;
        let mut index = erp_modules::AddonIndex::new(false);
        assert!(index.digest().unwrap().entries().is_empty());
        let first = super::native_addon_identity(&mut index).unwrap();
        let second = super::native_addon_identity(&mut index).unwrap();
        assert_eq!(first.digest(), second.digest());
        assert_eq!(first.entries().len(), 1);
        let entry = &first.entries()[0];
        assert_eq!(entry.name(), erp_data::model_data::ADDON.name);
        assert_eq!(entry.version(), env!("CARGO_PKG_VERSION"));
        assert_eq!(entry.content(), AddonContent::Embedded);
    }
}
