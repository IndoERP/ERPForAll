//! Bounded operator registration and disabling of durable feature state.

use std::future::Future;
use std::time::Duration;

use deadpool_postgres::Pool;
use tokio::time::Instant;
use tokio_postgres::Row;

use super::super::Digest;
use super::super::heartbeat::{Conn, get};
use super::super::{LOCK_CLASS, NodeError, READINESS_LOCK, sql};
use super::{FeatureKey, FeatureVersion};

const OPERATOR_BOUND: Duration = Duration::from_secs(30);
/// The transaction advisory lock serializing feature-key registration, after readiness coordination.
pub const FEATURE_REGISTRY_LOCK: (i32, i32) = (LOCK_CLASS, 6);
const READ_STATE: &str = "SELECT version, enabled, generation, enrollment_confirmed, \
    enrollment_actor IS NOT NULL, enrollment_at IS NOT NULL \
    FROM public.erp_node_feature WHERE key = $1";
const LOCK_STATE: &str = "SELECT version, enabled, generation, enrollment_confirmed, \
    enrollment_actor IS NOT NULL, enrollment_at IS NOT NULL \
    FROM public.erp_node_feature WHERE key = $1 FOR UPDATE";

/// An authoritative snapshot of one registered feature's state.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct FeatureStatus {
    enabled: bool,
    generation: i64,
    version: FeatureVersion,
    approved: Vec<Digest>,
    incompatible: u64,
}

impl FeatureStatus {
    /// The validated required binary capability version.
    pub fn required_version(&self) -> FeatureVersion {
        self.version
    }
    /// At most 32 exact approved digests, in canonical byte order.
    pub fn approved_digests(&self) -> &[Digest] {
        &self.approved
    }
    /// Fresh/recent non-stopped memberships that would block activation at this observation.
    pub fn incompatible_memberships(&self) -> u64 {
        self.incompatible
    }

    /// Whether the durable feature gate is enabled.
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Its nonnegative activation generation.
    pub fn generation(&self) -> i64 {
        self.generation
    }
}

/// Operator operations for a validated feature and its required binary version.
///
/// Use an operator pool: this API grants no relation privileges to request or heartbeat roles.
/// Registration never enables a feature and never changes an existing version or enrollment.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct FeatureRollout {
    key: FeatureKey,
    version: FeatureVersion,
    #[cfg(feature = "test-faults")]
    lose_commit_ack: bool,
}

/// Explicit operator acknowledgement that legacy standalone services were drained and their
/// credentials/sessions fenced. This records an external cutover precondition, not measured proof.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct LegacyEnrollment(());
impl LegacyEnrollment {
    /// Confirm the operator has completed the documented legacy drain and fencing procedure.
    pub fn confirmed_drained_and_fenced() -> Self {
        Self(())
    }
}

#[derive(Clone, Copy)]
enum Operation {
    Register,
    Status,
    Disable,
    Approve(Digest),
    Enable(LegacyEnrollment),
}

impl Operation {
    fn what(self) -> &'static str {
        match self {
            Self::Register => "registering a feature",
            Self::Status => "reading feature status",
            Self::Disable => "disabling a feature",
            Self::Approve(_) => "approving a feature digest",
            Self::Enable(_) => "enabling a feature",
        }
    }
}

enum CommitAck {
    Confirmed,
    Failed(tokio_postgres::Error),
    Unknown,
}

impl FeatureRollout {
    /// Select a validated feature and required capability version; no connection is opened.
    pub fn new(key: FeatureKey, version: FeatureVersion) -> Self {
        Self {
            key,
            version,
            #[cfg(feature = "test-faults")]
            lose_commit_ack: false,
        }
    }

    /// Test seam: discard acknowledgment after a real successful COMMIT, exercising the same
    /// unknown-outcome handling as an acknowledgment deadline. This does not emulate wire failure.
    #[cfg(feature = "test-faults")]
    #[doc(hidden)]
    #[must_use]
    pub fn with_lost_commit_ack_for_tests(mut self) -> Self {
        self.lose_commit_ack = true;
        self
    }

    /// Register a previously absent feature as disabled at generation zero.
    ///
    /// Serializes database-wide cardinality under readiness coordination and the feature registry
    /// advisory lock. Repeated registration preserves all existing state and requires the same version.
    ///
    /// # Errors
    ///
    /// [`NodeError::FeatureRefused`] for a ninth key, mismatched version or malformed state;
    /// [`NodeError::TableMissing`], [`NodeError::Timeout`] or [`NodeError::Database`] for store faults.
    /// Lost commit acknowledgment is reported as an unknown outcome requiring status reconciliation.
    pub async fn register(&self, pool: &Pool) -> Result<FeatureStatus, NodeError> {
        self.perform(pool, Operation::Register).await
    }

    /// Read one repeatable-read snapshot of state, approved digests and incompatible membership.
    /// No feature row lock or state mutation is performed.
    ///
    /// # Errors
    ///
    /// [`NodeError::FeatureRefused`] for missing state, mismatched version or malformed state;
    /// [`NodeError::TableMissing`], [`NodeError::Timeout`] or [`NodeError::Database`] for store faults.
    pub async fn status(&self, pool: &Pool) -> Result<FeatureStatus, NodeError> {
        self.perform(pool, Operation::Status).await
    }

    /// Disable an existing feature under exclusive state protection.
    ///
    /// An enabled-to-disabled transition increments the generation once. Already-disabled state is
    /// returned unchanged; enrollment and approvals are preserved. Exhaustion refuses atomically.
    ///
    /// # Errors
    ///
    /// [`NodeError::FeatureRefused`] for missing state, version mismatch, malformed state or exhausted
    /// generation; [`NodeError::TableMissing`], [`NodeError::Timeout`] or [`NodeError::Database`] for
    /// store faults. Lost commit acknowledgment is reported as an unknown outcome, never success.
    pub async fn disable(&self, pool: &Pool) -> Result<FeatureStatus, NodeError> {
        self.perform(pool, Operation::Disable).await
    }

    /// Approve an exact digest only when a fresh non-stopped member declares this capability.
    ///
    /// # Errors
    /// Refuses incompatible or absent declarations, unpublished helpers and more than 32 digests.
    /// Store faults retain their SQLSTATE and source; unknown commit outcomes require status.
    pub async fn approve(&self, pool: &Pool, digest: Digest) -> Result<FeatureStatus, NodeError> {
        self.perform(pool, Operation::Approve(digest)).await
    }

    /// Enable after authoritative membership checks and explicit legacy enrollment confirmation.
    ///
    /// # Errors
    /// Refuses any fresh/recent incompatible non-stopped membership, unpublished helpers or
    /// exhausted generation. No refusal changes durable state; commit uncertainty is an error.
    pub async fn enable(
        &self,
        pool: &Pool,
        confirmation: LegacyEnrollment,
    ) -> Result<FeatureStatus, NodeError> {
        self.perform(pool, Operation::Enable(confirmation)).await
    }

    fn refused(&self, database: &str, reason: impl Into<String>) -> NodeError {
        NodeError::FeatureRefused {
            database: database.to_owned(),
            feature: self.key.as_str().to_owned(),
            reason: reason.into(),
        }
    }

    fn decode(
        &self,
        row: &Row,
        database: &str,
        what: &'static str,
    ) -> Result<FeatureStatus, NodeError> {
        let version: i32 = get(row, 0, database, what)?;
        if version != self.version.get() {
            return Err(self.refused(
                database,
                format!(
                    "stored version {version} does not match required version {}",
                    self.version.get()
                ),
            ));
        }
        let enabled: bool = get(row, 1, database, what)?;
        let generation: i64 = get(row, 2, database, what)?;
        if generation < 0 {
            return Err(self.refused(database, "feature generation is negative"));
        }
        let enrolled: bool = get(row, 3, database, what)?;
        let actor: bool = get(row, 4, database, what)?;
        let at: bool = get(row, 5, database, what)?;
        if enrolled != (actor && at) || (enabled && !enrolled) {
            return Err(self.refused(database, "feature enrollment state is malformed"));
        }
        Ok(FeatureStatus {
            enabled,
            generation,
            version: self.version,
            approved: Vec::new(),
            incompatible: 0,
        })
    }

    async fn perform(&self, pool: &Pool, operation: Operation) -> Result<FeatureStatus, NodeError> {
        let what = operation.what();
        let deadline = Instant::now() + OPERATOR_BOUND;
        let conn = match tokio::time::timeout_at(deadline, pool.get()).await {
            Ok(Ok(object)) => Conn::new(object),
            Ok(Err(error)) => return Err(NodeError::pool("", what, error)),
            Err(_) => return Err(timed_out("", what)),
        };
        // Every error/cancellation discards this guard, including a failed transaction. Only an
        // acknowledged COMMIT recycles it; no best-effort rollback is represented as successful.
        let mut begin = sql::begin_sql(30_000, 30_000, false);
        begin.push_str(
            "; SET TRANSACTION ISOLATION LEVEL READ COMMITTED; SET LOCAL lock_timeout = 30000; SET LOCAL search_path = pg_catalog, public",
        );
        if matches!(operation, Operation::Status) {
            begin.push_str(
                "; SET TRANSACTION ISOLATION LEVEL REPEATABLE READ; SET TRANSACTION READ ONLY",
            );
        }
        run(deadline, "", what, conn.batch_execute(&begin)).await?;
        let row = run(
            deadline,
            "",
            what,
            conn.query_one(
                "SELECT pg_catalog.current_database(), pg_catalog.to_regclass('public.erp_node_feature') IS NOT NULL",
                &[],
            ),
        )
        .await?;
        let database: String = get(&row, 0, "", what)?;
        let exists: bool = get(&row, 1, &database, what)?;
        if !exists {
            return Err(NodeError::TableMissing {
                database,
                table: "erp_node_feature",
            });
        }
        match operation {
            Operation::Register
            | Operation::Disable
            | Operation::Approve(_)
            | Operation::Enable(_) => {
                let (class, key) = READINESS_LOCK;
                run(
                    deadline,
                    &database,
                    what,
                    conn.query_one(
                        "SELECT pg_catalog.pg_advisory_xact_lock($1, $2)",
                        &[&class, &key],
                    ),
                )
                .await?;
            }
            Operation::Status => {}
        }
        if matches!(operation, Operation::Register) {
            let (class, key) = FEATURE_REGISTRY_LOCK;
            run(
                deadline,
                &database,
                what,
                conn.query_one(
                    "SELECT pg_catalog.pg_advisory_xact_lock($1, $2)",
                    &[&class, &key],
                ),
            )
            .await?;
        }
        let statement = match operation {
            Operation::Status => READ_STATE,
            _ => LOCK_STATE,
        };
        let row = run(
            deadline,
            &database,
            what,
            conn.query_opt(statement, &[&self.key.as_str()]),
        )
        .await?;
        let mut status =
            match row {
                Some(row) => self.decode(&row, &database, what)?,
                None => match operation {
                    Operation::Status
                    | Operation::Disable
                    | Operation::Approve(_)
                    | Operation::Enable(_) => {
                        return Err(self.refused(
                            &database,
                            "feature state is missing; register it before use",
                        ));
                    }
                    Operation::Register => {
                        let count = run(
                            deadline,
                            &database,
                            what,
                            conn.query_one("SELECT count(*) FROM public.erp_node_feature", &[]),
                        )
                        .await?;
                        let count: i64 = get(&count, 0, &database, what)?;
                        if count >= 8 {
                            return Err(self.refused(
                                &database,
                                "a database registers at most 8 feature keys",
                            ));
                        }
                        let version = self.version.get();
                        let inserted = run(deadline, &database, what, conn.query_one(
                        "INSERT INTO public.erp_node_feature (key, version) VALUES ($1, $2) \
                         RETURNING version, enabled, generation, enrollment_confirmed, \
                         enrollment_actor IS NOT NULL, enrollment_at IS NOT NULL",
                        &[&self.key.as_str(), &version]
                    )).await?;
                        self.decode(&inserted, &database, what)?
                    }
                },
            };
        if matches!(operation, Operation::Approve(_) | Operation::Enable(_)) {
            // A helper owned by a LOGIN/operator role is never an activation prerequisite.
            let published = run(deadline, &database, what, conn.query_one(
                "SELECT (SELECT count(*) = 4 AND bool_and(p.prosecdef AND NOT r.rolcanlogin AND NOT r.rolsuper AND NOT r.rolcreaterole AND NOT r.rolcreatedb AND NOT r.rolbypassrls) FROM pg_catalog.pg_proc p JOIN pg_catalog.pg_roles r ON r.oid=p.proowner WHERE p.oid IN ('public.erp_node_feature_snapshot(text)'::regprocedure,'public.erp_node_feature_member(text,integer,uuid,bytea)'::regprocedure,'public.erp_node_feature_protect(text,integer,uuid,bytea,bigint,boolean)'::regprocedure,'public.erp_node_feature_guard()'::regprocedure)) AND EXISTS (SELECT FROM pg_catalog.pg_trigger WHERE tgrelid='public.erp_node'::regclass AND tgname='erp_node_feature_guard' AND tgfoid='public.erp_node_feature_guard()'::regprocedure AND tgenabled='A' AND NOT tgisinternal)", &[])).await?;
            if !get::<bool>(&published, 0, &database, what)? {
                return Err(self.refused(
                    &database,
                    "feature helpers and membership trigger are not safely published",
                ));
            }
            let trigger = run(
                deadline,
                &database,
                what,
                conn.query_opt(super::grant::TRIGGER_IDENTITY_SQL, &[&true]),
            )
            .await?;
            match trigger {
                Some(row) if get::<bool>(&row, 0, &database, what)? => {}
                _ => {
                    return Err(self.refused(
                        &database,
                        "feature helpers and membership trigger are not safely published",
                    ));
                }
            }
            for signature in super::grant::FUNCTIONS {
                let (volatility, result) =
                    super::grant::expected_shape(signature).map_err(|_| {
                        self.refused(&database, "built-in feature helper signature is missing")
                    })?;
                let row = run(
                    deadline,
                    &database,
                    what,
                    conn.query_one(
                        super::grant::HELPER_IDENTITY_SQL,
                        &[signature, &volatility, &result],
                    ),
                )
                .await?;
                let actual: String = get(&row, 0, &database, what)?;
                let expected = super::grant::expected_source(signature).map_err(|_| {
                    self.refused(&database, "built-in feature helper source is missing")
                })?;
                let owner = run(
                    deadline,
                    &database,
                    what,
                    conn.query_one(super::grant::OWNER_IDENTITY_SQL, &[signature, &false]),
                )
                .await?;
                if !get::<bool>(&owner, 1, &database, what)? {
                    return Err(
                        self.refused(&database, "feature helper owner is not safely published")
                    );
                }
                if actual != expected || !get::<bool>(&row, 1, &database, what)? {
                    return Err(self.refused(
                        &database,
                        "published feature helper differs from this binary",
                    ));
                }
            }
        }
        if let Operation::Approve(digest) = operation {
            if status.enabled {
                return Err(
                    self.refused(&database, "disable the feature before approving a digest")
                );
            }
            let bytes: &[u8] = digest.as_bytes();
            let row=run(deadline,&database,what,conn.query_one(
                "SELECT EXISTS(SELECT FROM public.erp_node n WHERE n.digest=$1 AND n.state <> 'stopped' AND n.expires_at > clock_timestamp() AND n.capabilities->$2 = to_jsonb($3::int4)) AND NOT EXISTS(SELECT FROM public.erp_node n WHERE n.digest=$1 AND n.state <> 'stopped' AND n.expires_at > clock_timestamp()-n.lease_ms*interval '1 millisecond' AND n.capabilities->$2 IS DISTINCT FROM to_jsonb($3::int4))",
                &[&bytes,&self.key.as_str(),&self.version.get()])).await?;
            if !get::<bool>(&row, 0, &database, what)? {
                return Err(self.refused(
                    &database,
                    "digest has no fresh compatible membership declaration",
                ));
            }
            let row=run(deadline,&database,what,conn.query_one("SELECT count(*)::int8, coalesce(bool_or(digest=$2),false) FROM public.erp_node_feature_digest WHERE feature=$1",&[&self.key.as_str(),&bytes])).await?;
            let count: i64 = get(&row, 0, &database, what)?;
            let exists: bool = get(&row, 1, &database, what)?;
            if !exists && count >= 32 {
                return Err(self.refused(&database, "a feature approves at most 32 digests"));
            }
            run(deadline,&database,what,conn.execute("INSERT INTO public.erp_node_feature_digest(feature,digest,version) VALUES($1,$2,$3) ON CONFLICT(feature,digest) DO NOTHING",&[&self.key.as_str(),&bytes,&self.version.get()])).await?;
        }
        if matches!(operation, Operation::Enable(_)) {
            let row=run(deadline,&database,what,conn.query_opt(
                "SELECT n.id::text,encode(n.digest,'hex') FROM public.erp_node n WHERE n.state <> 'stopped' AND n.expires_at > clock_timestamp()-n.lease_ms*interval '1 millisecond' AND (n.capabilities->$1 IS DISTINCT FROM to_jsonb($2::int4) OR NOT EXISTS(SELECT FROM public.erp_node_feature_digest d WHERE d.feature=$1 AND d.version=$2 AND d.digest=n.digest)) ORDER BY n.id LIMIT 1",
                &[&self.key.as_str(),&self.version.get()])).await?;
            if let Some(row) = row {
                let node: String = get(&row, 0, &database, what)?;
                let digest: String = get(&row, 1, &database, what)?;
                return Err(self.refused(&database,format!("incompatible fresh or recently expired membership exists: node {node} digest {digest}")));
            }
            let approved=run(deadline,&database,what,conn.query_one("SELECT EXISTS(SELECT FROM public.erp_node_feature_digest WHERE feature=$1 AND version=$2)",&[&self.key.as_str(),&self.version.get()])).await?;
            if !get::<bool>(&approved, 0, &database, what)? {
                return Err(self.refused(
                    &database,
                    "activation requires at least one approved digest",
                ));
            }
            if !status.enabled {
                let generation = status
                    .generation
                    .checked_add(1)
                    .ok_or_else(|| self.refused(&database, "feature generation is exhausted"))?;
                let row=run(deadline,&database,what,conn.query_one("UPDATE public.erp_node_feature SET enabled=true,generation=$2,enrollment_confirmed=true,enrollment_actor=session_user,enrollment_at=clock_timestamp() WHERE key=$1 RETURNING version,enabled,generation,enrollment_confirmed,enrollment_actor IS NOT NULL,enrollment_at IS NOT NULL",&[&self.key.as_str(),&generation])).await?;
                status = self.decode(&row, &database, what)?;
            }
        }
        if matches!(operation, Operation::Disable) && status.enabled {
            let generation = status
                .generation
                .checked_add(1)
                .ok_or_else(|| self.refused(&database, "feature generation is exhausted"))?;
            let row = run(
                deadline,
                &database,
                what,
                conn.query_one(
                    "UPDATE public.erp_node_feature SET enabled = false, generation = $2 \
                 WHERE key = $1 RETURNING version, enabled, generation, enrollment_confirmed, \
                 enrollment_actor IS NOT NULL, enrollment_at IS NOT NULL",
                    &[&self.key.as_str(), &generation],
                ),
            )
            .await?;
            status = self.decode(&row, &database, what)?;
        }
        let approvals=run(deadline,&database,what,conn.query("SELECT digest,version FROM public.erp_node_feature_digest WHERE feature=$1 ORDER BY digest LIMIT 33",&[&self.key.as_str()])).await?;
        if approvals.len() > 32 {
            return Err(self.refused(&database, "a feature approves at most 32 digests"));
        }
        for approval in approvals {
            let bytes: Vec<u8> = get(&approval, 0, &database, what)?;
            let version: i32 = get(&approval, 1, &database, what)?;
            if version != self.version.get() {
                return Err(self.refused(
                    &database,
                    "stored digest approval version differs from this binary",
                ));
            }
            let digest = Digest::from_slice(&bytes).ok_or_else(|| {
                self.refused(&database, "stored digest approval must contain 32 bytes")
            })?;
            status.approved.push(digest);
        }
        let row=run(deadline,&database,what,conn.query_one("SELECT count(*) FROM public.erp_node n WHERE n.state<>'stopped' AND n.expires_at>clock_timestamp()-n.lease_ms*interval '1 millisecond' AND (n.capabilities->$1 IS DISTINCT FROM to_jsonb($2::int4) OR NOT EXISTS(SELECT FROM public.erp_node_feature_digest d WHERE d.feature=$1 AND d.digest=n.digest AND d.version=$2))",&[&self.key.as_str(),&self.version.get()])).await?;
        let incompatible: i64 = get(&row, 0, &database, what)?;
        status.incompatible = u64::try_from(incompatible)
            .map_err(|_| self.refused(&database, "incompatible membership count is negative"))?;
        match operation {
            Operation::Status => {
                run(deadline, &database, what, conn.batch_execute("COMMIT")).await?
            }
            Operation::Register
            | Operation::Disable
            | Operation::Approve(_)
            | Operation::Enable(_) => {
                let acknowledgment =
                    match tokio::time::timeout_at(deadline, conn.batch_execute("COMMIT")).await {
                        Ok(Ok(())) => CommitAck::Confirmed,
                        Ok(Err(error)) => CommitAck::Failed(error),
                        Err(_) => CommitAck::Unknown,
                    };
                #[cfg(feature = "test-faults")]
                let acknowledgment = if self.lose_commit_ack {
                    match acknowledgment {
                        CommitAck::Confirmed => CommitAck::Unknown,
                        other => other,
                    }
                } else {
                    acknowledgment
                };
                match acknowledgment {
                    CommitAck::Confirmed => {}
                    CommitAck::Failed(error) => {
                        let mut error = NodeError::pg(&database, what, error);
                        if let NodeError::Database { message, .. } = &mut error {
                            message.push_str("; outcome unknown; reconcile status");
                        }
                        return Err(error);
                    }
                    CommitAck::Unknown => {
                        return Err(NodeError::Database {
                            database,
                            what,
                            message:
                                "commit acknowledgment timed out; outcome unknown; reconcile status"
                                    .to_owned(),
                            sqlstate: None,
                            source: None,
                        });
                    }
                }
            }
        }
        conn.recycle();
        Ok(status)
    }
}

fn timed_out(database: &str, what: &'static str) -> NodeError {
    NodeError::Timeout {
        database: database.to_owned(),
        what,
        after: OPERATOR_BOUND,
    }
}

async fn run<T>(
    deadline: Instant,
    database: &str,
    what: &'static str,
    future: impl Future<Output = Result<T, tokio_postgres::Error>>,
) -> Result<T, NodeError> {
    match tokio::time::timeout_at(deadline, future).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(error)) => Err(NodeError::pg(database, what, error)),
        Err(_) => Err(timed_out(database, what)),
    }
}
