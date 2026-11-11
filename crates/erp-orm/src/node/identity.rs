//! The process's identity: id, name, role, options and digests (design §5, §8).

use std::sync::{Arc, Mutex, PoisonError};

use deadpool_postgres::Pool;

use super::digest::{
    AddonsDigest, BuildIdentity, ConfigDigest, Digest, ResolvedBuild, SectionDigest,
    SharedSettings, combine,
};
use super::feature::FeatureSet;
use super::heartbeat::{Conn, with_grace};
use super::{NodeError, NodeId, NodeOptions, NodeRole};

/// One per process, shared by all its memberships ([`NodeMembership`](super::NodeMembership)).
///
/// ```no_run
/// # async fn example(pool: deadpool_postgres::Pool) -> Result<(), erp_orm::node::NodeError> {
/// use erp_orm::node::{AddonsDigest, NodeIdentity, NodeOptions, NodeRole};
/// let build = erp_orm::build_identity!();
/// let addons = AddonsDigest::builder().build()?;
/// let identity = NodeIdentity::builder(NodeRole::App, "app-1", "host-1", build, addons)
///     .options(NodeOptions::default())
///     .create(&pool)
///     .await?;
/// println!("node {} digest {}", identity.id(), identity.digest());
/// # Ok(())
/// # }
/// ```
#[derive(Debug)]
pub struct NodeIdentity {
    id: NodeId,
    name: String,
    host: String,
    pid: i32,
    role: NodeRole,
    dev: bool,
    options: NodeOptions,
    digest: Digest,
    config: ConfigDigest,
    build: ResolvedBuild,
    addons: AddonsDigest,
    features: FeatureSet,
    capabilities: serde_json::Value,
    details: Vec<(&'static str, Vec<u8>)>,
    /// Databases where a registration of this identity sent its commit and lost the answer: the
    /// next registration there may adopt the row it committed (design §4).
    pending: Mutex<Vec<String>>,
    /// Databases where this process holds a membership of this identity: one per database
    /// ([`MembershipClaim`]).
    active: Mutex<Vec<String>>,
}

impl NodeIdentity {
    /// Start an identity: `name` is the operator's label (1–63 bytes of `[A-Za-z0-9._-]`, may
    /// repeat), `host` the host name (1–255 bytes without control characters: it is shown in
    /// messages).
    pub fn builder(
        role: NodeRole,
        name: &str,
        host: &str,
        build: BuildIdentity,
        addons: AddonsDigest,
    ) -> NodeIdentityBuilder {
        NodeIdentityBuilder {
            role,
            name: name.to_owned(),
            host: host.to_owned(),
            build,
            addons,
            features: FeatureSet::default(),
            options: NodeOptions::default(),
            sections: Vec::new(),
            dev: false,
        }
    }

    /// The node id.
    pub fn id(&self) -> NodeId {
        self.id
    }

    /// The node name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The host.
    pub fn host(&self) -> &str {
        &self.host
    }

    /// The process id.
    pub fn pid(&self) -> u32 {
        self.pid.unsigned_abs()
    }

    pub(crate) fn pid_i32(&self) -> i32 {
        self.pid
    }

    /// The role.
    pub fn role(&self) -> NodeRole {
        self.role
    }

    /// Whether this is an `erp dev` node.
    pub fn is_dev(&self) -> bool {
        self.dev
    }

    /// The registry's settings (also section `node` of the configuration digest).
    pub fn options(&self) -> &NodeOptions {
        &self.options
    }

    /// The combined digest.
    pub fn digest(&self) -> Digest {
        self.digest
    }

    /// The configuration digest.
    pub fn config(&self) -> &ConfigDigest {
        &self.config
    }

    /// The resolved build.
    pub fn build(&self) -> &ResolvedBuild {
        &self.build
    }

    /// The addons digest.
    pub fn addons(&self) -> &AddonsDigest {
        &self.addons
    }

    /// Binary capabilities bound into this identity's combined digest.
    ///
    /// Declarations alone grant no admission; the database rollout protocol validates membership.
    pub fn features(&self) -> &FeatureSet {
        &self.features
    }

    /// The bounded capability map, encoded once at identity creation and borrowed by every write.
    pub(crate) fn capabilities(&self) -> &serde_json::Value {
        &self.capabilities
    }

    pub(crate) fn details(&self) -> &[(&'static str, Vec<u8>)] {
        &self.details
    }

    fn pending(&self) -> std::sync::MutexGuard<'_, Vec<String>> {
        // a panic while holding it cannot leave the list inconsistent: each change is one push
        // or one retain
        self.pending.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// A registration in `database` is about to send its commit.
    pub(crate) fn mark_pending(&self, database: &str) {
        let mut pending = self.pending();
        if !pending.iter().any(|d| d == database) {
            pending.push(database.to_owned());
        }
    }

    /// The registration in `database` is known to have committed or not.
    pub(crate) fn clear_pending(&self, database: &str) {
        self.pending().retain(|d| d != database);
    }

    /// Whether an earlier registration in `database` lost its commit answer.
    pub(crate) fn is_pending(&self, database: &str) -> bool {
        self.pending().iter().any(|d| d == database)
    }

    fn active(&self) -> std::sync::MutexGuard<'_, Vec<String>> {
        // as `pending`: each change is one push or one retain
        self.active.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// This process's one membership of an identity in one database (design §4): taken by a
/// registration once it knows the database, held by the [`NodeMembership`](super::NodeMembership)
/// owner handle, and given back when that handle is dropped (after a shutdown, or instead of
/// one). A second registration of the identity there is refused while it is held, before it can
/// touch the first one's row (an expired row would otherwise be replaced under a membership whose
/// task is only starved).
#[derive(Debug)]
pub(crate) struct MembershipClaim {
    identity: Arc<NodeIdentity>,
    database: String,
}

impl MembershipClaim {
    /// The claim on `database`, or `None` when this process holds one there already.
    pub(crate) fn take(identity: &Arc<NodeIdentity>, database: &str) -> Option<Self> {
        let mut active = identity.active();
        if active.iter().any(|d| d == database) {
            return None;
        }
        active.push(database.to_owned());
        drop(active);
        Some(Self {
            identity: Arc::clone(identity),
            database: database.to_owned(),
        })
    }
}

impl Drop for MembershipClaim {
    fn drop(&mut self) {
        let database = &self.database;
        self.identity.active().retain(|d| d != database);
    }
}

/// Builds a [`NodeIdentity`].
#[derive(Debug)]
pub struct NodeIdentityBuilder {
    role: NodeRole,
    name: String,
    host: String,
    build: BuildIdentity,
    addons: AddonsDigest,
    features: FeatureSet,
    options: NodeOptions,
    sections: Vec<SectionDigest>,
    dev: bool,
}

impl NodeIdentityBuilder {
    /// Declare trusted binary capabilities (default empty), bound into the combined digest.
    ///
    /// The reserved `node_features` settings section is always present, including for an empty
    /// set. Adding this section changes identities from binaries predating feature declarations;
    /// existing database identities are not rewritten. Operator settings cannot replace it.
    #[must_use]
    pub fn features(mut self, features: FeatureSet) -> Self {
        self.features = features;
        self
    }

    /// Describe a settings section into the configuration digest.
    ///
    /// # Errors
    ///
    /// The errors of [`SectionDigest::of`].
    pub fn settings(mut self, settings: &dyn SharedSettings) -> Result<Self, NodeError> {
        self.sections.push(SectionDigest::of(settings)?);
        Ok(self)
    }

    /// The registry's settings (default [`NodeOptions::default`]); described as section `node`.
    #[must_use]
    pub fn options(mut self, options: NodeOptions) -> Self {
        self.options = options;
        self
    }

    /// Mark the identity as an `erp dev` node: its registration is refused where non-dev nodes
    /// are live (design §17 decision 5).
    #[must_use]
    pub fn dev(mut self, dev: bool) -> Self {
        self.dev = dev;
        self
    }

    /// Resolve the digests (the build's on a blocking thread: it may hash the executable) and
    /// take a node id from the database (`SELECT uuidv7()`), within one interval and its grace.
    ///
    /// # Errors
    ///
    /// [`NodeError::InvalidSetting`] for an invalid name or host; [`NodeError::DuplicateSetting`]
    /// when a section was described twice (`node` included); [`NodeError::BuildIdentity`];
    /// [`NodeError::Timeout`] when the id is not read in time; [`NodeError::Database`] when it
    /// cannot be read.
    pub async fn create(self, pool: &Pool) -> Result<NodeIdentity, NodeError> {
        let options = self.options;
        let resolved = self.resolve().await?;
        const WHAT: &str = "taking a node id";
        let after = with_grace(options.interval());
        let deadline = tokio::time::Instant::now() + after;
        let timed_out = || NodeError::Timeout {
            database: String::new(),
            what: WHAT,
            after,
        };
        let client = match tokio::time::timeout_at(deadline, pool.get()).await {
            Ok(Ok(client)) => Conn::new(client),
            Ok(Err(e)) => return Err(NodeError::pool("", WHAT, e)),
            Err(_) => return Err(timed_out()),
        };
        let row =
            match tokio::time::timeout_at(deadline, client.query_one("SELECT uuidv7()::text", &[]))
                .await
            {
                Ok(row) => row.map_err(|e| NodeError::pg("", WHAT, e))?,
                // the guard discards the connection: it may still be mid-statement
                Err(_) => return Err(timed_out()),
            };
        client.recycle();
        let text: String = row
            .try_get(0)
            .map_err(|e| NodeError::undecodable("", WHAT, e))?;
        let id = NodeId::parse(&text).ok_or_else(|| NodeError::undecodable("", WHAT, &text))?;
        Ok(resolved.with_id(id))
    }

    /// Benchmark seam: [`create`](Self::create) without a database, under a fixed version-7
    /// id. Such an identity never registers anywhere.
    ///
    /// # Errors
    ///
    /// The errors of [`create`](Self::create) other than those of taking the id.
    #[cfg(any(feature = "test-faults", feature = "bench-seams"))]
    #[doc(hidden)]
    pub async fn create_detached_for_benches(self) -> Result<NodeIdentity, NodeError> {
        let mut bytes = [0u8; 16];
        bytes[6] = 0x70;
        Ok(self.resolve().await?.with_id(NodeId(bytes)))
    }

    /// Check the name and host, and resolve every digest and detail.
    async fn resolve(self) -> Result<Resolved, NodeError> {
        let name_ok = !self.name.is_empty()
            && self.name.len() <= 63
            && self
                .name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
        if !name_ok {
            return Err(NodeError::invalid_setting(
                "node.name",
                format!(
                    "invalid node name {:?}: 1 to 63 bytes of [A-Za-z0-9._-]",
                    self.name
                ),
            ));
        }
        if self.host.is_empty() || self.host.len() > 255 || self.host.chars().any(char::is_control)
        {
            return Err(NodeError::invalid_setting(
                "node.host",
                format!(
                    "invalid host {:?}: 1 to 255 bytes without control characters",
                    self.host
                ),
            ));
        }
        let mut sections = self.sections;
        sections.push(SectionDigest::of(&self.options)?);
        sections.push(SectionDigest::of(&self.features)?);
        let config = ConfigDigest::from_sections(sections)?;
        let unresolved = self.build;
        let build = tokio::task::spawn_blocking(move || unresolved.resolve())
            .await
            .map_err(|e| NodeError::BuildIdentity {
                message: format!("resolving the build identity failed: {e}"),
                source: None,
            })??;
        let digest = combine(&config.digest(), &build.digest(), &self.addons.digest());
        let details = vec![
            ("config_sections", config.detail()?),
            ("addons", self.addons.detail()?),
            ("build", build.detail()?),
        ];
        let pid = i32::try_from(std::process::id()).map_err(|_| {
            NodeError::invalid_setting("node.pid", "the process id does not fit in int4")
        })?;
        Ok(Resolved {
            name: self.name,
            host: self.host,
            pid,
            role: self.role,
            dev: self.dev,
            options: self.options,
            digest,
            config,
            build,
            addons: self.addons,
            features: self.features,
            details,
        })
    }
}

/// An identity without its id yet.
struct Resolved {
    name: String,
    host: String,
    pid: i32,
    role: NodeRole,
    dev: bool,
    options: NodeOptions,
    digest: Digest,
    config: ConfigDigest,
    build: ResolvedBuild,
    addons: AddonsDigest,
    features: FeatureSet,
    details: Vec<(&'static str, Vec<u8>)>,
}

impl Resolved {
    fn with_id(self, id: NodeId) -> NodeIdentity {
        NodeIdentity {
            id,
            name: self.name,
            host: self.host,
            pid: self.pid,
            role: self.role,
            dev: self.dev,
            options: self.options,
            digest: self.digest,
            config: self.config,
            build: self.build,
            addons: self.addons,
            capabilities: self.features.capabilities_json(),
            features: self.features,
            details: self.details,
            pending: Mutex::new(Vec::new()),
            active: Mutex::new(Vec::new()),
        }
    }
}
